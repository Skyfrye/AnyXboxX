//! The f16 results of 16-bit float operations, d16 reads and f16 interpolation: rounded to
//! nearest as the console rounds them ([`f16_nearest_on`]), from the bits on hosts of 32-lane
//! waves. A child module of the lowering, so it shares the emitter's state.

use super::*;

impl Lower<'_> {
    /// `f` as the f16 result of a 16-bit float operation: rounded to nearest
    /// ([`Self::f16_nearest`]) unless `VICTORY_F16_NEAREST=0` ([`f16_nearest_on`]).
    pub(super) fn f16_result(&mut self, f: V) -> V {
        if f16_nearest_on() {
            self.f16_nearest(f)
        } else {
            let i32t = self.s.t.i32;
            self.s.op(self.cur, "legacyF32ToF16", Attr::ReadNone, i32t, 130, &[f])
        }
    }

    /// `f` as an f16 in the low bits, rounded toward zero: the nearest-even
    /// conversion, one step back toward zero where it rounded away (which
    /// also turns an overflow to infinity into the largest finite half).
    pub(super) fn f16_toward_zero(&mut self, f: V) -> V {
        let c = self.cur;
        let i32t = self.s.t.i32;
        let h = self
            .s
            .op(c, "legacyF32ToF16", Attr::ReadNone, i32t, 130, &[f]);
        let back = self
            .s
            .op(c, "legacyF16ToF32", Attr::ReadNone, self.s.t.f32, 131, &[h]);
        let (fa, ba) = (self.unary(6, f), self.unary(6, back));
        // Ordered: never for NaN.
        let away = self.s.m.cmp(c, pred::OGT, ba, fa);
        let one = self.s.m.i32(1);
        let stepped = self.s.m.bin(c, Bin::Sub, h, one);
        self.s.m.select(c, away, stepped, h)
    }

    /// `f` as an f16 in the low bits, rounded to nearest, ties to even, as the console's
    /// 16-bit float operations round, whatever the host's own conversion does: D3D lets
    /// `f32tof16` round toward zero, and the NVIDIA driver's does (measured on an RTX 4070:
    /// `wave64_tests`). The conversion toward zero, or the next half away from zero where
    /// that one is nearer, or as near and the first one odd. Past the largest half the next
    /// one counts as 2^16, the bound IEEE rounds by, so 65520 and up become infinity.
    /// (`f` is an f32 result of two halves here; rounding it again to 16 bits gives the
    /// rounding of the exact result, 24 bits being more than twice 11 and two.)
    pub(super) fn f16_nearest(&mut self, f: V) -> V {
        // (on 32-lane hosts the same rounding from the bits, at a third of the cost)
        if host_max_wave() == 32 {
            return self.f16_nearest_bits(f);
        }
        let c = self.cur;
        let f32t = self.s.t.f32;
        let lo = self.f16_toward_zero(f);
        let (one, zero) = (self.s.m.i32(1), self.s.m.i32(0));
        let hi = self.s.m.bin(c, Bin::Add, lo, one);
        let size = |l: &mut Self, h: V| {
            let w = l.s.op(c, "legacyF16ToF32", Attr::ReadNone, f32t, 131, &[h]);
            l.unary(6, w)
        };
        let (below, above) = (size(self, lo), size(self, hi));
        let (magnitude, infinity) = (self.s.m.i32(0x7FFF), self.s.m.i32(0x7C00));
        let m = self.s.m.bin(c, Bin::And, hi, magnitude);
        let top = self.s.m.cmp(c, pred::EQ, m, infinity);
        let bound = self.s.m.f32(65536.0f32.to_bits());
        let above = self.s.m.select(c, top, bound, above);
        let fa = self.unary(6, f);
        // (both differences are exact: the three values lie within one half's step)
        let under = self.s.m.bin(c, Bin::Sub, fa, below);
        let over = self.s.m.bin(c, Bin::Sub, above, fa);
        // Ordered: never for NaN, which stays what the conversion made of it.
        let nearer = self.s.m.cmp(c, pred::OLT, over, under);
        let tie = self.s.m.cmp(c, pred::OEQ, over, under);
        let odd = self.s.m.bin(c, Bin::And, lo, one);
        let odd = self.s.m.cmp(c, pred::NE, odd, zero);
        let tie = self.s.m.bin(c, Bin::And, tie, odd);
        let up = self.s.m.bin(c, Bin::Or, nearer, tie);
        self.s.m.select(c, up, hi, lo)
    }

    /// [`Self::f16_nearest`] from `f`'s bits, without the host's conversion but for NaN
    /// (which stays what that conversion makes of it, as there): the same half for every
    /// other f32, in about 18 integer operations and one f32 addition, where the other takes
    /// four conversions and some 20 operations more (VALU f16 results on NVIDIA, the
    /// verifier's measurement of the combined base's card time).
    /// - From 2^-14 (a normal half): the f32 rebased to the half's exponent, plus a half step
    ///   less one and the bit that ends up last (ties to even), its top bits taken; up to
    ///   65520 that is also where it rounds to infinity.
    /// - Below (subnormal halves and zero): added to 0.5, whose f32 step is the subnormal
    ///   halves' (2^-24), so the f32 addition rounds to nearest as IEEE (and D3D) round it;
    ///   the bits past 0.5's are the half.
    /// - From 65536, infinity included: infinity.
    pub(super) fn f16_nearest_bits(&mut self, f: V) -> V {
        let c = self.cur;
        let i32t = self.s.t.i32;
        let x = self.as_int(f);
        let k = |l: &mut Self, n: u32| l.s.m.i32(n);
        let (abs, k13, one) = (k(self, 0x7FFF_FFFF), k(self, 13), k(self, 1));
        let a = self.s.m.bin(c, Bin::And, x, abs);
        let odd = self.s.m.bin(c, Bin::LShr, a, k13);
        let odd = self.s.m.bin(c, Bin::And, odd, one);
        // (15 - 127) << 23, plus 0xFFF: wrapping
        let rebase = k(self, 0xC800_0FFF);
        let n = self.s.m.bin(c, Bin::Add, a, rebase);
        let n = self.s.m.bin(c, Bin::Add, n, odd);
        let normal = self.s.m.bin(c, Bin::LShr, n, k13);
        let half = self.s.m.f32(0.5f32.to_bits());
        let fa = self.as_float(a);
        let s = self.s.m.bin(c, Bin::Add, fa, half);
        let s = self.as_int(s);
        let magic = k(self, 0x3F00_0000);
        let small = self.s.m.bin(c, Bin::Sub, s, magic);
        let min_normal = k(self, 0x3880_0000);
        let tiny = self.s.m.cmp(c, pred::IULT, a, min_normal);
        let h = self.s.m.select(c, tiny, small, normal);
        let (top, infinity) = (k(self, 0x4780_0000), k(self, 0x7C00));
        let big = self.s.m.cmp(c, pred::IUGE, a, top);
        let h = self.s.m.select(c, big, infinity, h);
        let (sign_bit, k16) = (k(self, 0x8000_0000), k(self, 16));
        let sign = self.s.m.bin(c, Bin::And, x, sign_bit);
        let sign = self.s.m.bin(c, Bin::LShr, sign, k16);
        let h = self.s.m.bin(c, Bin::Or, h, sign);
        let nan_from = k(self, 0x7F80_0000);
        let nan = self.s.m.cmp(c, pred::IUGT, a, nan_from);
        let host = self.s.op(c, "legacyF32ToF16", Attr::ReadNone, i32t, 130, &[f]);
        self.s.m.select(c, nan, host, h)
    }

    /// The f16 of an f32 result kept in 16 bits (d16 image and typed buffer reads, f16
    /// interpolation): on hosts of 32-lane waves (NVIDIA, whose `f32tof16` rounds toward
    /// zero, as WARP's does) rounded to nearest as the console rounds it, as
    /// [`Self::f16_result`] rounds VALU results; on other hosts the host's conversion, as
    /// before.
    pub(super) fn d16_half(&mut self, f: V) -> V {
        if host_max_wave() == 32 {
            return self.f16_result(f);
        }
        let i32t = self.s.t.i32;
        self.s.op(self.cur, "legacyF32ToF16", Attr::ReadNone, i32t, 130, &[f])
    }
}
