//! What a texture cut from a block shows before its first upload
//! (descriptors audit, finding F7).
//!
//! A texture cut from a block ([`super::texture`]) lies where another lay
//! once. Until something writes it, its texels are that texture's bytes read
//! in the new one's format: in a block-compressed one, noise in blocks of
//! 4x4 (rainbow blocks on a guard's forearms and hands). The pool counted on
//! every level being uploaded before anything read it, and nothing made sure
//! of it: a draw's table, a fresh table and the slots made when a host copy
//! is made show the host copy at once; the upload is left out when no staging
//! buffer can be had; and lists the card runs ahead of the one that uploads
//! read the slot by index.
//!
//! Two things keep those bytes from shaders:
//! - Zeros at placement (`VICTORY_POOL_ZERO`, on; 0: none, as before). A
//!   texture cut where another lay is written with zeros by a list of its
//!   own at the head of the next submission, ahead of every list that may
//!   read it, from a buffer of zeros in the card's memory. A texture cut from
//!   memory of a block no texture used holds zeros already: nothing is done.
//!   (It has no render target or depth flag, so neither a clear nor a
//!   discard initializes it: only a copy does.)
//! - Filled textures only (`VICTORY_FILLED_ONLY`, on; 0: shown at once, as
//!   before). No view of a texture cut from a block goes where lists read it
//!   until an upload into it was sent, or the list that reads it records one
//!   ahead of its draws. Meanwhile the slot shows the neutral stand-in
//!   (`crate::descriptor::host`'s, opaque grey).
//!
//! A line every five seconds counts both ([`line`]).

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

use crate::resource::HostTexture;

/// Whether textures cut where another lay get zeros first
/// (`VICTORY_POOL_ZERO`, on).
pub(crate) fn zero_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_POOL_ZERO").is_none_or(|v| v != "0"))
}

/// Whether a texture cut from a block is shown only once filled
/// (`VICTORY_FILLED_ONLY`, on).
pub(crate) fn filled_only() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_FILLED_ONLY").is_none_or(|v| v != "0"))
}

/// [`Fill::zero`]: no zeros wanted; wanted, and waiting for the next
/// submission; sent at the head of one; given up for an upload that writes
/// every level on the copy queue (`crate::texture::ahead`).
const NONE: u8 = 0;
const WAITING: u8 = 1;
const SENT: u8 = 2;
const TAKEN: u8 = 3;

/// What a host copy holds of its own: whether it was cut from a block, its
/// zeros, and whether an upload into it was sent.
pub(crate) struct Fill {
    pooled: bool,
    filled: AtomicBool,
    zero: AtomicU8,
    /// The submission its zeros went at the head of.
    zeroed_in: AtomicU64,
}

impl Fill {
    /// For host texture `texture`, just made.
    pub(crate) fn of(texture: &ID3D12Resource) -> Fill {
        let placed = super::placed(texture);
        Fill::placed(placed.is_some(), placed == Some(true) && zero_on())
    }

    /// For a texture not cut from a block (it holds zeros, or the host's own).
    pub(crate) fn none() -> Fill {
        Fill::placed(false, false)
    }

    fn placed(pooled: bool, zeros: bool) -> Fill {
        Fill {
            pooled,
            filled: AtomicBool::new(false),
            zero: AtomicU8::new(if zeros { WAITING } else { NONE }),
            zeroed_in: AtomicU64::new(0),
        }
    }

    /// Whether it may be shown where any list reads it: not cut from a
    /// block, or an upload into it was sent.
    pub(crate) fn ready(&self) -> bool {
        !self.pooled || !filled_only() || self.filled.load(Ordering::Acquire)
    }

    /// An upload into it was sent (or the host wrote it).
    pub(crate) fn sent(&self) {
        self.filled.store(true, Ordering::Release);
    }

    /// Whether it was cut from a block (tests).
    #[cfg(test)]
    pub(crate) fn pooled(&self) -> bool {
        self.pooled
    }

    /// One cut from a block (`pooled`) whose zeros wait (`zeros`), for
    /// tests.
    #[cfg(test)]
    pub(crate) fn for_tests(pooled: bool, zeros: bool) -> Fill {
        Fill::placed(pooled, zeros)
    }
}

/// Whether host copy `copy` may be shown to `list`'s draws: it is ready
/// ([`Fill::ready`]), or `list` records an upload into it ahead of them.
pub(crate) fn ready_for(copy: &HostTexture, list: &crate::host::HostList) -> bool {
    copy.fill.ready() || list.used(listed(copy))
}

/// What [`crate::host::HostList::first_use`] is told for a list that records
/// an upload into `copy` (bit 62: no other key of that set has it).
fn listed(copy: &HostTexture) -> usize {
    1 << 62 | (std::ptr::from_ref(copy) as usize) >> 2
}

/// `list` records an upload into `copy`, which writes what its views show:
/// its draws may show it from now on, and every list once `list` is sent.
pub(crate) fn uploading(list: &crate::host::HostList, copy: &Arc<HostTexture>) {
    if !copy.fill.pooled {
        return;
    }
    list.first_use(listed(copy));
    if !copy.fill.filled.load(Ordering::Relaxed) {
        let copy = copy.clone();
        list.defer_host(move || copy.fill.sent());
    }
}

/// `copy` is written now by other means than an upload a list sends: a copy
/// or resolve of the host's, a copy queue's upload that has run.
pub(crate) fn written(copy: &HostTexture) {
    copy.fill.sent();
}

/// Host copies whose zeros wait for the next submission.
static ZEROS_DUE: Mutex<Vec<Arc<HostTexture>>> = Mutex::new(Vec::new());

/// Host copy `copy` was just made: its zeros wait for the next submission if
/// it lies where another texture lay ([`zero_on`]).
pub(crate) fn placed(copy: &Arc<HostTexture>) {
    if copy.fill.zero.load(Ordering::Relaxed) == WAITING {
        lock(&ZEROS_DUE).push(copy.clone());
    }
}

/// The copy queue takes `copy`'s first upload, which writes every level of
/// it (`crate::texture::ahead`). Its zeros, if still waiting, are given up;
/// `false` if they are on the card now (the upload is the lists', behind
/// them).
pub(crate) fn for_copy_queue(copy: &HostTexture) -> bool {
    copy.fill.give_zeros_up(crate::host::submissions().1)
}

impl Fill {
    /// [`for_copy_queue`], with the submissions up to `finished` run.
    fn give_zeros_up(&self, finished: u64) -> bool {
        match self.zero.compare_exchange(WAITING, TAKEN, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => {
                ZEROS[2].fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(SENT) => self.zeroed_in.load(Ordering::Acquire) <= finished,
            Err(_) => true,
        }
    }
}

/// Bytes of the buffer of zeros the copies read: rows of a level are copied
/// that many at a time.
const SOURCE_BYTES: u64 = 8 << 20;

/// A buffer of zeros in the card's memory, and the upload buffer it is
/// filled from by the first list that reads it.
struct Source {
    buffer: ID3D12Resource,
    upload: ID3D12Resource,
    filled: AtomicBool,
}

fn source(host: &crate::host::Host) -> Option<&'static Source> {
    static SOURCE: std::sync::OnceLock<Option<Source>> = std::sync::OnceLock::new();
    SOURCE
        .get_or_init(|| {
            let buffer = host.uav_buffer(SOURCE_BYTES)?;
            crate::host::name(&buffer, || "zeros for textures cut where another lay".into());
            // (Copied from as soon as the next submission runs.)
            crate::host::mapping::shown(&buffer);
            let (upload, data) = host.upload_buffer(SOURCE_BYTES)?;
            // SAFETY: the upload buffer's mapping, `SOURCE_BYTES` long.
            unsafe { std::ptr::write_bytes(data, 0, SOURCE_BYTES as usize) };
            Some(Source { buffer, upload, filled: AtomicBool::new(false) })
        })
        .as_ref()
}

/// A list of zeros at the head of a submission: its allocator and list, the
/// submission it went with, and the textures it names, held until that ran.
struct Front {
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    number: u64,
    kept: Vec<Arc<HostTexture>>,
}

static FRONTS: Mutex<Vec<Front>> = Mutex::new(Vec::new());

/// Since the start: textures zeroed where another lay, their bytes, zeros
/// given up for an upload on the copy queue, textures gone before their
/// zeros were due, textures not zeroed whole (a row wider than the zeros).
pub(crate) static ZEROS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// At submission `number`, before any of its lists: the list that writes
/// zeros into the host copies cut where another texture lay since the last
/// submission. `None`: there are none.
pub(crate) fn front(host: &crate::host::Host, number: u64) -> Option<ID3D12CommandList> {
    let due = std::mem::take(&mut *lock(&ZEROS_DUE));
    if due.is_empty() {
        return None;
    }
    front_of(host, number, due)
}

/// [`front`] of the host copies `due`.
fn front_of(host: &crate::host::Host, number: u64, due: Vec<Arc<HostTexture>>) -> Option<ID3D12CommandList> {
    let mut zero = Vec::with_capacity(due.len());
    for copy in due {
        // Held by nothing else: no view shows it and no list names it.
        if Arc::strong_count(&copy) == 1 {
            if copy.fill.zero.compare_exchange(WAITING, NONE, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                ZEROS[3].fetch_add(1, Ordering::Relaxed);
            }
            continue;
        }
        if copy.fill.zero.compare_exchange(WAITING, SENT, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            copy.fill.zeroed_in.store(number, Ordering::Release);
            zero.push(copy);
        }
    }
    if zero.is_empty() {
        return None;
    }
    let source = source(host)?;
    let finished = crate::host::submissions().1;
    let mut fronts = lock(&FRONTS);
    let at = match fronts.iter().position(|f| f.number <= finished) {
        Some(at) => at,
        None => {
            // SAFETY: plain object creation.
            let made = unsafe {
                (|| -> windows::core::Result<Front> {
                    let allocator: ID3D12CommandAllocator =
                        host.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
                    let list: ID3D12GraphicsCommandList =
                        host.device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
                    list.Close()?;
                    Ok(Front { allocator, list, number: 0, kept: Vec::new() })
                })()
            };
            match made {
                Ok(front) => fronts.push(front),
                Err(e) => {
                    tracing::warn!(error = %e, "no host list for the zeros of textures cut where another lay");
                    return None;
                }
            }
            fronts.len() - 1
        }
    };
    let front = &mut fronts[at];
    front.kept.clear();
    let textures: Vec<(&ID3D12Resource, u32)> =
        zero.iter().map(|copy| (&copy.texture, copy.state.load(Ordering::Relaxed))).collect();
    // SAFETY: the front's last submission has finished (its number is
    // `finished` or older); the textures and the zeros live until this one
    // has (`kept`, and for ever).
    let recorded = unsafe {
        front.allocator.Reset().and_then(|()| {
            front.list.Reset(&front.allocator, None)?;
            if !source.filled.swap(true, Ordering::AcqRel) {
                front.list.CopyBufferRegion(&source.buffer, 0, &source.upload, 0, SOURCE_BYTES);
                front.list.ResourceBarrier(&[crate::host::transition(
                    &source.buffer,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
            }
            let (bytes, short) = record(&host.device, &front.list, &source.buffer, &textures);
            ZEROS[0].fetch_add(textures.len() as u64, Ordering::Relaxed);
            ZEROS[1].fetch_add(bytes, Ordering::Relaxed);
            ZEROS[4].fetch_add(short, Ordering::Relaxed);
            front.list.Close()
        })
    };
    if let Err(e) = recorded {
        tracing::warn!(error = %e, "the zeros of textures cut where another lay were not recorded");
        return None;
    }
    front.number = number;
    front.kept = zero;
    front.list.cast().ok()
}

/// Records into `list` zeros over every subresource of each of `textures`
/// (in the state each says, and left in it), read from `source`
/// ([`SOURCE_BYTES`] of zeros, a copy source). Returns the bytes written and
/// how many textures were not written whole.
///
/// # Safety
/// `list` is open; the resources are live until it has run.
pub(crate) unsafe fn record(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    source: &ID3D12Resource,
    textures: &[(&ID3D12Resource, u32)],
) -> (u64, u64) {
    let dest = D3D12_RESOURCE_STATE_COPY_DEST;
    let mut before = Vec::with_capacity(2 * textures.len());
    let mut after = Vec::with_capacity(textures.len());
    for &(texture, state) in textures {
        // What the block held of the texture that lay there is done with.
        before.push(D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_ALIASING,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Aliasing: std::mem::ManuallyDrop::new(D3D12_RESOURCE_ALIASING_BARRIER {
                    pResourceBefore: std::mem::ManuallyDrop::new(None),
                    // SAFETY: a borrowed pointer the barrier never releases.
                    pResourceAfter: unsafe { std::mem::transmute_copy(texture) },
                }),
            },
        });
        let state = D3D12_RESOURCE_STATES(state as i32);
        if state != dest {
            before.push(crate::host::transition(texture, state, dest));
            after.push(crate::host::transition(texture, dest, state));
        }
    }
    let (mut bytes, mut short) = (0, 0);
    // SAFETY: the caller's.
    unsafe {
        list.ResourceBarrier(&before);
        for &(texture, _) in textures {
            let (written, whole) = zeros_into(device, list, source, texture);
            bytes += written;
            short += u64::from(!whole);
        }
        if !after.is_empty() {
            list.ResourceBarrier(&after);
        }
    }
    (bytes, short)
}

/// Planes of a texture of `format` (2 for depth and stencil, video's
/// luma and chroma): each has subresources of its own.
fn planes(device: &ID3D12Device, format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT) -> u32 {
    let mut info = D3D12_FEATURE_DATA_FORMAT_INFO { Format: format, PlaneCount: 0 };
    // SAFETY: a plain query into a structure of its size.
    let asked = unsafe {
        device.CheckFeatureSupport(D3D12_FEATURE_FORMAT_INFO, (&raw mut info).cast(), size_of_val(&info) as u32)
    };
    if asked.is_ok() { u32::from(info.PlaneCount).max(1) } else { 1 }
}

/// Copies of zeros into every subresource of `texture`, which is in the copy
/// destination state: a level (and its slices) at once where the zeros hold
/// it, else as many rows of blocks at a time as they do. Returns the bytes
/// written and whether every row was.
///
/// # Safety
/// As [`record`].
unsafe fn zeros_into(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    source: &ID3D12Resource,
    texture: &ID3D12Resource,
) -> (u64, bool) {
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let layers = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D { 1 } else { u32::from(d.DepthOrArraySize) };
    let count = u32::from(d.MipLevels).max(1) * layers * planes(device, d.Format);
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
    let mut rows = vec![0u32; count as usize];
    let mut total = 0u64;
    // SAFETY: outputs for `count` subresources.
    unsafe {
        device.GetCopyableFootprints(
            &d,
            0,
            count,
            0,
            Some(footprints.as_mut_ptr()),
            Some(rows.as_mut_ptr()),
            None,
            Some(&mut total),
        );
    }
    if total == u64::MAX {
        return (0, false);
    }
    let (mut bytes, mut whole) = (0u64, true);
    for (i, (placed, &n)) in footprints.iter().zip(&rows).enumerate() {
        let f = placed.Footprint;
        let pitch = u64::from(f.RowPitch);
        if n == 0 || pitch == 0 || pitch > SOURCE_BYTES {
            whole &= n == 0;
            continue;
        }
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: i as u32 },
        };
        // Texel rows a row of blocks is (4 in block-compressed formats).
        let block = f.Height.div_ceil(n).max(1);
        let slice = pitch * u64::from(n);
        let slices = if slice * u64::from(f.Depth) <= SOURCE_BYTES { f.Depth.max(1) } else { 1 };
        let at_a_time = if slice <= SOURCE_BYTES { n } else { (SOURCE_BYTES / pitch) as u32 };
        let mut z = 0;
        while z < f.Depth.max(1) {
            let mut row = 0;
            while row < n {
                let k = at_a_time.min(n - row);
                let y = row * block;
                let height = (k * block).min(f.Height.saturating_sub(y));
                if height == 0 {
                    break;
                }
                let from = D3D12_TEXTURE_COPY_LOCATION {
                    // SAFETY: as above.
                    pResource: unsafe { std::mem::transmute_copy(source) },
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                            Offset: 0,
                            Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Height: height, Depth: slices, ..f },
                        },
                    },
                };
                // SAFETY: the caller's; the region lies in the subresource,
                // the footprint in the zeros.
                unsafe { list.CopyTextureRegion(&to, 0, y, z, &from, None) };
                bytes += pitch * u64::from(k) * u64::from(slices);
                row += k;
            }
            z += slices;
        }
    }
    (bytes, whole)
}

/// Where the stand-in was shown for a texture cut from a block that no
/// upload had filled: a title slot made with no list (a write, or the slots
/// made when a host copy is), one made for a draw whose list did not upload
/// it, a fresh table's slot, a slot a pass would have made again or
/// published.
#[derive(Clone, Copy)]
pub(crate) enum Stood {
    Written = 0,
    Drawn = 1,
    Tabled = 2,
    Remade = 3,
}

/// Since the start, by [`Stood`].
static STOOD: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// Flips of slots held back at the last pass that made flips: what they
/// would show is a texture cut from a block that no upload filled yet.
pub(crate) static FLIPS_HELD: AtomicU64 = AtomicU64::new(0);

pub(crate) fn stood(at: Stood) {
    STOOD[at as usize].fetch_add(1, Ordering::Relaxed);
}

/// Title slots shown at once to a list that records the upload of the
/// texture cut from a block they show, before that upload was sent: their
/// heap, slot and record, and the host copy. A decoration bake that may read
/// them by index uploads them itself first ([`take_early`]).
static EARLY: Mutex<Vec<(usize, u32, u64, Weak<HostTexture>)>> = Mutex::new(Vec::new());

/// Since the start: slots shown so ([`EARLY`]), and of them uploaded first
/// by a bake.
pub(crate) static EARLY_SHOWN: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Slot `index` of heap `heap`, made from the record `key`, shows host copy
/// `copy` to a list that uploads it, before that upload was sent.
pub(crate) fn shown_early(heap: usize, index: u32, key: u64, copy: &Arc<HostTexture>) {
    EARLY_SHOWN[0].fetch_add(1, Ordering::Relaxed);
    let mut early = lock(&EARLY);
    if early.len() >= 4096 {
        early.retain(|(_, _, _, copy)| copy.upgrade().is_some_and(|copy| !copy.fill.ready()));
    }
    early.push((heap, index, key, Arc::downgrade(copy)));
}

/// The slots of heap `heap` (and their records) shown early ([`shown_early`])
/// whose upload has still not been sent; those of the heap are forgotten.
pub(crate) fn take_early(heap: usize) -> Vec<(u32, u64)> {
    let mut early = lock(&EARLY);
    if early.is_empty() {
        return Vec::new();
    }
    let mut taken = Vec::new();
    early.retain(|(of, index, key, copy)| {
        if *of != heap {
            return true;
        }
        if copy.upgrade().is_some_and(|copy| !copy.fill.ready()) {
            taken.push((*index, *key));
        }
        false
    });
    taken
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The line every five seconds: what was done since the start, and the
/// zeros and stand-ins a frame since the last line.
pub(crate) fn line() -> String {
    static LAST: Mutex<(u64, [u64; 2])> = Mutex::new((0, [0; 2]));
    let z = ZEROS.each_ref().map(|n| n.load(Ordering::Relaxed));
    let s = STOOD.each_ref().map(|n| n.load(Ordering::Relaxed));
    let e = EARLY_SHOWN.each_ref().map(|n| n.load(Ordering::Relaxed));
    let stood: u64 = s.iter().sum();
    let frame = crate::frame::current();
    let mut last = lock(&LAST);
    let frames = frame.saturating_sub(last.0).max(1) as f64;
    let [zeroed, grey] = [z[0], stood].map(|n| n as f64);
    let lately = [(zeroed - last.1[0] as f64) / frames, (grey - last.1[1] as f64) / frames];
    *last = (frame, [z[0], stood]);
    format!(
        "textures cut from blocks, before their first upload (VICTORY_POOL_ZERO={}, VICTORY_FILLED_ONLY={}), a frame lately: \
         {:.1} zeroed where another texture lay, {:.1} slots shown the stand-in as never filled; since the start: {} zeroed \
         ({} MB, {} not whole), {} left to their upload on the copy queue, {} gone before their zeros; {stood} stand-ins \
         ({} title slots made with no list, {} for a draw whose list did not upload them, {} fresh tables' slots, {} not \
         made again or published), {} flips held at the last pass; {} slots shown at once to the list that uploads them \
         ({} of them uploaded first by a decoration bake)",
        u8::from(zero_on()),
        u8::from(filled_only()),
        lately[0],
        lately[1],
        z[0],
        z[1] >> 20,
        z[4],
        z[2],
        z[3],
        s[0],
        s[1],
        s[2],
        s[3],
        FLIPS_HELD.load(Ordering::Relaxed),
        e[0],
        e[1],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A texture cut from a block is shown where any list reads it only once
    /// an upload into it was sent; to a list that records one ahead of its
    /// draws, at once. One not cut from a block is shown at once.
    #[test]
    fn a_pooled_texture_is_shown_once_an_upload_was_sent() {
        let fill = Fill::placed(true, false);
        assert!(!fill.ready() || !filled_only());
        fill.sent();
        assert!(fill.ready());
        assert!(Fill::none().ready());
        // Zeros are wanted only where another texture lay.
        assert_eq!(Fill::placed(true, true).zero.load(Ordering::Relaxed), WAITING);
        assert_eq!(Fill::placed(true, false).zero.load(Ordering::Relaxed), NONE);
    }

    /// An upload on the copy queue (which writes every level) takes the place
    /// of zeros that still wait, and waits for zeros already sent until their
    /// submission has finished.
    #[test]
    fn the_copy_queue_gives_waiting_zeros_up_and_waits_for_sent_ones() {
        let waiting = Fill::placed(true, true);
        // Submissions up to 7 have run.
        let copy_queue = |fill: &Fill| fill.give_zeros_up(7);
        assert!(copy_queue(&waiting));
        assert_eq!(waiting.zero.load(Ordering::Relaxed), TAKEN);
        let sent = Fill::placed(true, true);
        sent.zero.store(SENT, Ordering::Relaxed);
        sent.zeroed_in.store(8, Ordering::Relaxed);
        assert!(!copy_queue(&sent), "submission 8 has not finished");
        sent.zeroed_in.store(7, Ordering::Relaxed);
        assert!(copy_queue(&sent));
        assert!(copy_queue(&Fill::none()));
    }

    /// A direct queue of its own and a list, run and waited for.
    struct Bench {
        queue: ID3D12CommandQueue,
        allocator: ID3D12CommandAllocator,
        list: ID3D12GraphicsCommandList,
        fence: ID3D12Fence,
        value: std::cell::Cell<u64>,
    }

    impl Bench {
        fn new(device: &ID3D12Device) -> Bench {
            // SAFETY: plain object creation.
            unsafe {
                let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
                Bench {
                    queue: device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).unwrap(),
                    list: device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).unwrap(),
                    allocator,
                    fence: device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap(),
                    value: std::cell::Cell::new(0),
                }
            }
        }

        /// Runs `lists` and then the bench's own, waits, and opens the
        /// bench's list again.
        fn run(&self, lists: &[ID3D12CommandList]) {
            // SAFETY: closed lists whose resources live until the wait.
            unsafe {
                self.list.Close().unwrap();
                let mut all: Vec<Option<ID3D12CommandList>> = lists.iter().cloned().map(Some).collect();
                all.push(Some(self.list.cast().unwrap()));
                self.queue.ExecuteCommandLists(&all);
                self.value.set(self.value.get() + 1);
                self.queue.Signal(&self.fence, self.value.get()).unwrap();
                self.fence.SetEventOnCompletion(self.value.get(), windows::Win32::Foundation::HANDLE::default()).unwrap();
                self.allocator.Reset().unwrap();
                self.list.Reset(&self.allocator, None).unwrap();
            }
        }
    }

    /// A texture of `desc` placed in a heap of its own where one whose every
    /// byte was 0xA5 lay, in the common state; its subresources' footprints
    /// and their bytes.
    struct Placed {
        _heap: ID3D12Heap,
        texture: ID3D12Resource,
        footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
        total: u64,
    }

    fn over_a_pattern(host: &crate::host::Host, bench: &Bench, desc: &D3D12_RESOURCE_DESC) -> Placed {
        let device = &host.device;
        let layers = if desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D { 1 } else { u32::from(desc.DepthOrArraySize) };
        let count = u32::from(desc.MipLevels) * layers;
        // SAFETY: plain queries and creation; the copy is waited for.
        unsafe {
            let info = device.GetResourceAllocationInfo(0, &[*desc]);
            let mut heap: Option<ID3D12Heap> = None;
            device
                .CreateHeap(
                    &D3D12_HEAP_DESC {
                        SizeInBytes: info.SizeInBytes.next_multiple_of(65536),
                        Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
                        Alignment: 0,
                        Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES,
                    },
                    &mut heap,
                )
                .unwrap();
            let heap = heap.unwrap();
            let place = || {
                let mut t: Option<ID3D12Resource> = None;
                device.CreatePlacedResource(&heap, 0, desc, D3D12_RESOURCE_STATE_COMMON, None, &mut t).unwrap();
                t.unwrap()
            };
            let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
            let mut total = 0;
            device.GetCopyableFootprints(desc, 0, count, 0, Some(footprints.as_mut_ptr()), None, None, Some(&mut total));
            let (pattern, data) = host.upload_buffer(total).unwrap();
            std::ptr::write_bytes(data, 0xA5, total as usize);
            let first = place();
            copies(&bench.list, &first, &pattern, &footprints, false);
            bench.run(&[]);
            drop(first);
            Placed { texture: place(), _heap: heap, footprints, total }
        }
    }

    /// Records copies of whole subresources between `texture` and `buffer`.
    ///
    /// # Safety
    /// An open list; the resources live until it has run.
    unsafe fn copies(
        list: &ID3D12GraphicsCommandList,
        texture: &ID3D12Resource,
        buffer: &ID3D12Resource,
        footprints: &[D3D12_PLACED_SUBRESOURCE_FOOTPRINT],
        into_buffer: bool,
    ) {
        for (i, placed) in footprints.iter().enumerate() {
            // SAFETY: borrowed pointers the locations never release; the
            // caller's.
            unsafe {
                let sub = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(texture),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: i as u32 },
                };
                let rows = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(buffer),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: *placed },
                };
                if into_buffer {
                    list.CopyTextureRegion(&rows, 0, 0, 0, &sub, None);
                } else {
                    list.CopyTextureRegion(&sub, 0, 0, 0, &rows, None);
                }
            }
        }
    }

    /// The bytes of every subresource of `placed`, as a copy reads them from
    /// the common state.
    fn read(host: &crate::host::Host, bench: &Bench, placed: &Placed) -> Vec<u8> {
        let back = host.read_back_buffer(placed.total).unwrap();
        let (common, source) = (D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_SOURCE);
        // SAFETY: an open list, live resources; mapped after the copy ran.
        unsafe {
            bench.list.ResourceBarrier(&[crate::host::transition(&placed.texture, common, source)]);
            copies(&bench.list, &placed.texture, &back, &placed.footprints, true);
            bench.list.ResourceBarrier(&[crate::host::transition(&placed.texture, source, common)]);
            bench.run(&[]);
            let mut mapped = std::ptr::null_mut();
            back.Map(0, None, Some(&mut mapped)).unwrap();
            let seen = std::slice::from_raw_parts(mapped.cast::<u8>(), placed.total as usize).to_vec();
            back.Unmap(0, None);
            seen
        }
    }

    /// The zeros of [`record`] reach every byte of every level and slice of a
    /// texture placed where another lay: a block-compressed one with a level
    /// larger than the zeros (copied in rows of blocks), an array of 4 bytes a
    /// texel, and a volume (its slices at once). The texture is left in the
    /// state it was in.
    #[test]
    fn zeros_cover_what_the_last_texture_in_the_block_left() {
        use windows::Win32::Graphics::Dxgi::Common::*;
        let Some(host) = crate::host::host() else { return };
        let device = &host.device;
        let bench = Bench::new(device);
        // 8192x4096 BC1, 14 levels: level 0 is 16 MiB, twice the zeros.
        let shapes = [
            (D3D12_RESOURCE_DIMENSION_TEXTURE2D, DXGI_FORMAT_BC1_UNORM, 8192u64, 4096u32, 1u16, 14u16),
            (D3D12_RESOURCE_DIMENSION_TEXTURE2D, DXGI_FORMAT_R8G8B8A8_UNORM, 300, 200, 2, 3),
            (D3D12_RESOURCE_DIMENSION_TEXTURE3D, DXGI_FORMAT_R8G8B8A8_UNORM, 32, 32, 16, 2),
        ];
        for (dimension, format, width, height, depth, levels) in shapes {
            let desc = D3D12_RESOURCE_DESC {
                Dimension: dimension,
                Width: width,
                Height: height,
                DepthOrArraySize: depth,
                MipLevels: levels,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                ..Default::default()
            };
            let placed = over_a_pattern(host, &bench, &desc);
            let left = read(host, &bench, &placed);
            println!(
                "format {} dimension {}: {} of {} bytes the last texture's before the zeros",
                format.0,
                dimension.0,
                left.iter().filter(|&&b| b == 0xA5).count(),
                left.len()
            );
            // Zeros from the card's memory, as the head of a submission
            // writes them.
            let zeros = host.uav_buffer(SOURCE_BYTES).unwrap();
            let (up, data) = host.upload_buffer(SOURCE_BYTES).unwrap();
            // SAFETY: the mapping, the open list, live resources.
            let (bytes, short) = unsafe {
                std::ptr::write_bytes(data, 0, SOURCE_BYTES as usize);
                bench.list.CopyBufferRegion(&zeros, 0, &up, 0, SOURCE_BYTES);
                bench.list.ResourceBarrier(&[crate::host::transition(
                    &zeros,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
                record(device, &bench.list, &zeros, &[(&placed.texture, 0)])
            };
            assert_eq!(short, 0);
            assert!(bytes > 0);
            bench.run(&[]);
            let seen = read(host, &bench, &placed);
            let left = seen.iter().filter(|&&b| b != 0).count();
            assert_eq!(left, 0, "format {} dimension {}: {left} bytes not zero", format.0, dimension.0);
            // SAFETY: a plain query.
            assert!(unsafe { device.GetDeviceRemovedReason() }.is_ok());
        }
    }

    /// The head of a submission ([`front`]) zeroes a host copy cut where
    /// another texture lay, once, and leaves it in the state the lists find
    /// it in; one nothing else holds any more is let be.
    #[test]
    fn the_head_of_a_submission_zeroes_a_texture_placed_where_another_lay() {
        use windows::Win32::Graphics::Dxgi::Common::*;
        let Some(host) = crate::host::host() else { return };
        let bench = Bench::new(&host.device);
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 256,
            Height: 256,
            DepthOrArraySize: 1,
            MipLevels: 9,
            Format: DXGI_FORMAT_BC7_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        let placed = over_a_pattern(host, &bench, &desc);
        let mut copy = HostTexture::for_tests(placed.texture.clone(), 0);
        copy.fill = Fill::for_tests(true, true);
        let copy = Arc::new(copy);
        // Nothing but the queue holds it: no view shows it, no list names it.
        let gone = {
            let mut gone = HostTexture::for_tests(over_a_pattern(host, &bench, &desc).texture, 0);
            gone.fill = Fill::for_tests(true, true);
            Arc::new(gone)
        };
        let gone_before = ZEROS[3].load(Ordering::Relaxed);
        // (A submission number none has: no submission of another test
        // takes this front again while the bench runs it.)
        let front = front_of(host, u64::MAX, vec![copy.clone(), gone]).expect("a list of zeros");
        assert_eq!(copy.fill.zero.load(Ordering::Relaxed), SENT);
        assert!(ZEROS[3].load(Ordering::Relaxed) > gone_before, "a texture nothing holds is let be");
        assert_eq!(copy.state.load(Ordering::Relaxed), 0, "left in the state it was in");
        bench.run(&[front]);
        let seen = read(host, &bench, &placed);
        assert!(seen.iter().all(|&b| b == 0), "{} bytes not zero", seen.iter().filter(|&&b| b != 0).count());
        // Sent once: nothing for the next submission.
        assert!(front_of(host, u64::MAX, vec![copy.clone()]).is_none());
        // SAFETY: a plain query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }

    /// Through the way lists go to the card: a host copy told of at its
    /// placement ([`placed`]) is zeroed at the head of the next submission,
    /// and left in its state.
    #[test]
    #[ignore = "sends a submission of the engine's own: run by hand with --ignored"]
    fn the_next_submission_zeroes_a_texture_placed_where_another_lay() {
        use windows::Win32::Graphics::Dxgi::Common::*;
        let Some(host) = crate::host::host() else { return };
        let bench = Bench::new(&host.device);
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 512,
            Height: 512,
            DepthOrArraySize: 1,
            MipLevels: 10,
            Format: DXGI_FORMAT_BC3_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        let placed_texture = over_a_pattern(host, &bench, &desc);
        let mut copy = HostTexture::for_tests(placed_texture.texture.clone(), 0);
        copy.fill = Fill::for_tests(true, true);
        let copy = Arc::new(copy);
        placed(&copy);
        let list = host.new_list().unwrap();
        list.close();
        host.execute(&[&list]);
        list.reset();
        assert_eq!(copy.fill.zero.load(Ordering::Relaxed), SENT);
        assert_eq!(copy.state.load(Ordering::Relaxed), 0);
        let seen = read(host, &bench, &placed_texture);
        assert!(seen.iter().all(|&b| b == 0), "{} bytes not zero", seen.iter().filter(|&&b| b != 0).count());
        // SAFETY: a plain query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }
}
