//! Who owns the process's private memory, in every build.
//!
//! The panel's "RAM" is the process's private bytes: 30.0 GB in run
//! pic-combo against 10.5 GB at the title screen, with 15-22 GB of it in
//! memory on a PC of 31 GB, so Windows pages. The diagnostics memory line
//! (`crate::copystat`) says much of what holds it, but only in diagnostics
//! builds, and the runs that are measured are release builds. Here, in
//! every build:
//!
//! - [`tick`] (once a second from the host's sweep) logs a line every
//!   `VICTORY_RAM_LINE` seconds (10 if unset, `0`: never) that splits the
//!   private bytes by owner: the title's own commits, asked of Windows
//!   ([`title_allocation`]), and the title memory given out of our heap
//!   ([`title_from_ours`]); our buffers in the PC's memory by heap, counted
//!   while they live ([`track`]); what holds them (host copies of title
//!   memory, texture staging, lists' scratch, sends); host lists and how
//!   long their recordings were; the process heap; and the rest: the
//!   backing the driver keeps for the card's own memory, and its arenas.
//! - `VICTORY_RAM_MAP=<seconds>` (off by default) walks the address space
//!   on a thread of its own that often and logs what is committed by kind
//!   and by allocation size, with what of it is in memory. A walk asks
//!   Windows about some 250,000 regions (0.9 s in the world).
//!
//! What the driver does with what we make, measured on the RTX 5080
//! (`tests::driver_allocations_probe`, `tests::allocator_cost_probe`):
//!
//! - Every buffer or heap in the card's own memory comes with as much
//!   write-combined private memory of ours, committed and untouched: the
//!   backing Windows charges for it. A 256 MiB buffer on the card is
//!   +256 MB of private bytes, +0 in memory. In run ram1 that was 11.8 GB of
//!   the 28.8 GB private (13.4 GB write-combined, 1.6 GB of it in memory;
//!   the card's memory in use 12.2 GB): the panel's "RAM" counts the card's
//!   memory once more, though none of it lies in RAM.
//! - A command allocator with its list takes 0.9 MB of private memory when
//!   made, in memory (0.6 MB in a 1 GB arena of the driver's, 0.3 MB in
//!   32 MB write-combined chunks); a list made on no allocator
//!   (`CreateCommandList1`) takes nothing. Recordings take about 170 bytes
//!   a copy command more, kept over resets: an allocator keeps the longest
//!   recording it ever held. Dropped, none of it goes back to Windows; the
//!   driver gives it to the next allocators it makes.
//! - Upload buffers of 64 KiB and up are allocations of their own
//!   (write-combined); smaller ones are cut from 32 MB chunks. The card's
//!   count of the PC's memory it uses (DXGI's non-local group, the
//!   panel's "SHARED") counts command memory, read-back and shared
//!   buffers, but no upload buffer.
//! - Freed buffers: the first 300 MB or so stay with the driver for reuse
//!   (private bytes do not fall); past that they go back.
//!
//! Run ram1 at 200 s (28.8 GB private, 19.3 GB in memory): the card's
//! backing 11.8 GB (none in memory); host copies of title memory 4.6 GB
//! (windows 3.5 GB; all in memory, the card reads them); the title's own
//! commits 4.6 GB (1.6 in memory) and 0.4 GB of audio out of our heap;
//! the driver's arenas 2.9 GB (2.4 in memory: 1,525 host lists' allocators
//! at 0.9 MB, pipelines, objects); the process heap 2.3 GB; our upload
//! buffers 1.1 GB (sends kept 0.46, staging blocks 0.38, lists' scratch
//! 0.21). Mapped and not private: views of the title's pages 12.8 GB (3.4
//! in memory) and 16 GB at host addresses (4.9 in memory: the whole-section
//! views of the title's pages, the pipeline library, the driver's cache).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering::Relaxed};

use windows::Win32::Graphics::Direct3D12::ID3D12Object;
use windows::core::{GUID, HRESULT, IUnknown, Interface};

const MB: f64 = (1u64 << 20) as f64;

/// Our buffers in the PC's memory, by heap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Heap {
    /// `D3D12_HEAP_TYPE_UPLOAD` (write-combined).
    Upload = 0,
    /// `D3D12_HEAP_TYPE_READBACK`.
    ReadBack = 1,
    /// Custom, the PC's memory, write-back (host copies of title memory).
    Shared = 2,
}

/// Alive by [`Heap`]: buffers and the bytes they take ([`charged`]).
static BUFFERS: [[AtomicU64; 2]; 3] = [const { [const { AtomicU64::new(0) }; 2] }; 3];

/// What a buffer asked for at `bytes` takes of private memory: whole
/// 64 KiB pages from 64 KiB up (a committed resource of its own), else
/// pieces of 4 KiB cut from the driver's chunks.
pub(crate) fn charged(bytes: u64) -> u64 {
    if bytes >= 64 << 10 { bytes.next_multiple_of(64 << 10) } else { bytes.max(1).next_multiple_of(4 << 10) }
}

fn counted(heap: usize, bytes: u64, made: bool) {
    let b = &BUFFERS[heap];
    if made {
        b[0].fetch_add(1, Relaxed);
        b[1].fetch_add(bytes, Relaxed);
    } else {
        b[0].fetch_sub(1, Relaxed);
        b[1].fetch_sub(bytes, Relaxed);
    }
}

/// Told when the buffer it is the private data of goes ([`track`]): a
/// COM object of ours with one method that matters, the last release.
#[repr(C)]
struct Notice {
    methods: &'static [*const (); 3],
    references: AtomicU32,
    heap: usize,
    bytes: u64,
}

struct Methods([*const (); 3]);
// SAFETY: function addresses only.
unsafe impl Sync for Methods {}
static NOTICE: Methods = Methods([query as *const (), add_ref as *const (), release as *const ()]);

unsafe extern "system" fn query(this: *mut std::ffi::c_void, iid: *const GUID, out: *mut *mut std::ffi::c_void) -> HRESULT {
    // SAFETY: COM's arguments.
    unsafe {
        if out.is_null() {
            return HRESULT(0x8000_4003_u32 as i32);
        }
        if iid.as_ref().is_some_and(|iid| *iid == IUnknown::IID) {
            add_ref(this);
            *out = this;
            return HRESULT(0);
        }
        *out = std::ptr::null_mut();
    }
    HRESULT(0x8000_4002_u32 as i32)
}

unsafe extern "system" fn add_ref(this: *mut std::ffi::c_void) -> u32 {
    // SAFETY: one of ours, alive while a reference to it is.
    unsafe { (*this.cast::<Notice>()).references.fetch_add(1, Relaxed) + 1 }
}

unsafe extern "system" fn release(this: *mut std::ffi::c_void) -> u32 {
    // SAFETY: as above; the last reference frees the box it was made as.
    unsafe {
        let left = (*this.cast::<Notice>()).references.fetch_sub(1, std::sync::atomic::Ordering::AcqRel) - 1;
        if left == 0 {
            let gone = Box::from_raw(this.cast::<Notice>());
            counted(gone.heap, gone.bytes, false);
        }
        left
    }
}

/// Counts `resource`, a buffer of ours of `bytes` in `heap`, in
/// [`BUFFERS`] while it lives (its private data holds a [`Notice`]).
pub(crate) fn track(resource: &impl Interface, heap: Heap, bytes: u64) {
    const KEY: GUID = GUID::from_u128(0x6b1f_3c2e_5a47_4d1e_9b6a_2f0c_8e4d_7a13);
    let Ok(object) = resource.cast::<ID3D12Object>() else { return };
    let bytes = charged(bytes);
    let notice = Box::into_raw(Box::new(Notice { methods: &NOTICE.0, references: AtomicU32::new(1), heap: heap as usize, bytes }));
    counted(heap as usize, bytes, true);
    // SAFETY: a COM object of ours with one reference, which this drops;
    // the resource takes its own, released when it goes.
    unsafe {
        let notice = IUnknown::from_raw(notice.cast());
        if object.SetPrivateDataInterface(&KEY, &notice).is_err() {
            // Not counted after all: it would never be told the buffer went.
            counted(heap as usize, bytes, false);
        }
    }
}

/// Title memory given out of our heap (heaps and descriptor heaps it made
/// through the device, its audio memory), in bytes.
static FROM_OURS: AtomicI64 = AtomicI64::new(0);

/// Title memory given out of our heap (`bytes` > 0) or given back.
pub fn title_from_ours(bytes: i64) {
    FROM_OURS.fetch_add(bytes, Relaxed);
}

/// The allocations in which the title committed private memory itself
/// (`XMemVirtualAlloc`), by their base: their committed bytes are asked of
/// Windows when the line is made ([`title_private`]).
static TITLE: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

/// The title committed private memory at `at`, in an allocation of its own.
pub fn title_allocation(at: usize) {
    use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
    // SAFETY: plain data filled in by the query.
    let mut m: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    if unsafe { VirtualQuery(at as *const _, &mut m, size_of_val(&m)) } == 0 {
        return;
    }
    let base = m.AllocationBase as usize;
    let mut bases = TITLE.lock().unwrap_or_else(|e| e.into_inner());
    if !bases.contains(&base) {
        bases.push(base);
    }
}

/// The private bytes committed in the title's allocations
/// ([`title_allocation`]); those let go since are forgotten.
fn title_private() -> u64 {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_FREE, MEM_PRIVATE, MEMORY_BASIC_INFORMATION, VirtualQuery};
    let mut bases = TITLE.lock().unwrap_or_else(|e| e.into_inner());
    let mut sum = 0u64;
    bases.retain(|&base| {
        let mut at = base;
        let mut alive = false;
        loop {
            // SAFETY: plain data filled in by the query.
            let mut m: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: a valid out pointer of the right size.
            if unsafe { VirtualQuery(at as *const _, &mut m, size_of_val(&m)) } == 0
                || m.State == MEM_FREE
                || m.AllocationBase as usize != base
            {
                break;
            }
            alive = true;
            if m.State == MEM_COMMIT && m.Type == MEM_PRIVATE {
                sum += m.RegionSize as u64;
            }
            at = (m.BaseAddress as usize).saturating_add(m.RegionSize);
        }
        alive
    });
    sum
}

/// The process's private bytes and working set.
pub(crate) fn process_memory() -> (u64, u64) {
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS { cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    (c.PagefileUsage as u64, c.WorkingSetSize as u64)
}

/// The PC's physical memory free and what is left to commit.
fn system_memory() -> (u64, u64) {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: plain data the call fills in.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: a sized structure.
    unsafe { GlobalMemoryStatusEx(&mut m) };
    (m.ullAvailPhys, m.ullAvailPageFile)
}

/// The process heap's bytes in blocks and committed: ours (every Rust
/// allocation), the graphics runtime's and the driver's C++ alike.
fn process_heap() -> (u64, u64) {
    use windows_sys::Win32::System::Memory::{GetProcessHeap, HEAP_SUMMARY, HeapSummary};
    let mut s = HEAP_SUMMARY { cb: size_of::<HEAP_SUMMARY>() as u32, ..Default::default() };
    // SAFETY: the process heap and a sized structure.
    if unsafe { HeapSummary(GetProcessHeap(), 0, &mut s) } == 0 {
        return (0, 0);
    }
    (s.cbAllocated as u64, s.cbCommitted as u64)
}

/// Every how many seconds the owner line is logged (`VICTORY_RAM_LINE`, 10
/// if unset, 0: never).
fn line_every() -> u64 {
    static EVERY: OnceLock<u64> = OnceLock::new();
    *EVERY.get_or_init(|| std::env::var("VICTORY_RAM_LINE").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(10))
}

/// Every how many seconds the address space is walked (`VICTORY_RAM_MAP`,
/// 0 or unset: never).
fn map_every() -> u64 {
    static EVERY: OnceLock<u64> = OnceLock::new();
    *EVERY.get_or_init(|| std::env::var("VICTORY_RAM_MAP").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0))
}

/// Once a second (the host's sweep): the owner line and the walk when due.
pub(crate) fn tick() {
    static LINE: AtomicU64 = AtomicU64::new(0);
    static MAP: AtomicU64 = AtomicU64::new(0);
    static WALKING: AtomicBool = AtomicBool::new(false);
    let now = crate::host::now_ms();
    let due = |last: &AtomicU64, every: u64| {
        if every == 0 {
            return false;
        }
        let was = last.load(Relaxed);
        if was == 0 {
            // The clock's first look: the first line comes a period later.
            let _ = last.compare_exchange(0, now.max(1), Relaxed, Relaxed);
            return false;
        }
        now >= was + every * 1000 && last.compare_exchange(was, now, Relaxed, Relaxed).is_ok()
    };
    if due(&LINE, line_every()) {
        tracing::info!("{}", line());
    }
    if due(&MAP, map_every()) && !WALKING.swap(true, Relaxed) {
        let walk = std::thread::Builder::new().name("victory-ram-map".into()).spawn(|| {
            walk();
            WALKING.store(false, Relaxed);
        });
        if walk.is_err() {
            WALKING.store(false, Relaxed);
        }
    }
}

/// The parts of private memory the line names, in bytes, for [`rest`].
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
struct Parts {
    private: u64,
    title: u64,
    buffers: u64,
    heap_committed: u64,
}

/// What private memory none of [`Parts`] names: the driver's own memory
/// (command memory, pipelines, objects) and the programs' data. Title
/// memory given out of our heap is in the heap's bytes.
fn rest(p: Parts) -> i64 {
    p.private as i64 - p.title as i64 - p.buffers as i64 - p.heap_committed as i64
}

/// The owner line.
fn line() -> String {
    let (private, working) = process_memory();
    let (free, left) = system_memory();
    let (heap_blocks, heap_committed) = process_heap();
    let title = title_private();
    let from_ours = FROM_OURS.load(Relaxed);
    let buffers: [[u64; 2]; 3] = std::array::from_fn(|h| [BUFFERS[h][0].load(Relaxed), BUFFERS[h][1].load(Relaxed)]);
    let copies = crate::shadow::memory();
    let staged = crate::texture::memory();
    let pool = crate::pool::memory();
    let alive: Vec<u64> = crate::host::ALIVE.iter().map(|a| a.load(Relaxed)).collect();
    let lists: Vec<u64> = crate::host::LISTS.iter().map(|a| a.load(Relaxed)).collect();
    let recorded = crate::host::recorded();
    let idle = crate::list::idle();
    let sends = crate::host::sends_kept();
    let [card, shared] = crate::host::host().and_then(|h| h.video_memory_groups()).map_or([0, 0], |[c, s]| [c[1], s[1]]);
    let all_buffers: u64 = buffers.iter().map(|b| b[1]).sum();
    let left_over = rest(Parts { private, title, buffers: all_buffers, heap_committed });
    let mb = |b: u64| b as f64 / MB;
    format!(
        "memory by owner (VICTORY_RAM_LINE): {:.0} MB private ({:.0} MB in memory; the PC has {:.0} MB free, {:.0} MB left \
         to commit); the title's: {:.0} MB it committed, {:.0} MB given out of our heap (its heaps, descriptor heaps, \
         audio); our buffers in the PC's memory, alive: upload {:.0} MB in {}, read-back {:.0} MB in {}, shared \
         {:.0} MB in {}; what holds them: host copies of title memory {} {:.0} MB (windows {} {:.0} MB), snapshots \
         kept {} {:.0} MB, texture staging {} {:.0} MB, staging blocks {} MB, lists' scratch {:.0} MB ({:.0} MB \
         spare), lists' other uploads {:.0} MB, sends kept {:.0} MB; host lists {} alive ({} idle), {} made and {} \
         preludes since the start ({} on their list's allocator), their longest \
         recordings {:.0}k calls in all, the longest {:.0}k; the process \
         heap {:.0} MB committed, {:.0} MB in blocks (ours, the runtime's, the driver's); the card counts {:.0} MB \
         of the PC's memory as its own; the rest {:.0} MB: of it about {:.0} MB the card's own memory in use, which \
         the driver backs with as much of ours, committed and not in memory, and {:.0} MB beside (the driver's \
         arenas: command allocators, pipelines, objects; the programs')",
        mb(private),
        mb(working),
        mb(free),
        mb(left),
        mb(title),
        from_ours as f64 / MB,
        mb(buffers[0][1]),
        buffers[0][0],
        mb(buffers[1][1]),
        buffers[1][0],
        mb(buffers[2][1]),
        buffers[2][0],
        copies[0],
        mb(copies[1]),
        copies[2],
        mb(copies[3]),
        copies[4],
        mb(copies[5]),
        staged[0],
        mb(staged[1]),
        pool[1],
        mb(alive[2]),
        mb(alive[4]),
        mb(alive[7]),
        mb(sends),
        alive[0],
        idle,
        lists[0],
        lists[1],
        crate::host::SHARED_PRELUDES.load(Relaxed),
        recorded[0] as f64 / 1e3,
        recorded[1] as f64 / 1e3,
        mb(heap_committed),
        mb(heap_blocks),
        mb(shared),
        left_over as f64 / MB,
        mb(card),
        (left_over - card as i64) as f64 / MB,
    )
}

/// The kinds the walk tells apart.
const KINDS: [&str; 8] = [
    "the title's private (console addresses)",
    "views of the title's pages (mapped, console addresses)",
    "write-combined (upload buffers; the driver's command memory and small buffers in 32 MB chunks)",
    "private of 1 GB and more (the driver's arenas, the title's audio)",
    "other private of 1 MB and more",
    "other private under 1 MB (the process heap's small segments)",
    "mapped at host addresses (files, the pipeline library)",
    "programs and libraries",
];

/// Kind of a committed region ([`KINDS`]): allocation's whole size, whether
/// it lies at a console address, its type and protection.
fn kind(whole: usize, console: bool, kind: u32, protect: u32) -> usize {
    use windows_sys::Win32::System::Memory::{MEM_IMAGE, MEM_MAPPED, PAGE_WRITECOMBINE};
    match (kind, console) {
        (MEM_IMAGE, _) => 7,
        (MEM_MAPPED, true) => 1,
        (MEM_MAPPED, false) => 6,
        (_, true) => 0,
        _ if protect & PAGE_WRITECOMBINE != 0 => 2,
        _ if whole >= 1 << 30 => 3,
        _ if whole >= 1 << 20 => 4,
        _ => 5,
    }
}

/// Private allocations at host addresses by (whole size, protection):
/// the allocations, their committed bytes, those in memory.
type Groups = std::collections::HashMap<(usize, u32), (std::collections::HashSet<usize>, u64, u64)>;

/// What a page asked about stands for: its kind, its group, its bytes.
type Stand = (usize, Option<(usize, u32)>, u64);

/// Asks which of the pages in `asked` are in memory and adds what each
/// stands for to its kind and group.
fn in_memory(asked: &mut Vec<[usize; 2]>, stands: &mut Vec<Stand>, present: &mut [u64; KINDS.len()], groups: &mut Groups) {
    use windows_sys::Win32::System::ProcessStatus::K32QueryWorkingSetEx;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: our own process; `asked` holds that many entries of two
    // words each, as the call fills them in.
    let ok = unsafe { K32QueryWorkingSetEx(GetCurrentProcess(), asked.as_mut_ptr().cast(), (asked.len() * 16) as u32) } != 0;
    if ok {
        for (a, &(k, group, bytes)) in asked.iter().zip(stands.iter()) {
            if a[1] & 1 != 0 {
                present[k] += bytes;
                if let Some(g) = group.and_then(|g| groups.get_mut(&g)) {
                    g.2 += bytes;
                }
            }
        }
    }
    asked.clear();
    stands.clear();
}

/// Walks the address space and logs what is committed by kind, and the
/// private allocations by (whole size, protection), with what of each is
/// in memory (one page asked about per 64 KiB).
fn walk() {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_FREE, MEM_PRIVATE, MEMORY_BASIC_INFORMATION, VirtualQuery};
    const CONSOLE: usize = 1 << 40;
    const STEP: usize = 64 << 10;
    let started = std::time::Instant::now();
    // Allocation, start, bytes, committed, type, protection.
    let mut regions: Vec<(usize, usize, usize, bool, u32, u32)> = Vec::new();
    let mut at = 0usize;
    loop {
        // SAFETY: plain data filled in by the query.
        let mut m: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        if unsafe { VirtualQuery(at as *const _, &mut m, size_of_val(&m)) } == 0 {
            break;
        }
        if m.State != MEM_FREE {
            regions.push((m.AllocationBase as usize, m.BaseAddress as usize, m.RegionSize, m.State == MEM_COMMIT, m.Type, m.Protect));
        }
        let next = (m.BaseAddress as usize).saturating_add(m.RegionSize);
        if next <= at {
            break;
        }
        at = next;
    }
    let mut whole: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for r in &regions {
        *whole.entry(r.0).or_default() += r.2;
    }
    let mut committed = [0u64; KINDS.len()];
    let mut present = [0u64; KINDS.len()];
    // Private at host addresses by (whole size, protection): allocations,
    // committed bytes, in memory.
    let mut groups = Groups::new();
    let mut asked: Vec<[usize; 2]> = Vec::with_capacity(4096);
    let mut stands: Vec<Stand> = Vec::with_capacity(4096);
    for r in regions.iter().filter(|r| r.3) {
        let size = whole.get(&r.0).copied().unwrap_or(0);
        let console = r.0 < CONSOLE;
        let k = kind(size, console, r.4, r.5);
        committed[k] += r.2 as u64;
        let group = (r.4 == MEM_PRIVATE && !console).then_some((size, r.5));
        if let Some(g) = group {
            let e = groups.entry(g).or_default();
            e.0.insert(r.0);
            e.1 += r.2 as u64;
        }
        let mut offset = 0;
        while offset < r.2 {
            asked.push([r.1 + offset, 0]);
            stands.push((k, group, STEP.min(r.2 - offset) as u64));
            if asked.len() == 4096 {
                in_memory(&mut asked, &mut stands, &mut present, &mut groups);
            }
            offset += STEP;
        }
    }
    if !asked.is_empty() {
        in_memory(&mut asked, &mut stands, &mut present, &mut groups);
    }
    let text: Vec<String> = (0..KINDS.len())
        .filter(|&k| committed[k] > 0)
        .map(|k| format!("{} {:.0} MB ({:.0} MB in memory)", KINDS[k], committed[k] as f64 / MB, present[k] as f64 / MB))
        .collect();
    tracing::info!("memory map (VICTORY_RAM_MAP; {} regions, {} ms): {}", regions.len(), started.elapsed().as_millis(), text.join("; "));
    let mut groups: Vec<((usize, u32), (usize, u64, u64))> = groups.into_iter().map(|(g, (n, c, p))| (g, (n.len(), c, p))).collect();
    groups.sort_by_key(|g| std::cmp::Reverse(g.1.1));
    let biggest: Vec<String> = groups
        .iter()
        .take(24)
        .map(|&((size, protect), (n, bytes, inside))| {
            format!(
                "{n} of {:.2} MB{}: {:.0} MB ({:.0} in memory)",
                size as f64 / MB,
                if protect & windows_sys::Win32::System::Memory::PAGE_WRITECOMBINE != 0 { " write-combined" } else { "" },
                bytes as f64 / MB,
                inside as f64 / MB
            )
        })
        .collect();
    tracing::info!(
        "memory map, private at host addresses by allocation size, the {} biggest groups of {} (committed): {}",
        biggest.len(),
        groups.len(),
        biggest.join("; ")
    );
}

#[cfg(test)]
mod tests {
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
    use windows::core::Interface;

    /// A device on the software adapter (`None` where there is none).
    fn warp() -> Option<ID3D12Device> {
        use windows::Win32::Graphics::Dxgi::*;
        // SAFETY: plain DXGI queries and device creation on the software
        // adapter.
        unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
            let adapter: IDXGIAdapter = factory.EnumWarpAdapter().ok()?;
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
            device
        }
    }

    #[test]
    fn buffers_are_charged_as_the_driver_gives_them() {
        assert_eq!(super::charged(0), 4 << 10);
        assert_eq!(super::charged(256), 4 << 10);
        assert_eq!(super::charged((4 << 10) + 1), 8 << 10);
        assert_eq!(super::charged(64 << 10), 64 << 10);
        assert_eq!(super::charged((64 << 10) + 1), 128 << 10);
        assert_eq!(super::charged(4 << 20), 4 << 20);
    }

    #[test]
    fn the_rest_is_what_no_part_names() {
        let parts = super::Parts { private: 30 << 30, title: 5 << 30, buffers: 6 << 30, heap_committed: 2 << 30 };
        assert_eq!(super::rest(parts), 17 << 30);
        let over = super::Parts { private: 1 << 30, title: 1 << 30, buffers: 1 << 30, heap_committed: 0 };
        assert_eq!(super::rest(over), -(1 << 30), "said as it is, not hidden");
    }

    /// A tracked buffer counts in its heap while it lives, and its notice
    /// takes it out when the last reference goes.
    #[test]
    fn tracked_buffers_count_while_they_live() {
        use super::{BUFFERS, Heap, charged, track};
        use std::sync::atomic::Ordering::Relaxed;
        let _turn = crate::indirect::tests::warp_turn();
        let Some(device) = warp() else { return };
        let upload = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_UPLOAD, ..Default::default() };
        let size = (1 << 20) + 5;
        let buffer = system_buffer(&device, upload, size, D3D12_RESOURCE_STATE_GENERIC_READ);
        let before = [BUFFERS[0][0].load(Relaxed), BUFFERS[0][1].load(Relaxed)];
        track(&buffer, Heap::Upload, size);
        let alive = [BUFFERS[0][0].load(Relaxed), BUFFERS[0][1].load(Relaxed)];
        assert_eq!(alive, [before[0] + 1, before[1] + charged(size)]);
        let second = buffer.clone();
        drop(buffer);
        assert_eq!(BUFFERS[0][1].load(Relaxed), alive[1], "another reference holds it");
        drop(second);
        assert_eq!([BUFFERS[0][0].load(Relaxed), BUFFERS[0][1].load(Relaxed)], before);
    }

    /// The title's own commits are asked of Windows: what is committed in
    /// its allocation counts, the rest of the reservation does not, and an
    /// allocation let go is forgotten.
    #[test]
    fn the_titles_commits_are_counted_while_they_last() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree};
        // SAFETY: a reservation of our own, then part of it committed.
        let at = unsafe { VirtualAlloc(std::ptr::null(), 4 << 20, MEM_RESERVE, PAGE_READWRITE) };
        assert!(!at.is_null());
        // SAFETY: inside the reservation.
        let part = unsafe { VirtualAlloc(at.cast::<u8>().add(1 << 20).cast(), 256 << 10, MEM_COMMIT, PAGE_READWRITE) };
        assert!(!part.is_null());
        super::title_allocation(part as usize);
        let held = super::title_private();
        assert!(held >= 256 << 10);
        assert!(super::TITLE.lock().unwrap().contains(&(at as usize)), "known by its allocation's base");
        // SAFETY: the whole reservation of ours.
        assert_ne!(unsafe { VirtualFree(at, 0, MEM_RELEASE) }, 0);
        let after = super::title_private();
        assert!(after + (256 << 10) <= held);
        assert!(!super::TITLE.lock().unwrap().contains(&(at as usize)), "forgotten once let go");
    }

    /// A list's peak is its longest recording, kept over restarts; the sum
    /// of peaks loses a list's when it goes.
    #[test]
    fn a_lists_peak_is_its_longest_recording() {
        let _turn = crate::indirect::tests::warp_turn();
        let Some(device) = warp() else { return };
        // SAFETY: plain creation.
        let (allocator, list): (ID3D12CommandAllocator, ID3D12GraphicsCommandList) = unsafe {
            let a: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
            let l = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &a, None).unwrap();
            (a, l)
        };
        let sum = crate::host::recorded()[0];
        let rec = crate::host::rec::Rec::new(list);
        let use_it = |n: usize| {
            for _ in 0..n {
                let _ = &*rec;
            }
        };
        use_it(40);
        assert_eq!(rec.peak(), 40, "the recording going on counts");
        rec.closed();
        assert_eq!(crate::host::recorded()[0], sum + 40);
        rec.restart();
        use_it(10);
        rec.closed();
        assert_eq!(rec.peak(), 40, "a shorter recording leaves the peak");
        rec.restart();
        use_it(55);
        rec.closed();
        assert_eq!(rec.peak(), 55);
        assert_eq!(crate::host::recorded()[0], sum + 55);
        assert!(crate::host::recorded()[1] >= 55);
        drop(rec);
        assert_eq!(crate::host::recorded()[0], sum);
        drop(allocator);
    }

    /// What `VICTORY_SHARE_PRELUDE` does, on the software adapter: a closed
    /// list made on no allocator records on another list's allocator once
    /// that list is closed, both run in one submission, and the allocator's
    /// reset after they ran lets go of both. While the list is open the
    /// allocator is refused (the prelude then takes one of its own).
    #[test]
    fn a_prelude_records_on_its_lists_allocator_once_the_list_is_closed() {
        let _turn = crate::indirect::tests::warp_turn();
        let Some(device) = warp() else { return };
        let (from, to) = (buffer(&device, 1 << 16), buffer(&device, 1 << 16));
        // SAFETY: plain creation, recording and a submission waited for.
        unsafe {
            let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
            let list: ID3D12GraphicsCommandList =
                device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).unwrap();
            let prelude: ID3D12GraphicsCommandList = device
                .cast::<ID3D12Device4>()
                .unwrap()
                .CreateCommandList1(0, D3D12_COMMAND_LIST_TYPE_DIRECT, D3D12_COMMAND_LIST_FLAG_NONE)
                .unwrap();
            assert!(prelude.Reset(&allocator, None).is_err(), "refused while the list records on it");
            list.CopyBufferRegion(&to, 0, &from, 0, 256);
            list.Close().unwrap();
            prelude.Reset(&allocator, None).unwrap();
            prelude.CopyBufferRegion(&to, 256, &from, 0, 256);
            prelude.Close().unwrap();
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() })
                .unwrap();
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            queue.ExecuteCommandLists(&[Some(prelude.cast().unwrap()), Some(list.cast().unwrap())]);
            queue.Signal(&fence, 1).unwrap();
            while fence.GetCompletedValue() < 1 {
                std::thread::yield_now();
            }
            allocator.Reset().unwrap();
            list.Reset(&allocator, None).unwrap();
            list.Close().unwrap();
        }
    }

    /// The process heap's numbers are there and follow a big block of ours.
    #[test]
    fn the_process_heap_is_summed() {
        let (blocks, committed) = super::process_heap();
        assert!(blocks > 0 && committed >= blocks, "{blocks} in blocks, {committed} committed");
        let started = std::time::Instant::now();
        let _ = super::process_heap();
        eprintln!("a heap summary: {:?}", started.elapsed());
        let big = vec![1u8; 64 << 20];
        let (with, _) = super::process_heap();
        eprintln!("a 64 MiB block: {:+} MB in blocks", (with as i64 - blocks as i64) >> 20);
        drop(big);
    }

    /// The line is made from every count without a host.
    #[test]
    fn the_owner_line_is_made_without_a_host() {
        let line = super::line();
        assert!(line.starts_with("memory by owner"), "{line}");
        assert!(line.contains("the rest"), "{line}");
        assert!(!line.contains('\n'), "one line: {line}");
    }

    fn private_and_working() -> (i64, i64) {
        use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut c = PROCESS_MEMORY_COUNTERS { cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
        // SAFETY: our own process and a sized structure.
        unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
        (c.PagefileUsage as i64, c.WorkingSetSize as i64)
    }

    fn buffer(device: &ID3D12Device, size: u64) -> ID3D12Resource {
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut b: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe { device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut b) }
            .expect("a buffer");
        b.expect("a buffer")
    }

    /// What a command allocator and its list cost in private memory on
    /// this PC's card, made, recorded into, reset and dropped
    /// (`cargo test -p victory-gpu --lib -- --ignored allocator_cost --nocapture`).
    #[test]
    #[ignore = "wants a card"]
    fn allocator_cost_probe() {
        let mut device: Option<ID3D12Device> = None;
        // SAFETY: plain device creation on the default adapter.
        unsafe { D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device) }.expect("a device");
        let device = device.expect("a device");
        let mb = |d: i64| d as f64 / (1 << 20) as f64;
        let (from, to) = (buffer(&device, 1 << 16), buffer(&device, 1 << 16));
        const N: usize = 64;
        let base = private_and_working();
        let mut made = Vec::new();
        for _ in 0..N {
            // SAFETY: plain creation.
            unsafe {
                let a: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).expect("allocator");
                let l: ID3D12GraphicsCommandList =
                    device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &a, None).expect("list");
                l.Close().expect("close");
                made.push((a, l));
            }
        }
        let after_made = private_and_working();
        eprintln!(
            "{N} allocators and lists made: private +{:.2} MB ({:.3} MB each), working set +{:.2} MB",
            mb(after_made.0 - base.0),
            mb(after_made.0 - base.0) / N as f64,
            mb(after_made.1 - base.1)
        );
        // Lists made closed on no allocator, as a prelude on its list's
        // allocator is (`VICTORY_SHARE_PRELUDE`).
        let device4: ID3D12Device4 = device.cast().expect("ID3D12Device4");
        let before = private_and_working();
        let alone: Vec<ID3D12GraphicsCommandList> = (0..N)
            .map(|_| {
                // SAFETY: plain creation.
                unsafe { device4.CreateCommandList1(0, D3D12_COMMAND_LIST_TYPE_DIRECT, D3D12_COMMAND_LIST_FLAG_NONE) }
                    .expect("list")
            })
            .collect();
        let after = private_and_working();
        eprintln!(
            "{N} lists made on no allocator: private +{:.2} MB ({:.3} MB each)",
            mb(after.0 - before.0),
            mb(after.0 - before.0) / N as f64
        );
        drop(alone);
        for copies in [1000usize, 10_000, 50_000] {
            let before = private_and_working();
            for (a, l) in &made[..8] {
                // SAFETY: recording on a list of ours whose allocator ran nothing.
                unsafe {
                    a.Reset().expect("reset");
                    l.Reset(a, None).expect("reset list");
                    for i in 0..copies {
                        l.CopyBufferRegion(&to, (i as u64 * 16) % 65536, &from, 0, 16);
                    }
                    l.Close().expect("close");
                }
            }
            let after = private_and_working();
            eprintln!(
                "8 lists of {copies} copies recorded: private +{:.2} MB ({:.3} MB each), working set +{:.2} MB",
                mb(after.0 - before.0),
                mb(after.0 - before.0) / 8.0,
                mb(after.1 - before.1)
            );
        }
        let before = private_and_working();
        for (a, l) in &made[..8] {
            // SAFETY: as above; a short recording after a long one.
            unsafe {
                a.Reset().expect("reset");
                l.Reset(a, None).expect("reset list");
                l.CopyBufferRegion(&to, 0, &from, 0, 16);
                l.Close().expect("close");
            }
        }
        let after = private_and_working();
        eprintln!("8 reset and recorded short: private {:+.2} MB", mb(after.0 - before.0));
        let before = private_and_working();
        drop(made);
        let after = private_and_working();
        eprintln!("all dropped: private {:+.2} MB", mb(after.0 - before.0));
        // Made again and recorded long: does the driver use what it kept?
        let make = |n: usize, copies: usize| -> Vec<(ID3D12CommandAllocator, ID3D12GraphicsCommandList)> {
            (0..n)
                .map(|_| {
                    // SAFETY: plain creation and recording of lists of ours.
                    unsafe {
                        let a: ID3D12CommandAllocator =
                            device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).expect("allocator");
                        let l: ID3D12GraphicsCommandList =
                            device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &a, None).expect("list");
                        for i in 0..copies {
                            l.CopyBufferRegion(&to, (i as u64 * 16) % 65536, &from, 0, 16);
                        }
                        l.Close().expect("close");
                        (a, l)
                    }
                })
                .collect()
        };
        for round in 0..3 {
            let before = private_and_working();
            let started = std::time::Instant::now();
            let lists = make(64, 20_000);
            let after = private_and_working();
            eprintln!(
                "round {round}: 64 made and recorded with 20000 copies: private {:+.2} MB, {:.3} ms each",
                mb(after.0 - before.0),
                started.elapsed().as_secs_f64() * 1e3 / 64.0
            );
            drop(lists);
            eprintln!("round {round}: dropped: private {:+.2} MB", mb(private_and_working().0 - after.0));
        }
        // One allocator, two lists recorded one after the other.
        // SAFETY: plain creation and recording.
        unsafe {
            let a: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).expect("allocator");
            let first: ID3D12GraphicsCommandList =
                device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &a, None).expect("list");
            first.CopyBufferRegion(&to, 0, &from, 0, 16);
            first.Close().expect("close");
            let second: ID3D12GraphicsCommandList =
                device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &a, None).expect("a second list on one allocator");
            second.Close().expect("close");
            second.Reset(&a, None).expect("a second list reset on the first's allocator");
            second.CopyBufferRegion(&to, 0, &from, 0, 16);
            second.Close().expect("close");
            eprintln!("two lists recorded one after the other on one allocator: fine");
        }
    }

    /// Committed bytes by allocation (base: whole size, committed, type,
    /// protection of its first committed region).
    fn regions() -> std::collections::HashMap<usize, (usize, usize, u32, u32)> {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_FREE, MEMORY_BASIC_INFORMATION, VirtualQuery};
        let mut out: std::collections::HashMap<usize, (usize, usize, u32, u32)> = std::collections::HashMap::new();
        let mut at = 0usize;
        loop {
            // SAFETY: plain data filled in by the query.
            let mut m: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: a valid out pointer of the right size.
            if unsafe { VirtualQuery(at as *const _, &mut m, size_of_val(&m)) } == 0 {
                break;
            }
            if m.State != MEM_FREE {
                let e = out.entry(m.AllocationBase as usize).or_insert((0, 0, m.Type, 0));
                e.0 += m.RegionSize;
                if m.State == MEM_COMMIT {
                    e.1 += m.RegionSize;
                    if e.3 == 0 {
                        e.3 = m.Protect;
                    }
                }
            }
            let next = (m.BaseAddress as usize).saturating_add(m.RegionSize);
            if next <= at {
                break;
            }
            at = next;
        }
        out
    }

    type Regions = std::collections::HashMap<usize, (usize, usize, u32, u32)>;

    /// What appeared or grew between two [`regions`], grouped by (whole
    /// size, type, protection).
    fn diff(before: &Regions, after: &Regions) -> String {
        let mut groups: std::collections::BTreeMap<(usize, u32, u32), (usize, i64)> = std::collections::BTreeMap::new();
        for (base, &(size, committed, kind, protect)) in after {
            let was = before.get(base).map_or(0, |b| b.1);
            if committed != was {
                let g = groups.entry((size, kind, protect)).or_default();
                g.0 += 1;
                g.1 += committed as i64 - was as i64;
            }
        }
        for (base, &(size, committed, kind, protect)) in before {
            if !after.contains_key(base) && committed > 0 {
                let g = groups.entry((size, kind, protect)).or_default();
                g.0 += 1;
                g.1 -= committed as i64;
            }
        }
        let mut v: Vec<_> = groups.into_iter().collect();
        v.sort_by_key(|(_, (_, b))| -b.abs());
        v.iter()
            .take(8)
            .map(|((size, kind, protect), (n, bytes))| {
                format!(
                    "{n} of {:.2} MB type {kind:#x} protect {protect:#x}: {:+.2} MB",
                    *size as f64 / 1048576.0,
                    *bytes as f64 / 1048576.0
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    }

    fn system_buffer(
        device: &ID3D12Device,
        heap: D3D12_HEAP_PROPERTIES,
        size: u64,
        state: D3D12_RESOURCE_STATES,
    ) -> ID3D12Resource {
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: if heap.Type == D3D12_HEAP_TYPE_CUSTOM {
                D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS
            } else {
                D3D12_RESOURCE_FLAG_NONE
            },
            ..Default::default()
        };
        let mut b: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe { device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut b) }
            .expect("a buffer");
        b.expect("a buffer")
    }

    /// Where the driver puts what we make: the allocations of the process
    /// that appear for command allocators, recordings and buffers in the
    /// PC's memory (`cargo test -p victory-gpu --lib -- --ignored
    /// driver_allocations --nocapture`).
    #[test]
    #[ignore = "wants a card"]
    fn driver_allocations_probe() {
        let mut device: Option<ID3D12Device> = None;
        // SAFETY: plain device creation on the default adapter.
        unsafe { D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device) }.expect("a device");
        let device = device.expect("a device");
        let (from, to) = (buffer(&device, 1 << 16), buffer(&device, 1 << 16));
        // SAFETY: plain DXGI queries.
        let adapter: windows::Win32::Graphics::Dxgi::IDXGIAdapter3 = unsafe {
            windows::Win32::Graphics::Dxgi::CreateDXGIFactory2::<windows::Win32::Graphics::Dxgi::IDXGIFactory4>(
                windows::Win32::Graphics::Dxgi::DXGI_CREATE_FACTORY_FLAGS(0),
            )
            .and_then(|f| f.EnumAdapterByLuid(device.GetAdapterLuid()))
        }
        .expect("the adapter");
        let shared = || {
            let mut m = windows::Win32::Graphics::Dxgi::DXGI_QUERY_VIDEO_MEMORY_INFO::default();
            // SAFETY: fills our structure.
            unsafe { adapter.QueryVideoMemoryInfo(0, windows::Win32::Graphics::Dxgi::DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, &mut m) }
                .expect("query");
            m.CurrentUsage as i64
        };
        let step = |what: &str, f: &mut dyn FnMut()| {
            let (p0, s0, map0) = (private_and_working().0, shared(), regions());
            f();
            let (p1, s1, map1) = (private_and_working().0, shared(), regions());
            eprintln!(
                "{what}: private {:+.2} MB, shared with the card {:+.2} MB; {}",
                (p1 - p0) as f64 / 1048576.0,
                (s1 - s0) as f64 / 1048576.0,
                diff(&map0, &map1)
            );
        };
        let mut lists = Vec::new();
        step("64 allocators and lists", &mut || {
            for _ in 0..64 {
                // SAFETY: plain creation.
                unsafe {
                    let a: ID3D12CommandAllocator =
                        device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).expect("allocator");
                    let l: ID3D12GraphicsCommandList =
                        device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &a, None).expect("list");
                    l.Close().expect("close");
                    lists.push((a, l));
                }
            }
        });
        step("8 of them record 20000 copies", &mut || {
            for (a, l) in &lists[..8] {
                // SAFETY: recording on lists of ours that run nothing.
                unsafe {
                    a.Reset().expect("reset");
                    l.Reset(a, None).expect("reset");
                    for i in 0..20_000u64 {
                        l.CopyBufferRegion(&to, (i * 16) % 65536, &from, 0, 16);
                    }
                    l.Close().expect("close");
                }
            }
        });
        let mut kept = Vec::new();
        let upload = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_UPLOAD, ..Default::default() };
        let readback = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
        let shared = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            ..Default::default()
        };
        step("64 upload buffers of 256 bytes", &mut || {
            for _ in 0..64 {
                kept.push(system_buffer(&device, upload, 256, D3D12_RESOURCE_STATE_GENERIC_READ));
            }
        });
        step("16 upload buffers of 4 MiB", &mut || {
            for _ in 0..16 {
                kept.push(system_buffer(&device, upload, 4 << 20, D3D12_RESOURCE_STATE_GENERIC_READ));
            }
        });
        step("16 shared (custom L0 write-back) buffers of 4 MiB", &mut || {
            for _ in 0..16 {
                kept.push(system_buffer(&device, shared, 4 << 20, D3D12_RESOURCE_STATE_COMMON));
            }
        });
        step("8 read-back buffers of 4 MiB", &mut || {
            for _ in 0..8 {
                kept.push(system_buffer(&device, readback, 4 << 20, D3D12_RESOURCE_STATE_COPY_DEST));
            }
        });
        step("one 64 MiB shared buffer mapped", &mut || {
            let b = system_buffer(&device, shared, 64 << 20, D3D12_RESOURCE_STATE_COMMON);
            let mut data = std::ptr::null_mut();
            // SAFETY: a mapping of our own buffer.
            unsafe { b.Map(0, None, Some(&mut data)) }.expect("map");
            kept.push(b);
        });
        step("one 256 MiB buffer in the card's memory", &mut || kept.push(buffer(&device, 256 << 20)));
        let mut heaps: Vec<ID3D12Heap> = Vec::new();
        step("four 64 MiB heaps in the card's memory", &mut || {
            for _ in 0..4 {
                let desc = D3D12_HEAP_DESC {
                    SizeInBytes: 64 << 20,
                    Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
                    Alignment: 0,
                    Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES,
                };
                let mut heap: Option<ID3D12Heap> = None;
                // SAFETY: plain creation.
                unsafe { device.CreateHeap(&desc, &mut heap) }.expect("a heap");
                heaps.push(heap.expect("a heap"));
            }
        });
        step("the heaps dropped", &mut || heaps.clear());
        step("all buffers dropped", &mut || kept.clear());
        // SAFETY: plain creation of a queue and a fence of our own.
        let (queue, fence): (ID3D12CommandQueue, ID3D12Fence) = unsafe {
            (
                device
                    .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() })
                    .expect("queue"),
                device.CreateFence(0, D3D12_FENCE_FLAG_NONE).expect("fence"),
            )
        };
        let mut value = 0u64;
        let mut run = |lists: &[(ID3D12CommandAllocator, ID3D12GraphicsCommandList)]| {
            value += 1;
            // SAFETY: closed lists of ours, run and waited for.
            unsafe {
                if let Some((_, l)) = lists.first() {
                    queue.ExecuteCommandLists(&[Some(l.cast::<ID3D12CommandList>().expect("list"))]);
                }
                queue.Signal(&fence, value).expect("signal");
                while fence.GetCompletedValue() < value {
                    std::thread::yield_now();
                }
            }
        };
        step("a submission run and waited for", &mut || run(&lists[8..9]));
        step("a second one, 200 ms later", &mut || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            run(&lists[8..9]);
        });
        step("64 upload buffers of 4 MiB made again", &mut || {
            for _ in 0..64 {
                kept.push(system_buffer(&device, upload, 4 << 20, D3D12_RESOURCE_STATE_GENERIC_READ));
            }
        });
        step("dropped, and a submission run", &mut || {
            kept.clear();
            run(&lists[8..9]);
        });
        step("all lists dropped, and a submission run", &mut || {
            lists.drain(..8);
            let one = lists.split_off(lists.len() - 1);
            lists.clear();
            run(&one);
        });
    }
}
