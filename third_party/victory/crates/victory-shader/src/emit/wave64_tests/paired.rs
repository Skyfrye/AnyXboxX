//! Console wave64 compute programs on a host of 32-lane waves in the paired form (one host wave
//! for each console wave, two lanes a thread: `super::super::paired`), whose LDS, lane reads
//! and masks cross the halves without barriers, checked against what the console's 64 lanes
//! compute; and the loose-LDS form, the last resort where the paired form does not take a
//! program. Lowered, validated by `dxil.dll` when present, and run on the host GPU when it
//! takes waves of 32 (WARP's are 4 lanes: neither form runs there).
//! Ported from Neptune (VIctory Edit.) 2026-11-25c's `wave64_tests.rs`, with `ds_bpermute_b32`
//! read within each 32-lane half as this tree has it (RDNA 2 ISA 10.4.4).

use super::lanes::{EXEC, LIT, VCC, ds, k, run, same_as_console, sop1, stores, v, vop1, vop2, vop3, vopc};
use super::*;
use crate::emit::WaveForm;

const fn sop2(op: u32, sdst: u32, src0: u32, src1: u32) -> u32 {
    0x8000_0000 | op << 23 | sdst << 16 | src1 << 8 | src0
}

/// A branch by `imm` dwords from the next instruction (or `s_barrier`, `imm` 0).
const fn sopp(op: u32, imm: i16) -> u32 {
    0xBF80_0000 | op << 16 | imm as u16 as u32
}

/// [`native_of`] with `lds` bytes of LDS (512-byte granules).
fn native_with(body: &[u32], threads: u32, lds: u32) -> NativeProgram {
    let mut native = native_of(body, true, threads);
    if let StageIo::Cs(cs) = &mut native.io {
        cs.rsrc2 |= lds.div_ceil(512) << 15;
    }
    native
}

/// The stages before emission, and an empty container.
fn stages(native: &NativeProgram) -> (ir::Program, structure::Structured, resource::Resources, io::Io, Vec<u8>) {
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, native).expect("io::raise");
    (p, st, res, io, crate::dxbc::build(&[]))
}

/// What [`super::super::emit`] gives a host of 32-lane waves for `native`, and the form it
/// names ([`super::super::take_wave_form`]); `pairs`: whether the paired form may be taken.
fn emitted_32(native: &NativeProgram, pairs: bool) -> (Result<HostShader, ShaderError>, Option<WaveForm>) {
    let (p, st, res, io, bytes) = stages(native);
    let c = super::super::Container::parse(&bytes).expect("container");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let off = super::super::TEST_PAIRS.replace(Some(pairs));
    let shader = super::super::emit(&c, native, &p, &st, &io, &res);
    let form = super::super::take_wave_form();
    super::super::TEST_PAIRS.set(off);
    lower::TEST_HOST_WAVE.set(was);
    (shader, form)
}

fn validated(shader: &HostShader) {
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
}

/// `native` in the paired form on a host of 32-lane waves, validated by `dxil.dll` when
/// present. Where [`super::super::emit`] names the paired form its choice, it is this one.
fn paired(native: &NativeProgram) -> HostShader {
    let (p, _, res, io, bytes) = stages(native);
    let c = super::super::Container::parse(&bytes).expect("container");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let shader = super::super::paired(&c, native, &p, &io, &res);
    lower::TEST_HOST_WAVE.set(was);
    let (shader, wave) = shader.expect("the paired form");
    assert_eq!(wave, Some(32));
    if let (Ok(chosen), Some(WaveForm::PAIRED)) = emitted_32(native, true) {
        assert!(chosen.dxil == shader.dxil, "emit's choice is not this paired form");
    }
    validated(&shader);
    shader
}

/// One console wave of 64 threads with 256 bytes of LDS. The even lanes write their dword in
/// one arm of a per-lane branch; the odd lanes theirs in the other arm, then, in that arm and
/// with no barrier, read their mirror lane's (63 - n, an even lane's); the even lanes read
/// theirs (an odd lane's) after the join. Then lanes 5 and 40 are read (`v_readlane_b32`), the
/// lanes holding more than 250 are counted (`s_bcnt1_i32_b64` of the compare's mask) and
/// counted below each lane (`v_mbcnt_lo/hi`): each of these takes lanes of both halves.
fn lds_across_halves() -> NativeProgram {
    let mut w = vec![vop2(0x1A, 1, k(2), 0)]; // v_lshlrev_b32 v1, 2, v0: this lane's dword
    w.extend(vop3(0x169, 2, v(0), k(7))); // v_mul_lo_u32 v2, v0, 7
    w.extend([
        vop2(0x26, 5, k(63), 0), // v_sub_nc_u32 v5, 63, v0: the mirror lane
        vop2(0x1A, 5, k(2), 5),  // v_lshlrev_b32 v5, 2, v5: its dword
        vop2(0x1B, 3, k(1), 0),  // v_and_b32 v3, 1, v0
        vopc(0xC2, k(0), 3),     // v_cmp_eq_u32 vcc, 0, v3: the even lanes
        sop1(0x24, 12, VCC),     // s_and_saveexec_b64 s[12:13], vcc
        sopp(0x08, 4),           // s_cbranch_execz past the even lanes' arm
        vop2(0x25, 4, LIT, 2),   // v_add_nc_u32 v4, 100, v2
        100,
    ]);
    w.extend(ds(0x0D, 0, 1, 4, 0)); // ds_write_b32 v1, v4
    w.extend([
        sop2(0x15, EXEC, 12, EXEC), // s_andn2_b64 exec, s[12:13], exec: the odd lanes
        sopp(0x08, 6),              // s_cbranch_execz past their arm
        vop2(0x25, 4, LIT, 2),      // v_add_nc_u32 v4, 200, v2
        200,
    ]);
    w.extend(ds(0x0D, 0, 1, 4, 0)); // ds_write_b32 v1, v4
    w.extend(ds(0x36, 6, 5, 0, 0)); // ds_read_b32 v6, v5: an even lane's, from the other arm
    w.extend([
        sop1(0x04, EXEC, 12), // s_mov_b64 exec, s[12:13]
        sop1(0x24, 12, VCC),  // s_and_saveexec_b64 s[12:13], vcc: the even lanes again
    ]);
    w.extend(ds(0x36, 6, 5, 0, 0)); // ds_read_b32 v6, v5: an odd lane's
    w.push(sop1(0x04, EXEC, 12)); // s_mov_b64 exec, s[12:13]
    w.extend(vop3(0x360, 14, v(6), k(5))); // v_readlane_b32 s14, v6, 5
    w.extend(vop3(0x360, 15, v(6), k(40))); // v_readlane_b32 s15, v6, 40
    w.extend([
        sop2(0x00, 14, 14, 15), // s_add_u32 s14, s14, s15
        vopc(0xC1, LIT, 6),     // v_cmp_lt_u32 vcc, 250, v6
        250,
        sop1(0x10, 16, VCC), // s_bcnt1_i32_b64 s16, vcc
    ]);
    w.extend(vop3(0x365, 7, VCC, k(0))); // v_mbcnt_lo_u32_b32 v7, vcc_lo, 0
    w.extend(vop3(0x366, 7, VCC + 1, v(7))); // v_mbcnt_hi_u32_b32 v7, vcc_hi, v7
    w.extend([
        vop1(0x01, 9, 14),  // v_mov_b32 v9, s14
        vop1(0x01, 10, 16), // v_mov_b32 v10, s16
    ]);
    w.extend(stores([6, 9, 10, 7]));
    native_with(&w, 64, 256)
}

/// `threads` threads (whole console waves of 64 or not) with 256 bytes of LDS. Every lane puts
/// 1000 in its dword; then the lanes below s0, inside `if (t < s0)`, write their number there
/// and, with no barrier, read the dword of lane n ^ 33 (the other half): its number if it is
/// below s0 too, else 1000. The branch also sets s7 (0x77 before), so the skip around it
/// cannot be left out: with no lane below s0 the wave skips it and keeps 0x77. Each lane also
/// counts the lanes below it (`v_mbcnt_lo/hi` of all ones: its console lane). LDS read and
/// written in one lane-branch region: no form in two host waves keeps the console's order
/// there (only the loose-LDS one takes it, approximate, each half also skipping on its own),
/// and before the paired form a 32-lane host refused the program, dropping every dispatch.
fn lds_in_a_lane_branch(threads: u32) -> NativeProgram {
    let mut w = vec![
        sop1(0x03, 7, LIT), // s_mov_b32 s7, 0x77
        0x77,
        vop2(0x1A, 1, k(2), 0),  // v_lshlrev_b32 v1, 2, v0: this lane's dword
        vop2(0x1D, 2, k(33), 0), // v_xor_b32 v2, 33, v0: the partner lane
        vop2(0x1A, 2, k(2), 2),  // v_lshlrev_b32 v2, 2, v2: its dword
        vop1(0x01, 12, LIT),     // v_mov_b32 v12, 1000
        1000,
        vop1(0x01, 5, k(0)), // v_mov_b32 v5, 0
    ];
    w.extend(ds(0x0D, 0, 1, 12, 0)); // ds_write_b32 v1, v12
    w.extend([
        vopc(0xC4, 0, 0),    // v_cmp_gt_u32 vcc, s0, v0
        sop1(0x24, 12, VCC), // s_and_saveexec_b64 s[12:13], vcc
        sopp(0x08, 5),       // s_cbranch_execz past the branch
    ]);
    w.extend(ds(0x0D, 0, 1, 0, 0)); // ds_write_b32 v1, v0
    w.extend(ds(0x36, 5, 2, 0, 0)); // ds_read_b32 v5, v2: the partner's dword
    w.extend([
        sop1(0x03, 7, k(5)),  // s_mov_b32 s7, 5
        sop1(0x04, EXEC, 12), // s_mov_b64 exec, s[12:13]
        vop1(0x01, 8, 7),     // v_mov_b32 v8, s7
    ]);
    w.extend(vop3(0x365, 9, 193, k(0))); // v_mbcnt_lo_u32_b32 v9, -1, 0
    w.extend(vop3(0x366, 9, 193, v(9))); // v_mbcnt_hi_u32_b32 v9, -1, v9: the console lane
    w.extend(stores([5, 8, 9, 0]));
    native_with(&w, threads, 256)
}

/// What the console's 64 lanes store for [`lds_in_a_lane_branch`] with `s0`.
fn lds_in_a_lane_branch_console(s0: u32) -> Vec<u32> {
    let s7 = if s0 == 0 { 0x77 } else { 5 };
    (0..64u32)
        .flat_map(|n| {
            let p = n ^ 33;
            let read = if n >= s0 { 0 } else if p < s0 { p } else { 1000 };
            [read, s7, n, n]
        })
        .collect()
}

#[test]
fn a_32_lane_host_takes_lds_in_a_lane_branch_in_the_paired_form() {
    let native = lds_in_a_lane_branch(64);
    // without the paired form only an approximate one takes it
    let (other, other_form) = emitted_32(&native, false);
    let (chosen, form) = emitted_32(&native, true);
    assert_eq!(other_form, Some(WaveForm("loose LDS (APPROXIMATE)")), "the form without pairs");
    assert_eq!(form, Some(WaveForm::PAIRED), "emit's form");
    let chosen = chosen.expect("emit");
    validated(&chosen);
    assert!(other.expect("the loose-LDS form").dxil != chosen.dxil);
    let _one = super::super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    for s0 in [64u32, 40, 33, 32, 10, 1, 0] {
        let Some(got) = run(&chosen, [s0, 0, 0, 0], 256) else { return };
        same_as_console(&format!("LDS in a lane branch, paired lanes, s0 = {s0}"), &got, &lds_in_a_lane_branch_console(s0));
    }
    eprintln!("ran on the hardware device: LDS shared in a lane branch as on the console");
}

#[test]
fn paired_lanes_share_lds_and_lanes_across_halves_without_barriers() {
    let shader = paired(&lds_across_halves());
    // the console: lane n's dword is 7n + 100 (even n) or 7n + 200, and lane n reads 63 - n's
    let dword = |n: u32| 7 * n + if n.is_multiple_of(2) { 100 } else { 200 };
    let read: Vec<u32> = (0..64).map(|n| dword(63 - n)).collect();
    let big = (0..64).filter(|&n| read[n] > 250).fold(0u64, |m, n| m | 1 << n);
    assert!(big as u32 != 0 && big >> 32 != 0, "lanes of both halves counted");
    let want: Vec<u32> = (0..64)
        .flat_map(|n| [read[n], read[5] + read[40], big.count_ones(), (big & ((1 << n) - 1)).count_ones()])
        .collect();
    let _one = super::super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Some(got) = run(&shader, [0; 4], 256) else { return };
    same_as_console("paired lanes", &got, &want);
    eprintln!("ran on the hardware device: LDS, lane reads and masks across the halves as on the console");
}

/// Two console waves of 64 in a group of 128 (two host waves of 32, 512 bytes of LDS). In
/// each, lane 0 zeroes the wave's dword of LDS; every lane loops (lane & 3) + 1 + wave times,
/// adding its lane number to its sum and 1 to the wave's dword each round (`ds_add_u32`; the
/// loop's branch is the wave's: it goes round while some lane is left); after the loop each
/// lane reads the wave's total, with no barrier; lanes 40-63 read their first lane's sum
/// (`v_readfirstlane_b32`: lane 40, an upper lane), then lanes 20-63 (lane 20, a lower one,
/// though upper lanes have one too); past the barrier, each lane reads the other wave's total.
#[test]
fn paired_lanes_loop_apart_and_meet_the_other_wave_at_the_barrier() {
    let mut w = vec![
        vop2(0x16, 1, k(6), 0),  // v_lshrrev_b32 v1, 6, v0: the console wave
        vop2(0x1B, 2, k(63), 0), // v_and_b32 v2, 63, v0: the lane in it
        vop2(0x1A, 11, k(2), 1), // v_lshlrev_b32 v11, 2, v1: the wave's dword
        vop1(0x01, 12, k(0)),    // v_mov_b32 v12, 0
        sop1(0x04, 22, EXEC),    // s_mov_b64 s[22:23], exec
        vopc(0xD2, k(0), 2),     // v_cmpx_eq_u32 exec, 0, v2: lane 0
    ];
    w.extend(ds(0x0D, 0, 11, 12, 0)); // ds_write_b32 v11, v12
    w.extend([
        sop1(0x04, EXEC, 22),   // s_mov_b64 exec, s[22:23]
        vop2(0x1B, 3, k(3), 2), // v_and_b32 v3, 3, v2
        vop2(0x25, 3, k(1), 3), // v_add_nc_u32 v3, 1, v3
        vop2(0x25, 3, v(1), 3), // v_add_nc_u32 v3, v1, v3: the rounds
        vop1(0x01, 4, k(0)),    // v_mov_b32 v4, 0
        vop1(0x01, 13, k(1)),   // v_mov_b32 v13, 1
        sop1(0x04, 20, EXEC),   // s_mov_b64 s[20:21], exec
        vop2(0x25, 4, v(2), 4), // loop: v_add_nc_u32 v4, v2, v4
    ]);
    w.extend(ds(0x00, 0, 11, 13, 0)); // ds_add_u32 v11, v13
    w.extend([
        vop2(0x25, 3, 193, 3),       // v_add_nc_u32 v3, -1, v3
        vopc(0xC5, k(0), 3),         // v_cmp_ne_u32 vcc, 0, v3
        sop2(0x0F, EXEC, EXEC, VCC), // s_and_b64 exec, exec, vcc
        sopp(0x09, -7),              // s_cbranch_execnz loop
        sop1(0x04, EXEC, 20),        // s_mov_b64 exec, s[20:21]
    ]);
    w.extend(ds(0x36, 5, 11, 0, 0)); // ds_read_b32 v5, v11: the wave's total
    w.extend([
        sop1(0x04, 24, EXEC), // s_mov_b64 s[24:25], exec
        vopc(0xD1, k(39), 2), // v_cmpx_lt_u32 exec, 39, v2: lanes 40-63
        vop1(0x02, 6, v(4)),  // v_readfirstlane_b32 s6, v4
        sop1(0x04, EXEC, 24), // s_mov_b64 exec, s[24:25]
        vopc(0xD1, k(19), 2), // v_cmpx_lt_u32 exec, 19, v2: lanes 20-63
        vop1(0x02, 7, v(4)),  // v_readfirstlane_b32 s7, v4
        sop1(0x04, EXEC, 24), // s_mov_b64 exec, s[24:25]
        vop1(0x01, 6, 6),     // v_mov_b32 v6, s6
        vop1(0x01, 9, 7),     // v_mov_b32 v9, s7
        0xD400_0000 | 0x346 << 16 | 6, // v_lshl_add_u32 v6, v9, 16, v6
        v(9) | k(16) << 9 | v(6) << 18,
        sopp(0x0A, 0),            // s_barrier
        vop2(0x1D, 14, k(4), 11), // v_xor_b32 v14, 4, v11: the other wave's dword
    ]);
    w.extend(ds(0x36, 7, 14, 0, 0)); // ds_read_b32 v7, v14
    w.extend(stores([4, 5, 6, 7]));
    let shader = paired(&native_with(&w, 128, 512));
    let rounds = |wave: u32, lane: u32| (lane & 3) + 1 + wave;
    let total = |wave: u32| (0..64).map(|lane| rounds(wave, lane)).sum::<u32>();
    let first = |wave: u32, lane: u32| lane * rounds(wave, lane);
    let want: Vec<u32> = (0..128)
        .flat_map(|t| {
            let (wave, lane) = (t / 64, t % 64);
            [first(wave, lane), total(wave), first(wave, 20) << 16 | first(wave, 40), total(1 - wave)]
        })
        .collect();
    let _one = super::super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Some(got) = run(&shader, [0; 4], 512) else { return };
    same_as_console("paired lanes, two console waves", &got, &want);
    eprintln!("ran on the hardware device: two console waves, their loops and LDS as on the console");
}

/// One console wave: `ds_bpermute_b32` from the mirror lane's address (read within the lane's
/// own half: lane (n & 32) | (31 - n & 31)); a DPP row shift whose row mask leaves rows 0 and 3
/// alone (rows 2 and 3 are lanes 32-63: rows are counted in console lanes); a `ds_swizzle_b32`
/// swapping the two 16-lane rows of each 32 lanes; `v_writelane_b32` of s0 into lane 45.
#[test]
fn paired_lanes_permute_shift_and_write_lanes_of_both_halves() {
    let mut w = vop3(0x169, 1, v(0), k(3)).to_vec(); // v_mul_lo_u32 v1, v0, 3
    w.extend([
        vop2(0x25, 1, k(1), 1),  // v_add_nc_u32 v1, 1, v1
        vop2(0x26, 2, k(63), 0), // v_sub_nc_u32 v2, 63, v0
        vop2(0x1A, 2, k(2), 2),  // v_lshlrev_b32 v2, 2, v2: the mirror lane's address
    ]);
    w.extend(ds(0xB3, 3, 2, 1, 0)); // ds_bpermute_b32 v3, v2, v1
    w.extend([
        vop1(0x01, 4, LIT), // v_mov_b32 v4, 1000
        1000,
        vop1(0x01, 4, 0xFA), // v_mov_b32 v4, v1 row_shr:1 row_mask:0x6 bank_mask:0xf
        0x6 << 28 | 0xF << 24 | 0x111 << 8 | 1,
    ]);
    w.extend(ds(0x35, 5, 1, 0, 0x401F)); // ds_swizzle_b32 v5, v1 offset:swizzle(BITMASK_PERM, xor 16)
    w.push(vop1(0x01, 6, v(0))); // v_mov_b32 v6, v0
    w.extend(vop3(0x361, 6, 0, k(45))); // v_writelane_b32 v6, s0, 45
    w.extend(stores([3, 4, 5, 6]));
    let shader = paired(&native_with(&w, 64, 0));
    let s0 = 0xABCD_1234;
    let x = |n: u32| 3 * n + 1;
    let want: Vec<u32> = (0..64)
        .flat_map(|n| {
            let permuted = x(n & 32 | (63 - n) & 31);
            let shifted = if matches!(n >> 4, 1 | 2) && n & 15 != 0 { x(n - 1) } else { 1000 };
            let swizzled = x(n & 32 | (n & 31) ^ 16);
            [permuted, shifted, swizzled, if n == 45 { s0 } else { n }]
        })
        .collect();
    let _one = super::super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Some(got) = run(&shader, [s0, 0, 0, 0], 256) else { return };
    same_as_console("paired lanes", &got, &want);
    eprintln!("ran on the hardware device: permutes, row masks, swizzles and lane writes as on the console");
}

/// [`program`]'s wave OR inside a per-lane branch, in the paired form: the OR of both halves'
/// lanes, whichever take the branch; nothing stored when none does.
#[test]
fn paired_lanes_or_a_wave_inside_a_lane_branch() {
    for (skip, name) in [(0xBF88_0000, "s_cbranch_execz"), (0xBF84_0000, "s_cbranch_scc0")] {
        let shader = paired(&program(skip));
        let _one = super::super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let cases = [(64u32, u32::MAX), (40, 0x000F_FFFF), (33, 0x0001_FFFF), (10, 0x1F), (1, 1), (0, 0)];
        for (below, expected) in cases {
            let Some(got) = on_the_card(&shader, [below, 0]) else { return };
            assert_eq!(got, expected, "{name}: {below} threads in the branch");
        }
    }
    eprintln!("ran on the hardware device: the OR of both halves in every case");
}

/// [`super::super::wave::WAVE_OR_V7_TO_S7`] as AMD's compiler expands it (the console upscaler
/// library's passes): lane 0's half read inside the section, the whole wave's OR read from lane
/// 63 once EXEC is back. Thread t holds the bit `1 << (t >> 1)`, every thread takes part and
/// stores the OR, all of them set.
#[test]
fn paired_lanes_read_a_wave_or_from_lane_63_after_the_restore() {
    let words = [
        0x7E0A_0281, // v_mov_b32 v5, 1
        0x2C0C_0081, // v_lshrrev_b32 v6, 1, v0
        0x340E_0B06, // v_lshlrev_b32 v7, v6, v5
        0xBE84_25C1, // s_or_saveexec_b64 s[4:5], -1
        0xD501_0001,
        0x0012_0E80, // v_cndmask_b32 v1, 0, v7, s[4:5]
        0x3802_02E9,
        0xDE54_C101, // v_or_b32 v1, v1, v1 dpp8:[1,0,3,2,5,4,7,6]
        0x3802_02E9,
        0x9360_1201, // v_or_b32 v1, v1, v1 dpp8:[2,2,0,0,6,6,4,4]
        0x3802_02E9,
        0x0009_2401, // v_or_b32 v1, v1, v1 dpp8:[4,4,4,4,0,0,0,0]
        0x3802_02FA,
        0xFF09_4001, // v_or_b32 v1, v1, v1 row_mirror bound_ctrl:1
        0xD778_0002,
        0x0201_0101, // v_permlanex16_b32 v2, v1, 0, 0
        0x3802_0302, // v_or_b32 v1, v2, v1
        0xD760_0007,
        0x0001_0101, // v_readlane_b32 s7, v1, 0
        0x3802_0207, // v_or_b32 v1, s7, v1: lanes 32-63 hold the whole wave's OR
        0xBEFE_0404, // s_mov_b64 exec, s[4:5]
        0xD760_0007,
        0x0001_7F01, // v_readlane_b32 s7, v1, 63
        0x7E00_0207, // v_mov_b32 v0, s7
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
    ];
    let shader = paired(&native(&words));
    let _one = super::super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Some(got) = on_the_card(&shader, [0, 0]) else { return };
    assert_eq!(got, u32::MAX, "the OR of both halves");
    eprintln!("ran on the hardware device: the OR of both halves");
}

/// One console wave of 64 threads, thread t holding the bit `1 << (t >> 1)`: the threads below
/// s0 OR theirs over the wave inside `if (t < s0) { ... }` ([`super::super::wave::WAVE_OR_V7_TO_S7`]
/// into s7, which holds 0x77 before), and every thread stores s7 after the branch: the wave's
/// OR, or 0x77 when no thread took the branch and the wave skipped it. A wave-wide value used
/// past a per-lane branch, as the title's tile classification and light clustering programs
/// use theirs: each host wave a wave of its own gives each half its own OR, 0x77 to the upper
/// half while s0 <= 32. Where only such an approximate form takes a program, [`emit`] gives
/// the paired form.
#[test]
fn a_wave_value_used_past_a_lane_branch_takes_the_paired_form_not_an_approximate_one() {
    let mut w = vec![
        sop1(0x03, 7, LIT), // s_mov_b32 s7, 0x77
        0x77,
        vop1(0x01, 5, k(1)),    // v_mov_b32 v5, 1
        vop2(0x16, 6, k(1), 0), // v_lshrrev_b32 v6, 1, v0
        vop2(0x1A, 7, v(6), 5), // v_lshlrev_b32 v7, v6, v5
        vopc(0xC4, 0, 0),       // v_cmp_gt_u32 vcc, s0, v0
        sop1(0x24, 12, VCC),    // s_and_saveexec_b64 s[12:13], vcc
        sopp(0x08, 20),         // s_cbranch_execz past the OR
    ];
    w.extend_from_slice(&super::super::wave::WAVE_OR_V7_TO_S7);
    w.extend([
        sop1(0x04, EXEC, 12), // s_mov_b64 exec, s[12:13]
        vop1(0x01, 8, 7),     // v_mov_b32 v8, s7
    ]);
    w.extend(stores([8, 7, 6, 0]));
    let native = native_of(&w, true, 64);
    let (chosen, form) = emitted_32(&native, true);
    let (other, other_form) = emitted_32(&native, false);
    let chosen = chosen.expect("emit");
    validated(&chosen);
    // without the paired form: approximate, or no form at all
    assert!(other_form.is_none_or(WaveForm::approximate), "the other form: {other_form:?}");
    assert_eq!(form, Some(WaveForm::PAIRED), "emit's form");
    // the console: every thread stores the OR of the threads below s0, or 0x77 with none
    let want = |s0: u32| -> Vec<u32> {
        let or = (0..64u32).filter(|&t| t < s0).fold(0u32, |m, t| m | 1 << (t >> 1));
        let s7 = if s0 == 0 { 0x77 } else { or };
        (0..64u32).flat_map(|t| [s7, 1 << (t >> 1), t >> 1, t]).collect()
    };
    let _one = super::super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    for s0 in [64u32, 40, 33, 32, 10, 1, 0] {
        let Some(got) = run(&chosen, [s0, 0, 0, 0], 256) else { return };
        same_as_console(&format!("s0 = {s0}"), &got, &want(s0));
    }
    // each half on its own: with s0 = 10 the upper half took no branch and kept 0x77
    if let Ok(halves) = other {
        let Some(halves) = run(&halves, [10, 0, 0, 0], 256) else { return };
        assert_eq!((want(10)[0], want(10)[4 * 40]), (0x1F, 0x1F));
        assert_eq!((halves[0], halves[4 * 40]), (0x1F, 0x77), "the approximate form's lanes 0 and 40");
    }
    eprintln!("ran on the hardware device: the wave's OR past the branch in every lane, not each half's");
}

/// Thread t holds the 16-bit float with bits 0x3C00 + 37 t; the wave's sum of halves
/// (`v_add_f16` steps of the wave OR's shape), in the paired form: each half's tree over its
/// 32 lanes, then the two halves' sums, the console's last addition.
#[test]
fn paired_lanes_sum_halves_as_the_consoles_tree() {
    let mut words = vec![
        0x340C_0085, // v_lshlrev_b32 v6, 5, v0
        0x340E_0082, // v_lshlrev_b32 v7, 2, v0
        0x4A0E_0F06, // v_add_nc_u32 v7, v6, v7
        0x4A0E_0F00, // v_add_nc_u32 v7, v0, v7
        0x4A0E_0EFF,
        0x0000_3C00, // v_add_nc_u32 v7, 0x3c00, v7
    ];
    words.extend(super::super::wave::WAVE_OR_V7_TO_S7.iter().map(|&w| if w >> 25 == 28 { w & 0x01FF_FFFF | 50 << 25 } else { w }));
    words.extend_from_slice(&[
        0x7E00_0207, // v_mov_b32 v0, s7
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
    ]);
    let shader = paired(&native(&words));
    let wide = |h: u16| f32::from_bits((u32::from(h >> 10) + 112) << 23 | u32::from(h & 0x3FF) << 13);
    let narrow = |f: f32| {
        let b = f.to_bits();
        let h = ((b >> 23) - 112) << 10 | (b >> 13 & 0x3FF);
        let rest = b & 0x1FFF;
        (h + u32::from(rest > 0x1000 || rest == 0x1000 && h & 1 == 1)) as u16
    };
    let mut tree: Vec<u16> = (0..64u16).map(|t| 0x3C00 + 37 * t).collect();
    while tree.len() > 1 {
        tree = tree.chunks(2).map(|c| narrow(wide(c[0]) + wide(c[1]))).collect();
    }
    let _one = super::super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Some(got) = on_the_card(&shader, [0, 0]) else { return };
    assert_eq!(got, u32::from(tree[0]));
    eprintln!("ran on the hardware device: the sum {:#x} of the console's tree", tree[0]);
}

/// [`lds_in_a_lane_branch`] in a group of 96 threads (one and a half console waves): the
/// paired form needs full console waves and two host waves cannot order those accesses, so
/// only the loose-LDS form takes it (approximate) rather than the 32-lane host dropping every
/// dispatch.
#[test]
fn lds_in_a_lane_branch_of_a_partial_wave_takes_the_loose_lds_form() {
    let native = lds_in_a_lane_branch(96);
    let (shader, form) = emitted_32(&native, true);
    let shader = shader.expect("the loose-LDS form");
    assert_eq!(form, Some(WaveForm("loose LDS (APPROXIMATE)")), "emit's form");
    assert!(form.is_some_and(WaveForm::approximate));
    validated(&shader);
}

/// A 64-lane host (AMD) runs console waves as they are: no paired or loose form, and no
/// form named.
#[test]
fn a_64_lane_host_takes_no_32_lane_form() {
    let native = lds_across_halves();
    let (p, st, res, io, bytes) = stages(&native);
    let c = super::super::Container::parse(&bytes).expect("container");
    let was = lower::TEST_HOST_WAVE.replace(Some(64));
    let shader = super::super::emit(&c, &native, &p, &st, &io, &res);
    let form = super::super::take_wave_form();
    lower::TEST_HOST_WAVE.set(was);
    shader.expect("64-lane waves");
    assert_eq!(form, None);
}

/// `v_writelane_b32` inside a lane branch (lanes 0-31, `s_and_saveexec` and a skip) into lane
/// 45, which the branch leaves out: the console writes it whatever EXEC holds, and
/// `v_readlane_b32` reads it back after the join. In the paired form the wave takes the
/// branch together and the write reaches the upper lane of host lane 13.
#[test]
fn paired_lanes_write_a_lane_the_branch_leaves_out() {
    let mut w = vec![
        vop1(0x01, 6, v(0)),  // v_mov_b32 v6, v0
        vopc(0xC4, k(32), 0), // v_cmp_gt_u32 vcc, 32, v0: lanes 0..31
        sop1(0x24, 12, VCC),  // s_and_saveexec_b64 s[12:13], vcc
        sopp(0x08, 2),        // s_cbranch_execz past the write
    ];
    w.extend(vop3(0x361, 6, 0, 1)); // v_writelane_b32 v6, s0, s1
    w.push(sop1(0x04, EXEC, 12)); // s_mov_b64 exec, s[12:13]
    w.extend(vop3(0x360, 14, v(6), 1)); // v_readlane_b32 s14, v6, s1
    w.push(vop1(0x01, 7, 14)); // v_mov_b32 v7, s14
    w.extend(stores([6, 7, 0, 0]));
    let shader = paired(&native_of(&w, true, 64));
    let val = 0xCAFE_BABE;
    let want: Vec<u32> = (0..64u32).flat_map(|t| [if t == 45 { val } else { t }, val, t, t]).collect();
    let _one = super::super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Some(got) = run(&shader, [val, 109, 0, 0], 256) else { return };
    same_as_console("a lane write inside a lane branch, paired lanes", &got, &want);
    eprintln!("ran on the hardware device: the lane write reached a lane outside the branch");
}

/// Two console waves of 64 with all 32 KiB of LDS a host group has: `wave_base`'s counter
/// is the program's dword 0, so the group meets once more after every wave has its base
/// before any thread writes it (the second of the entry's two group barriers; with a dword
/// to spare past the program's LDS one is enough). Each thread writes 3t + 1 to dword t,
/// and after the barrier reads the other wave's thread's (t ^ 64).
#[test]
fn paired_lanes_with_all_of_lds_meet_again_before_the_program_writes_it() {
    let mut w = vec![
        vop2(0x1A, 1, k(2), 0), // v_lshlrev_b32 v1, 2, v0: dword t
    ];
    w.extend(vop3(0x169, 2, v(0), k(3))); // v_mul_lo_u32 v2, v0, 3
    w.push(vop2(0x25, 2, k(1), 2)); // v_add_nc_u32 v2, 1, v2
    w.extend(ds(0x0D, 0, 1, 2, 0)); // ds_write_b32 v1, v2
    w.extend([
        sopp(0x0A, 0),             // s_barrier
        vop2(0x1D, 3, LIT, 1),     // v_xor_b32 v3, 256, v1: thread t ^ 64's dword
        256,
    ]);
    w.extend(ds(0x36, 4, 3, 0, 0)); // ds_read_b32 v4, v3
    w.extend(stores([4, 2, 0, 0]));
    // the group barriers the paired form adds
    let syncs = |native: &NativeProgram| {
        let (p, _, res, io, bytes) = stages(native);
        let c = super::super::Container::parse(&bytes).expect("container");
        let was = lower::TEST_HOST_WAVE.replace(Some(32));
        lower::GROUP_SYNCS.set(0);
        let shader = super::super::paired(&c, native, &p, &io, &res);
        let n = lower::GROUP_SYNCS.get();
        lower::TEST_HOST_WAVE.set(was);
        shader.expect("the paired form");
        n
    };
    let full = native_with(&w, 128, 32768);
    assert_eq!((syncs(&native_with(&w, 128, 512)), syncs(&full)), (1, 2), "the entry's group barriers");
    let shader = paired(&full);
    let want: Vec<u32> = (0..128u32).flat_map(|t| [3 * (t ^ 64) + 1, 3 * t + 1, t, t]).collect();
    let _one = super::super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Some(got) = run(&shader, [0; 4], 512) else { return };
    same_as_console("paired lanes with all of LDS", &got, &want);
    eprintln!("ran on the hardware device: two console waves numbered with the counter in their own LDS");
}

/// One console wave of 64 threads in a loop that each lane leaves when its own value comes up,
/// as a waterfall loop does: each round takes the wave's first lane's value
/// (`v_readfirstlane_b32`), and the lanes that hold it keep it with the round's number and
/// leave (`v_cmpx_eq_u32`, `s_andn2_wrexec_b64`, `s_cbranch_execnz` back). Lane t holds
/// t >> 4: lanes 0-15 leave in the first round, 16-31 in the second, 32-47 in the third and
/// 48-63 in the fourth. The lane numbers (`v_mbcnt`) and the count of the wave's lanes
/// (`s_bcnt1_i32_b64` of EXEC, kept as data) take lanes of both halves.
fn rounds_until_the_own_value() -> NativeProgram {
    let mut w = vec![vop2(0x16, 1, k(4), 0)]; // v_lshrrev_b32 v1, 4, v0
    w.extend(vop3(0x365, 4, 193, k(0))); // v_mbcnt_lo_u32_b32 v4, -1, 0
    w.extend(vop3(0x366, 4, 193, v(4))); // v_mbcnt_hi_u32_b32 v4, -1, v4
    w.extend([
        sop1(0x10, 18, EXEC),     // s_bcnt1_i32_b64 s18, exec
        vop1(0x01, 5, 18),        // v_mov_b32 v5, s18
        sop1(0x03, 15, k(0)),     // s_mov_b32 s15, 0: the round
        sop1(0x04, 16, EXEC),     // s_mov_b64 s[16:17], exec
        sop1(0x04, 12, EXEC),     // s_mov_b64 s[12:13], exec: the lanes still waiting
        vop1(0x02, 14, v(1)),     // v_readfirstlane_b32 s14, v1
        vopc(0xD2, 14, 1),        // v_cmpx_eq_u32 exec, s14, v1
        sop2(0x00, 15, 15, k(1)), // s_add_u32 s15, s15, 1
        vop1(0x01, 2, 14),        // v_mov_b32 v2, s14
        vop1(0x01, 3, 15),        // v_mov_b32 v3, s15
        sop1(0x3A, 12, 12),       // s_andn2_wrexec_b64 s[12:13], s[12:13]
        sopp(0x09, -7),           // s_cbranch_execnz to the v_readfirstlane_b32
        sop1(0x04, EXEC, 16),     // s_mov_b64 exec, s[16:17]
    ]);
    w.extend(stores([2, 3, 4, 5]));
    native_of(&w, true, 64)
}

/// A loop of one block that the lanes leave one by one is not code every lane runs in every
/// round: two host waves of 32 that exchanged the wave's first lane through LDS there would
/// meet at the group barriers of different rounds (the lower half's threads leave after one or
/// two rounds, the upper half's after three or four) and read each other's stale dwords. The
/// program takes the paired form, which gives what the console's 64 lanes compute. (Before,
/// the loop's block counted as one every lane runs, and the program took the halves.)
#[test]
fn a_loop_the_lanes_leave_one_by_one_is_not_where_two_host_waves_meet() {
    let native = rounds_until_the_own_value();
    let (shader, form) = emitted_32(&native, true);
    assert_eq!(form, Some(WaveForm::PAIRED), "the form for a host of 32-lane waves");
    let shader = shader.expect("emit");
    validated(&shader);
    let want: Vec<u32> = (0..64u32).flat_map(|t| [t >> 4, (t >> 4) + 1, t, 64]).collect();
    let _one = super::super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Some(got) = run(&shader, [0; 4], 256) else { return };
    same_as_console("rounds until the own value", &got, &want);
    eprintln!("ran on the hardware device: a loop the lanes leave one by one, as on the console");
}
