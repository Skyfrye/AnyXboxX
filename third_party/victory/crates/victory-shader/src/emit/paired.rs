//! The paired form of console wave64 compute programs on hosts of 32-lane waves
//! ([`with_paired_lanes`]): each host thread runs two console lanes, the lower and the upper
//! one, and what a lane runs is lowered once for each. A child module of the lowering, so it
//! shares the emitter's state.

use super::*;

/// Runs `f` with console wave64 compute programs on 32-lane hosts lowered in the paired form:
/// each console wave is one host wave of 32 lanes, and host lane n runs console lanes n and
/// n + 32. The group has half the console's threads; each thread runs what a lane does twice,
/// once for each of its lanes, in the order of the console's instructions, so the wave's lanes
/// stay together as on the console: what one writes to LDS, the next instruction of another
/// reads (a fence between the accesses keeps the host compiler from reordering them, as
/// [`Lower::lds_fence`] does for a wave the host runs whole; no thread waits for another).
/// Cross-lane operations reach any of the 64 lanes through the host wave's own operations on
/// either half (lane n + 32 is lane n's upper half), and a lane mask is the two ballots. The
/// caller makes every per-lane branch one the wave takes together, as the console does
/// (`structure::structure_wide`): the wave goes where some lane has to, and EXEC stays each
/// lane's guard. So control flow is the same in every thread of a host wave, and the
/// program's barriers are reached as on the console.
// shortcut: D3D12 orders no groupshared accesses of two threads without a group barrier; the
// host is taken to run a wave's threads in step where they all go the same way (as for the
// console's 32-lane programs that share LDS within a wave), the fences only keeping its
// compiler from moving one thread's accesses past each other.
pub(in crate::emit) fn with_paired_lanes<R>(f: impl FnOnce() -> R) -> R {
    let was = PAIRED.replace(true);
    let r = f();
    PAIRED.set(was);
    r
}

/// What the lowering keeps per console lane, for the paired form ([`with_paired_lanes`]): each
/// host thread runs two lanes of one console wave, and what a lane runs is lowered once for
/// each with this state exchanged between them ([`Lower::swap_lanes`]): the values (scalar ones
/// too, the same in both), the lane booleans and their joins, the handles of descriptors a lane
/// may have its own of, the phis still to fill, and whether the console launched the lane (a
/// partial final group, [`Lower::partial_active`]).
#[derive(Default)]
pub(super) struct LaneState {
    pub(super) vals: HashMap<Value, Val>,
    pub(super) floats: HashMap<Value, V>,
    pub(super) bools: HashMap<(usize, Bool), V>,
    pub(super) lane_bits: HashMap<Value, V>,
    pub(super) lane_fail: HashMap<Value, Why>,
    pub(super) mask_phis: HashMap<Value, V>,
    pub(super) pending_masks: Vec<(V, usize, usize)>,
    pub(super) data_phis: Vec<(V, usize, usize)>,
    pub(super) handles: HashMap<(usize, Site, u8, bool, (u32, u32)), V>,
    pub(super) strides: HashMap<(usize, Site, u8), V>,
    pub(super) clane: Option<V>,
    pub(super) partial_active: Option<V>,
}

/// The paired form: scalar values each console lane has its own of in the lowering, a waterfall
/// loop's first-lane reads (the lane's own dword, `resource::Waterfall`) and what scalar code
/// and joins make of them.
pub(super) fn lane_scalars(p: &Program, res: &Resources) -> HashSet<Value> {
    let mut own: HashSet<Value> = res.waterfall.reads.iter().copied().collect();
    if own.is_empty() {
        return own;
    }
    let order = crate::ir::reverse_postorder(&p.blocks);
    loop {
        let before = own.len();
        for &b in &order {
            let block = &p.blocks[b];
            for phi in &block.phis {
                if !matches!(phi.reg, Reg::V(_)) && phi.args.iter().any(|a| own.contains(a)) {
                    own.insert(phi.value);
                }
            }
            for inst in &block.insts {
                if matches!(inst.fields, Fields::Sop { .. } | Fields::Smem { .. })
                    && inst.reads.iter().any(|r| own.contains(&r.1))
                {
                    own.extend(inst.writes.iter().map(|w| w.1));
                }
            }
        }
        if own.len() == before {
            return own;
        }
    }
}

impl<'a> Lower<'a> {
    /// The paired form: makes the other console lane of this thread the one being lowered.
    pub(super) fn swap_lanes(&mut self) {
        let Some(t) = self.twin.as_deref_mut() else {
            return;
        };
        std::mem::swap(&mut self.vals, &mut t.vals);
        std::mem::swap(&mut self.floats, &mut t.floats);
        std::mem::swap(&mut self.bools, &mut t.bools);
        std::mem::swap(&mut self.lane_bits, &mut t.lane_bits);
        std::mem::swap(&mut self.lane_fail, &mut t.lane_fail);
        std::mem::swap(&mut self.mask_phis, &mut t.mask_phis);
        std::mem::swap(&mut self.pending_masks, &mut t.pending_masks);
        std::mem::swap(&mut self.data_phis, &mut t.data_phis);
        std::mem::swap(&mut self.handles, &mut t.handles);
        std::mem::swap(&mut self.strides, &mut t.strides);
        std::mem::swap(&mut self.clane, &mut t.clane);
        std::mem::swap(&mut self.partial_active, &mut t.partial_active);
        self.upper = !self.upper;
    }

    /// The paired form: `f` for each console lane of this thread, the lower one first; the lane
    /// being lowered stays the same.
    pub(super) fn both<T>(&mut self, mut f: impl FnMut(&mut Self) -> Result<T, Why>) -> Result<[T; 2], Why> {
        let was = self.upper;
        if was {
            self.swap_lanes();
        }
        let lo = f(self);
        self.swap_lanes();
        let hi = f(self);
        if !was {
            self.swap_lanes();
        }
        Ok([lo?, hi?])
    }

    /// The paired form: `lo` as the lower lane's value of `v`, `hi` as the upper lane's.
    pub(super) fn put_lanes(&mut self, v: Value, lo: Val, hi: Val) {
        let (mine, other) = if self.upper { (hi, lo) } else { (lo, hi) };
        self.vals.insert(v, mine);
        if let Some(t) = self.twin.as_deref_mut() {
            t.vals.insert(v, other);
        }
    }

    /// The console's thread group, whatever the host's is (the paired form's has half its
    /// threads).
    pub(super) fn console_group(&self) -> Option<[u32; 3]> {
        match self.x.native.io {
            StageIo::Cs(cs) => Some(cs.threads),
            _ => self.s.threads,
        }
    }

    /// [`Self::section_inst`] in the paired form: a value over the wave's lanes takes both
    /// lanes of every thread ([`Self::paired_shape`]), a lane's own value each lane's.
    pub(super) fn paired_section_inst(&mut self, b: usize, i: usize, start: usize) {
        const UNKNOWN: Why = "a whole-wave section step the simulation does not follow";
        let inst = &self.x.p.blocks[b].insts[i];
        for &(_, w) in &inst.writes {
            let shape = match self.section_out.contains(&w) {
                true => self.sections[&b][&start].shapes.get(&w).copied(),
                false => None,
            };
            let made = match shape {
                None => Err(UNKNOWN),
                Some(sh) => self.paired_value(sh),
            };
            match made {
                Ok([lo, hi]) => self.put_lanes(w, Val::I32(lo), Val::I32(hi)),
                Err(why) => self.put_lanes(w, Val::Missing(why), Val::Missing(why)),
            }
        }
    }

    /// A section's value ([`super::super::wave::Shape`]) in the paired form, for the lower and the
    /// upper lane of this thread.
    pub(super) fn paired_value(&mut self, shape: super::super::wave::Shape) -> Result<[V; 2], Why> {
        use super::super::wave::Shape;
        match shape {
            Shape::Active(..) | Shape::Prefix(..) | Shape::ActiveF64(..) => self.paired_shape(shape),
            sh => self.both(|l| l.shape(sh, false)),
        }
    }

    /// [`Self::shape`] of a reduction or prefix in the paired form, over the console wave's 64
    /// lanes: for the lower and the upper lane of this thread. Each half is a host wave
    /// operation over the lower or the upper lanes, and the halves join as two host waves' do
    /// in `shape`: a reduction of the two results, a prefix of the upper lanes after the lower
    /// lanes' total.
    pub(super) fn paired_shape(&mut self, shape: super::super::wave::Shape) -> Result<[V; 2], Why> {
        use super::super::wave::{Op, Shape};
        let (op, src, mask, ident, prefix) = match shape {
            Shape::ActiveF64(lo, hi, mask, high) => return self.paired_f64_sum(lo, hi, mask, high),
            Shape::Active(op, src, mask, ident) => (op, src, mask, ident, None),
            Shape::Prefix(op, src, mask, ident, inclusive) => (op, src, mask, ident, Some(inclusive)),
            _ => return Err("a whole-wave section value of another shape"),
        };
        let &m = self.x.st.masks.get(&mask).ok_or("a wave reduction's untracked mask")?;
        let c = self.cur;
        let [lo, hi] = self.both(|l| {
            let on = l.boolean(m, c)?;
            let on = l.launched(c, on, mask);
            let x = l.i32_of(src)?;
            let id = l.s.m.i32(ident);
            Ok(l.s.m.select(c, on, x, id))
        })?;
        self.s.flags |= flags::WAVE_OPS;
        if op == Op::AddH {
            if prefix.is_some() {
                return Err("a wave prefix of an operation other than a sum");
            }
            let (lo, hi) = (self.half_tree(lo), self.half_tree(hi));
            let r = self.join_half_sums(lo, hi);
            return Ok([r, r]);
        }
        let float = matches!(op, Op::AddF | Op::MinF | Op::MaxF);
        let (lo, hi) = if float { (self.as_float(lo), self.as_float(hi)) } else { (lo, hi) };
        let r = match prefix {
            Some(inclusive) => {
                let mine = self.host_wave_op(op, lo, float, Some(inclusive))?;
                let theirs = self.host_wave_op(op, hi, float, Some(inclusive))?;
                let below = self.host_wave_op(op, lo, float, None)?;
                [mine, self.s.m.bin(c, Bin::Add, theirs, below)]
            }
            None => {
                let lo = self.host_wave_op(op, lo, float, None)?;
                let hi = self.host_wave_op(op, hi, float, None)?;
                let r = self.join_halves(op, lo, hi, float);
                [r, r]
            }
        };
        Ok(if float { r.map(|x| self.as_int(x)) } else { r })
    }

    /// `op` of `arg` (`float`: a float, else bits) over this host wave's active lanes; with
    /// `prefix` the sum over the lanes below each lane (`Some(true)`: and the lane's own).
    pub(super) fn host_wave_op(&mut self, op: super::super::wave::Op, arg: V, float: bool, prefix: Option<bool>) -> Result<V, Why> {
        use super::super::wave::Op;
        let c = self.cur;
        let (i32t, f32t) = (self.s.t.i32, self.s.t.f32);
        let ty = if float { f32t } else { i32t };
        let sfx = if float { "f32" } else { "i32" };
        Ok(match (op, prefix) {
            (Op::AddU | Op::AddF, Some(inclusive)) => {
                let (k0, k1) = (self.s.m.i8(0), self.s.m.i8(1));
                let r = self.s.op(c, &format!("wavePrefixOp.{sfx}"), Attr::NoUnwind, ty, 121, &[arg, k0, k1]);
                if inclusive { self.s.m.bin(c, Bin::Add, r, arg) } else { r }
            }
            (_, Some(_)) => return Err("a wave prefix of an operation other than a sum"),
            (Op::Or | Op::And | Op::Xor, None) => {
                let k = self.s.m.i8(match op {
                    Op::And => 0,
                    Op::Or => 1,
                    _ => 2,
                });
                self.s.op(c, "waveActiveBit.i32", Attr::NoUnwind, i32t, 120, &[arg, k])
            }
            (_, None) => {
                let (code, unsigned) = match op {
                    Op::AddU => (0, 1),
                    Op::AddF => (0, 0),
                    Op::MinU => (2, 1),
                    Op::MaxU => (3, 1),
                    Op::MinI | Op::MinF => (2, 0),
                    _ => (3, 0),
                };
                let (k, u) = (self.s.m.i8(code), self.s.m.i8(unsigned));
                self.s.op(c, &format!("waveActiveOp.{sfx}"), Attr::NoUnwind, ty, 119, &[arg, k, u])
            }
        })
    }

    /// The results of a reduction by `op` (`float`: floats, else bits) over the two halves of
    /// a console wave, joined into the wave's.
    pub(super) fn join_halves(&mut self, op: super::super::wave::Op, lo: V, hi: V, float: bool) -> V {
        use super::super::wave::Op;
        let c = self.cur;
        let ty = if float { self.s.t.f32 } else { self.s.t.i32 };
        let sfx = if float { "f32" } else { "i32" };
        match op {
            Op::AddU | Op::AddF => self.s.m.bin(c, Bin::Add, lo, hi),
            Op::Or => self.s.m.bin(c, Bin::Or, lo, hi),
            Op::And => self.s.m.bin(c, Bin::And, lo, hi),
            Op::Xor => self.s.m.bin(c, Bin::Xor, lo, hi),
            _ => {
                // FMax 35, FMin 36, IMax 37, IMin 38, UMax 39, UMin 40
                let code = match op {
                    Op::MaxF => 35,
                    Op::MinF => 36,
                    Op::MinU => 40,
                    Op::MaxU => 39,
                    Op::MinI => 38,
                    _ => 37,
                };
                self.s.op(c, &format!("binary.{sfx}"), Attr::ReadNone, ty, code, &[lo, hi])
            }
        }
    }

    /// The first five levels of [`Self::half_sum`]'s tree over this host wave of 32, every
    /// lane of it here (the paired form): its lanes' sum in the low half, lane 0's high half.
    pub(super) fn half_tree(&mut self, x: V) -> V {
        let c = self.cur;
        let i32t = self.s.t.i32;
        let lane = self.s.op(c, "waveGetLaneIndex", Attr::ReadOnly, i32t, 111, &[]);
        let mut sum = x;
        for step in 0..5 {
            let bit = self.s.m.i32(1 << step);
            let partner = self.s.m.bin(c, Bin::Xor, lane, bit);
            let theirs = self.read_lane(sum, partner);
            sum = self.add_halves(sum, theirs);
        }
        let (high, low, zero) = (self.s.m.i32(0xFFFF_0000), self.s.m.i32(0xFFFF), self.s.m.i32(0));
        let first = self.read_lane(x, zero);
        let first = self.s.m.bin(c, Bin::And, first, high);
        let sum = self.s.m.bin(c, Bin::And, sum, low);
        self.s.m.bin(c, Bin::Or, first, sum)
    }

    /// The last addition of [`Self::half_sum`]'s tree: the sums of a console wave's two halves
    /// (from [`Self::half_tree`]), with the high half of the lower one's (lane 0's).
    pub(super) fn join_half_sums(&mut self, lo: V, hi: V) -> V {
        let c = self.cur;
        let total = self.add_halves(lo, hi);
        let (high, low) = (self.s.m.i32(0xFFFF_0000), self.s.m.i32(0xFFFF));
        let total = self.s.m.bin(c, Bin::And, total, low);
        let first = self.s.m.bin(c, Bin::And, lo, high);
        self.s.m.bin(c, Bin::Or, first, total)
    }

    /// [`Self::active_f64_sum`] in the paired form: the lower lanes' sum, then the upper lanes'
    /// added, as two host waves add theirs; dword `high` of it, for both lanes of this thread.
    pub(super) fn paired_f64_sum(&mut self, lo: Value, hi: Value, mask: Value, high: bool) -> Result<[V; 2], Why> {
        let &m = self.x.st.masks.get(&mask).ok_or("a wave reduction's untracked mask")?;
        let c = self.cur;
        let f64t = self.s.m.double();
        let [d0, d1] = self.both(|l| {
            let on = l.boolean(m, c)?;
            let on = l.launched(c, on, mask);
            let (dl, dh) = (l.i32_of(lo)?, l.i32_of(hi)?);
            let z = l.s.m.i32(0);
            let dl = l.s.m.select(c, on, dl, z);
            let dh = l.s.m.select(c, on, dh, z);
            Ok(l.s.op(c, "makeDouble.f64", Attr::ReadNone, f64t, 101, &[dl, dh]))
        })?;
        self.s.flags |= flags::WAVE_OPS | flags::DOUBLES;
        let sum = |l: &mut Self, d: V| {
            let (k0, k0b) = (l.s.m.i8(0), l.s.m.i8(0));
            l.s.op(c, "waveActiveOp.f64", Attr::NoUnwind, f64t, 119, &[d, k0, k0b])
        };
        let (s0, s1) = (sum(self, d0), sum(self, d1));
        let total = self.s.m.bin(c, Bin::Add, s0, s1);
        let i32t = self.s.t.i32;
        let split_ty = self.s.m.named_struct("dx.types.splitdouble", vec![i32t, i32t]);
        let split = self.s.op(c, "splitDouble.f64", Attr::ReadNone, split_ty, 102, &[total]);
        let x = self.s.m.extract(c, split, u32::from(high));
        Ok([x, x])
    }

    /// Instruction `i` of IR block `b` in the paired form ([`with_paired_lanes`]), for both
    /// console lanes of the thread: what a lane does is lowered for the lower lane, then for
    /// the upper one, which keeps the console's order of the wave's instructions; scalar
    /// control (the title's barrier) and scalar memory writes once; a read of another lane
    /// once with what both lanes hold ([`Self::paired_cross`]). LDS accesses are ordered by
    /// [`Self::lds_fence`]: what one lane wrote (or still read) does not move past another
    /// lane's access the console made after it.
    pub(super) fn paired_inst(&mut self, b: usize, i: usize) -> Result<(), Stop> {
        let inst: &'a Inst = &self.x.p.blocks[b].insts[i];
        let name = inst.name();
        let once = matches!(inst.fields, Fields::Sopp { .. })
            || matches!(inst.fields, Fields::Smem { .. }) && (name.contains("store") || name.contains("atomic"));
        if once {
            self.inst(b, i)?;
            self.copy_to_twin(inst);
            return Ok(());
        }
        // One lane of a VGPR a whole-wave section left: the wave's value, both lanes'.
        if name == "v_readlane_b32"
            && let Some(&(_, w)) = inst.writes.first()
            && let Some(sh) = self.sections.get(&b).and_then(|s| {
                s.iter().filter(|s| s.1.end < i).find_map(|s| s.1.reads.get(&w).copied())
            })
        {
            match self.paired_value(sh) {
                Ok([lo, hi]) => self.put_lanes(w, Val::I32(lo), Val::I32(hi)),
                Err(why) => self.put_lanes(w, Val::Missing(why), Val::Missing(why)),
            }
            return Ok(());
        }
        let cross = match name {
            // (not a lane a uniform scalar was written to, whose value `Self::valu` knows)
            _ if self.x.res.restored_lanes.contains_key(&(b, i)) => false,
            "v_readlane_b32" => true,
            // (not a waterfall loop's read, the lane's own dword: `Self::valu`)
            "v_readfirstlane_b32" => {
                !inst.writes.first().is_some_and(|w| self.x.res.waterfall.reads.contains(&w.1))
            }
            _ => false,
        };
        if cross {
            if let Err(why) = self.paired_cross(inst, b, i) {
                for &(_, w) in &inst.writes {
                    self.put_lanes(w, Val::Missing(why), Val::Missing(why));
                }
            }
            return Ok(());
        }
        self.inst(b, i)?;
        self.swap_lanes();
        let r = self.inst(b, i);
        self.swap_lanes();
        r
    }

    /// The paired form: what `inst` wrote in the lane being lowered, for the other lane too (a
    /// scalar result, the same in both).
    pub(super) fn copy_to_twin(&mut self, inst: &Inst) {
        let Some(t) = self.twin.as_deref_mut() else {
            return;
        };
        for &(_, w) in &inst.writes {
            if let Some(&v) = self.vals.get(&w) {
                t.vals.insert(w, v);
            }
        }
    }

    /// The paired form: an instruction that reads another lane of the console wave, any of its
    /// 64: `v_readlane_b32` and `v_readfirstlane_b32` (scalar results, the same in both lanes of
    /// the thread). Lane n and lane n + 32 are the lower and the upper lane of host lane n.
    pub(super) fn paired_cross(&mut self, inst: &Inst, b: usize, i: usize) -> Result<(), Why> {
        self.cross_lane(false)?;
        let c = self.cur;
        let (k31, k32, z) = (self.s.m.i32(31), self.s.m.i32(32), self.s.m.i32(0));
        if inst.name() == "v_readlane_b32" {
            // the lane: a scalar or a constant, its low six bits (wave64)
            let lane = self.src(inst, 1)?;
            let [lo, hi] = self.both(|l| l.src(inst, 0))?;
            let host = self.s.m.bin(c, Bin::And, lane, k31);
            let upper = self.s.m.bin(c, Bin::And, lane, k32);
            let upper = self.s.m.cmp(c, pred::NE, upper, z);
            let (lo, hi) = (self.read_lane(lo, host), self.read_lane(hi, host));
            let r = self.s.m.select(c, upper, hi, lo);
            self.write_i(inst, r, None)?;
        } else {
            let [(lo, in_lo), (hi, in_hi)] = self.both(|l| {
                let x = l.src(inst, 0)?;
                // (a lane a partial final group keeps for its barriers is not in EXEC)
                let guard = l.memory_guard(b, i)?;
                Ok(l.first_lane(x, guard))
            })?;
            // the first lane in EXEC is a lower one if there is one; with none, lane 0
            let hi = self.s.m.select(c, in_hi, hi, lo);
            let r = self.s.m.select(c, in_lo, lo, hi);
            self.write_i(inst, r, None)?;
        }
        self.copy_to_twin(inst);
        Ok(())
    }

    /// [`Self::scc_some`] in the paired form, for lane mask `m` (SCC `scc`): the host wave
    /// asks both lanes of each thread.
    pub(super) fn paired_scc_some(&mut self, scc: Value, m: Bool) -> Result<V, Why> {
        let at = match self.def_block.get(&scc).copied() {
            None => self.s.entry,
            Some(b) if b == self.cur_ir => self.cur,
            Some(b) => self.tail[b].ok_or("a lane mask written after its use")?,
        };
        let [lo, hi] = self.both(|l| {
            let bit = l.boolean(m, at)?;
            // (the lanes a partial final group keeps for its barriers have no bit)
            Ok(match l.partial_active {
                Some(a) => l.s.m.bin(at, Bin::And, bit, a),
                None => bit,
            })
        })?;
        let either = self.s.m.bin(at, Bin::Or, lo, hi);
        self.s.flags |= flags::WAVE_OPS;
        Ok(self.s.op(at, "waveAnyTrue", Attr::NoUnwind, self.s.t.i1, 113, &[either]))
    }

    /// [`Self::wave_wide`] in the paired form (`any`: some lane, else every lane): the host
    /// wave is the console's, each thread with two of its lanes; every lane is here (control
    /// flow is the wave's), and a lane outside EXEC says what it says on the console (no set
    /// bit for "some lane", no objection to "every lane"), as does a lane the console did not
    /// launch.
    pub(super) fn paired_wave_wide(&mut self, any: bool, when: Bool) -> Result<V, Why> {
        let c = self.cur;
        let [lo, hi] = self.both(|l| {
            let bit = l.boolean(when, c)?;
            Ok(match l.partial_active {
                Some(a) if any => l.s.m.bin(c, Bin::And, bit, a),
                Some(a) => {
                    let t = l.s.m.bool(true);
                    let out = l.s.m.bin(c, Bin::Xor, a, t);
                    l.s.m.bin(c, Bin::Or, bit, out)
                }
                None => bit,
            })
        })?;
        let pair = self.s.m.bin(c, if any { Bin::Or } else { Bin::And }, lo, hi);
        self.s.flags |= flags::WAVE_OPS;
        let (name, code) = if any { ("waveAnyTrue", 113) } else { ("waveAllTrue", 114) };
        Ok(self.s.op(c, name, Attr::NoUnwind, self.s.t.i1, code, &[pair]))
    }
}
