//! Our own work on the card, timed by the card (`VICTORY_OUR_CARD_TIMES`; on
//! by default, `0`: not taken).
//!
//! The frame line's "on the GPU" is the time the submitting thread waits for
//! the card, and the card's clock counts the time something of ours is
//! queued: neither says how much of that the card spends on the title's
//! draws and how much on passes of ours (sends up to copies in the card's
//! memory, the hashes and compares of those copies, steps done on the card,
//! preludes with their twin copies and uploads, copies and passes of twins
//! inside lists, the clamps of indirect arguments). Each of those gets a pair
//! of timestamps around it in the list it is recorded into, resolved into
//! memory the PC reads when that list is closed; every submission gets a
//! pair around all of its lists. A line per second sums them by kind, per
//! frame.
//!
//! A timestamp is written when the work recorded before it has finished
//! (`a_timestamp_waits_for_what_is_recorded_before_it`: on the RTX 5080 a
//! pair behind a 17 ms pass reads 0.000 ms, one around it 17.5 ms), so a
//! pair measures from the end of what ran before it to the end of what it
//! holds: work of ours that overlaps the end of the title's is counted
//! short, never long. Of the passes recorded into the title's lists (twins,
//! planes, clamps) a list that runs more than once is counted once. The
//! pairs cost the card about 0.5 us each (`bench_what_a_thousand_pairs_cost`).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

use super::{Host, host};

/// What a pair of timestamps is around.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    /// All the lists of a submission, ours and the title's.
    Submission,
    /// Copies in the card's memory: what changed on their CPU side, sent up
    /// ahead of a submission's lists (copies and the words' pass).
    SendsUp,
    /// The same pieces hashed again after they were sent up.
    Hashes,
    /// The compare behind a submission's lists of what its shaders may have
    /// written.
    Compares,
    /// Steps of a submission done on the card in front of its lists
    /// (`VICTORY_CARD_STEPS`).
    Steps,
    /// A list's prelude: copies between depth textures and their twins.
    PreludeTwins,
    /// A list's prelude: uploads of textures it samples, found late.
    PreludeUploads,
    /// Copies between a depth texture and its twin inside a list.
    ListTwins,
    /// Depth planes drawn from their twins inside a list.
    Planes,
    /// Clamps of indirect arguments, one execution's or a batch's.
    Clamps,
    /// Fetches of what shaders wrote to copies in the card's memory, on the
    /// drawing queue.
    Fetches,
}

const KINDS: usize = 11;

impl Kind {
    const ALL: [Kind; KINDS] = [
        Kind::Submission,
        Kind::SendsUp,
        Kind::Hashes,
        Kind::Compares,
        Kind::Steps,
        Kind::PreludeTwins,
        Kind::PreludeUploads,
        Kind::ListTwins,
        Kind::Planes,
        Kind::Clamps,
        Kind::Fetches,
    ];

    fn index(self) -> usize {
        Kind::ALL.iter().position(|&k| k == self).unwrap_or(0)
    }

    /// As the line names it.
    fn text(self) -> &'static str {
        match self {
            Kind::Submission => "submissions",
            Kind::SendsUp => "sends up to copies in the card's memory",
            Kind::Hashes => "hashes after them",
            Kind::Compares => "compares behind the lists",
            Kind::Steps => "steps done on the card",
            Kind::PreludeTwins => "preludes' twin copies",
            Kind::PreludeUploads => "preludes' uploads",
            Kind::ListTwins => "twin copies in lists",
            Kind::Planes => "depth planes",
            Kind::Clamps => "indirect clamps",
            Kind::Fetches => "fetches on the drawing queue",
        }
    }
}

/// Whether the card's times are taken (`VICTORY_OUR_CARD_TIMES=0`: not).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_OUR_CARD_TIMES").is_none_or(|v| v != "0"))
}

/// Pairs for passes of ours, used in turn by blocks of [`BLOCK`]: some
/// hundreds of blocks a frame, read every second.
const PAIRS: u32 = 65536;
/// A list takes pairs this many at a time, and resolves those it used in
/// one call when it is closed: a resolve of its own behind each pair cost
/// the card 3.3 us (`bench_what_a_thousand_pairs_cost`: 1,000 short passes
/// 30.8 ms, 31.3 with pairs resolved once, 34.0 with each resolved).
const BLOCK: u32 = 8;
/// Pairs for whole submissions, with their lists made ahead: some thirty a
/// frame; one is taken again some fifteen frames later, when its lists
/// have long run.
const WHOLE: u32 = 512;

struct Clock {
    heap: ID3D12QueryHeap,
    buffer: ID3D12Resource,
    /// The mapping of `buffer`: two tick counts per pair.
    back: usize,
    /// Card ticks per millisecond.
    per_ms: f64,
    /// Per pair: 0 not out, else its kind's index plus one.
    kinds: Box<[AtomicU8]>,
    /// For whole submissions: the list in front of one and the list behind.
    ends: Vec<(ID3D12CommandList, ID3D12CommandList)>,
    _allocator: ID3D12CommandAllocator,
}

// SAFETY: free-threaded D3D12 objects and a mapping written by the card and
// read through atomics or once a pair is complete.
unsafe impl Send for Clock {}
// SAFETY: as above.
unsafe impl Sync for Clock {}

static CLOCK: OnceLock<Option<Clock>> = OnceLock::new();
static NEXT: AtomicU32 = AtomicU32::new(0);
static NEXT_WHOLE: AtomicU32 = AtomicU32::new(0);
/// Since the last line, by kind: ticks, pairs.
static SUMS: [[AtomicU64; 2]; KINDS] = [const { [const { AtomicU64::new(0) }; 2] }; KINDS];
/// Since the last line: pairs read whose times made no sense (the second
/// before the first, or a second apart).
static ODD: AtomicU64 = AtomicU64::new(0);
/// [`NEXT`] at the last gathering: more than [`PAIRS`] taken since then
/// overwrote some before they were read.
static GATHERED: AtomicU32 = AtomicU32::new(0);
static OVERWRITTEN: AtomicU64 = AtomicU64::new(0);

fn clock() -> Option<&'static Clock> {
    if !on() {
        return None;
    }
    CLOCK
        .get_or_init(|| {
            let host = host()?;
            match make(host) {
                Ok(clock) => Some(clock),
                Err(e) => {
                    tracing::warn!(error = %e, "our work on the card is not timed");
                    None
                }
            }
        })
        .as_ref()
}

fn make(host: &Host) -> windows::core::Result<Clock> {
    let pairs = PAIRS + WHOLE;
    let Some((buffer, back)) = host.shared_buffer(16 * u64::from(pairs)) else {
        return Err(windows::core::Error::from(windows::Win32::Foundation::E_OUTOFMEMORY));
    };
    super::name(&buffer, || "victory: our work on the card, timed".into());
    // SAFETY: plain object creation; a fresh mapping of that size; lists
    // recorded once and closed.
    unsafe {
        std::ptr::write_bytes(back, 0, 16 * pairs as usize);
        let mut heap: Option<ID3D12QueryHeap> = None;
        host.device.CreateQueryHeap(
            &D3D12_QUERY_HEAP_DESC {
                Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                Count: 2 * pairs,
                NodeMask: 0,
            },
            &mut heap,
        )?;
        let heap = heap.ok_or_else(windows::core::Error::empty)?;
        let frequency = host.queue.GetTimestampFrequency()?;
        let allocator: ID3D12CommandAllocator = host.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
        let mut ends = Vec::with_capacity(WHOLE as usize);
        for at in PAIRS..pairs {
            let mut two = Vec::with_capacity(2);
            for second in [false, true] {
                let list: ID3D12GraphicsCommandList =
                    host.device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
                list.EndQuery(&heap, D3D12_QUERY_TYPE_TIMESTAMP, 2 * at + u32::from(second));
                if second {
                    list.ResolveQueryData(&heap, D3D12_QUERY_TYPE_TIMESTAMP, 2 * at, 2, &buffer, 16 * u64::from(at));
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
            back: back as usize,
            per_ms: frequency.max(1) as f64 / 1e3,
            kinds: (0..pairs).map(|_| AtomicU8::new(0)).collect(),
            ends,
            _allocator: allocator,
        })
    }
}

impl Clock {
    /// The two tick counts of pair `at`.
    fn stamps(&self, at: u32) -> [&AtomicU64; 2] {
        // SAFETY: the mapping holds two aligned tick counts per pair; the
        // card writes them whole, the PC through these.
        [0, 1].map(|i| unsafe { &*((self.back + 16 * at as usize + 8 * i) as *const AtomicU64) })
    }

    /// Pair `at` is taken for `kind`.
    fn out(&self, at: u32, kind: Kind) {
        for stamp in self.stamps(at) {
            stamp.store(0, Ordering::Relaxed);
        }
        self.kinds[at as usize].store(kind.index() as u8 + 1, Ordering::Release);
    }
}

/// A pair taken for work of ours being recorded ([`Pairs::begin`],
/// [`Pairs::end`]).
#[must_use]
pub(crate) struct Mark(u32);

/// The pairs one list (open) has taken, by blocks: first pair, pairs used.
/// [`Pairs::resolve`] before the list is closed.
#[derive(Default)]
pub(crate) struct Pairs(Vec<(u32, u32)>);

impl Pairs {
    /// The first timestamp of a pair for `kind`, recorded into `list`
    /// ahead of the work; `None` when the times are not taken.
    pub(crate) fn begin(&mut self, list: &ID3D12GraphicsCommandList, kind: Kind) -> Option<Mark> {
        let clock = clock()?;
        let at = match self.0.last_mut() {
            Some((first, used)) if *used < BLOCK => {
                *used += 1;
                *first + *used - 1
            }
            _ => {
                // (`PAIRS` is a multiple of `BLOCK`: a block never wraps.)
                let first = NEXT.fetch_add(BLOCK, Ordering::Relaxed) % PAIRS;
                self.0.push((first, 1));
                first
            }
        };
        clock.out(at, kind);
        // SAFETY: an open list and our query heap.
        unsafe { list.EndQuery(&clock.heap, D3D12_QUERY_TYPE_TIMESTAMP, 2 * at) };
        Some(Mark(at))
    }

    /// The second timestamp of `mark`'s pair, behind the work; in the same
    /// list as [`Pairs::begin`].
    pub(crate) fn end(&self, list: &ID3D12GraphicsCommandList, mark: Option<Mark>) {
        let (Some(Mark(at)), Some(Some(clock))) = (mark, CLOCK.get()) else {
            return;
        };
        // SAFETY: an open list and our query heap.
        unsafe { list.EndQuery(&clock.heap, D3D12_QUERY_TYPE_TIMESTAMP, 2 * at + 1) };
    }

    /// The pairs into memory the PC reads, a call per block; behind every
    /// [`Pairs::end`] of the list (each pair taken was ended).
    pub(crate) fn resolve(&mut self, list: &ID3D12GraphicsCommandList) {
        let Some(Some(clock)) = CLOCK.get() else {
            self.0.clear();
            return;
        };
        for (first, used) in self.0.drain(..) {
            // SAFETY: an open list, our query heap and buffer (its custom
            // heap's buffers take copies in the common state); every query
            // of the block's used pairs was ended in this list.
            unsafe {
                list.ResolveQueryData(
                    &clock.heap,
                    D3D12_QUERY_TYPE_TIMESTAMP,
                    2 * first,
                    2 * used,
                    &clock.buffer,
                    16 * u64::from(first),
                );
            }
        }
    }

    /// The list is reset: what it took is let go.
    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }
}

/// [`Pairs`] for a list that is recorded and closed in one go: `record`
/// gets the list's pairs, which are resolved behind what it records.
pub(crate) fn timing<T>(list: &ID3D12GraphicsCommandList, record: impl FnOnce(&mut Pairs) -> T) -> T {
    let mut pairs = Pairs::default();
    let done = record(&mut pairs);
    pairs.resolve(list);
    done
}

/// The lists that go in front of and behind all of a submission's.
pub(crate) fn whole() -> Option<(ID3D12CommandList, ID3D12CommandList)> {
    let clock = clock()?;
    let i = NEXT_WHOLE.fetch_add(1, Ordering::Relaxed) % WHOLE;
    clock.out(PAIRS + i, Kind::Submission);
    let (front, behind) = &clock.ends[i as usize];
    Some((front.clone(), behind.clone()))
}

/// Adds the pairs the card has written to the sums.
fn gather(clock: &Clock) {
    let most = (clock.per_ms * 1e3) as u64;
    let next = NEXT.load(Ordering::Relaxed);
    let taken = next.wrapping_sub(GATHERED.swap(next, Ordering::Relaxed));
    OVERWRITTEN.fetch_add(u64::from(taken.saturating_sub(PAIRS)), Ordering::Relaxed);
    for at in 0..PAIRS + WHOLE {
        let kind = clock.kinds[at as usize].load(Ordering::Acquire);
        if kind == 0 {
            continue;
        }
        let [from, to] = clock.stamps(at).map(|s| s.load(Ordering::Relaxed));
        if from == 0 || to == 0 {
            continue;
        }
        if clock.kinds[at as usize].compare_exchange(kind, 0, Ordering::AcqRel, Ordering::Relaxed).is_err() {
            continue;
        }
        match to.checked_sub(from).filter(|&ticks| ticks < most) {
            Some(ticks) => {
                let sums = &SUMS[usize::from(kind - 1).min(KINDS - 1)];
                sums[0].fetch_add(ticks, Ordering::Relaxed);
                sums[1].fetch_add(1, Ordering::Relaxed);
            }
            None => {
                ODD.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Since the last call, by kind: milliseconds and pairs.
fn take() -> Option<[(f64, u64); KINDS]> {
    let clock = CLOCK.get()?.as_ref()?;
    gather(clock);
    Some(std::array::from_fn(|k| {
        let [ticks, pairs] = [0, 1].map(|i| SUMS[k][i].swap(0, Ordering::Relaxed));
        (ticks as f64 / clock.per_ms, pairs)
    }))
}

/// For the frame-rate line, over `frames` frames.
pub(crate) fn line(frames: f64) {
    let Some(sums) = take() else {
        return;
    };
    let frames = frames.max(1.0);
    let (whole, submissions) = (sums[0].0 / frames, sums[0].1 as f64 / frames);
    // Among a submission's lists, and sent on their own before or after
    // them (the sends up and their hashes go ahead of the submission, in a
    // list of their own; the fetches behind it).
    let inside = [
        Kind::Compares,
        Kind::Steps,
        Kind::PreludeTwins,
        Kind::PreludeUploads,
        Kind::ListTwins,
        Kind::Planes,
        Kind::Clamps,
    ];
    let apart = [Kind::SendsUp, Kind::Hashes, Kind::Fetches];
    let ms = |kinds: &[Kind]| kinds.iter().map(|k| sums[k.index()].0).sum::<f64>() / frames;
    let parts = |kinds: &[Kind]| {
        let mut text = Vec::new();
        for kind in kinds {
            let (ms, pairs) = sums[kind.index()];
            if pairs > 0 {
                text.push(format!("{} {:.2} ms ({:.1})", kind.text(), ms / frames, pairs as f64 / frames));
            }
        }
        text.join(", ")
    };
    let ours = ms(&inside);
    tracing::info!(
        "the card's times per frame, by its timestamps (VICTORY_OUR_CARD_TIMES=0: not taken): {submissions:.1} submissions \
         ran {whole:.1} ms, the title's lists {:.1} of it and ours {ours:.1} ({}); ours sent apart from them {:.1} ms ({}) \
         (ms, pairs a frame); {} pairs read out of order, {} taken again before they were read",
        (whole - ours).max(0.0),
        parts(&inside),
        ms(&apart),
        parts(&apart),
        ODD.swap(0, Ordering::Relaxed),
        OVERWRITTEN.swap(0, Ordering::Relaxed)
    );
}

#[cfg(test)]
mod bench;

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Threading::{INFINITE, WaitForSingleObject};

    /// Many groups that each count for a while: some milliseconds on any
    /// card.
    pub(super) const LONG: &str = r#"
#define RS "UAV(u0)"
RWByteAddressBuffer sum : register(u0);
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint x = id.x;
    [loop] for (uint i = 0; i < 20000u; i++) x = x * 1664525u + 1013904223u;
    sum.Store((id.x & 63u) * 4u, x);
}
"#;

    /// Runs what `record` records into a list of the host's queue and waits.
    fn run(host: &Host, record: impl FnOnce(&ID3D12GraphicsCommandList)) {
        // SAFETY: a list of the test's own; the host's fence and event
        // under its lock.
        unsafe {
            let allocator: ID3D12CommandAllocator =
                host.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
            let list: ID3D12GraphicsCommandList =
                host.device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).unwrap();
            record(&list);
            list.Close().unwrap();
            let mut submitted = host.submitted.lock().unwrap_or_else(|e| e.into_inner());
            *submitted += 1;
            host.queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
            host.queue.Signal(&host.fence, *submitted).unwrap();
            host.fence.SetEventOnCompletion(*submitted, host.event).unwrap();
            WaitForSingleObject(host.event, INFINITE);
        }
    }

    /// The pass of [`LONG`], with its buffer.
    fn long_pass(host: &Host) -> (ID3D12RootSignature, ID3D12PipelineState, ID3D12Resource) {
        let code = super::super::compile(LONG, windows::core::s!("main"), windows::core::s!("cs_5_1")).unwrap();
        // SAFETY: plain object creation; the code lives through the calls.
        unsafe {
            let root: ID3D12RootSignature = host.device.CreateRootSignature(0, &code).unwrap();
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
                .unwrap();
            (root, pso, host.uav_buffer(1 << 16).unwrap())
        }
    }

    fn dispatch(list: &ID3D12GraphicsCommandList, (root, pso, sum): &(ID3D12RootSignature, ID3D12PipelineState, ID3D12Resource), groups: u32) {
        // SAFETY: an open list; live objects.
        unsafe {
            list.SetComputeRootSignature(root);
            list.SetPipelineState(pso);
            list.SetComputeRootUnorderedAccessView(0, sum.GetGPUVirtualAddress());
            list.Dispatch(groups, 1, 1);
        }
    }

    /// A pair around a pass says how long the pass ran, and is read once;
    /// a pair whose list never ran is not read. (Run with the others on
    /// WARP, or `-- --ignored` for the card's times.)
    #[test]
    fn a_pair_times_the_pass_it_is_around() {
        let Some(host) = host() else {
            return;
        };
        if !on() || clock().is_none() {
            return;
        }
        let pass = long_pass(host);
        let _ = take();
        // Recorded, never run.
        {
            // SAFETY: a list of the test's own, never sent.
            let list: ID3D12GraphicsCommandList = unsafe {
                let allocator: ID3D12CommandAllocator =
                    host.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
                host.device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).unwrap()
            };
            let mut pairs = Pairs::default();
            let mark = pairs.begin(&list, Kind::Planes);
            pairs.end(&list, mark);
            pairs.resolve(&list);
        }
        let started = std::time::Instant::now();
        run(host, |list| {
            timing(list, |pairs| {
                let mark = pairs.begin(list, Kind::Compares);
                dispatch(list, &pass, 4096);
                pairs.end(list, mark);
            })
        });
        let wall = started.elapsed().as_secs_f64() * 1e3;
        let sums = take().unwrap();
        let (ms, pairs) = sums[Kind::Compares.index()];
        assert_eq!(pairs, 1, "{sums:?}");
        assert_eq!(sums[Kind::Planes.index()].1, 0, "a pair that never ran");
        assert!(ms > 0.0 && ms <= wall + 1.0, "{ms} ms timed, {wall} ms waited");
        // Read once.
        assert_eq!(take().unwrap()[Kind::Compares.index()].1, 0);
    }

    /// A timestamp is written when what was recorded before it has run: a
    /// pair behind a long pass, with nothing between its two, times next to
    /// nothing; around it, the pass. Device test: WARP runs one thing at a
    /// time and says nothing about a card.
    #[test]
    #[ignore = "device test: wants the card (run with --ignored --nocapture)"]
    fn a_timestamp_waits_for_what_is_recorded_before_it() {
        let Some(host) = host() else {
            return;
        };
        if clock().is_none() {
            return;
        }
        let pass = long_pass(host);
        let _ = take();
        run(host, |list| {
            timing(list, |pairs| {
                dispatch(list, &pass, 16384);
                let behind = pairs.begin(list, Kind::Hashes);
                pairs.end(list, behind);
                let around = pairs.begin(list, Kind::Compares);
                dispatch(list, &pass, 16384);
                pairs.end(list, around);
            })
        });
        let sums = take().unwrap();
        let (behind, around) = (sums[Kind::Hashes.index()].0, sums[Kind::Compares.index()].0);
        println!("a pair behind a pass {behind:.3} ms, around one {around:.3} ms");
        assert!(around > 0.5, "{around} ms");
        assert!(behind < around / 10.0, "{behind} ms behind against {around} ms around");
    }
}
