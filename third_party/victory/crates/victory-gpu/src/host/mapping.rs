//! When new memory of ours is mapped into the card's address space, found
//! out without a wait at its making.
//!
//! A heap or resource is resident once it is made, but its mapping into the
//! card's address space is work of the system's that runs later: lists sent
//! after the making wait for it, lists sent before run on. Those read our
//! descriptor heaps as they are, so a slot that shows new memory to them
//! before it is mapped is a page fault (the device losses of 2026-11-25).
//!
//! Each object was made not resident, made resident (`MakeResident`) and
//! waited for with a copy on a queue of ours, under one lock, on the thread
//! that made it: on an RTX 5080 some 2,000 objects a second (the copies of
//! buffer views' own bytes, `crate::shadow::exact`), 0.12 ms in
//! `MakeResident` and 0.07 ms in the wait each, 85 ms a frame on the
//! threads that record and translate lists, the longest wait 757 ms (run
//! nb1). `MakeResident` bought nothing: the losses went on with it alone.
//!
//! Now memory is made resident, and the copy that tells when it is mapped is
//! sent at once and not waited for: memory made before a [`ticket`] is
//! mapped once the queue's fence reaches it. One copy answers for all the
//! memory made before it, and only one is on the queue at a time. What shows
//! the memory in a slot asks first ([`shown`]): by then the answer is mostly
//! there, and a wait is for what was made a moment ago. Memory only lists
//! recorded after it name (scratch, upload and read-back buffers, root
//! views) is never waited for. Textures in blocks are not asked for: their
//! slots are shown only once the submissions begun before them have run
//! (`crate::descriptor::host::show_new_later`), and their blocks are made
//! ahead (`crate::pool`).
//!
//! `VICTORY_MAPPED_LATER=0`: every object is made not resident, made
//! resident and waited for where it is made, as before;
//! `VICTORY_WAIT_RESIDENT=0`: nothing is waited for at all.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::core::Interface;

/// Whether the card's mapping of new memory is found out without a wait
/// where it is made (`VICTORY_MAPPED_LATER`, on; 0: waited for there, as
/// before). Only while memory is waited for at all ([`super::wait_resident`]).
pub(crate) fn later() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    super::wait_resident() && *ON.get_or_init(|| std::env::var_os("VICTORY_MAPPED_LATER").is_none_or(|v| v != "0"))
}

/// What new memory is, for the counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum What {
    /// A block of textures (`crate::pool`).
    TextureBlock,
    /// A texture of its own.
    Texture,
    /// A block of copies of views' own bytes (`crate::pool::shared_piece`).
    CopyBlock,
    /// A buffer of its own: a host copy of title memory, its card side, a
    /// buffer of ours.
    Buffer,
}

impl What {
    const ALL: [What; 4] = [What::TextureBlock, What::Texture, What::CopyBlock, What::Buffer];

    fn index(self) -> usize {
        self as usize
    }

    fn name(self) -> &'static str {
        match self {
            What::TextureBlock => "blocks of textures",
            What::Texture => "textures",
            What::CopyBlock => "blocks of view copies",
            What::Buffer => "buffers",
        }
    }
}

/// The queue the copies go to, and the copy.
struct Aside {
    queue: ID3D12CommandQueue,
    list: ID3D12CommandList,
    /// The value of the last signal sent (0: none).
    sent: u64,
    _keep: (ID3D12CommandAllocator, [ID3D12Resource; 2]),
}
// SAFETY: D3D12 objects are free-threaded; used under the lock.
unsafe impl Send for Aside {}

static ASIDE: Mutex<Option<Aside>> = Mutex::new(None);
/// The queue's fence, read without the lock.
static FENCE: OnceLock<ID3D12Fence> = OnceLock::new();
/// The value the next signal will have: memory made now is mapped once the
/// fence reaches it.
static NEXT: AtomicU64 = AtomicU64::new(1);
/// The highest value the fence was seen to have.
static DONE: AtomicU64 = AtomicU64::new(0);

/// Per kind since the last line: made; asked for when shown; of those, waited
/// for; microseconds of those waits; the longest.
static COUNTS: [[AtomicU64; 5]; 4] = [const { [const { AtomicU64::new(0) }; 5] }; 4];
/// Since the last line: copies sent.
static SENT: AtomicU64 = AtomicU64::new(0);

/// The ticket of memory made just now (after its making returned): it is
/// mapped once [`done`] says so of it.
pub(crate) fn ticket() -> u64 {
    NEXT.load(Ordering::SeqCst)
}

/// Whether memory of ticket `t` is mapped (0: nothing to wait for).
pub(crate) fn done(t: u64) -> bool {
    if t == 0 || DONE.load(Ordering::Acquire) >= t {
        return true;
    }
    let Some(fence) = FENCE.get() else {
        return false;
    };
    // SAFETY: a plain query of a live fence.
    let reached = unsafe { fence.GetCompletedValue() };
    // (All ones: the device is gone, and nothing will be mapped.)
    if reached == u64::MAX {
        return true;
    }
    DONE.fetch_max(reached, Ordering::AcqRel);
    reached >= t
}

fn aside(device: &ID3D12Device) -> windows::core::Result<Aside> {
    let buffer = |kind: D3D12_HEAP_TYPE, state: D3D12_RESOURCE_STATES| -> windows::core::Result<ID3D12Resource> {
        let mut made: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe {
            device.CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() },
                D3D12_HEAP_FLAG_NONE,
                &D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: 256,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    ..Default::default()
                },
                state,
                None,
                &mut made,
            )?;
        }
        made.ok_or_else(windows::core::Error::empty)
    };
    let from = buffer(D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ)?;
    let to = buffer(D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST)?;
    // SAFETY: plain creation and recording; the objects the list names are kept.
    unsafe {
        let queue: ID3D12CommandQueue =
            device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_COPY, ..Default::default() })?;
        let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)?;
        let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None)?;
        list.CopyBufferRegion(&to, 0, &from, 0, 16);
        list.Close()?;
        if FENCE.get().is_none() {
            let _ = FENCE.set(device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?);
        }
        Ok(Aside { queue, list: list.cast()?, sent: 0, _keep: (allocator, [from, to]) })
    }
}

/// What [`send`] did.
#[derive(Debug, PartialEq, Eq)]
enum Sent {
    /// A copy that answers for `t` went before.
    Before,
    /// The copy on the queue answers only for memory made before it: one
    /// more goes once it is done.
    Busy(u64),
    /// One went now.
    Now,
}

/// Sends the copy that answers for ticket `t` (and for all memory made so
/// far), unless one went already or one is still on the queue. Never waits.
fn send(device: &ID3D12Device, t: u64) -> windows::core::Result<Sent> {
    let mut aside = ASIDE.lock().unwrap_or_else(|e| e.into_inner());
    if aside.is_none() {
        *aside = Some(self::aside(device)?);
    }
    let Some(a) = aside.as_mut() else {
        return Ok(Sent::Before);
    };
    if a.sent >= t {
        return Ok(Sent::Before);
    }
    if !done(a.sent) {
        return Ok(Sent::Busy(a.sent));
    }
    let Some(fence) = FENCE.get() else {
        return Ok(Sent::Before);
    };
    // Memory made from here on waits for the next one; this one goes after
    // everything made before (`t` was read after its making).
    let value = NEXT.fetch_add(1, Ordering::SeqCst);
    // SAFETY: a closed list of ours, not on the queue now (the last copy is
    // done); a fence of the device.
    unsafe {
        a.queue.ExecuteCommandLists(&[Some(a.list.clone())]);
        a.queue.Signal(fence, value)?;
    }
    a.sent = value;
    SENT.fetch_add(1, Ordering::Relaxed);
    Ok(Sent::Now)
}

/// Memory of ticket `t` is mapped when this returns (the copy for it is sent
/// if it did not go yet). Under no lock of ours while it waits.
fn wait(device: &ID3D12Device, t: u64) -> windows::core::Result<()> {
    while !done(t) {
        let until = match send(device, t)? {
            Sent::Before | Sent::Now => t,
            Sent::Busy(on_queue) => on_queue,
        };
        let Some(fence) = FENCE.get() else {
            return Ok(());
        };
        // SAFETY: without an event the call returns once the fence has the
        // value.
        unsafe { fence.SetEventOnCompletion(until, HANDLE::default())? };
    }
    Ok(())
}

/// New memory (`object`, of kind `what`) was made: its copy is sent, and
/// what shows it asks [`shown`] first. With [`later`] off, it is waited for
/// here, as before. Its ticket (0: mapped already).
pub(crate) fn made(device: &ID3D12Device, object: &impl Interface, what: What) -> u64 {
    if !super::wait_resident() {
        return 0;
    }
    COUNTS[what.index()][0].fetch_add(1, Ordering::Relaxed);
    if !later() {
        let _ = super::mapped(device);
        return 0;
    }
    let t = ticket();
    let _ = send(device, t);
    if !done(t) {
        let mut fresh = FRESH.lock().unwrap_or_else(|e| e.into_inner());
        fresh.retain(|_, &mut (ticket, _)| !done(ticket));
        fresh.insert(object.as_raw() as usize, (t, what));
        NEWEST.fetch_max(t, Ordering::AcqRel);
    }
    t
}

/// Objects made whose copy may not be done yet, by their address: their
/// ticket and kind.
static FRESH: std::sync::LazyLock<Mutex<std::collections::HashMap<usize, (u64, What)>>> =
    std::sync::LazyLock::new(Default::default);
/// The newest of their tickets (once it is done, none is waited for).
static NEWEST: AtomicU64 = AtomicU64::new(0);

/// A slot of ours is about to show `object` to lists that may already be on
/// the card: it is mapped when this returns. Mostly nothing to look up.
pub(crate) fn shown(object: &impl Interface) {
    let newest = NEWEST.load(Ordering::Acquire);
    if done(newest) {
        return;
    }
    let raw = object.as_raw() as usize;
    let Some((t, what)) = FRESH.lock().unwrap_or_else(|e| e.into_inner()).get(&raw).copied() else {
        return;
    };
    if let Some(device) = super::HOST.get().and_then(Option::as_ref).map(|host| &host.device) {
        wait_for(device, t, what);
    }
}

/// [`wait`], counted for `what`: ticket `t` is done when this returns.
pub(crate) fn wait_for(device: &ID3D12Device, t: u64, what: What) {
    let counts = &COUNTS[what.index()];
    counts[1].fetch_add(1, Ordering::Relaxed);
    if done(t) {
        return;
    }
    let started = std::time::Instant::now();
    let _ = wait(device, t);
    let us = started.elapsed().as_micros() as u64;
    counts[2].fetch_add(1, Ordering::Relaxed);
    counts[3].fetch_add(us, Ordering::Relaxed);
    counts[4].fetch_max(us, Ordering::Relaxed);
}

/// With the frame line: per kind, new memory made, asked for when shown and
/// waited for then, per frame over `frames`. Nothing while nothing was made.
pub(crate) fn line(frames: f64) {
    if !later() {
        return;
    }
    let mut parts = Vec::new();
    for what in What::ALL {
        let [made, asked, waited, us, most] = std::array::from_fn(|i| COUNTS[what.index()][i].swap(0, Ordering::Relaxed));
        if made + asked > 0 {
            parts.push(format!(
                "{} {:.1} made, {:.1} asked for when shown, {:.2} waited for ({:.2} ms, the longest {:.2})",
                what.name(),
                made as f64 / frames,
                asked as f64 / frames,
                waited as f64 / frames,
                us as f64 / 1e3 / frames,
                most as f64 / 1e3
            ));
        }
    }
    let sent = SENT.swap(0, Ordering::Relaxed);
    if parts.is_empty() && sent == 0 {
        return;
    }
    tracing::info!(
        "new memory of ours per frame, mapped without a wait where it is made (VICTORY_MAPPED_LATER=0: waited for there): {}; {:.1} copies sent to learn when it is mapped",
        parts.join("; "),
        sent as f64 / frames
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Memory made is not waited for where it is made: the copy that says
    /// when it is mapped is sent and its ticket comes back, and a slot that
    /// shows it finds it mapped (waiting, if it must, for that copy alone).
    /// One copy answers for everything made before it, and only one is on
    /// the queue at a time.
    #[test]
    fn new_memory_is_mapped_without_a_wait_where_it_is_made() {
        let Some(host) = super::super::host() else {
            return; // no host GPU here
        };
        if !later() {
            return;
        }
        let device = &host.device;
        let made = |n: u64| host.uav_buffer(4096 * n).expect("a buffer");
        let a = made(1);
        let t = super::made(device, &a, What::Buffer);
        assert!(t > 0, "a ticket");
        // Asked again for the same ticket: nothing more goes (or the copy of
        // another thread's is still on the queue).
        let again = send(device, t).unwrap();
        assert!(matches!(again, Sent::Before | Sent::Busy(_)), "{again:?}");
        // Made after it: the same ticket or a later one.
        let b = made(2);
        let u = super::made(device, &b, What::Buffer);
        assert!(u >= t, "{u} after {t}");
        shown(&b);
        assert!(done(u), "shown once mapped");
        shown(&a);
        assert!(done(t));
        // Two made one after the other: one copy answers for both, sent when
        // it is waited for.
        let c = made(3);
        let v = ticket();
        let d = made(4);
        let w = ticket();
        assert!(w >= v);
        wait(device, w).unwrap();
        assert!(done(v) && done(w));
        drop((c, d));
        // SAFETY: a plain query.
        assert!(unsafe { device.GetDeviceRemovedReason() }.is_ok());
    }

    /// Before, every buffer of its own was made not resident, made resident
    /// and waited for where it was made. Now none is: making a hundred
    /// buffers sends at most a copy per buffer and waits for none.
    #[test]
    fn making_buffers_waits_for_nothing() {
        let Some(host) = super::super::host() else {
            return; // no host GPU here
        };
        if !later() {
            return;
        }
        // (What waits where memory is made counts there, and nothing else.)
        let waits = || [super::super::RESIDENT[0].load(Ordering::Relaxed), super::super::MAPPED[0].load(Ordering::Relaxed)];
        let before = waits();
        let mut kept = Vec::new();
        for _ in 0..100 {
            kept.push(host.shared_buffer(256).expect("a buffer").0);
            kept.push(host.uav_buffer(256).expect("a buffer"));
        }
        assert_eq!(waits(), before, "no buffer made resident or waited for where it was made");
        assert_eq!(super::super::not_resident(), D3D12_HEAP_FLAG_NONE, "made resident");
    }
}
