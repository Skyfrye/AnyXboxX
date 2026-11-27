//! The card's memory as the system counts it, and the line that says every
//! five seconds, in release builds too, what holds it.
//!
//! The system counts a block of textures ([`crate::pool`]) as used from
//! the moment it is made to the moment it goes back, whatever lies free
//! inside it. Counting those free bytes as room, the texture queue saw 89 %
//! of the budget where the system saw 100 % (audit vram V2), and kept
//! making textures; and the sweep let textures go from all over the
//! blocks, which freed pieces of them and none of the card's memory.

use std::sync::atomic::Ordering::Relaxed;

/// Whether room on the card is what the system's budget leaves of what it
/// counts in use (on by default): the free bytes inside texture blocks are
/// used memory, as for the system, the empty blocks kept are those under
/// the line, and the sweep lets textures go by whole blocks, which go back
/// to the system ([`plan`]). `VICTORY_SYSTEM_ROOM=0`: the free bytes in
/// blocks count as room, and the sweep lets textures go by age from any
/// block, as before.
pub(crate) fn system_room() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SYSTEM_ROOM").is_none_or(|v| v != "0"))
}

/// What the card's memory in use comes to for the texture queue's line:
/// what the system says is in use (`used`), with `kept` (what is kept for
/// targets and buffer copies to come). As before ([`system_room`] off):
/// less the bytes free in texture blocks (`free_in_blocks`), which the
/// system counts as used. Blocks on their way back count: they are not back.
pub(crate) fn queue_count(used: u64, free_in_blocks: u64, kept: u64, system: bool) -> u64 {
    if system { used } else { used.saturating_sub(free_in_blocks) }.saturating_add(kept)
}

/// The same for the sweep: less what is on its way back to the system
/// (`giving_back`, blocks the sweep left to empty), which it let go already.
pub(crate) fn sweep_count(queue: u64, giving_back: u64, system: bool) -> u64 {
    if system { queue.saturating_sub(giving_back) } else { queue }
}

/// A host copy the sweep may let go while the card is short: its title
/// texture, the block it lies in (`None`: memory of its own), its bytes,
/// and when it was last used (lower: longer ago).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Idle {
    pub(crate) at: usize,
    pub(crate) block: Option<u32>,
    pub(crate) bytes: u64,
    pub(crate) used: u64,
}

/// The block of a title texture's host copy as the sweep's first look
/// reads it, with no lock (`crate::resource`): a block's number, or these.
/// A copy with memory of its own.
pub(crate) const OWN: u32 = u32::MAX - 1;
/// No copy.
pub(crate) const NO_COPY: u32 = u32::MAX;

/// Of idle host copies, their blocks as read with no lock (`blocks`), the
/// ones worth asking their lock and references: those with memory of their
/// own, and those of blocks with as many idle copies as live pieces
/// (`lives`, by block number). A copy in a block that holds one in use, or
/// another texture, would give nothing back; one with no copy has nothing
/// to give. (Every idle copy was asked, once a second under pressure, on the
/// submitting thread, the bottleneck: 100,000 and more of them, with a
/// global lock each. Round 2's review, item 7.)
pub(crate) fn worth_asking(blocks: &[u32], lives: &[u32]) -> Vec<bool> {
    let mut idle_in = vec![0u32; lives.len()];
    for &block in blocks {
        if let Some(n) = idle_in.get_mut(block as usize) {
            *n += 1;
        }
    }
    blocks
        .iter()
        .map(|&block| match block {
            OWN => true,
            NO_COPY => false,
            block => idle_in.get(block as usize).is_some_and(|&n| lives.get(block as usize) == Some(&n)),
        })
        .collect()
}

/// Host copies the sweep lets go together, and the bytes that gives back
/// to the system; `block`: the block they all lie in.
#[derive(Debug, PartialEq)]
pub(crate) struct Unit {
    pub(crate) textures: Vec<usize>,
    pub(crate) gives: u64,
    pub(crate) block: Option<u32>,
}

/// The order in which idle host copies go while the card is short, by what
/// their going gives back to the system: every copy of a block whose every
/// piece (`lives`, by block number) is an idle copy, which gives the block
/// back, and copies with memory of their own; the unit whose newest use is
/// the oldest first, the fewest copies for a byte given back first among
/// equals. Copies in a block that holds one in use stay: their going would
/// give nothing back (the pieces would be free inside a block the system
/// still counts).
///
/// (By copies per byte first, every whole block giving the same 64 MiB, the
/// blocks of the largest class, few large textures in view, always went
/// before blocks of small copies idle for minutes, after 1.5 s of rest;
/// and with no blocks, `VICTORY_TEXTURE_POOL=0`, the largest copy first
/// rather than the one unused longest. Round 2's review, items 4 and 10.)
pub(crate) fn plan(idle: &[Idle], lives: &[u32], block_bytes: u64) -> Vec<Unit> {
    let mut by_block: std::collections::HashMap<u32, Vec<&Idle>> = std::collections::HashMap::new();
    // (Each with its newest use.)
    let mut units: Vec<(u64, Unit)> = Vec::new();
    for i in idle {
        match i.block {
            Some(block) => by_block.entry(block).or_default().push(i),
            None if i.bytes > 0 => units.push((i.used, Unit { textures: vec![i.at], gives: i.bytes, block: None })),
            None => {}
        }
    }
    for (block, group) in by_block {
        if lives.get(block as usize).is_some_and(|&live| live as usize == group.len()) {
            let newest = group.iter().map(|i| i.used).max().unwrap_or(0);
            let textures = group.iter().map(|i| i.at).collect();
            units.push((newest, Unit { textures, gives: block_bytes, block: Some(block) }));
        }
    }
    units.sort_by(|(a_used, a), (b_used, b)| {
        let a_cost = a.textures.len() as u128 * u128::from(b.gives);
        let b_cost = b.textures.len() as u128 * u128::from(a.gives);
        a_used.cmp(b_used).then(a_cost.cmp(&b_cost)).then(a.textures.cmp(&b.textures))
    });
    units.into_iter().map(|(_, unit)| unit).collect()
}

/// Since the start: units of [`plan`] let go whole, their bytes given back
/// (blocks and textures of their own), and units begun that could not be
/// let go whole (a copy in them was in use by then).
pub(crate) static LET_GO: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// The counts of the last line, to say what changed since.
static LAST: std::sync::Mutex<[u64; 10]> = std::sync::Mutex::new([0; 10]);

/// The memory line: the system's budget and use, what of ours holds the
/// card's memory, and what the queue and the sweep did since the last line.
pub(crate) fn line() -> String {
    let mb = |bytes: u64| bytes >> 20;
    let memory = crate::host::host().and_then(|h| h.video_memory()).unwrap_or_default();
    let [budget, used, _, shared] = memory;
    let blocks = crate::pool::texture_census();
    let staging = crate::pool::staging_census();
    let giving_back = crate::pool::giving_back();
    let system = system_room();
    let counted = crate::resource::counted_in_use(used);
    let kept = crate::shadow::card_room_kept() + crate::resource::targets_room_kept();
    let host_bytes = crate::resource::HOST_TEXTURES[1].load(Relaxed);
    let kinds: [u64; 7] = std::array::from_fn(|i| crate::resource::KINDS[i].load(Relaxed));
    let (sides, before, windows) = crate::shadow::card_bytes();
    let [whole, gave, broken] = [0, 1, 2].map(|i| LET_GO[i].load(Relaxed));
    let [drained, emptied] = crate::pool::drained();
    let now = [
        kinds[2],
        kinds[3],
        drained,
        blocks.released,
        blocks.made,
        crate::draw::HEAP_TEXTURES_FULL.load(Relaxed),
        whole,
        gave,
        broken,
        emptied,
    ];
    let since = {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        let since: [u64; 10] = std::array::from_fn(|i| now[i].saturating_sub(last[i]));
        *last = now;
        since
    };
    let percent = |part: u64| if budget == 0 { 0.0 } else { part as f64 * 100.0 / budget as f64 };
    let classes = if crate::pool::classes() {
        format!(
            "with pieces of up to 64 KiB, up to 1 MiB and more {}, {} and {} (VICTORY_POOL_CLASSES=0: of any size together)",
            blocks.by_class[0], blocks.by_class[1], blocks.by_class[2]
        )
    } else {
        format!("{} with pieces of any size (VICTORY_POOL_CLASSES=0)", blocks.by_class.iter().sum::<u64>())
    };
    let (made_to, from, to) = crate::resource::lines(budget);
    format!(
        "card memory (VICTORY_SYSTEM_ROOM={}: {}): the system's budget {} MB, in use {} MB ({:.1} %), past the budget {} MB (what the \
         system may move to the PC's memory); the PC's memory in use by the card {} MB, of it {} MB of our staging blocks; counted in \
         use {} MB, with {} MB kept for targets and buffer copies to come, by the sweep less {} MB on its way back: textures picked \
         by index are made up to {} MB, idle ones let go from {} MB down to {} MB. Ours: {} blocks of textures {} MB, textures in {} pieces {} MB, free \
         in them {} MB, resting {} MB, {} empty, {} left to empty, {classes}; host copies the title fills {} alive {} MB, {} MB of \
         them outside blocks; other host textures (targets, depth, unordered access, twins) {} MB; buffer copies' card sides {} MB, \
         their bytes from before {} MB, windows' {} MB. Since the last line: {} copies let go ({} MB), {} blocks let go whole ({} MB \
         back), {} begun and left (a copy was used again), {} blocks left to empty ({} that did went back once empty), {} back to \
         the system in all, {} made; {} texture passes \
         refused for want of room ({} since the start)",
        u8::from(system),
        if system { "free bytes in blocks count as used" } else { "free bytes in blocks count as room" },
        mb(budget),
        mb(used),
        percent(used),
        mb(used.saturating_sub(budget)),
        mb(shared),
        mb(staging.blocks * crate::pool::BLOCK),
        mb(counted),
        mb(kept),
        mb(giving_back),
        mb(made_to),
        mb(from),
        mb(to),
        blocks.blocks,
        mb(blocks.blocks * crate::pool::BLOCK),
        blocks.pieces,
        mb(blocks.held),
        mb(blocks.free),
        mb(blocks.resting),
        blocks.empty,
        blocks.draining,
        kinds[0],
        mb(kinds[1]),
        mb(kinds[1].saturating_sub(blocks.held)),
        mb(host_bytes.saturating_sub(kinds[1])),
        mb(sides),
        mb(before),
        mb(windows),
        since[0],
        mb(since[1]),
        since[6],
        mb(since[7]),
        since[8],
        since[2],
        since[9],
        since[3],
        since[4],
        since[5],
        now[5],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_system_counts_free_bytes_in_blocks_as_used() {
        // Run nbc-measure's end: 14,081 MB in use of 14,580, 1,769 MB free
        // in blocks. As before that was 12,312 MB, under nine tenths.
        let mb = |n: u64| n << 20;
        assert_eq!(queue_count(mb(14_081), mb(1_769), 0, false), mb(12_312));
        assert_eq!(queue_count(mb(14_081), mb(1_769), 0, true), mb(14_081));
        assert_eq!(queue_count(mb(14_081), mb(1_769), mb(800), true), mb(14_881));
        assert!(queue_count(mb(14_081), mb(1_769), 0, false) < mb(14_580) / 100 * 90);
        assert!(queue_count(mb(14_081), mb(1_769), 0, true) > mb(14_580) / 100 * 90);
    }

    /// Blocks on their way back to the system are no room for the queue
    /// until they are back, and the sweep counts them as gone. At 96.6 % of
    /// the budget with 1.7 GB of blocks emptying, the queue sees 96.6 %, past
    /// its line of nine tenths, and the sweep 85 %, so it lets no more go for
    /// them. (The queue saw 85 % too and made new textures, which the blocks
    /// emptying would not take: past the budget until those were back.
    /// Round 2's review, item 2.)
    #[test]
    fn memory_on_its_way_back_is_no_room_yet() {
        let mb = |n: u64| n << 20;
        let budget = mb(14_580);
        let used = budget / 1000 * 966;
        let back = mb(1_700);
        let queue = queue_count(used, 0, 0, true);
        assert_eq!(queue, used);
        assert!(queue > budget / 100 * 90, "no room for the queue");
        let sweep = sweep_count(queue, back, true);
        assert!(sweep < budget / 100 * 86, "{} MB for the sweep", sweep >> 20);
        // As before: nothing on its way back.
        assert_eq!(sweep_count(queue, back, false), queue);
    }

    /// The line is made from counters alone, with or without a card, and
    /// says what changed since the last one.
    #[test]
    fn the_memory_line_says_what_holds_the_card() {
        let line = line();
        println!("{line}");
        assert!(line.starts_with("card memory (VICTORY_SYSTEM_ROOM=1: free bytes in blocks count as used): the system's budget"), "{line}");
        for part in ["blocks of textures", "left to empty", "texture passes refused for want of room", "back to the system"] {
            assert!(line.contains(part), "{part}: {line}");
        }
    }

    const BLOCK: u64 = 64 << 20;

    fn idle(at: usize, block: Option<u32>, bytes: u64, used: u64) -> Idle {
        Idle { at, block, bytes, used }
    }

    /// Only whole blocks and copies of their own go, the one whose newest use
    /// is the oldest first; a block with a copy in use is left alone.
    #[test]
    fn the_sweep_lets_whole_blocks_go_and_leaves_blocks_in_use() {
        let lives = [3, 2, 4];
        let idle = [
            idle(10, Some(0), 64 << 10, 5),
            idle(11, Some(0), 64 << 10, 7),
            idle(12, Some(0), 64 << 10, 6),
            idle(20, Some(1), 1 << 20, 9),
            idle(21, Some(1), 1 << 20, 8),
            // Block 2 holds four, one of them in use: not idle.
            idle(30, Some(2), 64 << 10, 1),
            idle(31, Some(2), 64 << 10, 1),
            idle(32, Some(2), 64 << 10, 1),
            idle(40, None, 64 << 20, 3),
            idle(41, None, 16 << 20, 2),
        ];
        let plan = plan(&idle, &lives, BLOCK);
        let order: Vec<(Vec<usize>, u64, Option<u32>)> = plan.into_iter().map(|u| (u.textures, u.gives, u.block)).collect();
        assert_eq!(
            order,
            vec![
                (vec![41], 16 << 20, None),
                (vec![40], 64 << 20, None),
                (vec![10, 11, 12], BLOCK, Some(0)),
                (vec![20, 21], BLOCK, Some(1)),
            ]
        );
    }

    /// A block of small copies unused for minutes goes before a block of a
    /// few large textures that rested a second and a half: by their newest
    /// use, whatever their count. Of two used last at once, the one of fewer
    /// copies first. (By the count first, the large textures in view went
    /// and were uploaded again over and over. Round 2's review, item 4.)
    #[test]
    fn the_block_unused_longest_goes_first_whatever_its_count() {
        let lives = [1000, 2, 3];
        let mut idle_ones: Vec<Idle> = (0..1000).map(|n| idle(n, Some(0), 64 << 10, 100)).collect();
        idle_ones.extend([idle(5000, Some(1), 32 << 20, 90_000), idle(5001, Some(1), 32 << 20, 90_000)]);
        idle_ones.extend([idle(6000, Some(2), 4 << 20, 90_000), idle(6001, Some(2), 4 << 20, 90_000), idle(6002, Some(2), 4 << 20, 1)]);
        let plan = plan(&idle_ones, &lives, BLOCK);
        assert_eq!(plan.iter().map(|u| u.block).collect::<Vec<_>>(), vec![Some(0), Some(1), Some(2)]);
    }

    /// Of blocks that cost the same, the one unused the longest first.
    #[test]
    fn of_equal_blocks_the_one_unused_longest_goes_first() {
        let lives = [1, 1];
        let idle = [idle(1, Some(0), 64 << 10, 50), idle(2, Some(1), 64 << 10, 40)];
        let plan = plan(&idle, &lives, BLOCK);
        assert_eq!(plan.iter().map(|u| u.block).collect::<Vec<_>>(), vec![Some(1), Some(0)]);
    }

    /// The sweep's first look asks only the idle copies whose going may give
    /// memory back: copies of their own, and those of blocks all of whose
    /// pieces are idle copies; not those of a block with a piece in use, nor
    /// textures with no copy. (Every idle copy was asked its lock and
    /// references. Round 2's review, item 7.)
    #[test]
    fn the_sweep_asks_only_copies_whose_going_gives_memory_back() {
        // Block 0: two pieces, both idle; block 1: three, two idle; block 2:
        // one idle of none live (a stale number).
        let lives = [2, 3, 0];
        let blocks = [0, 1, OWN, 0, 1, NO_COPY, 2, 7];
        assert_eq!(worth_asking(&blocks, &lives), vec![true, false, true, true, false, false, false, false]);
        assert_eq!(worth_asking(&[], &lives), Vec::<bool>::new());
    }
}
