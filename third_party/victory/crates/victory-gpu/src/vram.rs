//! The card's memory as the system counts it.
//!
//! The system counts a block of textures ([`crate::pool`]) as used from
//! the moment it is made to the moment it goes back, whatever lies free
//! inside it. Counting those free bytes as room, the texture queue saw 89 %
//! of the budget where the system saw 100 % (audit vram V2), and kept
//! making textures; and the sweep let textures go from all over the
//! blocks, which freed pieces of them and none of the card's memory.

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

/// What the card's memory in use comes to for the texture queue's line
/// and the sweep: what the system says is in use (`used`), less what is on
/// its way back to it (`giving_back`, blocks the sweep emptied). As before
/// ([`system_room`] off): less the bytes free in texture blocks
/// (`free_in_blocks`) too, which the system counts as used.
pub(crate) fn counted(used: u64, free_in_blocks: u64, giving_back: u64, system: bool) -> u64 {
    if system { used.saturating_sub(giving_back) } else { used.saturating_sub(free_in_blocks) }
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
/// back, and copies with memory of their own; the fewest copies for a byte
/// given back first, the longest unused first among equals. Copies in a
/// block that holds one in use stay: their going would give nothing back
/// (the pieces would be free inside a block the system still counts).
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
        a_cost.cmp(&b_cost).then(a_used.cmp(b_used)).then(a.textures.cmp(&b.textures))
    });
    units.into_iter().map(|(_, unit)| unit).collect()
}

/// Since the start: units of [`plan`] let go whole, their bytes given back
/// (blocks and textures of their own), and units begun that could not be
/// let go whole (a copy in them was in use by then).
pub(crate) static LET_GO: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_system_counts_free_bytes_in_blocks_as_used() {
        // Run nbc-measure's end: 14,081 MB in use of 14,580, 1,769 MB free
        // in blocks. As before that was 12,312 MB, under nine tenths.
        let mb = |n: u64| n << 20;
        assert_eq!(counted(mb(14_081), mb(1_769), 0, false), mb(12_312));
        assert_eq!(counted(mb(14_081), mb(1_769), 0, true), mb(14_081));
        assert_eq!(counted(mb(14_081), mb(1_769), mb(128), true), mb(13_953));
        assert!(counted(mb(14_081), mb(1_769), 0, false) < mb(14_580) / 100 * 90);
        assert!(counted(mb(14_081), mb(1_769), 0, true) > mb(14_580) / 100 * 90);
    }

    const BLOCK: u64 = 64 << 20;

    fn idle(at: usize, block: Option<u32>, bytes: u64, used: u64) -> Idle {
        Idle { at, block, bytes, used }
    }

    /// Only whole blocks and copies of their own go, the cheapest first: a
    /// block of two copies before one of three, a copy of its own of 64 MiB
    /// with them; a block with a copy in use is left alone.
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
                (vec![40], 64 << 20, None),
                (vec![20, 21], BLOCK, Some(1)),
                (vec![10, 11, 12], BLOCK, Some(0)),
                (vec![41], 16 << 20, None),
            ]
        );
    }

    /// Of blocks that cost the same, the one unused the longest first.
    #[test]
    fn of_equal_blocks_the_one_unused_longest_goes_first() {
        let lives = [1, 1];
        let idle = [idle(1, Some(0), 64 << 10, 50), idle(2, Some(1), 64 << 10, 40)];
        let plan = plan(&idle, &lives, BLOCK);
        assert_eq!(plan.iter().map(|u| u.block).collect::<Vec<_>>(), vec![Some(1), Some(0)]);
    }
}
