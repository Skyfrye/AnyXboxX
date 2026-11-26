//! Tests: the D3D12 runtime's background shader threads, as the host finds
//! them.
//!
//! The runtime runs the driver's background work (recompiling shaders it has
//! measured, for minutes after pipelines were made or loaded) on four "D3D
//! Background Thread"s in our process. On the previous base they were 2.3
//! cores in a profile of the world, and on this one 3-3.7 cores for the first
//! four and a half minutes of a warm run (RTX 5080). The previous base put
//! them at idle priority from a thread of its own when the runtime refused
//! to switch that work off (`VICTORY_D3D12_BACKGROUND=0`, refused without
//! Windows' Developer Mode). That is not ported: the runtime makes them at
//! idle priority itself and keeps them there while they work
//! (`the_runtimes_background_threads_run_at_idle_priority` on the RTX
//! 5080: -15 at the device's creation, and in every sample while the driver
//! worked on 200 new pipelines, 1.2-1.7 s of CPU each), so it changed
//! nothing. What they cost the title's threads is what idle priority does
//! not keep away: the other half of a core they share, memory and the CPU's
//! power budget.

#[cfg(test)]
pub(crate) mod tests {
    use windows_sys::Win32::Foundation::{
        CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessId, GetThreadDescription, GetThreadPriority, GetThreadTimes, OpenThread,
        THREAD_PRIORITY_IDLE, THREAD_QUERY_LIMITED_INFORMATION,
    };

    /// What the runtime names its background threads (followed by a number).
    const NAME: &str = "D3D Background Thread";

    /// Calls `f` with the id and a handle (query rights, closed after) of
    /// each thread of this process whose name starts with `prefix`.
    pub(crate) fn each_thread_named(prefix: &str, mut f: impl FnMut(u32, HANDLE)) {
        // SAFETY: plain Win32 calls; the snapshot and thread handles are
        // closed below, the entry is sized as the call asks, and the
        // description is freed as `GetThreadDescription` says.
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return;
            }
            let pid = GetCurrentProcessId();
            let mut entry: THREADENTRY32 = std::mem::zeroed();
            entry.dwSize = size_of::<THREADENTRY32>() as u32;
            let mut more = Thread32First(snapshot, &mut entry) != 0;
            while more {
                let tid = entry.th32ThreadID;
                if entry.th32OwnerProcessID == pid {
                    let thread = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, tid);
                    if !thread.is_null() {
                        let mut name: *mut u16 = std::ptr::null_mut();
                        if GetThreadDescription(thread, &mut name) >= 0 && !name.is_null() {
                            let len = (0..).take_while(|&i| *name.add(i) != 0).count();
                            let text =
                                String::from_utf16_lossy(std::slice::from_raw_parts(name, len));
                            LocalFree(name.cast());
                            if text.starts_with(prefix) {
                                f(tid, thread);
                            }
                        }
                        CloseHandle(thread);
                    }
                }
                more = Thread32Next(snapshot, &mut entry) != 0;
            }
            CloseHandle(snapshot);
        }
    }

    /// Each of the runtime's background threads: its id, its priority and
    /// the CPU time it has used (ms).
    fn background_threads() -> Vec<(u32, i32, u64)> {
        let ms =
            |t: FILETIME| (u64::from(t.dwHighDateTime) << 32 | u64::from(t.dwLowDateTime)) / 10_000;
        let mut out = Vec::new();
        each_thread_named(NAME, |tid, thread| {
            let zero = FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            };
            let (mut made, mut ended, mut kernel, mut user) = (zero, zero, zero, zero);
            // SAFETY: an open handle with query rights; out pointers ours.
            let (priority, _) = unsafe {
                (
                    GetThreadPriority(thread),
                    GetThreadTimes(thread, &mut made, &mut ended, &mut kernel, &mut user),
                )
            };
            out.push((tid, priority, ms(kernel) + ms(user)));
        });
        out
    }

    /// A thread named so is found by its name, with its priority.
    #[test]
    fn threads_are_found_by_their_name() {
        let (stop, wait) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("victory-test Background Thread 9".into())
            .spawn(move || {
                let _ = wait.recv();
            })
            .unwrap();
        // Give it time to start, so that it is in the snapshot.
        std::thread::sleep(std::time::Duration::from_millis(100));
        let mut found = Vec::new();
        // SAFETY (in the closure): an open handle with query rights.
        each_thread_named("victory-test Background Thread", |_, h| {
            found.push(unsafe { GetThreadPriority(h) })
        });
        assert_eq!(found, [0], "one thread, at normal priority");
        drop(stop);
        let _ = thread.join();
    }

    /// On the real card: the runtime makes its background threads with the
    /// device at idle priority, and they stay there while the driver works
    /// on 200 new pipelines (on the RTX 5080: four threads at -15, with
    /// 1.2-1.7 s of CPU each the first time, tens of ms once the driver's
    /// own cache held those pipelines). Nothing of ours needs to lower them.
    /// Run by hand, `--nocapture` shows them:
    /// `cargo test -p victory-gpu --lib background -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs the real D3D12 device"]
    fn the_runtimes_background_threads_run_at_idle_priority() {
        use windows::Win32::Graphics::Direct3D12::*;
        let host = crate::host::host().expect("a host device");
        let first = background_threads();
        eprintln!("with the device: {first:?}");
        assert!(!first.is_empty(), "no thread named {NAME:?}");
        let mut kept = Vec::new();
        for n in 0..200u32 {
            let source = format!(
                r#"
#define RS "UAV(u0)"
RWByteAddressBuffer sum : register(u0);
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {{
    float x = id.x * {n}.5;
    float y = 0;
    [loop] for (uint i = 0; i < 64u + {n}u; i++) {{
        x = sin(x) * {n}.25 + cos(y);
        y = sqrt(abs(x)) + exp(-y) * {n}.0;
        if (x > y) y = log(abs(x) + 1);
    }}
    sum.Store(id.x * 4u, asuint(x + y));
}}
"#
            );
            let code = crate::host::compile(
                &source,
                windows::core::s!("main"),
                windows::core::s!("cs_5_1"),
            )
            .expect("compiled");
            // SAFETY: plain object creation; the code lives through the calls.
            unsafe {
                let root: ID3D12RootSignature = host
                    .device
                    .CreateRootSignature(0, &code)
                    .expect("a root signature");
                let pso: ID3D12PipelineState = host
                    .device
                    .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                        pRootSignature: std::mem::transmute_copy(&root),
                        CS: D3D12_SHADER_BYTECODE {
                            pShaderBytecode: code.as_ptr().cast(),
                            BytecodeLength: code.len(),
                        },
                        ..Default::default()
                    })
                    .expect("a pipeline");
                kept.push((root, pso));
            }
        }
        let mut seen = first;
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(250));
            let now = background_threads();
            eprintln!("{now:?}");
            seen.extend(now);
        }
        assert!(
            seen.iter()
                .all(|&(_, priority, _)| priority == THREAD_PRIORITY_IDLE),
            "a background thread above idle priority: {seen:?}"
        );
    }
}
