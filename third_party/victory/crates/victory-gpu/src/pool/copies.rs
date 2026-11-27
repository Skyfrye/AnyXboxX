//! The buffers of copies of buffer views' own bytes (`crate::shadow::exact`),
//! placed in blocks of the PC's memory made once.
//!
//! Each such copy was a committed buffer of its own, made not resident, made
//! resident and waited for until mapped, under one lock, on the thread that
//! records or translates the list that asked: on an RTX 5080, which has no
//! raw twins, some 440 a frame (the title's parameter blocks at ring
//! positions), 0.19 ms each (run nb1). A buffer placed in a block that is
//! made, made resident and mapped once costs no allocation, no residency
//! step and no wait. A buffer's place is 64 KiB (its placement), as a
//! committed buffer's allocation was; its place is free again when the
//! last reference to the buffer goes (an object of ours the buffer holds
//! says so), as the memory of a committed buffer was given back then.
//! `VICTORY_COPY_BLOCKS=0`: each copy a buffer of its own again.

use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::{GUID, HRESULT, IUnknown, Interface};

/// Bytes of a block, and of a buffer's place in it.
const BLOCK: u64 = 64 << 20;
const PLACE: u64 = 64 << 10;
/// Places in a block.
const PLACES: u32 = (BLOCK / PLACE) as u32;
/// Empty blocks kept.
const SPARE: usize = 2;

pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_COPY_BLOCKS").is_none_or(|v| v != "0"))
}

/// A block: its memory, the ticket of its mapping
/// (`crate::host::mapping`), the places taken in it now, and the places
/// below which some were taken before.
struct Block<H> {
    memory: H,
    ticket: u64,
    used: u32,
    high: u32,
}

/// The blocks and the free places (block, place), the last given back
/// first.
struct Places<H> {
    blocks: Vec<Option<Block<H>>>,
    free: Vec<(u32, u32)>,
}

/// A place taken: block, place, the block's memory and ticket, and whether
/// the place was used before (its bytes are not zeros).
type Taken<H> = (u32, u32, H, u64, bool);

impl<H: Clone> Places<H> {
    const fn new() -> Places<H> {
        Places { blocks: Vec::new(), free: Vec::new() }
    }

    /// A free place; `make` makes a block (and its ticket) when there is
    /// none.
    fn take(&mut self, make: impl FnOnce() -> Option<(H, u64)>) -> Option<Taken<H>> {
        if self.free.is_empty() {
            let (memory, ticket) = make()?;
            self.add(memory, ticket);
        }
        let (b, p) = self.free.pop()?;
        let block = self.blocks.get_mut(b as usize)?.as_mut()?;
        block.used += 1;
        let again = p < block.high;
        block.high = block.high.max(p + 1);
        Some((b, p, block.memory.clone(), block.ticket, again))
    }

    /// A new block, its places free (taken after those free already).
    fn add(&mut self, memory: H, ticket: u64) {
        let i = match self.blocks.iter().position(Option::is_none) {
            Some(i) => i,
            None => {
                self.blocks.push(None);
                self.blocks.len() - 1
            }
        };
        self.blocks[i] = Some(Block { memory, ticket, used: 0, high: 0 });
        // Under those free already, the lowest first.
        let new = (0..PLACES).rev().map(|p| (i as u32, p));
        self.free.splice(0..0, new);
    }

    /// Place `p` of block `b` is free again; past [`SPARE`] empty blocks, an
    /// empty one goes.
    fn give(&mut self, b: u32, p: u32) {
        let Some(Some(block)) = self.blocks.get_mut(b as usize) else {
            return;
        };
        block.used = block.used.saturating_sub(1);
        self.free.push((b, p));
        if block.used == 0 && self.blocks.iter().flatten().filter(|k| k.used == 0).count() > SPARE {
            self.blocks[b as usize] = None;
            self.free.retain(|&(fb, _)| fb != b);
        }
    }

    fn blocks(&self) -> usize {
        self.blocks.iter().flatten().count()
    }
}

static PLACED: Mutex<Places<ID3D12Heap>> = Mutex::new(Places::new());

fn lock() -> std::sync::MutexGuard<'static, Places<ID3D12Heap>> {
    PLACED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Since the last line: buffers placed, of them in places used before, and
/// blocks made.
static COUNTS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
/// Places given back since the start.
static GIVEN: AtomicU64 = AtomicU64::new(0);

/// Free places below which the next block is made, beside.
const AHEAD: usize = PLACES as usize / 4;

/// Makes a block on a thread of its own, unless one is being made: the
/// threads that take places do not wait for its making or its mapping.
fn grow() {
    static GROWING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if GROWING.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new().name("victory view copies".into()).spawn(|| {
        if let Some(host) = crate::host::host()
            && let Some((heap, ticket)) = block(&host.device)
        {
            lock().add(heap, ticket);
        }
        GROWING.store(false, Ordering::Release);
    });
    if spawned.is_err() {
        GROWING.store(false, Ordering::Release);
    }
}

/// Makes blocks until `places` are free (measurements).
#[cfg(test)]
pub(crate) fn reserve(device: &ID3D12Device, places: usize) {
    while lock().free.len() < places {
        if !reserve_one(device) {
            return;
        }
    }
}

/// Makes a block (measurements).
#[cfg(test)]
pub(crate) fn reserve_one(device: &ID3D12Device) -> bool {
    let Some((heap, ticket)) = block(device) else { return false };
    lock().add(heap, ticket);
    true
}

/// A block: the PC's memory that the CPU maps (write-back) and shaders read
/// and write, as [`crate::host::Host::shared_buffer`]'s; and its ticket.
fn block(device: &ID3D12Device) -> Option<(ID3D12Heap, u64)> {
    let desc = D3D12_HEAP_DESC {
        SizeInBytes: BLOCK,
        Properties: D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            ..Default::default()
        },
        Alignment: 0,
        Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS | crate::host::not_resident(),
    };
    let mut heap: Option<ID3D12Heap> = None;
    // SAFETY: plain creation.
    if let Err(e) = unsafe { device.CreateHeap(&desc, &mut heap) } {
        crate::residency::allocation_error(&e);
        tracing::warn!(error = %e, "no block of view copies");
        return None;
    }
    let heap = heap?;
    if let Err(e) = crate::host::resident(device, &heap) {
        tracing::warn!(error = %e, "a block of view copies is not resident");
        return None;
    }
    crate::host::name(&heap, || "copies of views' own bytes (a block)".into());
    crate::ram::track(&heap, crate::ram::Heap::Shared, BLOCK);
    let ticket = crate::host::mapping::made(device, &heap, crate::host::mapping::What::CopyBlock);
    COUNTS[2].fetch_add(1, Ordering::Relaxed);
    Some((heap, ticket))
}

/// A buffer of `bytes` (64 KiB at most) that shaders read and write and the
/// CPU maps, all zeros, placed in a block, and its mapping; `None`: not
/// placed (too big, off, or the host refused), for a buffer of its own.
pub(crate) fn buffer(device: &ID3D12Device, bytes: u64) -> Option<(ID3D12Resource, *mut u8)> {
    if !on() || bytes == 0 || bytes > PLACE {
        return None;
    }
    let (b, p, heap, ticket, again) = {
        let mut places = lock();
        let taken = places.take(|| block(device))?;
        if places.free.len() < AHEAD {
            grow();
        }
        taken
    };
    // A new block's mapping: waited for (outside the lock) by the first
    // copy in it, if it is not there by then.
    if !crate::host::mapping::done(ticket) {
        crate::host::mapping::wait_for(device, ticket, crate::host::mapping::What::CopyBlock);
    }
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    let mut made: Option<ID3D12Resource> = None;
    let mut data = std::ptr::null_mut();
    // SAFETY: the place is no one else's; the heap lives while the buffer
    // does (the buffer holds it).
    let result = unsafe {
        device
            .CreatePlacedResource(&heap, u64::from(p) * PLACE, &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut made)
            .and_then(|()| made.as_ref().ok_or_else(windows::core::Error::empty)?.Map(0, None, Some(&mut data)))
    };
    let buffer = match (result, made) {
        (Ok(()), Some(made)) if !data.is_null() => made,
        (result, _) => {
            if let Err(e) = result {
                tracing::warn!(error = %e, bytes, "no buffer placed in a block of view copies");
            }
            lock().give(b, p);
            return None;
        }
    };
    if !give_back_with(&buffer, b, p) {
        // Never given back: the place stays taken, nothing is placed over
        // a buffer that may be alive.
        tracing::warn!("a placed view copy cannot say when it goes");
    }
    COUNTS[0].fetch_add(1, Ordering::Relaxed);
    if again {
        COUNTS[1].fetch_add(1, Ordering::Relaxed);
        // As a buffer of its own: zeros.
        // SAFETY: the mapping of the buffer's bytes, ours alone.
        unsafe { std::ptr::write_bytes(data.cast::<u8>(), 0, bytes as usize) };
    }
    #[cfg(feature = "diagnostics")]
    crate::book::made(&buffer, || "a copy of a view's bytes (placed)".into());
    Some((buffer, data.cast()))
}

/// For the frame line: buffers placed per frame over `frames`, of them in
/// places used before, blocks made, and blocks and places held now.
pub(crate) fn line(frames: f64) -> Option<String> {
    let [placed, again, blocks] = [0, 1, 2].map(|i| COUNTS[i].swap(0, Ordering::Relaxed));
    let (alive, held) = {
        let places = lock();
        (places.blocks(), places.blocks.iter().flatten().map(|b| u64::from(b.used)).sum::<u64>())
    };
    (placed + blocks > 0 || alive > 0).then(|| {
        format!(
            "{:.1} placed per frame in blocks made once ({:.1} in places used before, {blocks} blocks made; VICTORY_COPY_BLOCKS=0: buffers of their own), {held} held in {alive} blocks of {} MiB",
            placed as f64 / frames,
            again as f64 / frames,
            BLOCK >> 20
        )
    })
}

/// An object the buffer holds and releases when it goes: its place is free
/// then.
#[repr(C)]
struct Reclaim {
    methods: &'static [*const (); 3],
    references: AtomicU32,
    block: u32,
    place: u32,
}

struct Methods([*const (); 3]);
// SAFETY: function addresses only.
unsafe impl Sync for Methods {}

static RECLAIM: Methods = Methods([query as *const (), add_ref as *const (), release as *const ()]);

/// Makes `buffer` give place `place` of block `block` back when the last
/// reference to it goes; false if it cannot.
fn give_back_with(buffer: &ID3D12Resource, block: u32, place: u32) -> bool {
    const KEY: GUID = GUID::from_u128(0x2b41_7c9e_8d3a_4f61_b0c5_9e17_a4d3_6f28);
    let made = Box::into_raw(Box::new(Reclaim { methods: &RECLAIM.0, references: AtomicU32::new(1), block, place }));
    // SAFETY: a COM object of ours with one reference, which this takes and
    // drops at the end; the buffer takes its own.
    unsafe {
        let object = IUnknown::from_raw(made.cast());
        if buffer.SetPrivateDataInterface(&KEY, &object).is_err() {
            std::mem::forget(object);
            return false;
        }
    }
    true
}

unsafe extern "system" fn query(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    const E_NOINTERFACE: HRESULT = HRESULT(0x8000_4002_u32 as i32);
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
    E_NOINTERFACE
}

unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
    // SAFETY: one of ours, alive while a reference to it is.
    unsafe { (*this.cast::<Reclaim>()).references.fetch_add(1, Ordering::Relaxed) + 1 }
}

unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    // SAFETY: as above; the last reference frees the box it was made as.
    unsafe {
        let left = (*this.cast::<Reclaim>()).references.fetch_sub(1, Ordering::AcqRel) - 1;
        if left == 0 {
            let gone = Box::from_raw(this.cast::<Reclaim>());
            lock().give(gone.block, gone.place);
            GIVEN.fetch_add(1, Ordering::Relaxed);
        }
        left
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Places are taken from one block until it is full, the last given back
    /// is taken first (and said to be used before), and empty blocks past
    /// the spares go.
    #[test]
    fn places_are_taken_from_blocks_made_once() {
        let mut places: Places<u32> = Places::new();
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            Some((made.get(), 7))
        };
        let first: Vec<_> = (0..PLACES).map(|_| places.take(make).unwrap()).collect();
        assert_eq!(made.get(), 1, "one block for {PLACES} places");
        assert_eq!((first[0].0, first[0].1, first[0].3, first[0].4), (0, 0, 7, false));
        assert!(first.iter().all(|t| !t.4), "a new block's places are zeros");
        let next = places.take(make).unwrap();
        assert_eq!((next.0, next.1, made.get()), (1, 0, 2), "a second block once the first is full");
        places.give(0, 5);
        let again = places.take(make).unwrap();
        assert_eq!((again.0, again.1, again.4), (0, 5, true), "the last given back, used before");
        // Every place back: the blocks stay (two spares), a fourth empty one goes.
        for t in &first {
            places.give(t.0, t.1);
        }
        places.give(next.0, next.1);
        assert_eq!(places.blocks(), 2);
        let mut more = Vec::new();
        for _ in 0..(3 * PLACES) {
            more.push(places.take(make).unwrap());
        }
        assert_eq!(places.blocks(), 3);
        for t in more {
            places.give(t.0, t.1);
        }
        assert_eq!(places.blocks(), SPARE, "empty blocks past the spares go");
        assert!(places.free.iter().all(|&(b, _)| places.blocks[b as usize].is_some()), "no free place in a block gone");
    }

    /// A placed copy is a buffer of the bytes asked for (its views' width),
    /// in a block: CPU writes are what the card reads, and its place is
    /// free again once the buffer goes.
    #[test]
    fn a_placed_copy_is_a_buffer_of_its_own_in_a_block() {
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        if !on() {
            return;
        }
        let device = &host.device;
        let (a, data) = buffer(device, 512).expect("a placed buffer");
        // SAFETY: a live buffer.
        let desc = unsafe { a.GetDesc() };
        assert_eq!(desc.Width, 512);
        assert_ne!(desc.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0, 0);
        // SAFETY: the buffer's mapping, 512 bytes.
        unsafe {
            assert!(std::slice::from_raw_parts(data, 512).iter().all(|&b| b == 0), "zeros");
            std::ptr::write_bytes(data, 0xab, 512);
        }
        let (b, _) = buffer(device, 4096).expect("another");
        // SAFETY: live buffers.
        let (ga, gb) = unsafe { (a.GetGPUVirtualAddress(), b.GetGPUVirtualAddress()) };
        assert!(ga.abs_diff(gb).is_multiple_of(PLACE), "places of one block or another");
        let given = GIVEN.load(Ordering::Relaxed);
        drop(a);
        assert!(GIVEN.load(Ordering::Relaxed) > given, "its place is free once it goes");
        assert!(buffer(device, PLACE + 1).is_none(), "bigger ones are buffers of their own");
        drop(b);
        // SAFETY: a plain query.
        assert!(unsafe { device.GetDeviceRemovedReason() }.is_ok());
    }
}
