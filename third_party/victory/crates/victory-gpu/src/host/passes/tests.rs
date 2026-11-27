use super::*;
use windows::Win32::System::Threading::{INFINITE, WaitForSingleObject};

/// One frame in `every` is timed, from the `every`-th on: the first frames
/// make pipelines.
#[test]
fn one_frame_in_every_is_timed() {
    let picked: Vec<u64> = (0..100).filter(|&f| timed(f, 30)).collect();
    assert_eq!(picked, [30, 60, 90]);
    assert!((1..5).all(|f| timed(f, 1)) && !timed(0, 1));
}

/// A ring over memory of the test's own, which stands for what the card
/// resolves into.
fn ring() -> (Ring, Vec<u64>) {
    let mut back = vec![0u64; RING as usize];
    let ring = Ring::new(RING / BLOCK, back.as_mut_ptr() as usize);
    (ring, back)
}

/// A list takes its stamps a block at a time, the block's stamps zeroed
/// when it takes it: the stamp after a full block is the first of a new
/// one.
#[test]
fn a_list_takes_its_stamps_a_block_at_a_time() {
    let (ring, _back) = ring();
    let mut blocks = Vec::new();
    let first = ring.take(&mut blocks);
    ring.stamp(first + 3).store(77, Ordering::Relaxed);
    let taken: Vec<u32> = (1..BLOCK).map(|_| ring.take(&mut blocks)).collect();
    assert_eq!(taken, (first + 1..first + BLOCK).collect::<Vec<_>>());
    assert_eq!(blocks.len(), 1);
    let next = ring.take(&mut blocks);
    assert_eq!((blocks.len(), blocks[1].first, blocks[1].used), (2, next, 1));
    assert_eq!(blocks[0], Block { first, used: BLOCK, time: blocks[0].time });
    // Taken again, the first block's stamps are zeroed.
    let again = ring.take_block(first / BLOCK, first);
    assert_eq!(ring.stamp(first + 3).load(Ordering::Relaxed), 0);
    assert_eq!(again, blocks[0].time + 1);
}

/// A block another list has taken since is lost to the list that took it
/// first; a list sent again after another took its block spoils that
/// one's stamps (caught when it is sent), until the block is taken anew.
#[test]
fn a_block_taken_again_or_written_over_is_not_read() {
    let (ring, _back) = ring();
    let first = ring.take_block(5, 5 * BLOCK);
    assert!(!ring.lost(5, first));
    ring.sent(5, first);
    assert!(!ring.lost(5, first), "sent while it still holds the block");
    let second = ring.take_block(5, 5 * BLOCK);
    assert!(ring.lost(5, first), "another list took the block");
    assert!(!ring.lost(5, second));
    // The first list is sent again: its run writes over the second's.
    ring.sent(5, first);
    assert!(ring.lost(5, second));
    let third = ring.take_block(5, 5 * BLOCK);
    assert!(!ring.lost(5, third));
}

/// What comes for a frame already read is too late, and counted so.
#[test]
fn what_comes_after_its_frame_was_read_is_late() {
    READ_UP_TO.fetch_max(10, Ordering::Relaxed);
    let before = LATE.load(Ordering::Relaxed);
    file(5, |f| f.submissions.push((0, 1)));
    assert!(LATE.load(Ordering::Relaxed) > before);
    assert!(!held(&FRAMES).iter().any(|f| f.frame == 5));
}

/// Many groups that each count for a while: some milliseconds on any card.
const LONG: &str = r#"
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

/// Dispatches of known cost, one, two and four times 8,192 groups, each
/// in a span of its own in a timed list: the spans come back from the
/// card, each put down to its work, in order and growing with the work, and
/// the pair around the submission holds them.
/// Device test: WARP runs one thing at a time and says nothing about a
/// card.
#[test]
#[ignore = "device test: wants the card (run with --ignored --nocapture)"]
fn spans_of_dispatches_of_known_cost_come_back_in_order() {
    TAKEN.store(true, Ordering::Relaxed);
    let Some(host) = host() else {
        return;
    };
    let Some(clock) = clock() else {
        return;
    };
    let code = super::super::compile(LONG, windows::core::s!("main"), windows::core::s!("cs_5_1")).unwrap();
    // A frame of its own, timed whatever `VICTORY_PASS_TIMES_EVERY` says.
    let frame = every() * 1_000_003;
    const WORKS: [&str; 3] = ["one", "two", "four"];
    let lane = Lane::default();
    // SAFETY: plain object creation; a list of the test's own, recorded,
    // sent and waited for under the host's lock.
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
        let sum = host.uav_buffer(1 << 16).unwrap();
        let allocator: ID3D12CommandAllocator = host.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
        let list: ID3D12GraphicsCommandList =
            host.device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).unwrap();
        let rec = Rec::new(list.clone());
        lane.arm(frame);
        assert_eq!(lane.armed(), Some(frame));
        for (n, text) in WORKS.into_iter().enumerate() {
            lane.ours(&rec, text);
            list.SetComputeRootSignature(&root);
            list.SetPipelineState(&pso);
            list.SetComputeRootUnorderedAccessView(0, sum.GetGPUVirtualAddress());
            list.Dispatch(8192 << n, 1, 1);
        }
        lane.close(&rec);
        assert_eq!((lane.armed(), lane.recorded()), (None, Some(frame)));
        list.Close().unwrap();
        let mut submitted = host.submitted.lock().unwrap_or_else(|e| e.into_inner());
        *submitted += 1;
        // As a submission with a timed list is sent: a pair of stamps
        // around all of its lists.
        let (front, behind) = around([None, lane.recorded()].into_iter()).expect("a submission with a timed list");
        host.queue.ExecuteCommandLists(&[Some(front), Some(list.cast().unwrap()), Some(behind)]);
        host.queue.Signal(&host.fence, *submitted).unwrap();
        host.fence.SetEventOnCompletion(*submitted, host.event).unwrap();
        WaitForSingleObject(host.event, INFINITE);
    }
    let recorded = {
        let mut frames = held(&FRAMES);
        let at = frames.iter().position(|f| f.frame == frame).expect("the list's spans went to its frame");
        frames.remove(at)
    };
    assert_eq!(recorded.lists.len(), 1);
    let stamps: Vec<u64> = {
        let list = &recorded.lists[0];
        list.segments.iter().map(|s| s.slot).chain([list.end]).map(|at| clock.ring.stamp(at).load(Ordering::Relaxed)).collect()
    };
    assert!(stamps.windows(2).all(|w| w[0] > 0 && w[1] > w[0]), "stamps in order: {stamps:?}");
    let mut totals = rank::Totals::default();
    clock.read(recorded, &mut totals);
    assert_eq!((totals.frames, totals.lists, totals.not_run, totals.overwritten, totals.odd), (1, 1, 0, 0, 0));
    // The submission holds the spans, and little else.
    assert_eq!(totals.submissions, 1);
    assert!(totals.whole >= totals.ticks && totals.ticks as f64 > 0.9 * totals.whole as f64, "{} of {}", totals.ticks, totals.whole);
    let ms: Vec<f64> = WORKS.iter().map(|&w| totals.rows[&Key::Ours(w)].ticks as f64 / clock.per_ms).collect();
    println!("spans of 8192, 16384 and 32768 groups: {ms:.3?} ms");
    assert!(ms[0] > 0.0, "{ms:?}");
    // Twice the groups, at least half again the time (the first span also
    // holds what the card does to begin).
    assert!(ms[1] > 1.5 * ms[0] && ms[2] > 1.5 * ms[1], "{ms:?}");
    println!("{}", rank::text(&totals, clock.per_ms, every(), TOP, |_| String::new()));
}
