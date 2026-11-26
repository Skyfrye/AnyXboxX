//! Write generations of title memory, by the page that was written
//! (`VICTORY_PAGE_GENERATIONS`; on by default. `=0`: the texture pass is told
//! of writes at the address they went through and looks again at what it
//! uploaded by sampled fingerprints, five times, as before; that way is kept
//! for one release).
//!
//! The title shows one 64 KiB physical page at several addresses, and writes
//! it through one while a shader picks the texture there by index at
//! another. The texture pass was told of a write at the address it went
//! through alone, and of the title's own CPU writes not at all; what it
//! uploaded was looked at again five times, by 48 KiB of it, then never. In
//! run nb1 103 textures were found changed by those looks with nothing to
//! tell of it, 102 of them on pages the title shows elsewhere too, 13 at the
//! last look (576 frames, two minutes at 4 fps, after the upload).
//!
//! Here a change is told once, by whoever makes it, at what it changed
//! ([`Page`]): a physical page whatever view it went through, or for memory
//! that is no view the address itself. Each change takes the next tick
//! ([`now`]). A texture slot keeps the pages its upload read, and the tick
//! from before that read ([`Watchers`]); it is due again as soon as one of
//! them has a later one, however long after its upload.
//!
//! Who tells, and when:
//! - the title's CPU writes: the guard's fault at whichever view of a
//!   physical page (`crate::watch`, once it opened the block, before the
//!   store is made: the slot guards its memory again after its tick, so a
//!   store not made by its read faults again);
//! - the title's CPU writes to memory of its own (no view): the kernel's
//!   write watch, when it is asked (`crate::watch`'s askings, and the texture
//!   pass's own for the slots there, [`Watchers::asks`]);
//! - writes of ours, once their bytes are in: DirectStorage into physical
//!   pages ([`physical_written`]) or to addresses, kernel reads into title
//!   memory, what shaders wrote landed in it, copies back ([`written`]);
//! - the runtime mapping, unmapping, committing or freeing memory at an
//!   address ([`readdressed`]).
//! What is neither guarded nor write-watched (memory of the host's heap) is
//! looked at whole, by its digest, twice a second ([`Kept::Looked`]).
//!
//! Only pages some slot watches are kept; a change of any other costs a look
//! at a counting filter, no lock.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};

/// The unit: the console's page, the guards' block.
const PAGE: usize = 64 << 10;

/// What a 64 KiB page of title memory is, whatever address shows it: a
/// physical page (a section and the page's index in it), or the title's own
/// memory at an address, which only that address shows. The latter is also
/// the key of the address itself: what it shows changed (mapped, unmapped,
/// committed, freed).
///
/// Packed in 64 bits: a section handle past 39 bits or a page index past 24
/// shares its key with another, which makes slots due that need not be,
/// never the other way.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Page(u64);

impl Page {
    const SHOWN: u64 = 1 << 63;

    /// Page `index` of the physical pages of section `section`.
    pub(crate) fn shown(section: usize, index: u64) -> Page {
        Page(Self::SHOWN | (section as u64 & ((1 << 39) - 1)) << 24 | index & ((1 << 24) - 1))
    }

    /// The page at `address` (any address in it).
    pub(crate) fn at(address: usize) -> Page {
        Page((address / PAGE) as u64 & !Self::SHOWN)
    }

    fn bucket(self) -> usize {
        (self.0.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> (64 - FILTER_BITS)) as usize
    }
}

impl std::fmt::Debug for Page {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0 & Self::SHOWN != 0 {
            write!(f, "Shown({:#x}, {})", (self.0 >> 24) & ((1 << 39) - 1), self.0 & ((1 << 24) - 1))
        } else {
            write!(f, "At({:#x})", (self.0 as usize).wrapping_mul(PAGE))
        }
    }
}

/// See the file.
pub(crate) fn on() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_ON.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PAGE_GENERATIONS").is_none_or(|v| v != "0"))
}

#[cfg(test)]
thread_local! {
    /// For a test of the way before (`Some(false)`) on its own thread.
    pub(crate) static TEST_ON: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Diagnostic (`VICTORY_PAGE_CHECK=1`, off by default): the fingerprint
/// looks of the way before run beside page generations, and a change they
/// find that no generation told of is warned of. A run with it on says
/// whether any way of writing texture memory is still untold.
pub(crate) fn check() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PAGE_CHECK").is_some_and(|v| v == "1"))
}

/// The last tick given to a change; a change told after this returns gets a
/// higher one.
pub(crate) fn now() -> u64 {
    TICK.load(Ordering::SeqCst)
}

static TICK: AtomicU64 = AtomicU64::new(1);

/// Buckets of the counting filter ([`FILTER`]), as bits.
const FILTER_BITS: u32 = 16;

/// For each bucket, how many watches there are of pages in it: a change of
/// pages whose buckets are all 0 concerns no slot and takes no lock. Raised
/// under [`TABLE`]'s lock, after the page is in it.
static FILTER: [AtomicU32; 1 << FILTER_BITS] = [const { AtomicU32::new(0) }; 1 << FILTER_BITS];

/// Pages watched by any slot, in all.
static WATCHED: AtomicU64 = AtomicU64::new(0);

/// Changes kept for the watchers to read ([`Watchers::moved`]): older ones
/// go, and a watcher that had not read them compares all it watches.
const LOG_MOST: usize = 1 << 17;

struct Table {
    /// The pages watched: how many watches of each there are, and the tick
    /// of its last change since it was first watched (0: none).
    pages: HashMap<Page, (u32, u64)>,
    /// Changes of watched pages, in order, with their ticks.
    log: VecDeque<(Page, u64)>,
    /// The number of the first change in `log`, counted since the start.
    base: u64,
}

static TABLE: LazyLock<Mutex<Table>> =
    LazyLock::new(|| Mutex::new(Table { pages: HashMap::new(), log: VecDeque::new(), base: 0 }));

fn table() -> MutexGuard<'static, Table> {
    TABLE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Who told of a change, for the counts ([`STATS`]).
#[derive(Clone, Copy)]
pub(crate) enum Teller {
    /// A guard's fault: the title's CPU wrote through some view.
    Fault = 0,
    /// The kernel's write watch, asked about the title's own memory.
    Kernel = 1,
    /// A write of ours, its bytes in ([`written`]).
    Ours = 2,
    /// DirectStorage into physical pages ([`physical_written`]).
    Physical = 3,
    /// A mapping changed ([`readdressed`]).
    Mapping = 4,
    /// A guard was taken off ([`unguarded`]).
    Opened = 5,
}

/// Since the start: changes of watched pages told, by [`Teller`]; changes
/// told of pages nobody watches (no lock taken: the filter said so); slots
/// made due ([`Watchers::moved`]); times a watcher fell behind the kept
/// changes and compared all it watches; askings of the kernel for slots of
/// the title's own memory ([`Watchers::asks`]); looks by digest at slots of
/// memory nothing tracks, and those that found it changed; slots made due
/// by a physical page's change; slots whose guard was not all up when they
/// were read ([`Kept::Open`]).
pub(crate) static STATS: [AtomicU64; 14] = [const { AtomicU64::new(0) }; 14];
const UNWATCHED: usize = 6;
const DUE: usize = 7;
const LOST: usize = 8;
const ASKED: usize = 9;
const LOOKS: usize = 10;
const LOOKS_CHANGED: usize = 11;
/// Slots made due by a change of a physical page (the rest: by their
/// address, or memory of the title's own).
const DUE_SHOWN: usize = 12;
const OPEN: usize = 13;

fn count(what: usize, n: u64) {
    if n != 0 {
        STATS[what].fetch_add(n, Ordering::Relaxed);
    }
}

/// `pages` changed: those watched get a new tick.
fn changed(pages: &[Page], teller: Teller) {
    if pages.is_empty() || WATCHED.load(Ordering::SeqCst) == 0 {
        return;
    }
    // Most writes are of memory no slot watches (buffers): no lock for them.
    if !pages.iter().any(|p| FILTER[p.bucket()].load(Ordering::SeqCst) != 0) {
        count(UNWATCHED, 1);
        return;
    }
    let mut t = table();
    let tick = TICK.fetch_add(1, Ordering::SeqCst) + 1;
    let Table { pages: watched, log, base } = &mut *t;
    let mut told = 0;
    for &page in pages {
        if let Some(entry) = watched.get_mut(&page) {
            entry.1 = tick;
            log.push_back((page, tick));
            told += 1;
        }
    }
    if log.len() > LOG_MOST {
        let gone = LOG_MOST / 2;
        log.drain(..gone);
        *base += gone as u64;
    }
    count(teller as usize, told);
}

/// The pages `[at, at + len)` shows, by what they are (each 64 KiB block's
/// physical page if a view shows it there, else the block's own), into
/// `out`.
fn contents(at: usize, len: usize, out: &mut Vec<Page>) {
    crate::inplace::physical(at, at.saturating_add(len), |block, shown| {
        out.push(match shown {
            Some((section, index)) => Page::shown(section, index),
            None => Page::at(block),
        });
    });
}

/// What a slot that reads title memory `[at, at + len)` watches: every
/// block's address, and the physical page a view shows there; in order,
/// each once. (What views show there may change before the memory is read:
/// see [`watch`] for the order that loses no change.)
pub(crate) fn pages_of(at: usize, len: usize) -> Vec<Page> {
    let mut out = Vec::new();
    crate::inplace::physical(at, at.saturating_add(len), |block, shown| {
        out.push(Page::at(block));
        if let Some((section, index)) = shown {
            out.push(Page::shown(section, index));
        }
    });
    out.sort_unstable();
    out.dedup();
    out
}

/// Bytes `[at, at + len)` of title memory changed and are in: written by
/// the title or by us, through that address (whichever view it is of its
/// pages). Called once they are there.
pub(crate) fn written(at: usize, len: usize) {
    if !on() || len == 0 || WATCHED.load(Ordering::SeqCst) == 0 {
        return;
    }
    let mut pages = Vec::new();
    contents(at, len, &mut pages);
    changed(&pages, Teller::Ours);
}

/// Tells [`written`] of `[at, at + len)` when dropped: for a writer with
/// several ways out, once its bytes are in.
pub(crate) struct Writing(pub(crate) usize, pub(crate) usize);

impl Drop for Writing {
    fn drop(&mut self) {
        written(self.0, self.1);
    }
}

/// A guard's fault opened the block at `at` for a write of the title's.
pub(crate) fn faulted(at: usize) {
    if !on() || WATCHED.load(Ordering::SeqCst) == 0 {
        return;
    }
    let mut pages = Vec::new();
    contents(at / PAGE * PAGE, PAGE, &mut pages);
    changed(&pages, Teller::Fault);
}

/// The kernel's write watch found pages of the title's own memory written:
/// their numbers (64 KiB each).
pub(crate) fn written_own(pages: impl IntoIterator<Item = usize>) {
    if !on() || WATCHED.load(Ordering::SeqCst) == 0 {
        return;
    }
    let pages: Vec<Page> = pages.into_iter().map(|page| Page::at(page * PAGE)).collect();
    changed(&pages, Teller::Kernel);
}

/// The runtime put bytes `[offset, offset + len)` into the physical pages of
/// section `section` (its handle, as `crate::title_memory_section` was told)
/// through a view of its own (DirectStorage): every slot that reads them, at
/// whatever address the title shows them, now or before, is due. Call when
/// the bytes are there.
pub fn title_physical_written(section: usize, offset: u64, len: usize) {
    physical_written(section, offset, len);
}

/// Bytes `[offset, offset + len)` of section `section`'s physical pages
/// changed and are in (DirectStorage writes them through a view of its own,
/// whatever views the title has of them, if any).
pub(crate) fn physical_written(section: usize, offset: u64, len: usize) {
    if !on() || len == 0 || WATCHED.load(Ordering::SeqCst) == 0 {
        return;
    }
    let (first, last) = (offset / PAGE as u64, (offset + len as u64).div_ceil(PAGE as u64));
    let pages: Vec<Page> = (first..last).map(|index| Page::shown(section, index)).collect();
    changed(&pages, Teller::Physical);
}

/// What the title memory `[at, at + len)` shows changed: the runtime mapped,
/// unmapped, committed or freed it. Told at the addresses only: other views
/// of the pages show what they did.
pub(crate) fn readdressed(at: usize, len: usize) {
    if !on() || len == 0 || WATCHED.load(Ordering::SeqCst) == 0 {
        return;
    }
    let (first, last) = (at / PAGE, at.saturating_add(len).div_ceil(PAGE));
    let pages: Vec<Page> = (first..last).map(|block| Page::at(block * PAGE)).collect();
    changed(&pages, Teller::Mapping);
}

/// The guards were taken off the blocks at these addresses (`crate::watch`
/// gave write access back): a slot that read one of them relied on its guard
/// for the title's next write, and reads it again, guarding it anew. Told
/// at the addresses: what a write through them changes is told by its
/// fault, or by its writer.
pub(crate) fn unguarded(blocks: impl IntoIterator<Item = usize>) {
    if !on() || WATCHED.load(Ordering::SeqCst) == 0 {
        return;
    }
    let pages: Vec<Page> = blocks.into_iter().map(Page::at).collect();
    changed(&pages, Teller::Opened);
}

/// Pages `pages` are watched once more.
fn hold(pages: &[Page]) {
    let mut t = table();
    for &page in pages {
        let entry = t.pages.entry(page).or_insert((0, 0));
        if entry.0 == 0 {
            WATCHED.fetch_add(1, Ordering::SeqCst);
        }
        entry.0 += 1;
        FILTER[page.bucket()].fetch_add(1, Ordering::SeqCst);
    }
}

/// Pages `pages` are watched once less.
fn let_go(pages: &[Page]) {
    let mut t = table();
    for &page in pages {
        let Some(entry) = t.pages.get_mut(&page) else {
            continue;
        };
        entry.0 -= 1;
        FILTER[page.bucket()].fetch_sub(1, Ordering::SeqCst);
        if entry.0 == 0 {
            t.pages.remove(&page);
            WATCHED.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// Watches slot `slot` of a heap on title memory `[at, at + len)`, which it
/// is about to read for an upload; returns how that memory tells of writes.
/// `with` hands the heap's [`Watchers`] to what it is given, under the
/// heap's lock for that call alone (not for [`arm`]'s guarding). In the
/// order that loses no change: the pages are watched first
/// ([`Watchers::begin`]), then the tick taken and the memory guarded or
/// asked about ([`arm`]), then what views show there is looked up again: a
/// mapping changed meanwhile is a change of the address's own page, told
/// after the tick or seen here.
pub(crate) fn watch(slot: u32, at: usize, len: usize, mut with: impl FnMut(&mut dyn FnMut(&mut Watchers))) -> Kept {
    let pages = pages_of(at, len);
    with(&mut |w| w.begin(slot, &pages));
    let (since, kept) = arm(at, len);
    let mut again = Some(pages_of(at, len));
    with(&mut |w| w.watch(slot, again.take().unwrap_or_default(), since, kept));
    kept
}

/// The number the next change kept will have.
fn head() -> u64 {
    let t = table();
    t.base + t.log.len() as u64
}

/// How a slot's memory tells of the title's CPU writes to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kept {
    /// Guarded (view memory): each write faults and is told.
    Told,
    /// View memory not all guarded when it was read (a block the title had
    /// just written rests open, or the kernel was writing it): due again at
    /// the next pass, which guards it then.
    Open,
    /// The title's own memory under the kernel's write watch: told when the
    /// kernel is asked ([`Watchers::asks`]); address and bytes.
    Asked(usize, usize),
    /// Neither: address, bytes, its digest when the slot was watched, and
    /// when it is looked at next (`crate::host::now_ms`).
    Looked(usize, usize, u128, u64),
}

/// Milliseconds between looks at memory nothing tracks ([`Kept::Looked`]).
const LOOK_MS: u64 = 500;

impl Kept {
    /// Its place in [`KINDS`].
    fn kind(self) -> usize {
        match self {
            Kept::Told | Kept::Open => 0,
            Kept::Asked(..) => 1,
            Kept::Looked(..) => 2,
        }
    }
}

/// Slots watching now, of every heap, by how their memory tells of writes:
/// guarded (or to be read again), asked, looked at.
static KINDS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// A slot's memory is about to be read for an upload: what its slot is to
/// watch from, and how its memory tells of the title's writes. Guards the
/// memory, if it is view memory, or asks the kernel about it, if it is the
/// title's own under write watch (`crate::watch::writes`), so that from here
/// on its next write is told.
///
/// The order is what makes no write go untold: view memory takes the tick
/// first and is guarded after it (a fault before the guard went up opened
/// the block before this guards it again, and the store, if not yet made,
/// faults again); the title's own memory is asked first and takes the tick
/// after (a write after the asking is in the kernel's next answer). View
/// memory the guards left open for now is read again at the next pass
/// ([`Kept::Open`]); what they never guard is looked at by its digest.
/// The slot's pages must be watched already ([`Watchers::begin`]): a change
/// told of pages nobody watches is dropped, and the fault that told it
/// opened the block, so the title's next stores make none.
pub(crate) fn arm(at: usize, len: usize) -> (u64, Kept) {
    let end = at.saturating_add(len);
    let views = crate::inplace::shown_bytes(at, len).is_some_and(|n| n != 0);
    let before = now();
    let asked = crate::watch::writes(at, end, true).is_some();
    let after = now();
    let looked = || Kept::Looked(at, len, digest_of(at, len), crate::host::now_ms() + LOOK_MS);
    match (views, asked) {
        (true, true) => match crate::watch::armed(at, end) {
            crate::watch::Armed::Whole => (before, Kept::Told),
            crate::watch::Armed::Soon => (before, Kept::Open),
            crate::watch::Armed::Never => (before, looked()),
        },
        (false, true) => (after, Kept::Asked(at, len)),
        (true, false) => (before, looked()),
        (false, false) => (after, looked()),
    }
}

/// The digest of what is there of title memory `[at, at + len)` (zeros for
/// what is not), read through the kernel.
pub(crate) fn digest_of(at: usize, len: usize) -> u128 {
    thread_local! {
        static READ: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    READ.with(|read| {
        let mut read = read.borrow_mut();
        read.resize(len, 0);
        crate::host::read_there(at as *const u8, &mut read[..len]);
        let digest = crate::texture::digest(&read[..len]).0;
        if read.len() > 64 << 20 {
            *read = Vec::new();
        }
        digest
    })
}

/// What a slot watches.
struct Watch {
    pages: Box<[Page]>,
    /// The tick from before its upload read the memory.
    since: u64,
    kept: Kept,
}

/// The slots of one heap that watch pages ([`on`]): each with the pages its
/// last upload read and the tick from before that read.
#[derive(Default)]
pub(crate) struct Watchers {
    slots: HashMap<u32, Watch>,
    /// For each page watched, the slots watching it.
    by_page: HashMap<Page, Vec<u32>>,
    /// The number of the next kept change to read.
    cursor: u64,
    /// Whether `cursor` was set (at the first watch: no slot cares for what
    /// changed before that).
    started: bool,
    /// The ranges of slots of the title's own memory ([`Kept::Asked`]), each
    /// with how many slots read it.
    asked: BTreeMap<(usize, usize), u32>,
    /// The frame they were last asked about, and where the next asking of a
    /// frame that could not ask about all of them goes on.
    asked_in: u64,
    ask_from: (usize, usize),
    /// Slots read while their guard was not all up ([`Kept::Open`]), for the
    /// next [`Watchers::moved`].
    open: Vec<u32>,
}

impl Watchers {
    /// Slot `slot` is about to read its memory, the pages `pages`: they are
    /// watched from now on, before [`arm`] takes the slot's tick, and the
    /// heap reads the changes kept from before that tick too. Until
    /// [`Watchers::watch`] says what the read saw, any change of them makes
    /// the slot due (an upload too many at worst).
    ///
    /// (Watched only after the tick, a change between the two was dropped:
    /// no slot watched its pages yet. The fault that told it had opened the
    /// block, so the title's later stores made none, and the slot kept the
    /// bytes from before them until something guarded the block again; most
    /// often at a slot's first upload, as a title fills a texture it just
    /// described.)
    pub(crate) fn begin(&mut self, slot: u32, pages: &[Page]) {
        self.start();
        if self.slots.get(&slot).is_some_and(|w| *w.pages == *pages) {
            return;
        }
        let kept = self.slots.get(&slot).map_or(Kept::Told, |w| w.kept);
        self.replace(slot, pages.to_vec(), 0, kept);
    }

    /// Slot `slot` read its memory, the pages `pages`, after tick `since`.
    pub(crate) fn watch(&mut self, slot: u32, pages: Vec<Page>, since: u64, kept: Kept) {
        self.start();
        if kept == Kept::Open {
            self.open.push(slot);
            count(OPEN, 1);
        }
        if let Some(watch) = self.slots.get_mut(&slot)
            && *watch.pages == *pages
        {
            watch.since = since;
            let before = std::mem::replace(&mut watch.kept, kept);
            if before != kept {
                KINDS[before.kind()].fetch_sub(1, Ordering::Relaxed);
                KINDS[kept.kind()].fetch_add(1, Ordering::Relaxed);
                self.unask(before);
                self.ask(kept);
            }
            return;
        }
        self.replace(slot, pages, since, kept);
    }

    /// At the first watch the heap reads the changes kept from here on (none
    /// before concerns a slot of it).
    fn start(&mut self) {
        if !self.started {
            self.cursor = head();
            self.started = true;
        }
    }

    /// Slot `slot` watches `pages` from tick `since` in place of what it
    /// watched: the new pages are held before the old ones are let go, so
    /// that a page in both is watched all along.
    fn replace(&mut self, slot: u32, pages: Vec<Page>, since: u64, kept: Kept) {
        hold(&pages);
        if let Some(old) = self.slots.remove(&slot) {
            let_go(&old.pages);
            self.unlist(slot, &old.pages);
            self.unask(old.kept);
            KINDS[old.kept.kind()].fetch_sub(1, Ordering::Relaxed);
        }
        for &page in &pages {
            self.by_page.entry(page).or_default().push(slot);
        }
        self.ask(kept);
        KINDS[kept.kind()].fetch_add(1, Ordering::Relaxed);
        self.slots.insert(slot, Watch { pages: pages.into_boxed_slice(), since, kept });
    }

    /// `slot` out of the lists of `pages`' watchers.
    fn unlist(&mut self, slot: u32, pages: &[Page]) {
        for page in pages {
            if let Some(slots) = self.by_page.get_mut(page) {
                slots.retain(|&s| s != slot);
                if slots.is_empty() {
                    self.by_page.remove(page);
                }
            }
        }
    }

    fn ask(&mut self, kept: Kept) {
        if let Kept::Asked(at, len) = kept {
            *self.asked.entry((at, at.saturating_add(len))).or_insert(0) += 1;
        }
    }

    fn unask(&mut self, kept: Kept) {
        if let Kept::Asked(at, len) = kept {
            let key = (at, at.saturating_add(len));
            if let Some(n) = self.asked.get_mut(&key) {
                *n -= 1;
                if *n == 0 {
                    self.asked.remove(&key);
                }
            }
        }
    }

    /// Slot `slot` watches nothing any more.
    pub(crate) fn forget(&mut self, slot: u32) {
        let Some(watch) = self.slots.remove(&slot) else {
            return;
        };
        let_go(&watch.pages);
        self.unlist(slot, &watch.pages);
        self.unask(watch.kept);
        KINDS[watch.kept.kind()].fetch_sub(1, Ordering::Relaxed);
    }

    /// Whether changes were kept since the last [`Watchers::moved`].
    pub(crate) fn pending(&self) -> bool {
        self.started && (!self.open.is_empty() || head() > self.cursor)
    }

    /// The slots read while their guard was not all up ([`Kept::Open`])
    /// since the last call, to be read again at a pass to come; in order,
    /// each once.
    pub(crate) fn reopen(&mut self) -> Vec<u32> {
        let mut out: Vec<u32> = std::mem::take(&mut self.open)
            .into_iter()
            .filter(|slot| self.slots.get(slot).is_some_and(|w| w.kept == Kept::Open))
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// The slots one of whose pages changed after their tick, since the last
    /// call; in order, each once.
    pub(crate) fn moved(&mut self) -> Vec<u32> {
        if !self.started {
            return Vec::new();
        }
        let mut out = Vec::new();
        let (changes, lost) = {
            let t = table();
            let head = t.base + t.log.len() as u64;
            let from = self.cursor;
            self.cursor = head;
            if from < t.base {
                // Fell behind what is kept: every slot by its pages' last
                // changes.
                for (&slot, watch) in &self.slots {
                    if watch.pages.iter().any(|p| t.pages.get(p).is_some_and(|e| e.1 > watch.since)) {
                        out.push(slot);
                    }
                }
                (Vec::new(), true)
            } else {
                (t.log.range((from - t.base) as usize..).copied().collect::<Vec<_>>(), false)
            }
        };
        count(LOST, u64::from(lost));
        let mut shown = Vec::new();
        for (page, tick) in changes {
            let Some(slots) = self.by_page.get(&page) else {
                continue;
            };
            for &slot in slots {
                if self.slots.get(&slot).is_some_and(|w| w.since < tick) {
                    out.push(slot);
                    if page.0 & Page::SHOWN != 0 {
                        shown.push(slot);
                    }
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        shown.sort_unstable();
        shown.dedup();
        count(DUE, out.len() as u64);
        count(DUE_SHOWN, shown.len() as u64);
        out
    }

    /// Once a frame (`frame`): the ranges of the title's own memory to ask
    /// the kernel about ([`Kept::Asked`]), at most `most` of them, those
    /// that touch as one, in turns from where the last left off. Each is
    /// the asked ranges it holds, joined. Asking ([`crate::watch::writes`])
    /// tells what was written ([`written_own`]).
    pub(crate) fn asks(&mut self, frame: u64, most: usize) -> Vec<Vec<(usize, usize)>> {
        if self.asked.is_empty() || self.asked_in == frame {
            return Vec::new();
        }
        self.asked_in = frame;
        let mut out: Vec<Vec<(usize, usize)>> = Vec::new();
        let take = |range: (usize, usize), out: &mut Vec<Vec<(usize, usize)>>| -> bool {
            // (In address order, but for the turn back to the lowest.)
            let joins = out
                .last()
                .is_some_and(|run| range.0 >= run[0].0 && run.iter().map(|r| r.1).max().is_some_and(|end| end >= range.0));
            if joins && let Some(run) = out.last_mut() {
                run.push(range);
                return true;
            }
            if out.len() >= most {
                return false;
            }
            out.push(vec![range]);
            true
        };
        let from = self.ask_from;
        let mut next = (0, 0);
        let mut all = true;
        for (&range, _) in self.asked.range(from..).chain(self.asked.range(..from)) {
            if !take(range, &mut out) {
                next = range;
                all = false;
                break;
            }
        }
        self.ask_from = if all { (0, 0) } else { next };
        count(ASKED, out.len() as u64);
        out
    }

    /// Slots of memory nothing tracks ([`Kept::Looked`]) whose look is due at
    /// `now_ms`, up to about `bytes` of memory: slot, address, bytes.
    pub(crate) fn looks(&self, now_ms: u64, bytes: usize) -> Vec<(u32, usize, usize)> {
        let mut out = Vec::new();
        let mut left = bytes;
        for (&slot, watch) in &self.slots {
            if let Kept::Looked(at, len, _, next) = watch.kept
                && next <= now_ms
            {
                if len > left && !out.is_empty() {
                    break;
                }
                left = left.saturating_sub(len);
                out.push((slot, at, len));
            }
        }
        out
    }

    /// Slot `slot`'s memory `[at, at + len)` has digest `digest` at `now_ms`:
    /// whether that is a change (the slot is then due).
    pub(crate) fn looked(&mut self, slot: u32, at: usize, len: usize, digest: u128, now_ms: u64) -> bool {
        let Some(watch) = self.slots.get_mut(&slot) else {
            return false;
        };
        let Kept::Looked(was_at, was_len, was, _) = watch.kept else {
            return false;
        };
        if (was_at, was_len) != (at, len) {
            return false;
        }
        watch.kept = Kept::Looked(at, len, digest, now_ms + LOOK_MS);
        count(LOOKS, 1);
        let changed = was != digest;
        count(LOOKS_CHANGED, u64::from(changed));
        changed
    }
}

impl Drop for Watchers {
    fn drop(&mut self) {
        let slots: Vec<u32> = self.slots.keys().copied().collect();
        for slot in slots {
            self.forget(slot);
        }
    }
}

/// The page generations line for the log.
pub(crate) fn line() -> String {
    let n: [u64; 14] = std::array::from_fn(|i| STATS[i].load(Ordering::Relaxed));
    let kinds: [u64; 3] = std::array::from_fn(|i| KINDS[i].load(Ordering::Relaxed));
    format!(
        "page generations (VICTORY_PAGE_GENERATIONS=0: notices by address and fingerprint looks, as before), since the start: \
         changes of watched pages told by guard faults {}, by the kernel's write watch {}, by our writes {}, by DirectStorage \
         into physical pages {}, by mappings {}, by guards taken off {}; {} changes of memory no slot watches; {} slots made \
         due ({} by a physical page, whatever view it was written through; {} read while their guard was not all up), {} \
         times a heap fell behind the changes kept; {} askings of the kernel for slots of the title's own memory, {} looks by \
         digest at memory nothing tracks ({} found it changed); now {} pages watched by {} slots guarded, {} asked, {} looked at",
        n[Teller::Fault as usize],
        n[Teller::Kernel as usize],
        n[Teller::Ours as usize],
        n[Teller::Physical as usize],
        n[Teller::Mapping as usize],
        n[Teller::Opened as usize],
        n[UNWATCHED],
        n[DUE],
        n[DUE_SHOWN],
        n[OPEN],
        n[LOST],
        n[ASKED],
        n[LOOKS],
        n[LOOKS_CHANGED],
        WATCHED.load(Ordering::Relaxed),
        kinds[0],
        kinds[1],
        kinds[2],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two views of one section's pages (`crate::watch::tests::two_views`),
    /// kept mapped: what the guards know of their blocks (guarded, opened
    /// and when) must not become another test's fresh memory at the same
    /// addresses.
    fn kept_views(bytes: usize) -> (usize, usize, usize) {
        crate::watch::tests::two_views(bytes)
    }

    /// Watches slot `slot` of `watchers` on `[at, at + len)` as the texture
    /// pass does before it reads the memory ([`super::watch`]).
    fn watch(watchers: &mut Watchers, slot: u32, at: usize, len: usize) -> Kept {
        super::watch(slot, at, len, |f| f(watchers))
    }

    #[test]
    fn pages_pack_apart() {
        assert_ne!(Page::shown(4, 1), Page::shown(8, 1));
        assert_ne!(Page::shown(4, 1), Page::shown(4, 2));
        assert_ne!(Page::shown(0, 1), Page::at(PAGE));
        assert_eq!(Page::at(PAGE), Page::at(2 * PAGE - 1));
        assert_eq!(format!("{:?}", Page::shown(0x48, 3)), "Shown(0x48, 3)");
        assert_eq!(format!("{:?}", Page::at(0x7fee_0001_0000)), "At(0x7fee00010000)");
    }

    /// The title writes a texture's pages with its CPU through another view
    /// of them: the guard's fault there tells the page, and the slot that
    /// reads it at the first view is due. (Before, nothing told the texture
    /// pass of a CPU write at any view; five sampled looks went over it.)
    #[test]
    fn a_title_write_through_another_view_makes_the_slot_due() {
        const SIZE: usize = 2 * PAGE;
        let (_, one, two) = kept_views(SIZE);
        let mut watchers = Watchers::default();
        assert_eq!(watch(&mut watchers, 1, one, SIZE), Kept::Told);
        assert_eq!(watchers.moved(), [] as [u32; 0], "nothing written yet");
        // SAFETY: the second view, read-write once the handler opened it.
        unsafe { ((two + PAGE + 8) as *mut u8).write(1) };
        assert_eq!(watchers.moved(), [1]);
        assert_eq!(watchers.moved(), [] as [u32; 0], "told once");
        // Read again for an upload, and written again long after: due again.
        watch(&mut watchers, 1, one, SIZE);
        for _ in 0..1000 {
            assert!(watchers.moved().is_empty());
        }
        // SAFETY: as above, the first view this time.
        unsafe { (one as *mut u8).write(2) };
        assert_eq!(watchers.moved(), [1]);
    }

    /// A write through another view between the slot's tick and the watch of
    /// its pages is not lost, nor are the title's stores after it (the fault
    /// had opened the block: they made none). Before, the pages were held
    /// only after the tick, the change was dropped as one of pages nobody
    /// watched, and the slot never saw the stores (round 1's review probe).
    #[test]
    fn a_write_between_the_tick_and_the_watch_is_not_lost() {
        const SIZE: usize = PAGE;
        TEST_ON.set(Some(true));
        let (_, one, two) = kept_views(SIZE);
        let mut watchers = Watchers::default();
        // What the texture pass does, with the title's write where the
        // window was: after the tick, before the pages were held.
        let pages = pages_of(one, SIZE);
        watchers.begin(1, &pages);
        let (since, kept) = arm(one, SIZE);
        // SAFETY: the second view, read-write once the handler opened it.
        unsafe { ((two + 8) as *mut u8).write(1) };
        watchers.watch(1, pages_of(one, SIZE), since, kept);
        // SAFETY: as above.
        unsafe { ((two + 4096) as *mut u8).write(2) };
        assert_eq!(watchers.moved(), [1], "the writes after the slot's tick reach it");
    }

    /// A slot whose pages change (its texture moved) holds the new ones
    /// before it lets the old ones go: a page in both is watched all along,
    /// and keeps its last change. (Let go first, it was out of the table
    /// for a moment, a change of it then was dropped, and it came back with
    /// no change known.)
    #[test]
    fn a_slot_holds_its_new_pages_before_it_lets_the_old_go() {
        TEST_ON.set(Some(true));
        let at = own_memory(3 * PAGE);
        let shared = Page::at(at + PAGE);
        let entry = |page: Page| table().pages.get(&page).copied();
        let mut watchers = Watchers::default();
        watchers.watch(1, pages_of(at, 2 * PAGE), now(), Kept::Told);
        changed(&[shared], Teller::Ours);
        let told = entry(shared).map(|e| e.1);
        assert!(told.is_some_and(|tick| tick > 0));
        // Its texture moved a page on: the page in both stays watched.
        watchers.watch(1, pages_of(at + PAGE, 2 * PAGE), now(), Kept::Told);
        assert_eq!(entry(shared).map(|e| e.1), told, "its last change kept");
        assert_eq!(entry(shared).map(|e| e.0), Some(1));
        assert!(entry(Page::at(at)).is_none());
        drop(watchers);
        assert!(entry(shared).is_none());
    }

    /// `bytes` of memory of our own (no view, not write-watched) that no
    /// other test is given: never freed, so its addresses, by which its
    /// pages are watched, are this test's alone. (Heap memory of a `Vec` is
    /// another test's next, and the counts of its pages with it.)
    fn own_memory(bytes: usize) -> usize {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
        // SAFETY: a fresh allocation, kept for the life of the process.
        let at = unsafe { VirtualAlloc(std::ptr::null(), bytes, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) } as usize;
        assert_ne!(at, 0);
        at
    }

    /// A block the title has just written rests open (`crate::watch`'s
    /// guard rest): a slot read meanwhile could not count on a fault for the
    /// next write, and is read again at a pass to come, guarded then.
    #[test]
    fn a_slot_read_while_its_block_rests_open_is_read_again() {
        const SIZE: usize = PAGE;
        let (_, one, _) = kept_views(SIZE);
        let mut watchers = Watchers::default();
        assert_eq!(watch(&mut watchers, 1, one, SIZE), Kept::Told);
        crate::watch::REST_IN_TEST.set(std::time::Duration::from_secs(60));
        // SAFETY: the first view, read-write once the handler opened it.
        unsafe { ((one + 8) as *mut u8).write(1) };
        assert_eq!(watchers.moved(), [1]);
        // Read again while the block rests: not guarded, so not told.
        assert_eq!(watch(&mut watchers, 1, one, SIZE), Kept::Open);
        assert_eq!(watchers.reopen(), [1]);
        assert!(watchers.reopen().is_empty(), "once");
        // The rest over, the next read guards it.
        crate::watch::REST_IN_TEST.set(std::time::Duration::ZERO);
        assert_eq!(watch(&mut watchers, 1, one, SIZE), Kept::Told);
        assert!(watchers.reopen().is_empty());
    }

    /// Our own writes (a kernel read, a landing, DirectStorage) told at one
    /// view reach the slot at another; one told at the pages themselves
    /// reaches every view.
    #[test]
    fn our_writes_reach_every_view_of_the_pages() {
        const SIZE: usize = 2 * PAGE;
        let (section, one, two) = kept_views(SIZE);
        let mut watchers = Watchers::default();
        watch(&mut watchers, 1, one, SIZE);
        watch(&mut watchers, 2, two + PAGE, PAGE);
        crate::watch::writing(two + 16, 32);
        assert_eq!(watchers.moved(), [1], "slot 2 reads the second page alone");
        {
            let _write = crate::watch::kernel_writing(one + PAGE, 64);
        }
        assert_eq!(watchers.moved(), [1, 2]);
        physical_written(section, PAGE as u64 + 100, 8);
        assert_eq!(watchers.moved(), [1, 2]);
        crate::watch::landed_beside(&[(two + PAGE) / 4096], &[(two + PAGE, 64)]);
        assert_eq!(watchers.moved(), [1, 2]);
    }

    /// A copy of ours into title memory (a texture copied back, a copy at
    /// submission) tells the slots that read it once its bytes are in.
    #[test]
    fn a_copy_into_title_memory_tells_when_it_is_in() {
        let mut memory = vec![0u8; 2 * PAGE];
        let at = memory.as_mut_ptr() as usize;
        let mut watchers = Watchers::default();
        watchers.watch(4, pages_of(at, 2 * PAGE), now(), Kept::Told);
        let bytes = [7u8; 256];
        crate::host::copy_mapped((at + PAGE) as *mut u8, bytes.as_ptr(), bytes.len(), true);
        assert_eq!(memory[PAGE..PAGE + 256], bytes);
        assert_eq!(watchers.moved(), [4]);
        drop(watchers);
        drop(memory);
    }

    /// The runtime maps or unmaps memory at an address: the slots that read
    /// it there are due, not those that read the same pages elsewhere.
    #[test]
    fn a_mapping_makes_due_the_slots_at_its_address() {
        const SIZE: usize = 2 * PAGE;
        let (_, one, two) = kept_views(SIZE);
        let mut watchers = Watchers::default();
        watch(&mut watchers, 1, one, SIZE);
        watch(&mut watchers, 2, two, SIZE);
        readdressed(two + PAGE, PAGE);
        assert_eq!(watchers.moved(), [2]);
        crate::watch::title_memory_unmapping(one, PAGE);
        assert_eq!(watchers.moved(), [1]);
    }

    /// The title's own memory under the kernel's write watch: a CPU write is
    /// told when the kernel is asked, which the texture pass does once a
    /// frame for its slots there.
    #[test]
    fn the_write_watch_tells_of_writes_to_the_titles_own_memory() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree};
        const MEM_WRITE_WATCH: u32 = 0x20_0000;
        // SAFETY: a fresh write-watched allocation, freed at the end.
        let p = unsafe { VirtualAlloc(std::ptr::null(), 4 * PAGE, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE) }
            as usize;
        assert_ne!(p, 0);
        let mut watchers = Watchers::default();
        assert_eq!(watch(&mut watchers, 7, p + PAGE, 2 * PAGE), Kept::Asked(p + PAGE, 2 * PAGE));
        assert!(watchers.moved().is_empty());
        // SAFETY: committed pages of ours.
        unsafe { ((p + 2 * PAGE + 8) as *mut u8).write(1) };
        assert!(watchers.moved().is_empty(), "not told before the kernel is asked");
        let asks = watchers.asks(1, 64);
        assert_eq!(asks, [vec![(p + PAGE, p + 3 * PAGE)]]);
        assert!(watchers.asks(1, 64).is_empty(), "once a frame");
        for run in asks {
            assert!(crate::watch::writes(run[0].0, run[0].1, false).is_some());
        }
        assert_eq!(watchers.moved(), [7]);
        // SAFETY: our allocation.
        unsafe { VirtualFree(p as *mut _, 0, MEM_RELEASE) };
    }

    /// Memory nothing guards nor watches (the host's heap) is looked at by
    /// its digest, and a change found makes the slot due.
    #[test]
    fn memory_nothing_tracks_is_looked_at_by_its_digest() {
        let mut memory = vec![0u8; 3 * PAGE];
        let at = memory.as_mut_ptr() as usize;
        let mut watchers = Watchers::default();
        let Kept::Looked(_, _, digest, next) = watch(&mut watchers, 3, at, 3 * PAGE) else {
            panic!("plain memory is looked at");
        };
        assert!(watchers.looks(next - 1, usize::MAX).is_empty(), "not before its time");
        assert_eq!(watchers.looks(next, usize::MAX), [(3, at, 3 * PAGE)]);
        assert!(!watchers.looked(3, at, 3 * PAGE, digest, next));
        // SAFETY: our own vector, which nothing else reads now.
        unsafe { ((at + PAGE) as *mut u8).write_volatile(5) };
        let now = digest_of(at, 3 * PAGE);
        assert_ne!(now, digest);
        assert!(watchers.looks(next, usize::MAX).is_empty(), "looked at, next in half a second");
        assert!(watchers.looked(3, at, 3 * PAGE, now, next + LOOK_MS));
        drop(memory);
    }

    /// A heap that falls behind the changes kept compares all it watches by
    /// their pages' last changes: none is lost.
    #[test]
    fn a_watcher_behind_the_changes_kept_loses_none() {
        const SIZE: usize = PAGE;
        let (_, one, two) = kept_views(SIZE);
        let mut watchers = Watchers::default();
        watch(&mut watchers, 1, one, SIZE);
        watch(&mut watchers, 2, two, SIZE);
        crate::watch::writing(two, 8);
        // Other changes, more than are kept.
        let mut other = Watchers::default();
        let memory = vec![0u8; PAGE];
        watch(&mut other, 9, memory.as_ptr() as usize, PAGE);
        for _ in 0..=LOG_MOST {
            changed(&[Page::at(memory.as_ptr() as usize)], Teller::Ours);
        }
        assert_eq!(watchers.moved(), [1, 2]);
        assert!(watchers.moved().is_empty());
        drop((watchers, other, memory));
    }

    /// The log line says who told of changes and what watches now.
    #[test]
    fn the_line_counts_who_told() {
        let mut memory = vec![0u8; PAGE];
        let at = memory.as_mut_ptr() as usize;
        let mut watchers = Watchers::default();
        watchers.watch(5, pages_of(at, PAGE), now(), Kept::Told);
        let before = STATS[Teller::Ours as usize].load(Ordering::Relaxed);
        written(at, 8);
        assert!(STATS[Teller::Ours as usize].load(Ordering::Relaxed) > before);
        let line = line();
        assert!(line.starts_with("page generations (VICTORY_PAGE_GENERATIONS=0"), "{line}");
        assert!(line.contains(" by our writes ") && line.contains(" slots guarded, "), "{line}");
        assert_eq!(watchers.moved(), [5]);
        drop(watchers);
        drop(memory);
    }

    /// Watching a slot's pages again with the same pages keeps the count of
    /// watches; another slot's forgetting leaves the first watched.
    #[test]
    fn watches_are_counted() {
        let memory = vec![0u8; 2 * PAGE];
        let at = memory.as_ptr() as usize;
        let pages = pages_of(at, 2 * PAGE);
        let counted = |page: Page| table().pages.get(&page).map_or(0, |e| e.0);
        let mut watchers = Watchers::default();
        watchers.watch(1, pages.clone(), now(), Kept::Told);
        watchers.watch(1, pages.clone(), now(), Kept::Told);
        watchers.watch(2, pages.clone(), now(), Kept::Told);
        assert_eq!(counted(pages[0]), 2);
        watchers.forget(2);
        assert_eq!(counted(pages[0]), 1);
        changed(&pages[..1], Teller::Ours);
        assert_eq!(watchers.moved(), [1]);
        drop(watchers);
        assert_eq!(counted(pages[0]), 0);
        drop(memory);
    }
}
