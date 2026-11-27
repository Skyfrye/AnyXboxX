//! The D3D12 runtime's background shader threads, as the host finds them,
//! and where they may run ([`confine`]).
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

/// What the runtime names its background threads (followed by a number).
pub(crate) const NAME: &str = "D3D Background Thread";

/// Calls `f` with the id and a handle (query rights, closed after) of
/// each thread of this process whose name starts with `prefix`.
pub(crate) fn each_thread_named(prefix: &str, mut f: impl FnMut(u32, windows_sys::Win32::Foundation::HANDLE)) {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE, LocalFree};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcessId, GetThreadDescription, OpenThread, THREAD_QUERY_LIMITED_INFORMATION};
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

/// How many cores the runtime's background threads are kept on
/// (`VICTORY_BACKGROUND_CORES=<n>`, the first n cores' logical processors;
/// unset or 0: wherever the system runs them).
///
/// Why: in the opening cutscene on the 9800X3D (8 cores, 16 threads) they
/// took 2.6-2.9 cores for minutes (run p1-full2, threadwatch: 26-29 s of CPU
/// in every 10 s, three minutes in), while the process as a whole was on
/// 11.7 of the 16 threads. Idle priority gives them only what is idle, but
/// an idle thread of a core runs beside a busy one and takes half of its
/// units: the submitting thread, which the frame waited for (38 ms of a 40
/// ms frame), and the threads its refreshes are shared with ran beside them.
/// Kept on one core they still get its idle time; the driver's recompiles
/// (better shaders for the card, which is not the limit there) come later.
fn cores_asked() -> Option<usize> {
    static ASKED: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    let asked = *ASKED.get_or_init(|| std::env::var("VICTORY_BACKGROUND_CORES").ok()?.trim().parse::<usize>().ok().filter(|&n| n > 0));
    // Diagnostics: on and off while the title runs (`crate::ab`), one core
    // unless the variable says.
    #[cfg(feature = "diagnostics")]
    if crate::ab::bit("VICTORY_BACKGROUND_CORES").is_some() {
        return Some(asked.unwrap_or(1));
    }
    asked
}

/// The mask the threads get now: the first cores', or (switched off while
/// the title runs, `crate::ab`) the process's own.
fn mask_now(first: usize) -> usize {
    #[cfg(feature = "diagnostics")]
    if crate::ab::pick("VICTORY_BACKGROUND_CORES") == Some(false) {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessAffinityMask};
        let (mut process, mut system) = (0usize, 0usize);
        // SAFETY: our own process; out pointers ours.
        if unsafe { GetProcessAffinityMask(GetCurrentProcess(), &mut process, &mut system) } != 0 {
            return process;
        }
    }
    first
}

/// Keeps the threads named `prefix...` that are not in `kept` on the
/// processors of `mask` (an affinity, which the system holds to), and adds
/// them to `kept`; how many refused.
fn keep_named(prefix: &str, mask: usize, kept: &mut Vec<u32>) -> usize {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenThread, SetThreadAffinityMask, THREAD_QUERY_INFORMATION, THREAD_SET_INFORMATION};
    let mut refused = 0;
    each_thread_named(prefix, |tid, _| {
        if kept.contains(&tid) {
            return;
        }
        // SAFETY: a thread of our own process, by its id; the handle is
        // closed here; the mask names processors of group 0.
        let set = unsafe {
            let thread = OpenThread(THREAD_SET_INFORMATION | THREAD_QUERY_INFORMATION, 0, tid);
            if thread.is_null() {
                0
            } else {
                let was = SetThreadAffinityMask(thread, mask);
                CloseHandle(thread);
                was
            }
        };
        if set != 0 {
            kept.push(tid);
        } else {
            refused += 1;
        }
    });
    refused
}

/// With [`cores_asked`]: a thread of ours keeps the runtime's background
/// threads on those cores, looking for new ones every few seconds (the
/// runtime makes them with the device, and may make more later).
pub(crate) fn confine() {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority};
    let Some(cores) = cores_asked() else {
        return;
    };
    let mask = crate::cores::first_cores_mask(cores);
    if mask == 0 {
        tracing::warn!(cores, "the runtime's background threads are left where they are (VICTORY_BACKGROUND_CORES): no such cores");
        return;
    }
    let spawned = std::thread::Builder::new().name("victory-background-cores".into()).spawn(move || {
        // SAFETY: this thread's own priority.
        unsafe { SetThreadPriority(GetCurrentThread(), crate::bands::BACKGROUND) };
        let mut kept: Vec<u32> = Vec::new();
        let began = std::time::Instant::now();
        let (mut was, mut said) = (0, 0);
        loop {
            let mask = mask_now(mask);
            if mask != was {
                // Every thread again (`crate::ab`).
                kept.clear();
                was = mask;
            }
            let before = kept.len();
            let refused = keep_named(NAME, mask, &mut kept);
            if kept.len() > before || refused > said {
                said = said.max(refused);
                tracing::info!(
                    threads = kept.len(),
                    refused,
                    cores,
                    mask = format_args!("{mask:#x}"),
                    "the runtime's background threads are kept on the first cores (VICTORY_BACKGROUND_CORES)"
                );
            }
            // Often while the title starts, then now and then (with
            // `crate::ab` in step with its phases).
            let every = if began.elapsed() < std::time::Duration::from_secs(60) { 2000 } else { 10_000 };
            #[cfg(feature = "diagnostics")]
            let every = if crate::ab::bit("VICTORY_BACKGROUND_CORES").is_some() { 250 } else { every };
            std::thread::sleep(std::time::Duration::from_millis(every));
        }
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "no thread to keep the runtime's background threads on their cores");
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetThreadPriority, GetThreadTimes, THREAD_PRIORITY_IDLE};

    pub(crate) use super::each_thread_named;
    use super::NAME;

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

    /// Threads named so are kept on the processors asked for, each once,
    /// and the others are left alone.
    #[test]
    fn threads_named_so_are_kept_on_the_cores_asked_for() {
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, GetProcessAffinityMask, OpenThread, SetThreadAffinityMask, THREAD_QUERY_INFORMATION,
            THREAD_SET_INFORMATION,
        };
        let (stop, wait) = std::sync::mpsc::channel::<()>();
        let wait = std::sync::Arc::new(std::sync::Mutex::new(wait));
        let spawn = |name: &str| {
            let wait = wait.clone();
            std::thread::Builder::new()
                .name(name.into())
                .spawn(move || {
                    let _ = wait.lock().unwrap().recv();
                })
                .unwrap()
        };
        let (a, b, other) = (spawn("victory-test-keep kept 1"), spawn("victory-test-keep kept 2"), spawn("victory-test-keep other"));
        std::thread::sleep(std::time::Duration::from_millis(100));
        let (mut process, mut system) = (0usize, 0usize);
        // SAFETY: our own process; out pointers ours.
        assert_ne!(unsafe { GetProcessAffinityMask(GetCurrentProcess(), &mut process, &mut system) }, 0);
        let mask = crate::cores::first_cores_mask(1);
        assert!(mask != 0 && mask & process == mask);
        let mut kept = Vec::new();
        assert_eq!(super::keep_named("victory-test-keep kept", mask, &mut kept), 0);
        assert_eq!(kept.len(), 2);
        // Asked again: nothing new.
        assert_eq!(super::keep_named("victory-test-keep kept", mask, &mut kept), 0);
        assert_eq!(kept.len(), 2);
        // What each thread's affinity was: the mask for those kept, the
        // process's for the other.
        let mut was = Vec::new();
        each_thread_named("victory-test-keep ", |tid, _| {
            // SAFETY: a thread of ours by its id, closed here.
            unsafe {
                let thread = OpenThread(THREAD_SET_INFORMATION | THREAD_QUERY_INFORMATION, 0, tid);
                was.push((kept.contains(&tid), SetThreadAffinityMask(thread, process)));
                windows_sys::Win32::Foundation::CloseHandle(thread);
            }
        });
        was.sort_unstable();
        assert_eq!(was, [(false, process), (true, mask), (true, mask)]);
        drop(stop);
        for t in [a, b, other] {
            let _ = t.join();
        }
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
