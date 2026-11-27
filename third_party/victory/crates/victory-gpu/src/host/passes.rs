//! The card's time of the title's own work, by pipeline
//! (`VICTORY_PASS_TIMES=1`, diagnostics builds; off by default).
//!
//! The frame line says how long the card works on a frame, and `ours` how
//! much of that is passes of ours; neither says which of the title's
//! translated programs the rest goes to. Here one frame in
//! `VICTORY_PASS_TIMES_EVERY` (30 unless set) gets a timestamp in its lists
//! wherever the work changes: at the first draw or dispatch of another host
//! pipeline (or of the same one into another render target; indirect
//! executions and mesh dispatches too), at the first of the title's copies,
//! clears and resolves after other work, and at a pass of ours among the
//! title's draws (the twins, depth planes and indirect clamps `ours`
//! times). A span runs from one stamp to the next, or to the end of the
//! list, and is put down to what began it: a pipeline by its shaders'
//! hashes, with the format and size of what it drew into; the title's
//! copies; or the pass of ours. Every submission that holds such a list
//! gets a pair of stamps around all of its lists, as `ours::whole` does,
//! so the line says how much of the submissions' time the spans cover (the
//! rest: preludes, the compares behind the lists, what a list records
//! before its first draw, lists without draws).
//!
//! As with `ours`'s pairs, a stamp is written when the work recorded
//! before it has finished (`a_timestamp_waits_for_what_is_recorded_before_it`):
//! a span runs from the end of the work before it to the end of its own.
//! Where two pipelines' work overlaps on the card, the overlap is counted
//! once, to the one that finishes first.
//!
//! Lists take their stamps in blocks of [`BLOCK`] out of a ring of
//! [`RING`], resolved into memory the PC reads when the list is closed; a
//! timed frame is read [`LAG`] frames after it was recorded, by the thread
//! that writes the frame lines, so nothing waits for the card. A list sent
//! again after the ring came round and another list took its block writes
//! over that one's times: caught when it is sent, as in `ours`, and not
//! read. A stamp costs the card some 0.5-0.8 us and a resolve some 3 us
//! (`ours`'s bench), and stamps keep the card from overlapping the work on
//! either side of them: a timed frame runs somewhat longer than the
//! others, which are not touched. Every 5 s a line ranks the pipelines
//! that took the most time a timed frame ([`rank`]), with what their
//! programs are ([`props`]).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

use super::rec::Rec;
use super::{Host, host};
use crate::pipeline::Shader;
use crate::pipeline::host::HostPipeline;

mod props;
mod rank;

pub(crate) use rank::Target;

/// Stamps for lists, taken a block at a time: some thousands a timed frame.
const RING: u32 = 1 << 18;
/// A list takes stamps this many at a time, resolved in one call each when
/// it is closed.
const BLOCK: u32 = 64;
/// Pairs for whole submissions with timed lists: some forty a timed frame.
const WHOLE: u32 = 512;
/// Frames presented after a timed one before its stamps are read: its
/// lists have run by then (the queue's line runs a frame or two behind).
const LAG: u64 = 6;
/// Pipelines the line names (`VICTORY_PASS_TIMES_TOP` sets another count).
const TOP: usize = 25;

/// How many pipelines the line names: `VICTORY_PASS_TIMES_TOP`, [`TOP`]
/// unless set.
fn top() -> usize {
    static TOP_SET: OnceLock<usize> = OnceLock::new();
    *TOP_SET.get_or_init(|| {
        std::env::var("VICTORY_PASS_TIMES_TOP").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(TOP).max(1)
    })
}

/// Whether the title's work is timed by pipeline (`VICTORY_PASS_TIMES=1`).
pub(crate) fn on() -> bool {
    #[cfg(test)]
    if TAKEN.load(Ordering::Relaxed) {
        return true;
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PASS_TIMES").is_some_and(|v| v == "1"))
}

/// Tests: the times are taken whatever the switch says.
#[cfg(test)]
static TAKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// One frame in how many is timed (`VICTORY_PASS_TIMES_EVERY`, 30 unless
/// set).
fn every() -> u64 {
    static EVERY: OnceLock<u64> = OnceLock::new();
    *EVERY.get_or_init(|| {
        std::env::var("VICTORY_PASS_TIMES_EVERY").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(30).max(1)
    })
}

/// Whether the lists begun after `frame` frames were presented are timed:
/// one frame in `every`, from the `every`-th on.
fn timed(frame: u64, every: u64) -> bool {
    frame >= every && frame.is_multiple_of(every)
}

/// The frame a list begun now is recorded for: the frames presented so far.
pub(crate) fn frame_now() -> u64 {
    crate::queue::presented()
}

/// What a span of a list's work is put down to.
#[derive(Clone)]
pub(crate) enum Work {
    /// A pipeline of the title, by its stages' shaders: vertex or mesh,
    /// then pixel; or compute.
    Pipeline(Kind, [Option<Arc<Shader>>; 2]),
    /// The title's copies, clears and resolves.
    Copies,
    /// A pass of ours among the title's draws (`ours::Kind`'s text).
    Ours(&'static str),
}

/// What a pipeline runs.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Kind {
    Draw,
    Mesh,
    Compute,
}

impl Work {
    /// The work of `p`'s draws or dispatches.
    pub(crate) fn of(p: &HostPipeline) -> Work {
        let kind = match (p.compute, p.mesh) {
            (true, _) => Kind::Compute,
            (false, true) => Kind::Mesh,
            (false, false) => Kind::Draw,
        };
        let mut shaders = p.stages.iter().map(|s| Some(s.shader.clone()));
        Work::Pipeline(kind, [shaders.next().flatten(), shaders.next().flatten()])
    }
}

/// One span a list began: its first stamp, what it is of, the render
/// target it drew into, its draws or dispatches (or copies, or passes of
/// ours) and of those the indirect executions.
struct Segment {
    slot: u32,
    work: Work,
    target: Target,
    calls: u32,
    indirect: u32,
}

/// A block of stamps a list took: its first, how many it used, and the
/// times the block had been taken then.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Block {
    first: u32,
    used: u32,
    time: u32,
}

/// What a host list keeps of its timing: the frame its recording is timed
/// for, and its stamps.
#[derive(Default)]
pub(crate) struct Lane {
    /// The frame the recording is timed for, plus one (0: not timed).
    armed: AtomicU64,
    /// The frame its last closed recording was timed for, plus one, until
    /// the list is reset.
    closed: AtomicU64,
    marks: Mutex<Marks>,
}

#[derive(Default)]
struct Marks {
    blocks: Vec<Block>,
    segments: Vec<Segment>,
    /// What the open span is of (0: none): a host pipeline's address, one
    /// of ours' text, or [`COPIES`]; and the target it draws into.
    open: (usize, Target),
    /// The blocks of the closed recording, for [`Lane::sent`].
    sent: Vec<(u32, u32)>,
}

/// [`Marks::open`] for the title's copies.
const COPIES: usize = 1;

fn held<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Lane {
    /// The list begins a recording made while `frame` frames were
    /// presented: timed if that frame is.
    pub(crate) fn arm(&self, frame: u64) {
        if !on() {
            return;
        }
        self.clear();
        if timed(frame, every()) && clock().is_some() {
            self.armed.store(frame + 1, Ordering::Relaxed);
        }
    }

    /// The frame of a recording being timed, if it is.
    pub(crate) fn armed(&self) -> Option<u64> {
        self.armed.load(Ordering::Relaxed).checked_sub(1)
    }

    /// The frame a closed recording was timed for, if it was.
    pub(crate) fn recorded(&self) -> Option<u64> {
        self.closed.load(Ordering::Relaxed).checked_sub(1)
    }

    /// The list is reset: what it took is let go.
    pub(crate) fn clear(&self) {
        if !on() {
            return;
        }
        self.armed.store(0, Ordering::Relaxed);
        self.closed.store(0, Ordering::Relaxed);
        let mut marks = held(&self.marks);
        marks.blocks.clear();
        marks.segments.clear();
        marks.sent.clear();
        marks.open = (0, Target::default());
    }

    /// A draw or dispatch of `what` (a host pipeline's address) into
    /// `target` is recorded next: a stamp if that is not what the open span
    /// is of.
    pub(crate) fn open(&self, list: &Rec, what: usize, work: impl FnOnce() -> Work, target: Target) {
        if self.armed.load(Ordering::Relaxed) == 0 {
            return;
        }
        let Some(clock) = clock() else {
            return;
        };
        let mut marks = held(&self.marks);
        if marks.open == (what, target) {
            if let Some(last) = marks.segments.last_mut() {
                last.calls += 1;
            }
            return;
        }
        let slot = clock.take(&mut marks.blocks);
        // SAFETY: an open list and our query heap.
        unsafe { list.EndQuery(&clock.heap, D3D12_QUERY_TYPE_TIMESTAMP, slot) };
        marks.segments.push(Segment {
            slot,
            work: work(),
            target,
            calls: 1,
            indirect: 0,
        });
        marks.open = (what, target);
    }

    /// One of the title's copies, clears or resolves is recorded next.
    pub(crate) fn copies(&self, list: &Rec) {
        self.open(list, COPIES, || Work::Copies, Target::default());
    }

    /// A pass of ours (`ours::Kind`'s `text`) is recorded next.
    pub(crate) fn ours(&self, list: &Rec, text: &'static str) {
        self.open(list, text.as_ptr() as usize, || Work::Ours(text), Target::default());
    }

    /// The draw or dispatch just counted is an indirect execution.
    pub(crate) fn indirect(&self) {
        if self.armed.load(Ordering::Relaxed) == 0 {
            return;
        }
        if let Some(last) = held(&self.marks).segments.last_mut() {
            last.indirect += 1;
        }
    }

    /// The list is about to be closed: the last stamp, the stamps into
    /// memory the PC reads, and the spans to their frame.
    pub(crate) fn close(&self, list: &Rec) {
        let armed = self.armed.swap(0, Ordering::Relaxed);
        let (Some(frame), Some(Some(clock))) = (armed.checked_sub(1), CLOCK.get()) else {
            return;
        };
        let mut marks = held(&self.marks);
        marks.open = (0, Target::default());
        if marks.segments.is_empty() {
            marks.blocks.clear();
            return;
        }
        let end = clock.take(&mut marks.blocks);
        // SAFETY: an open list, our query heap and buffer (its custom
        // heap's buffers take copies in the common state); every stamp of
        // each block's used ones was ended in this list.
        unsafe {
            list.EndQuery(&clock.heap, D3D12_QUERY_TYPE_TIMESTAMP, end);
            for b in &marks.blocks {
                list.ResolveQueryData(
                    &clock.heap,
                    D3D12_QUERY_TYPE_TIMESTAMP,
                    b.first,
                    b.used,
                    &clock.buffer,
                    8 * u64::from(b.first),
                );
            }
        }
        let blocks = std::mem::take(&mut marks.blocks);
        marks.sent = blocks.iter().map(|b| (b.first, b.time)).collect();
        let segments = std::mem::take(&mut marks.segments);
        drop(marks);
        self.closed.store(armed, Ordering::Relaxed);
        file(frame, |f| f.lists.push(Recorded { blocks, segments, end }));
    }

    /// The list is sent to the card: if another list has taken one of its
    /// blocks since (the ring came round), this run writes over that one's
    /// times, which are then not read.
    pub(crate) fn sent(&self) {
        if self.closed.load(Ordering::Relaxed) == 0 {
            return;
        }
        let Some(Some(clock)) = CLOCK.get() else {
            return;
        };
        for &(first, time) in &held(&self.marks).sent {
            clock.ring.sent(first / BLOCK, time);
        }
    }
}

/// The PC's side of the ring: per block the times it was taken, and the
/// time whose stamps a list sent again wrote over (0: none); the mapping
/// the card resolves into, a tick count per stamp.
struct Ring {
    back: usize,
    times: Box<[AtomicU32]>,
    spoiled: Box<[AtomicU32]>,
}

impl Ring {
    /// A ring of `blocks` blocks over `back`, which holds a tick count per
    /// stamp (zeroed, alive as long as this).
    fn new(blocks: u32, back: usize) -> Ring {
        Ring {
            back,
            times: (0..blocks).map(|_| AtomicU32::new(0)).collect(),
            spoiled: (0..blocks).map(|_| AtomicU32::new(0)).collect(),
        }
    }

    /// The tick count of stamp `at`.
    fn stamp(&self, at: u32) -> &AtomicU64 {
        // SAFETY: the mapping holds an aligned tick count per stamp; the
        // card writes them whole, the PC through this.
        unsafe { &*((self.back + 8 * at as usize) as *const AtomicU64) }
    }

    /// Block `block` (its stamps from `first`) is taken: its stamps zeroed,
    /// and the time it is taken.
    fn take_block(&self, block: u32, first: u32) -> u32 {
        for at in first..first + BLOCK {
            self.stamp(at).store(0, Ordering::Relaxed);
        }
        self.times[block as usize].fetch_add(1, Ordering::AcqRel).wrapping_add(1)
    }

    /// A list that took `block` at `time` is sent: if the block was taken
    /// again since, its stamps are the other list's no more.
    fn sent(&self, block: u32, time: u32) {
        let now = self.times[block as usize].load(Ordering::Acquire);
        if now != time {
            self.spoiled[block as usize].store(now, Ordering::Release);
        }
    }

    /// Whether the stamps of `block` taken at `time` are not that list's
    /// any more: taken again, or written over by a list sent again.
    fn lost(&self, block: u32, time: u32) -> bool {
        self.times[block as usize].load(Ordering::Acquire) != time
            || self.spoiled[block as usize].load(Ordering::Acquire) == time
    }

    /// The next stamp of a list whose blocks are `blocks` (a ring of
    /// [`RING`] stamps).
    fn take(&self, blocks: &mut Vec<Block>) -> u32 {
        match blocks.last_mut() {
            Some(b) if b.used < BLOCK => {
                b.used += 1;
                b.first + b.used - 1
            }
            _ => {
                // (`RING` is a multiple of `BLOCK`: a block never wraps.)
                let first = NEXT.fetch_add(BLOCK, Ordering::Relaxed) % RING;
                let time = self.take_block(first / BLOCK, first);
                blocks.push(Block { first, used: 1, time });
                first
            }
        }
    }
}

struct Clock {
    heap: ID3D12QueryHeap,
    buffer: ID3D12Resource,
    /// Card ticks per millisecond.
    per_ms: f64,
    ring: Ring,
    /// For whole submissions: the list in front of one and the list behind,
    /// and per pair the times it was taken.
    ends: Vec<(ID3D12CommandList, ID3D12CommandList)>,
    whole: Box<[AtomicU32]>,
    _allocator: ID3D12CommandAllocator,
}

// SAFETY: free-threaded D3D12 objects and a mapping written by the card and
// read through atomics.
unsafe impl Send for Clock {}
// SAFETY: as above.
unsafe impl Sync for Clock {}

static CLOCK: OnceLock<Option<Clock>> = OnceLock::new();
static NEXT: AtomicU32 = AtomicU32::new(0);
static NEXT_WHOLE: AtomicU32 = AtomicU32::new(0);

fn clock() -> Option<&'static Clock> {
    if !on() {
        return None;
    }
    CLOCK
        .get_or_init(|| {
            let host = host()?;
            match make(host) {
                Ok(clock) => {
                    tracing::info!(
                        every = every(),
                        "the title's work on the card is timed by pipeline (VICTORY_PASS_TIMES=1): one frame in {}",
                        every()
                    );
                    Some(clock)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "the title's work on the card is not timed by pipeline");
                    None
                }
            }
        })
        .as_ref()
}

fn make(host: &Host) -> windows::core::Result<Clock> {
    let stamps = RING + 2 * WHOLE;
    let Some((buffer, back)) = host.shared_buffer(8 * u64::from(stamps)) else {
        return Err(windows::core::Error::from(windows::Win32::Foundation::E_OUTOFMEMORY));
    };
    super::name(&buffer, || "victory: the title's work on the card, timed by pipeline".into());
    // SAFETY: plain object creation; a fresh mapping of that size; lists
    // recorded once and closed.
    unsafe {
        std::ptr::write_bytes(back, 0, 8 * stamps as usize);
        let mut heap: Option<ID3D12QueryHeap> = None;
        host.device.CreateQueryHeap(
            &D3D12_QUERY_HEAP_DESC {
                Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                Count: stamps,
                NodeMask: 0,
            },
            &mut heap,
        )?;
        let heap = heap.ok_or_else(windows::core::Error::empty)?;
        let frequency = host.queue.GetTimestampFrequency()?;
        let allocator: ID3D12CommandAllocator = host.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
        let mut ends = Vec::with_capacity(WHOLE as usize);
        for pair in 0..WHOLE {
            let at = RING + 2 * pair;
            let mut two = Vec::with_capacity(2);
            for second in [false, true] {
                let list: ID3D12GraphicsCommandList =
                    host.device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
                list.EndQuery(&heap, D3D12_QUERY_TYPE_TIMESTAMP, at + u32::from(second));
                if second {
                    list.ResolveQueryData(&heap, D3D12_QUERY_TYPE_TIMESTAMP, at, 2, &buffer, 8 * u64::from(at));
                }
                list.Close()?;
                two.push(list.cast::<ID3D12CommandList>()?);
            }
            let behind = two.pop().ok_or_else(windows::core::Error::empty)?;
            let front = two.pop().ok_or_else(windows::core::Error::empty)?;
            ends.push((front, behind));
        }
        Ok(Clock {
            heap,
            buffer,
            per_ms: frequency.max(1) as f64 / 1e3,
            ring: Ring::new(RING / BLOCK, back as usize),
            ends,
            whole: (0..WHOLE).map(|_| AtomicU32::new(0)).collect(),
            _allocator: allocator,
        })
    }
}

impl Clock {
    fn take(&self, blocks: &mut Vec<Block>) -> u32 {
        self.ring.take(blocks)
    }

    /// What the card wrote for the timed frame `f`, into `into`.
    fn read(&self, f: Frame, into: &mut rank::Totals) {
        let most = (self.per_ms * 1e3) as u64;
        let mut any = false;
        for list in f.lists {
            into.lists += 1;
            if list.blocks.iter().any(|b| self.ring.lost(b.first / BLOCK, b.time)) {
                into.overwritten += 1;
                continue;
            }
            let starts: Vec<u32> = list.segments.iter().map(|s| s.slot).collect();
            let Some((ticks, odd)) =
                rank::spans(&starts, list.end, |at| self.ring.stamp(at).load(Ordering::Relaxed), most)
            else {
                into.not_run += 1;
                continue;
            };
            any = true;
            into.odd += odd;
            for (s, ticks) in list.segments.iter().zip(ticks) {
                into.add(&s.work, s.target, ticks, s.calls, s.indirect);
            }
        }
        for (pair, time) in f.submissions {
            if self.whole[pair as usize].load(Ordering::Acquire) != time {
                continue;
            }
            let at = RING + 2 * pair;
            let [from, to] = [at, at + 1].map(|at| self.ring.stamp(at).load(Ordering::Relaxed));
            if from != 0 && to >= from && to - from < most {
                into.submissions += 1;
                into.whole += to - from;
            }
        }
        if any {
            into.frames += 1;
        }
    }
}

/// A closed recording of a timed frame: its blocks, its spans, its last
/// stamp.
struct Recorded {
    blocks: Vec<Block>,
    segments: Vec<Segment>,
    end: u32,
}

/// What a timed frame's lists and submissions left to read.
#[derive(Default)]
struct Frame {
    frame: u64,
    lists: Vec<Recorded>,
    /// Pairs around submissions: the pair, and the times it was taken.
    submissions: Vec<(u32, u32)>,
}

static FRAMES: Mutex<Vec<Frame>> = Mutex::new(Vec::new());
/// Frames below this one were read: what comes for them is too late.
static READ_UP_TO: AtomicU64 = AtomicU64::new(0);
/// Since the last line: lists or submissions of frames already read.
static LATE: AtomicU64 = AtomicU64::new(0);
static TOTALS: Mutex<Option<rank::Totals>> = Mutex::new(None);

/// `add` to what timed frame `frame` left.
fn file(frame: u64, add: impl FnOnce(&mut Frame)) {
    let mut frames = held(&FRAMES);
    if frame < READ_UP_TO.load(Ordering::Relaxed) {
        LATE.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let at = match frames.iter().position(|f| f.frame == frame) {
        Some(at) => at,
        None => {
            frames.push(Frame { frame, ..Frame::default() });
            frames.len() - 1
        }
    };
    add(&mut frames[at]);
}

/// The lists that go in front of and behind all of a submission's, if one
/// of `runs` was recorded in a timed frame.
pub(crate) fn around(frames: impl Iterator<Item = Option<u64>>) -> Option<(ID3D12CommandList, ID3D12CommandList)> {
    let clock = clock()?;
    let frame = frames.flatten().next()?;
    let pair = NEXT_WHOLE.fetch_add(1, Ordering::Relaxed) % WHOLE;
    let at = RING + 2 * pair;
    for at in [at, at + 1] {
        clock.ring.stamp(at).store(0, Ordering::Relaxed);
    }
    let time = clock.whole[pair as usize].fetch_add(1, Ordering::AcqRel).wrapping_add(1);
    file(frame, |f| f.submissions.push((pair, time)));
    let (front, behind) = &clock.ends[pair as usize];
    Some((front.clone(), behind.clone()))
}

/// The frames `LAG` presents old are read into the totals (the thread that
/// writes the frame lines, every second).
pub(crate) fn gather() {
    let Some(Some(clock)) = CLOCK.get() else {
        return;
    };
    let up_to = frame_now().saturating_sub(LAG);
    let ready: Vec<Frame> = {
        let mut frames = held(&FRAMES);
        READ_UP_TO.fetch_max(up_to, Ordering::Relaxed);
        let (ready, kept) = std::mem::take(&mut *frames).into_iter().partition(|f| f.frame < up_to);
        *frames = kept;
        ready
    };
    if ready.is_empty() {
        return;
    }
    let mut totals = held(&TOTALS);
    let totals = totals.get_or_insert_with(rank::Totals::default);
    for f in ready {
        clock.read(f, totals);
    }
}

/// The ranked line (every 5 s, on the thread that writes the frame lines).
pub(crate) fn line() {
    let Some(Some(clock)) = CLOCK.get() else {
        return;
    };
    gather();
    let Some(mut totals) = held(&TOTALS).take() else {
        return;
    };
    totals.late = LATE.swap(0, Ordering::Relaxed);
    if totals.frames == 0 {
        return;
    }
    let text = rank::text(&totals, clock.per_ms, every(), top(), props::text);
    tracing::info!("{text}");
}

/// Short names of the stages, as the line names them.
fn stage(stage: &str) -> &'static str {
    match stage {
        "vertex" => "vs",
        "pixel" => "ps",
        "compute" => "cs",
        "mesh" => "ms",
        _ => "?s",
    }
}

/// The shaders a row names, for [`props`]: those of a pipeline.
fn shaders(work: &Work) -> impl Iterator<Item = &Arc<Shader>> {
    let none: &[Option<Arc<Shader>>] = &[];
    match work {
        Work::Pipeline(_, shaders) => shaders.as_slice(),
        _ => none,
    }
    .iter()
    .flatten()
}

/// A key of a work's rows: its shaders' addresses (shaders are never let
/// go: the address names one), or which other work it is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Key {
    Pipeline(Kind, [usize; 2]),
    Copies,
    Ours(&'static str),
}

impl Work {
    fn key(&self) -> Key {
        match self {
            Work::Pipeline(kind, shaders) => Key::Pipeline(
                *kind,
                [0, 1].map(|i| shaders[i].as_ref().map_or(0, |s| Arc::as_ptr(s) as usize)),
            ),
            Work::Copies => Key::Copies,
            Work::Ours(text) => Key::Ours(text),
        }
    }

    /// As the line names it: its shaders' stages and hashes.
    fn name(&self) -> String {
        match self {
            Work::Pipeline(_, _) => {
                let names: Vec<String> = shaders(self).map(|s| format!("{} {}", stage(s.stage), s.hash)).collect();
                if names.is_empty() { "a pipeline without shaders".into() } else { names.join(" ") }
            }
            Work::Copies => "the title's copies, clears and resolves".into(),
            Work::Ours(text) => format!("ours: {text}"),
        }
    }
}

type Rows = HashMap<Key, rank::Row>;

#[cfg(test)]
mod tests;
