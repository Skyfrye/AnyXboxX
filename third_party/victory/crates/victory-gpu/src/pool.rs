//! Host memory for the textures the title fills, and for their staging
//! buffers, cut from blocks made once.
//!
//! Each such texture had an allocation of its own and its staging buffer
//! another. On an RX 7900 XTX an allocation takes about 310 microseconds
//! whatever its size, on one thread (`texture::allocation_costs`), and a
//! texture cut from a block 6: the pass that makes the textures shaders
//! pick by index (`draw.rs` `heap_textures`) got through 350-500 of the
//! 24,000 queued when the world loads in each of its 250 ms turns, some
//! forty seconds of a world without its textures.
//! `VICTORY_TEXTURE_POOL=0` makes every one an allocation of its own again.
//!
//! A piece given back is used again only two seconds later (nothing the card
//! was told before that is still running: Windows resets a card that takes
//! that long over one submission), and by then no list names it (the owners' rule, as for allocations of their own): a
//! texture's piece comes back when the last reference to the host texture
//! goes (an object of ours the texture holds says so), a staging buffer's
//! when the last list that recorded its copy is reset.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{GUID, HRESULT, IUnknown, Interface};

pub(crate) mod copies;

/// Bytes of a block; pieces of more than half of one are not pooled (with
/// blocks made ahead, [`ahead`]: of more than one).
pub(crate) const BLOCK: u64 = 64 << 20;
/// Blocks with nothing in them that are kept.
const SPARE: usize = 2;
/// The same while the card has room ([`room`]): 4 GiB of them.
const SPARE_KEPT: usize = 64;
/// The same of blocks of staging buffers, which are the PC's memory: run
/// crash-1 held 5.2 GB of them.
const STAGING_KEPT: usize = 8;
/// How many are kept now.
static SPARES: AtomicUsize = AtomicUsize::new(SPARE);

/// Size classes of the pieces of texture blocks: a block holds pieces of
/// one class at a time, of up to 64 KiB, up to 1 MiB, or more
/// (`VICTORY_POOL_CLASSES=0`: of any size together, as before).
///
/// Most host copies of the levels a view shows (`resource::held_only`)
/// take one 64 KiB piece: 151,592 of them in run nbc-measure (2026-11-26),
/// 10,700 MB, 72 KB each, and 39,605 were replaced by copies with more
/// levels. Placed lowest first among larger textures, each one replaced
/// left a hole between them that only another small texture fits, and a
/// block emptied next to never: 1,769 MB of the blocks were free at the end,
/// with the card at 96.6 % of its budget. In a block of small pieces every
/// free piece fits the next small texture, and a block whose copies were
/// all replaced or let go empties and goes back to the system.
const CLASS_TOPS: [u64; 2] = [64 << 10, 1 << 20];

fn class_of(len: u64) -> u8 {
    CLASS_TOPS.iter().position(|&top| len <= top).unwrap_or(CLASS_TOPS.len()) as u8
}

pub(crate) fn classes() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_POOL_CLASSES").is_none_or(|v| v != "0"))
}

/// Milliseconds a block the sweep let every texture of go
/// ([`drain`]) is left to empty: no piece is cut from it while another
/// block, or a new one, can take it. Its textures go once the submissions
/// running then have run, and their pieces rest [`REST`]: a few seconds.
/// After this a block that did not empty (a texture in it was used again)
/// takes pieces as any other.
const DRAIN_MS: u64 = 10_000;

/// Blocks left to empty since the start ([`drain`]).
static DRAINED: AtomicU64 = AtomicU64::new(0);

/// Sets how many empty blocks stay, by the card's memory (budget and use):
/// [`SPARE_KEPT`] below 85 % ([`crate::host::keep_freed`]), else [`SPARE`].
/// With the system's count of room on ([`crate::vram::system_room`]): as
/// many as lie under that line ([`kept_spares`]), and the empty blocks past
/// them go back to the system now, not at the pool's next texture.
/// Asked once a second.
pub(crate) fn room(memory: Option<[u64; 4]>) {
    let system = crate::vram::system_room();
    let tick = now();
    if !crate::residency::on() {
        if !system {
            SPARES.store(spares(memory, crate::host::keep_freed()), Ordering::Relaxed);
            return;
        }
        if let Ok(mut pool) = TEXTURES.try_lock() {
            let limit = kept_spares(memory, crate::host::keep_freed(), pool.empty());
            SPARES.store(limit, Ordering::Relaxed);
            pool.reclaim(tick, limit, false);
        }
        return;
    }
    // (`VICTORY_RESIDENCY=1`, codec-d/cool_fox c64fc9b4/e94d277a: fewer
    // spares while the system is short of memory, and idle blocks let go now,
    // not at the pool's next allocation.)
    let pressure = crate::residency::pressured();
    let keep = crate::host::keep_freed() && !pressure;
    let mut limit = spares(memory, keep);
    if !system {
        SPARES.store(limit, Ordering::Relaxed);
    }
    if let Ok(mut pool) = TEXTURES.try_lock() {
        if system {
            limit = kept_spares(memory, keep, pool.empty());
            SPARES.store(limit, Ordering::Relaxed);
        }
        pool.reclaim(tick, limit, true);
    }
    if let Ok(mut pool) = STAGING.try_lock() {
        let staging_limit = if crate::residency::critical_commit() {
            0
        } else if pressure {
            1
        } else {
            limit
        };
        pool.reclaim(tick, staging_limit, true);
    }
}

fn spares(memory: Option<[u64; 4]>, keep: bool) -> usize {
    match memory {
        Some([budget, used, ..]) if keep && u128::from(used) * 100 < u128::from(budget) * 85 => SPARE_KEPT,
        _ => SPARE,
    }
}

/// The empty blocks kept, `empty` of them there now, as the system counts
/// the card's memory: those that lie under 85 % of its budget (the empty
/// ones count as used), between [`SPARE`] and [`SPARE_KEPT`]. The rule of
/// [`spares`] jumps from 64 to 2 at the line: right where the sweep lets
/// textures go down to ([`crate::host::vram_keep`]), 62 blocks would go
/// back and be made again ahead ([`ahead`]) each time the use crossed it.
fn kept_spares(memory: Option<[u64; 4]>, keep: bool, empty: usize) -> usize {
    match memory {
        Some([budget, used, ..]) if keep && budget > 0 => {
            let line = i128::from(budget / 100 * 85);
            let fit = (line - i128::from(used)).div_euclid(i128::from(BLOCK));
            (empty as i128 + fit).clamp(SPARE as i128, SPARE_KEPT as i128) as usize
        }
        _ => SPARE,
    }
}

/// Milliseconds until a piece given back is used again. (It was three
/// frames, a tenth of a second while the title shows a loading screen at 30
/// a second, with up to 18 submissions in the air.)
const REST: u64 = 2500;

fn now() -> u64 {
    crate::host::now_ms()
}

pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_POOL").is_none_or(|v| v != "0"))
}

/// How many blocks of textures are made ahead, so that one has been there
/// for a while when its first piece is taken (`VICTORY_RIPE_BLOCKS`, 12;
/// 0: none, and new blocks are used at once, as before).
///
/// A page fault of lists already on the card, in a texture made the same
/// tenth of a second as the first piece of a block made for it, lost the
/// device in runs n08 and n11 (2026-11-25): the title gives a slot that
/// those lists read by index a new texture. Why the card does not have the
/// new block's addresses yet is not known (n11 waited for `MakeResident`;
/// `indirect::tests::new_blocks_under_readers` never loses it). A block that
/// every submission begun before its making has left behind ([`Ripe`]) has
/// no such lists: pieces are cut from those first.
fn ahead() -> usize {
    static AHEAD: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *AHEAD.get_or_init(|| std::env::var("VICTORY_RIPE_BLOCKS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(12))
}

/// Submissions begun and finished ([`crate::host::submissions`]), and how
/// many blocks to make ahead (0: every block is ripe at once).
#[derive(Clone, Copy)]
struct Ripe {
    started: u64,
    finished: u64,
    ahead: usize,
}

impl Ripe {
    const ALL: Ripe = Ripe { started: 0, finished: 0, ahead: 0 };

    fn now() -> Ripe {
        let (started, finished) = crate::host::submissions();
        // Not more than are kept ([`room`]): they would go and be made again.
        Ripe { started, finished, ahead: ahead().min(SPARES.load(Ordering::Relaxed)) }
    }

    fn is<B>(self, block: &Block<B>) -> bool {
        self.ahead == 0 || block.made <= self.finished
    }
}

/// Pieces cut from blocks that were not ripe, for want of a ripe one.
static UNRIPE: AtomicU64 = AtomicU64::new(0);

/// The free ranges of a block: start and length, never two that touch.
struct Ranges(BTreeMap<u64, u64>);

impl Ranges {
    fn new(size: u64) -> Ranges {
        Ranges(BTreeMap::from([(0, size)]))
    }

    /// The lowest place for `len` bytes at a multiple of `align`.
    fn take(&mut self, len: u64, align: u64) -> Option<u64> {
        let (start, size, at) = self.0.iter().find_map(|(&start, &size)| {
            let at = start.next_multiple_of(align);
            (at + len <= start + size).then_some((start, size, at))
        })?;
        self.0.remove(&start);
        if at > start {
            self.0.insert(start, at - start);
        }
        if at + len < start + size {
            self.0.insert(at + len, start + size - at - len);
        }
        Some(at)
    }

    fn give(&mut self, mut start: u64, mut len: u64) {
        if let Some((&before, &size)) = self.0.range(..start).next_back()
            && before + size == start
        {
            self.0.remove(&before);
            (start, len) = (before, len + size);
        }
        if let Some(size) = self.0.remove(&(start + len)) {
            len += size;
        }
        self.0.insert(start, len);
    }

    fn whole(&self, size: u64) -> bool {
        self.0.get(&0) == Some(&size)
    }
}

/// A part of a block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Piece {
    block: usize,
    start: u64,
    len: u64,
}

struct Block<B> {
    memory: B,
    free: Ranges,
    /// The end of the highest piece ever taken: above it the memory is as
    /// it was made (zeros).
    used_to: u64,
    /// The newest submission begun when it was made ([`Ripe`]).
    made: u64,
    /// When that was (ms).
    #[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
    since: u64,
    /// The size class of its pieces ([`CLASS_TOPS`]); `None` while it is
    /// empty, or without classes.
    class: Option<u8>,
    /// Pieces taken from it and not given back.
    live: u32,
    /// Until when it is left to empty ([`DRAIN_MS`]); 0: it is not.
    draining: u64,
}

impl<B> Block<B> {
    /// Whether a piece of `class` may be cut from it at `now` (`None`: any).
    fn takes(&self, class: Option<u8>, now: u64) -> bool {
        self.draining <= now && class.is_none_or(|c| self.class.is_none_or(|own| own == c))
    }
}

struct Pool<B> {
    /// `None`: a block let go, its place to be used again.
    blocks: Vec<Option<Block<B>>>,
    /// Pieces given back, and when each may be used again.
    resting: VecDeque<(u64, Piece)>,
    /// The empty blocks it was last told to keep, and the most it keeps.
    spare: usize,
    most: usize,
    /// No new block is asked for before this time after one was refused
    /// ([`Pool::grow`]).
    retry_growth_at: u64,
}

impl<B: Clone> Pool<B> {
    const fn new(most: usize) -> Pool<B> {
        Pool { blocks: Vec::new(), resting: VecDeque::new(), spare: 0, most, retry_growth_at: 0 }
    }

    /// A piece of `len` bytes and the memory it lies in, and whether that
    /// part was ever used before; `make` makes a block when none has room.
    fn take(&mut self, len: u64, align: u64, now: u64, make: impl FnMut() -> Option<B>) -> Option<(Piece, B, bool)> {
        self.take_ripe(len, align, now, Ripe::ALL, None, make)
    }

    /// The same from a block that is ripe, if one has room; `make` makes one
    /// ahead too, when few are empty. With a size `class` ([`classes`]):
    /// from a block of that class, else an empty one. A block left to empty
    /// ([`Pool::drain`]) gives a piece only when no block can be made.
    fn take_ripe(
        &mut self,
        len: u64,
        align: u64,
        now: u64,
        ripe: Ripe,
        class: Option<u8>,
        mut make: impl FnMut() -> Option<B>,
    ) -> Option<(Piece, B, bool)> {
        // With blocks made ahead, what would be an allocation of its own (and
        // new when the card first reads it) lies in one of those too.
        if len == 0 || len > if ripe.ahead > 0 { BLOCK } else { BLOCK / 2 } {
            return None;
        }
        self.rested(now);
        let mut found = None;
        // With classes, the blocks of its class first: an empty block is
        // taken only when none of them has room, so empty ones stay empty.
        let passes: &[bool] = if class.is_some() { &[true, false] } else { &[false] };
        'look: for &own in passes {
            for (i, block) in self.blocks.iter_mut().enumerate() {
                if let Some(block) = block
                    && ripe.is(block)
                    && block.takes(class, now)
                    && (!own || block.class.is_some())
                    && let Some(start) = block.free.take(len, align)
                {
                    found = Some((i, start));
                    break 'look;
                }
            }
        }
        if found.is_none() && ripe.ahead > 0 {
            // The one that has been there the longest.
            let mut unripe: Vec<(u64, usize)> = self
                .blocks
                .iter()
                .enumerate()
                .filter_map(|(i, b)| b.as_ref().filter(|b| b.takes(class, now)).map(|b| (b.made, i)))
                .collect();
            unripe.sort_unstable();
            found = unripe.into_iter().find_map(|(_, i)| Some((i, self.blocks[i].as_mut()?.free.take(len, align)?)));
        }
        // A failed 64 MiB block must not trigger another driver call for each
        // small texture. Existing free ranges remain usable during backoff.
        let (i, start) = match found {
            Some(found) => found,
            None => match self.grow(now, &mut make) {
                Some(memory) => {
                    let i = self.add(memory, ripe.started);
                    (i, self.blocks[i].as_mut()?.free.take(len, align)?)
                }
                // None to be had: one left to empty, all the same.
                None => self.blocks.iter_mut().enumerate().find_map(|(i, b)| {
                    let b = b.as_mut().filter(|b| b.takes(class, u64::MAX))?;
                    Some((i, b.free.take(len, align)?))
                })?,
            },
        };
        if ripe.ahead > 0 {
            if !ripe.is(self.blocks[i].as_ref()?) {
                let n = UNRIPE.fetch_add(1, Ordering::Relaxed) + 1;
                if n.is_power_of_two() {
                    tracing::info!("textures cut from blocks made while lists ran that still run: {n}");
                }
            }
            let empty = self.blocks.iter().flatten().filter(|b| b.free.whole(BLOCK)).count();
            if empty < ripe.ahead
                && let Some(memory) = self.grow(now, &mut make)
            {
                self.add(memory, ripe.started);
            }
        }
        let block = self.blocks[i].as_mut()?;
        let again = start < block.used_to;
        block.used_to = block.used_to.max(start + len);
        if class.is_some() {
            block.class = class;
        }
        block.live += 1;
        // (Only a block left to empty for want of another gets here so.)
        block.draining = 0;
        Some((Piece { block: i, start, len }, block.memory.clone(), again))
    }

    /// A new block from `make`, unless one was refused less than 500 ms ago
    /// (codec-d/cool_fox 08e4574d: a refused 64 MiB block is not asked for
    /// again for each small texture; the blocks there are still used).
    fn grow(&mut self, now: u64, make: &mut impl FnMut() -> Option<B>) -> Option<B> {
        if now < self.retry_growth_at {
            return None;
        }
        let block = make();
        if block.is_none() {
            self.retry_growth_at = now.saturating_add(500);
        }
        block
    }

    fn add(&mut self, memory: B, made: u64) -> usize {
        let i = match self.blocks.iter().position(Option::is_none) {
            Some(i) => i,
            None => {
                self.blocks.push(None);
                self.blocks.len() - 1
            }
        };
        self.blocks[i] = Some(Block {
            memory,
            free: Ranges::new(BLOCK),
            used_to: 0,
            made,
            since: crate::host::now_ms(),
            class: None,
            live: 0,
            draining: 0,
        });
        i
    }

    fn give(&mut self, piece: Piece, now: u64) {
        if let Some(block) = self.blocks.get_mut(piece.block).and_then(Option::as_mut) {
            block.live = block.live.saturating_sub(1);
        }
        self.resting.push_back((now + REST, piece));
    }

    /// Leaves `block` to empty until [`DRAIN_MS`] from `now`: the sweep let
    /// all of its textures go. False if it is empty already or gone.
    fn drain(&mut self, block: usize, now: u64) -> bool {
        match self.blocks.get_mut(block).and_then(Option::as_mut) {
            Some(b) if !b.free.whole(BLOCK) => {
                b.draining = now + DRAIN_MS;
                true
            }
            _ => false,
        }
    }

    /// Bytes of the blocks left to empty at `now`: memory on its way back
    /// to the system.
    fn giving_back(&self, now: u64) -> u64 {
        self.blocks.iter().flatten().filter(|b| b.draining > now).count() as u64 * BLOCK
    }

    /// The pieces taken of each block, by its number (0 for a place of none).
    fn lives(&self) -> Vec<u32> {
        self.blocks.iter().map(|b| b.as_ref().map_or(0, |b| b.live)).collect()
    }

    fn empty(&self) -> usize {
        self.blocks.iter().flatten().filter(|b| b.free.whole(BLOCK)).count()
    }

    /// Makes the pieces that rested long enough free, and lets go of empty
    /// blocks past [`SPARE`].
    fn rested(&mut self, now: u64) {
        self.reclaim(now, SPARES.load(Ordering::Relaxed), false);
    }

    /// [`Pool::rested`] with at most `limit` empty blocks kept; `sweep`: the
    /// empty blocks are looked at even if no piece came back.
    fn reclaim(&mut self, now: u64, limit: usize, sweep: bool) {
        let mut any = false;
        while let Some(&(ready, piece)) = self.resting.front()
            && ready <= now
        {
            self.resting.pop_front();
            if let Some(block) = self.blocks[piece.block].as_mut() {
                block.free.give(piece.start, piece.len);
                if block.free.whole(BLOCK) {
                    // Empty: any class may take it, and it is not left to
                    // empty any more.
                    block.class = None;
                    block.draining = 0;
                    any = true;
                }
            }
        }
        // (Without a piece given back too, once there is less room.)
        let most = limit.min(self.most);
        let previous = std::mem::replace(&mut self.spare, most);
        if sweep || any || most < previous {
            let mut spare = 0;
            for block in &mut self.blocks {
                if block.as_ref().is_some_and(|b| b.free.whole(BLOCK)) {
                    spare += 1;
                    if spare > most {
                        *block = None;
                    }
                }
            }
        }
    }

    fn blocks(&self) -> usize {
        self.blocks.iter().flatten().count()
    }
}

/// A mapped upload buffer's memory.
#[derive(Clone)]
struct Upload(ID3D12Resource, *mut u8);
// SAFETY: the mapping is written by the owners of its pieces, each its own.
unsafe impl Send for Upload {}

static TEXTURES: Mutex<Pool<ID3D12Heap>> = Mutex::new(Pool::new(SPARE_KEPT));
static STAGING: Mutex<Pool<Upload>> = Mutex::new(Pool::new(STAGING_KEPT));

fn lock<T>(m: &'static Mutex<T>) -> std::sync::MutexGuard<'static, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Bytes of the pieces textures hold now.
static HELD: AtomicU64 = AtomicU64::new(0);

/// Bytes of the texture blocks no texture holds: the card's memory in use
/// by us that new textures take first. The system counts them as used: a
/// block goes back to it only once it is empty ([`giving_back`]).
pub(crate) fn texture_room() -> u64 {
    (lock(&TEXTURES).blocks() as u64 * BLOCK).saturating_sub(HELD.load(Ordering::Relaxed))
}

/// Bytes of the texture blocks the sweep let every texture of go, that are
/// on their way back to the system ([`DRAIN_MS`]).
pub(crate) fn giving_back() -> u64 {
    lock(&TEXTURES).giving_back(now())
}

/// The pieces taken of each texture block, by its number.
pub(crate) fn texture_lives() -> Vec<u32> {
    lock(&TEXTURES).lives()
}

/// Leaves texture blocks to empty: the sweep let every texture in them go.
pub(crate) fn drain(blocks: &[u32]) {
    let now = now();
    let mut pool = lock(&TEXTURES);
    for &block in blocks {
        if pool.drain(block as usize, now) {
            DRAINED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The number of the block `texture` was placed in, if it was.
pub(crate) fn block_of(texture: &ID3D12Resource) -> Option<u32> {
    let mut size = std::mem::size_of::<*mut c_void>() as u32;
    let mut out: *mut c_void = std::ptr::null_mut();
    // SAFETY: a pointer's room for the interface the texture was given
    // (`give_back_with`), which the call adds a reference to.
    unsafe { texture.GetPrivateData(&RECLAIM_KEY, &mut size, Some((&raw mut out).cast())) }.ok()?;
    if out.is_null() || size as usize != std::mem::size_of::<*mut c_void>() {
        return None;
    }
    // SAFETY: one of ours (only `give_back_with` sets this key), alive while
    // the texture is; the reference the call added is dropped with `object`.
    unsafe {
        let object = IUnknown::from_raw(out);
        let block = (*out.cast::<Reclaim>()).piece.block;
        drop(object);
        u32::try_from(block).ok()
    }
}

/// For the memory line: blocks of textures and of staging buffers, in MiB.
#[cfg(feature = "diagnostics")]
pub(crate) fn memory() -> [u64; 2] {
    [lock(&TEXTURES).blocks() as u64 * (BLOCK >> 20), lock(&STAGING).blocks() as u64 * (BLOCK >> 20)]
}

/// Part of a block of mapped upload memory, back in the pool when dropped.
pub(crate) struct StagingPiece(Piece);

impl Drop for StagingPiece {
    fn drop(&mut self) {
        lock(&STAGING).give(self.0, now());
    }
}

/// `bytes` of upload memory, all zeros: the buffer it lies in, where in
/// that buffer, and its mapping. `None`: not pooled.
pub(crate) fn staging(host: &crate::host::Host, bytes: u64) -> Option<(StagingPiece, ID3D12Resource, u64, *mut u8)> {
    if !on() {
        return None;
    }
    // Copies from a buffer begin at multiples of 512.
    let bytes = bytes.next_multiple_of(512);
    let (piece, memory, again) = lock(&STAGING).take(bytes, 512, now(), || {
        // (`VICTORY_RESIDENCY=1`: no new block for a texture picked by index
        // while commit is short; those a draw samples now still get one.)
        let speculative = crate::texture::PICKING.get().is_some() && !crate::texture::BAKING.get();
        if speculative && !crate::residency::allow_staging_growth(false) {
            return None;
        }
        let (buffer, data) = host.upload_buffer(BLOCK)?;
        crate::host::name(&buffer, || "staging of textures (a block)".into());
        Some(Upload(buffer, data))
    })?;
    let owned = StagingPiece(piece);
    // SAFETY: the piece lies in the block's mapping.
    let data = unsafe { memory.1.add(piece.start as usize) };
    if again {
        // What is not detiled into it (an unmapped texture) uploads zeros,
        // as from a buffer of its own.
        // SAFETY: the piece is ours alone.
        unsafe { std::ptr::write_bytes(data, 0, bytes as usize) };
    }
    Some((owned, memory.0, piece.start, data))
}

/// An object the host texture holds and releases when it goes: its piece
/// is free then.
#[repr(C)]
struct Reclaim {
    methods: &'static [*const (); 3],
    references: AtomicU32,
    piece: Piece,
}

struct Methods([*const (); 3]);
// SAFETY: function addresses only.
unsafe impl Sync for Methods {}

static RECLAIM: Methods = Methods([query as *const (), add_ref as *const (), release as *const ()]);

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
            HELD.fetch_sub(gone.piece.len, Ordering::Relaxed);
            lock(&TEXTURES).give(gone.piece, now());
        }
        left
    }
}

/// Whether textures of 64 KiB and less are placed at multiples of 4 KiB in
/// their block, where the host allows it (`VICTORY_POOL_SMALL=1`; otherwise
/// at multiples of 64 KiB, as every other texture).
///
/// The two device losses of the night of 2026-11-25 with the pool in the
/// report (runs steps-ab2 and start-3) faulted in the same kind of texture:
/// 256x256, block-compressed, 7 levels, placed at a 4 KiB multiple that is no
/// 64 KiB one (offsets 0x406000 and 0x3fe6000 of their blocks), never used
/// before, read by the card the moment its slot was made; the descriptor's
/// base of both lies 0xa000 into the piece, on a 64 KiB multiple, and its
/// first level of 32 KiB from there ends 0x2000 past the piece. Why the card
/// faulted there is not proven; textures at 64 KiB multiples have none of
/// that, for some memory (at most 60 KiB a small texture).
fn small_placement() -> bool {
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick("VICTORY_POOL_SMALL") {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_POOL_SMALL").is_some_and(|v| v != "0"))
}

/// Diagnostics: whether the card reads a pooled texture inside the piece it
/// was placed in. A block's address is taken as the lowest of its textures'
/// descriptor bases less their pieces' starts (a texture's first level may
/// lie past its piece's start, never before it); a texture whose first level
/// the card would read from past its piece's end is said loudly.
#[cfg(feature = "diagnostics")]
fn where_the_card_reads(heap: &ID3D12Heap, texture: &ID3D12Resource, piece: Piece) {
    static BLOCKS: Mutex<Vec<(usize, u64)>> = Mutex::new(Vec::new());
    static SAID: AtomicU64 = AtomicU64::new(0);
    let Some(base) = crate::book::address(texture) else { return };
    let Some(seen) = base.checked_sub(piece.start) else { return };
    let raw = heap.as_raw() as usize;
    let block = {
        let mut blocks = BLOCKS.lock().unwrap_or_else(|e| e.into_inner());
        match blocks.iter_mut().find(|b| b.0 == raw) {
            Some(b) => {
                b.1 = b.1.min(seen);
                b.1
            }
            None => {
                if blocks.len() >= 4096 {
                    blocks.remove(0);
                }
                blocks.push((raw, seen));
                seen
            }
        }
    };
    let into = base - block - piece.start;
    // SAFETY: a live texture.
    let desc = unsafe { texture.GetDesc() };
    // The first level's bytes, as a copy would have them.
    let mut level = 0u64;
    if let Some(host) = crate::host::made_host() {
        // SAFETY: plain query of a valid description.
        unsafe { host.device.GetCopyableFootprints(&desc, 0, 1, 0, None, None, None, Some(&raw mut level)) };
    }
    if into + level > piece.len {
        let n = SAID.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 50 || n.is_power_of_two() {
            // (Debug only: it assumes a texture's address is its block's
            // start and its piece's offset, which this driver does not
            // keep: 56 false alarms in run pool-1.)
            tracing::debug!(
                times = n,
                base = format_args!("{base:#x}"),
                piece_start = format_args!("{:#x}", piece.start),
                piece_len = format_args!("{:#x}", piece.len),
                into = format_args!("{into:#x}"),
                first_level_bytes = format_args!("{level:#x}"),
                width = desc.Width,
                height = desc.Height,
                levels = desc.MipLevels,
                format = desc.Format.0,
                "POOL: the card reads a pooled texture's first level past the end of the piece it was placed in"
            );
        }
    }
    // Every placement offset and where the first level lies, a few of them.
    static SHOWN: AtomicU64 = AtomicU64::new(0);
    if into > 0 && SHOWN.fetch_add(1, Ordering::Relaxed) < 40 {
        tracing::info!(
            into = format_args!("{into:#x}"),
            piece_start = format_args!("{:#x}", piece.start),
            piece_len = format_args!("{:#x}", piece.len),
            width = desc.Width,
            height = desc.Height,
            levels = desc.MipLevels,
            format = desc.Format.0,
            alignment = desc.Alignment,
            "pool: a texture's first level lies past its piece's start"
        );
    }
}

/// Textures made in the last second ([`fresh`]), and when.
static RECENT: Mutex<VecDeque<(usize, u64)>> = Mutex::new(VecDeque::new());

/// Whether `texture` was made in a block in the last second.
pub(crate) fn fresh(texture: &ID3D12Resource) -> bool {
    let raw = texture.as_raw() as usize;
    let since = now().saturating_sub(1000);
    RECENT.lock().unwrap_or_else(|e| e.into_inner()).iter().rev().take_while(|r| r.1 >= since).any(|r| r.0 == raw)
}

fn made_now(texture: &ID3D12Resource) {
    let now = now();
    let raw = texture.as_raw() as usize;
    {
        let mut recent = RECENT.lock().unwrap_or_else(|e| e.into_inner());
        while recent.front().is_some_and(|r| r.1 + 1000 < now) {
            recent.pop_front();
        }
        recent.push_back((raw, now));
    }
    let begun = crate::host::submissions().0;
    NEWEST_BEGUN.fetch_max(begun, Ordering::Relaxed);
    let mut unseen = UNSEEN.lock().unwrap_or_else(|e| e.into_inner());
    let finished = crate::host::submissions().1;
    // Those every submission begun before them has left behind are no
    // one's concern any more (and a raw pointer may come again).
    unseen.retain(|_, b| *b > finished);
    unseen.insert(raw, begun);
}

/// Pooled textures made while submissions begun before them may still run:
/// the submissions begun when each was made.
static UNSEEN: std::sync::LazyLock<Mutex<std::collections::HashMap<usize, u64>>> = std::sync::LazyLock::new(Default::default);
/// The newest of those numbers (nothing to look up once it has finished).
static NEWEST_BEGUN: AtomicU64 = AtomicU64::new(0);

/// Whether a texture was made in a block while a submission begun before
/// it may still run ([`begun_before`]).
pub(crate) fn any_unseen() -> bool {
    NEWEST_BEGUN.load(Ordering::Relaxed) > crate::host::submissions().1
}

/// The submissions begun when `texture` was made in a block, if one of
/// those may still be on the card: lists already sent then must not find it
/// in a slot they read by index ([`crate::descriptor::host::show_new_later`]).
/// Every device loss with the pool in the report (runs n08, n11, n15,
/// steps-ab2, start-3 and the user's play-1125-2107) was a page fault at the
/// first byte of a texture made that very millisecond, its title slot
/// changed "at once" while lists sent before it read that slot by index.
pub(crate) fn begun_before(texture: &ID3D12Resource) -> Option<u64> {
    if !any_unseen() {
        return None;
    }
    let finished = crate::host::submissions().1;
    let raw = texture.as_raw() as usize;
    let unseen = UNSEEN.lock().unwrap_or_else(|e| e.into_inner());
    unseen.get(&raw).copied().filter(|&begun| begun > finished)
}

/// The key of the [`Reclaim`] a pooled texture holds.
const RECLAIM_KEY: GUID = GUID::from_u128(0x6f0e2f0c_5a59_4b1e_9a3f_7f4d2c9be713);

/// Makes `texture` give `piece` back when the last reference to it goes.
fn give_back_with(texture: &ID3D12Resource, piece: Piece) {
    let made = Box::into_raw(Box::new(Reclaim { methods: &RECLAIM.0, references: AtomicU32::new(1), piece }));
    // SAFETY: a COM object of ours with one reference, which this takes and
    // drops at the end; the texture takes its own.
    unsafe {
        let object = IUnknown::from_raw(made.cast());
        if texture.SetPrivateDataInterface(&RECLAIM_KEY, &object).is_err() {
            // Never given back: the piece stays taken, nothing is reused
            // under a texture that may be alive.
            std::mem::forget(object);
        }
    }
}

/// A texture of `desc` (no target, depth or unordered access) in a block;
/// `None`: not pooled, or the host refused.
///
/// # Safety
/// `desc` is a valid description.
pub(crate) unsafe fn texture(
    device: &ID3D12Device,
    desc: *const D3D12_RESOURCE_DESC,
    state: D3D12_RESOURCE_STATES,
) -> Option<ID3D12Resource> {
    if !on() {
        return None;
    }
    // Small textures lie at multiples of 4 KiB where the host allows it
    // (the top mip is 64 KiB at most) and [`small_placement`] says so: 64 KiB
    // each otherwise.
    // SAFETY: the caller's description.
    let mut d = unsafe { *desc };
    d.Alignment = if small_placement() { u64::from(D3D12_SMALL_RESOURCE_PLACEMENT_ALIGNMENT) } else { 0 };
    // SAFETY: plain queries.
    let mut info = unsafe { device.GetResourceAllocationInfo(0, &[d]) };
    if info.SizeInBytes == u64::MAX || info.Alignment != d.Alignment {
        d.Alignment = 0;
        info = unsafe { device.GetResourceAllocationInfo(0, &[d]) };
    }
    if info.SizeInBytes == u64::MAX {
        return None;
    }
    let class = classes().then(|| class_of(info.SizeInBytes));
    #[cfg_attr(not(feature = "diagnostics"), allow(unused_variables))]
    let (piece, heap, again) = lock(&TEXTURES).take_ripe(info.SizeInBytes, info.Alignment.max(1), now(), Ripe::now(), class, || {
        let desc = D3D12_HEAP_DESC {
            SizeInBytes: BLOCK,
            Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
            Alignment: 0,
            Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES | crate::host::not_resident(),
        };
        let mut heap: Option<ID3D12Heap> = None;
        // SAFETY: plain creation.
        if let Err(e) = unsafe { device.CreateHeap(&desc, &mut heap) } {
            crate::residency::allocation_error(&e);
            tracing::warn!(error = %e, "no block of texture memory");
        }
        if let Some(h) = &heap {
            if let Err(e) = crate::host::resident(device, h) {
                tracing::warn!(error = %e, "a block of texture memory is not resident");
                return None;
            }
            // Its textures' slots are shown once the submissions begun before
            // them have run (`show_new_later`), and blocks are made ahead
            // (`Ripe`): nothing waits for the block's mapping then.
            let t = crate::host::mapping::made(device, h, crate::host::mapping::What::TextureBlock);
            if !crate::descriptor::host::show_new_later() {
                crate::host::mapping::wait_for(device, t, crate::host::mapping::What::TextureBlock);
            }
        }
        heap
    })?;
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: the piece is no one else's; the heap lives while a piece of
    // it is taken.
    let result = unsafe { device.CreatePlacedResource(&heap, piece.start, &d, state, None, &mut made) };
    match (result, made) {
        (Ok(()), Some(made)) => {
            HELD.fetch_add(piece.len, Ordering::Relaxed);
            give_back_with(&made, piece);
            made_now(&made);
            #[cfg(feature = "diagnostics")]
            {
                crate::book::made(&made, || "a texture in a block".into());
                where_the_card_reads(&heap, &made, piece);
                crate::book::block(&heap, &made, piece.start, BLOCK);
                crate::book::note(made.as_raw() as usize, || {
                    let block = lock(&TEXTURES).blocks.get(piece.block).and_then(Option::as_ref).map(|b| (b.made, b.since));
                    let (submission, since) = block.unwrap_or_default();
                    format!(
                        "; piece {:#x}+{:#x} of block {} {:#x} made {:.1} s before it at submission {submission}, used before: {again}, made at submissions {:?}",
                        piece.start,
                        piece.len,
                        piece.block,
                        heap.as_raw() as usize,
                        now().saturating_sub(since) as f64 / 1e3,
                        crate::host::submissions()
                    )
                });
            }
            Some(made)
        }
        _ => {
            lock(&TEXTURES).give(piece, now());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_block_is_not_retried_for_each_small_upload() {
        let mut pool: Pool<u32> = Pool::new(2);
        let calls = std::cell::Cell::new(0);
        let refuse = || {
            calls.set(calls.get() + 1);
            None
        };
        for size in [704008, 179720, 179720, 24080, 48648, 524288, 89616, 351760] {
            assert!(pool.take(size, 512, 12, refuse).is_none());
        }
        assert_eq!(calls.get(), 1);
        assert!(pool.take(24080, 512, 1012, || Some(1)).is_some());
    }

    #[test]
    fn budget_tick_reclaims_idle_blocks_without_reusing_live_or_resting_pieces() {
        let mut pool = Pool::new(STAGING_KEPT);
        let (held, _, _) = pool.take(BLOCK / 2, 1, 0, || Some(1)).unwrap();
        let (resting, _, _) = pool.take(BLOCK / 2, 1, 0, || Some(2)).unwrap();
        pool.give(resting, 0);
        for i in 0..12 {
            pool.add(i, 0);
        }
        pool.reclaim(0, SPARE_KEPT, true);
        assert_eq!(pool.blocks(), STAGING_KEPT + 1);
        pool.reclaim(REST - 1, SPARE, true);
        assert_eq!(pool.blocks(), SPARE + 1);
        assert_eq!(pool.resting.len(), 1);
        pool.reclaim(REST, 0, true);
        assert_eq!(pool.blocks(), 1);
        assert!(pool.resting.is_empty());
        pool.give(held, REST);
        pool.reclaim(2 * REST, 0, true);
        assert_eq!(pool.blocks(), 0);
    }

    #[test]
    fn ranges_are_cut_aligned_and_join_again() {
        let mut r = Ranges::new(1024);
        assert_eq!(r.take(100, 64), Some(0));
        assert_eq!(r.take(100, 64), Some(128));
        assert_eq!(r.take(28, 4), Some(100));
        assert_eq!(r.take(2000, 4), None);
        r.give(128, 100);
        r.give(0, 100);
        assert!(!r.whole(1024));
        r.give(100, 28);
        assert!(r.whole(1024));
    }

    #[test]
    fn empty_blocks_stay_while_the_card_has_room() {
        assert_eq!(spares(Some([100, 84, 0, 0]), true), SPARE_KEPT);
        assert_eq!(spares(Some([100, 85, 0, 0]), true), SPARE);
        assert_eq!(spares(Some([100, 10, 0, 0]), false), SPARE);
        assert_eq!(spares(None, true), SPARE);
    }

    /// As the system counts: the empty blocks that lie under the line, one
    /// fewer for each block past it, never a jump at the line.
    #[test]
    fn empty_blocks_kept_are_those_under_the_line() {
        let budget = 100 * BLOCK;
        assert_eq!(kept_spares(Some([budget, 70 * BLOCK, 0, 0]), true, 3), (3 + 15).min(SPARE_KEPT));
        assert_eq!(kept_spares(Some([budget, 85 * BLOCK, 0, 0]), true, 12), 12);
        assert_eq!(kept_spares(Some([budget, 85 * BLOCK + 1, 0, 0]), true, 12), 11);
        assert_eq!(kept_spares(Some([budget, 90 * BLOCK, 0, 0]), true, 12), 7);
        assert_eq!(kept_spares(Some([budget, 99 * BLOCK, 0, 0]), true, 12), SPARE);
        assert_eq!(kept_spares(Some([budget, 10 * BLOCK, 0, 0]), true, 12), SPARE_KEPT);
        assert_eq!(kept_spares(Some([budget, 10 * BLOCK, 0, 0]), false, 12), SPARE);
        assert_eq!(kept_spares(None, true, 12), SPARE);
    }

    /// The held levels' way: a copy of 64 KiB, a larger texture, and so on;
    /// then each small copy is replaced by one with more levels. With size
    /// classes the small copies' blocks empty and go back; mixed in with
    /// the larger textures each left a hole no block could give back.
    #[test]
    fn small_copies_lie_in_blocks_of_their_own_that_empty_when_they_are_replaced() {
        let run = |classes: bool| {
            let mut pool: Pool<u32> = Pool::new(SPARE_KEPT);
            let made = std::cell::Cell::new(0);
            let make = || {
                made.set(made.get() + 1);
                Some(made.get())
            };
            let class = |len: u64| classes.then(|| class_of(len));
            let mut small = Vec::new();
            for _ in 0..2000 {
                small.push(pool.take_ripe(64 << 10, 65536, 0, Ripe::ALL, class(64 << 10), make).unwrap().0);
                pool.take_ripe(2 << 20, 65536, 0, Ripe::ALL, class(2 << 20), make).unwrap();
            }
            let before = pool.blocks();
            for piece in small {
                pool.give(piece, 10);
                pool.take_ripe(256 << 10, 65536, 10, Ripe::ALL, class(256 << 10), make).unwrap();
            }
            let grown = pool.blocks();
            pool.reclaim(10 + REST, 0, false);
            (before, grown, pool.blocks(), free(&pool))
        };
        // 2,000 copies of 64 KiB are two blocks; the copies of 256 KiB 500
        // MiB, eight more.
        let (before, grown, after, left) = run(true);
        assert_eq!(grown, before + 8);
        assert_eq!(after, grown - 2, "the two blocks of small copies went back");
        assert!(left < 2 * BLOCK, "no holes left behind: {left}");
        let (_, grown, after, left) = run(false);
        assert_eq!(after, grown, "without classes no block empties");
        assert!(left >= 2000 * (64 << 10), "every replaced copy left a hole: {left}");
    }

    /// Bytes free in a pool's blocks, and its empty blocks.
    fn free<B: Clone>(pool: &Pool<B>) -> u64 {
        pool.blocks.iter().flatten().map(|b| b.free.0.values().sum::<u64>()).sum()
    }

    fn empties<B: Clone>(pool: &Pool<B>) -> usize {
        pool.blocks.iter().flatten().filter(|b| b.free.whole(BLOCK)).count()
    }

    /// A small texture goes where small ones are, not into an empty block
    /// while one of its class has room; and no class shares a block.
    #[test]
    fn classes_keep_to_their_blocks_and_leave_empty_ones_empty() {
        let mut pool: Pool<u32> = Pool::new(SPARE_KEPT);
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            Some(made.get())
        };
        let small = |pool: &mut Pool<u32>, now| pool.take_ripe(64 << 10, 65536, now, Ripe::ALL, Some(0), make).unwrap().0;
        let large = |pool: &mut Pool<u32>, now| pool.take_ripe(4 << 20, 65536, now, Ripe::ALL, Some(2), make).unwrap().0;
        let x = large(&mut pool, 0);
        let a = small(&mut pool, 0);
        assert_eq!((x.block, a.block), (0, 1), "a block of its own");
        // The large one's block is empty now, below the small one's.
        pool.give(x, 0);
        pool.reclaim(REST, SPARE_KEPT, false);
        assert_eq!(empties(&pool), 1);
        let d = small(&mut pool, REST);
        assert_eq!(d.block, a.block, "the small ones stay in theirs");
        assert_eq!(empties(&pool), 1);
        // And the empty one goes to whichever class needs a block.
        let e = large(&mut pool, REST);
        assert_eq!(e.block, x.block);
        assert_eq!(pool.blocks(), 2);
    }

    /// A block the sweep let every texture of go gets no new piece while
    /// another block can be made, counts as on its way back until it is
    /// empty, and goes back then; one that did not empty in time takes
    /// pieces again.
    #[test]
    fn a_block_left_to_empty_empties_and_goes_back() {
        let mut pool: Pool<u32> = Pool::new(SPARE_KEPT);
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            Some(made.get())
        };
        let take = |pool: &mut Pool<u32>, now| pool.take_ripe(64 << 10, 65536, now, Ripe::ALL, Some(0), make).unwrap().0;
        let pieces: Vec<Piece> = (0..10).map(|_| take(&mut pool, 0)).collect();
        assert_eq!(pool.lives(), vec![10]);
        assert!(pool.drain(0, 0));
        assert_eq!(pool.giving_back(0), BLOCK);
        // Not cut from while it empties: a new block.
        let other = take(&mut pool, 1);
        assert_eq!(other.block, 1);
        for p in pieces {
            pool.give(p, 2);
        }
        assert_eq!(pool.lives(), vec![0, 1]);
        assert_eq!(pool.giving_back(2 + REST - 1), BLOCK);
        pool.reclaim(2 + REST, 0, false);
        assert_eq!(pool.blocks(), 1, "it went back");
        assert_eq!(pool.giving_back(2 + REST), 0);
        // One that does not empty: used again once its time is over, and at
        // once when no block can be made.
        assert!(pool.drain(1, 3 * REST));
        let p = pool.take_ripe(64 << 10, 65536, 3 * REST + DRAIN_MS, Ripe::ALL, Some(0), make).unwrap().0;
        assert_eq!(p.block, 1);
        assert!(pool.drain(1, 4 * REST + DRAIN_MS));
        let p = pool.take_ripe(64 << 10, 65536, 4 * REST + DRAIN_MS + 1, Ripe::ALL, Some(0), || None).unwrap().0;
        assert_eq!(p.block, 1);
        assert_eq!(pool.giving_back(4 * REST + DRAIN_MS + 1), 0, "taken again, it does not go back");
    }

    #[test]
    fn pieces_never_overlap_and_rest_before_they_are_used_again() {
        let mut pool: Pool<u32> = Pool::new(SPARE);
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            Some(made.get())
        };
        let mut taken: Vec<Piece> = Vec::new();
        for i in 0..500u64 {
            let (p, _, again) = pool.take(65536 * (1 + i % 7), 65536, 0, make).unwrap();
            assert!(!again);
            assert!(taken.iter().all(|q| q.block != p.block || p.start + p.len <= q.start || q.start + q.len <= p.start));
            taken.push(p);
        }
        let blocks = pool.blocks();
        assert!(blocks >= 2);
        // Given back at 10: not used again before 10 + REST.
        let first = taken[0];
        pool.give(first, 10);
        let (p, _, _) = pool.take(first.len, 65536, 10 + REST - 1, make).unwrap();
        assert_ne!((p.block, p.start), (first.block, first.start));
        let (q, _, again) = pool.take(first.len, 65536, 10 + REST, make).unwrap();
        assert_eq!((q.block, q.start, again), (first.block, first.start, true));
        // Everything back: the empty blocks past the spare ones go.
        for p in taken.drain(1..).chain([p, q]) {
            pool.give(p, 20);
        }
        pool.rested(20 + REST);
        assert_eq!(pool.blocks(), blocks.min(SPARE));
        // Too big for a block, and a block that cannot be made.
        assert!(pool.take(BLOCK, 65536, 30, make).is_none());
        let mut none: Pool<u32> = Pool::new(SPARE);
        assert!(none.take(65536, 65536, 0, || None).is_none());
    }

    #[test]
    fn pieces_come_from_blocks_older_than_the_lists_that_run() {
        let mut pool: Pool<u64> = Pool::new(SPARE_KEPT);
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            Some(made.get())
        };
        let at = |started, finished| Ripe { started, finished, ahead: 2 };
        let take = |pool: &mut Pool<u64>, ripe| pool.take_ripe(BLOCK / 2, 65536, 0, ripe, None, make).unwrap().1;
        // Nothing runs: at once, and two are made ahead, one a time.
        assert_eq!(take(&mut pool, at(0, 0)), 1);
        assert_eq!(made.get(), 2);
        assert_eq!(take(&mut pool, at(0, 0)), 1);
        assert_eq!(made.get(), 3);
        // 4 is made while submission 5 runs.
        assert_eq!(take(&mut pool, at(5, 4)), 2);
        assert_eq!(made.get(), 4);
        assert_eq!(take(&mut pool, at(5, 4)), 2);
        assert_eq!(take(&mut pool, at(6, 4)), 3);
        assert_eq!(take(&mut pool, at(6, 4)), 3);
        // None is ripe: the oldest; it is ripe once 5 has finished.
        assert_eq!(take(&mut pool, at(7, 4)), 4);
        assert_eq!(take(&mut pool, at(7, 5)), 4);
        // A whole block's worth too.
        assert_eq!(pool.take_ripe(BLOCK, 65536, 0, at(8, 7), None, make).unwrap().1, 5);
        // Off: the first with room, whatever its age, none ahead.
        let n = made.get();
        assert_eq!(pool.take(BLOCK / 2, 65536, 0, make).unwrap().1, 6);
        assert_eq!(made.get(), n);
        assert!(pool.take(BLOCK, 65536, 0, make).is_none());
    }

    /// A texture placed in a heap holds no reference to it that its count
    /// shows (this host, 2026-11-25: 1 before placing, 1 with the texture):
    /// the pool must not count on one, and does not (a block goes only with
    /// every piece back and rested, [`Pool::rested`]). Said here, and the
    /// count is the same again once the texture went.
    #[test]
    fn a_placed_texture_holds_its_heap() {
        let Some(host) = crate::host::host() else { return };
        let desc = D3D12_HEAP_DESC {
            SizeInBytes: 1 << 20,
            Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
            Alignment: 0,
            Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES,
        };
        let mut heap: Option<ID3D12Heap> = None;
        // SAFETY: plain creation.
        unsafe { host.device.CreateHeap(&desc, &mut heap) }.unwrap();
        let heap = heap.unwrap();
        let before = crate::host::references(&heap);
        let texture_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 256,
            Height: 256,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        let mut made: Option<ID3D12Resource> = None;
        // SAFETY: a live heap, a valid description.
        unsafe { host.device.CreatePlacedResource(&heap, 0, &texture_desc, D3D12_RESOURCE_STATE_COMMON, None, &mut made) }.unwrap();
        let made = made.unwrap();
        let held = crate::host::references(&heap);
        println!("heap references: {before} before placing, {held} with a texture in it");
        assert!(held >= before);
        drop(made);
        assert_eq!(crate::host::references(&heap), before);
    }

    /// A pooled texture's piece comes back when its last reference goes, not
    /// before; its staging piece begins as zeros.
    #[test]
    fn a_texture_gives_its_piece_back_when_it_goes() {
        let Some(host) = crate::host::host() else { return };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 256,
            Height: 256,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        // SAFETY: a valid description.
        let Some(t) = (unsafe { texture(&host.device, &desc, D3D12_RESOURCE_STATE_COMMON) }) else { return };
        assert_eq!(crate::host::references(&t), 1, "what the texture holds is no reference to it");
        let resting = || lock(&TEXTURES).resting.len();
        let before = resting();
        let other = t.clone();
        drop(t);
        assert_eq!(resting(), before);
        drop(other);
        assert_eq!(resting(), before + 1);
        // SAFETY: the piece's mapping, 4096 bytes of it ours.
        unsafe {
            let (piece, _, at, data) = staging(host, 4096).unwrap();
            assert!(at.is_multiple_of(512));
            std::ptr::write_bytes(data, 0xAB, 4096);
            drop(piece);
            lock(&STAGING).rested(u64::MAX);
            let (_piece, _, _, data) = staging(host, 4096).unwrap();
            assert!((0..4096).all(|i| *data.add(i) == 0));
            // SAFETY: plain query.
            assert!(host.device.GetDeviceRemovedReason().is_ok());
        }
    }

    /// On the card: small textures lie in blocks of their own, which a
    /// texture says ([`block_of`]), and a block all of whose textures went
    /// goes back to the system, which then counts its 64 MiB less in use.
    /// (By hand, alone: `cargo test -p victory-gpu --lib -- --ignored
    /// an_emptied_block`; other tests' textures would share the pool. With
    /// `VICTORY_POOL_CLASSES=0` it fails: the small textures share blocks
    /// with the large ones, and none empties.)
    #[test]
    #[ignore = "on the card, by hand"]
    fn an_emptied_block_of_small_textures_goes_back_to_the_system() {
        let Some(host) = crate::host::host() else { return };
        if !on() {
            return;
        }
        let desc = |side: u64| D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: side,
            Height: side as u32,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        let make = |side: u64| {
            // SAFETY: a valid description.
            unsafe { texture(&host.device, &desc(side), D3D12_RESOURCE_STATE_COMMON) }.expect("a texture in a block")
        };
        // 64x64: 16 KiB, a piece of 64 KiB; 1024x1024: 4 MiB. A large one
        // between each two small ones, as the held levels' copies come.
        let mut small = Vec::new();
        let mut large = Vec::new();
        for i in 0..2048 {
            small.push(make(64));
            if i % 64 == 0 {
                large.push(make(1024));
            }
        }
        let small_blocks: std::collections::BTreeSet<u32> = small.iter().map(|t| block_of(t).expect("placed")).collect();
        let large_blocks: std::collections::BTreeSet<u32> = large.iter().map(|t| block_of(t).expect("placed")).collect();
        assert!(small_blocks.is_disjoint(&large_blocks), "{small_blocks:?} {large_blocks:?}");
        assert!(small_blocks.len() >= 2, "2,048 pieces of 64 KiB are two blocks");
        let used = || host.video_memory().expect("the system's count")[1];
        let lives_before = lock(&TEXTURES).lives();
        drop(small);
        let lives = lock(&TEXTURES).lives();
        // The blocks only these textures were in.
        let emptied = small_blocks.iter().filter(|&&b| lives[b as usize] == 0).count() as u64;
        assert!(emptied >= 1, "{lives_before:?} {lives:?}");
        std::thread::sleep(std::time::Duration::from_millis(REST + 200));
        let before = used();
        lock(&TEXTURES).reclaim(now(), 0, false);
        let after = used();
        println!("in use as the system counts: {} MB, then {} MB once {emptied} blocks emptied", before >> 20, after >> 20);
        assert!(before >= after + emptied * BLOCK, "{before} {after}");
        assert!(large.iter().all(|t| block_of(t).is_some_and(|b| large_blocks.contains(&b))));
        // A texture of its own was placed in no block.
        let mut own: Option<ID3D12Resource> = None;
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
        // SAFETY: a valid description.
        unsafe {
            host.device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc(64), D3D12_RESOURCE_STATE_COMMON, None, &mut own)
        }
        .unwrap();
        assert_eq!(block_of(&own.unwrap()), None);
        // SAFETY: plain query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }
}
