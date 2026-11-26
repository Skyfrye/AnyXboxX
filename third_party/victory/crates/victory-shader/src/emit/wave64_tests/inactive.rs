//! Reads of other lanes (`ds_bpermute_b32`, DPP) on hosts of 32-lane waves (NVIDIA) whose
//! source lane is outside EXEC: by a guard (EXEC narrowed in straight code) or because it left
//! through a branch, so that the host does not run it at the instruction at all, where
//! NVIDIA's shuffle gives no defined value. The console's `ds_bpermute_b32` reads 0 from such a
//! lane; its DPP (`fi` clear) reads 0 with `bound_ctrl` and else leaves the reading lane's
//! destination, and with `fi` reads the lane's register (as the reference interpreter,
//! `PrototypeScarlett/emu/rdna2_exec_ext.py`, runs them). Run on the hardware device when it
//! takes waves of 32.

use super::lanes::{EXEC, LIT, VCC, ds, emitted, k, run, same_as_console, sop1, stores, v, vop1, vop2, vop3, vopc};
use super::*;

/// One console wave of 32 lanes. Every lane holds 500 + n in v4; inside `if (n even)` (a lane
/// branch: the odd lanes are not at the permute) the even lanes set v4 to 100 + n and read
/// lane n ^ 1's v4 (`ds_bpermute_b32`): an odd lane, outside EXEC, so 0. The odd lanes keep 77.
#[test]
fn a_permute_of_lanes_that_left_through_a_branch_reads_zero() {
    let mut w = vec![
        vop2(0x1D, 1, k(1), 0), // v_xor_b32 v1, 1, v0
        vop2(0x1A, 1, k(2), 1), // v_lshlrev_b32 v1, 2, v1: lane n ^ 1's address
        vop1(0x01, 3, LIT),     // v_mov_b32 v3, 77
        77,
        vop2(0x25, 4, LIT, 0), // v_add_nc_u32 v4, 500, v0
        500,
        vop2(0x1B, 2, k(1), 0), // v_and_b32 v2, 1, v0
        vopc(0xC2, k(0), 2),    // v_cmp_eq_u32 vcc_lo, 0, v2: the even lanes
        sop1(0x3C, 12, VCC),    // s_and_saveexec_b32 s12, vcc_lo
        0xBF88_0004,            // s_cbranch_execz past the branch
        vop2(0x25, 4, LIT, 0),  // v_add_nc_u32 v4, 100, v0
        100,
    ];
    w.extend(ds(0xB3, 3, 1, 4, 0)); // ds_bpermute_b32 v3, v1, v4
    w.push(sop1(0x03, EXEC, 12)); // s_mov_b32 exec_lo, s12
    w.extend(stores([3, 4, 0, 0]));
    let native = native_of(&w, false, 32);
    let want: Vec<u32> = (0..32u32)
        .flat_map(|n| if n % 2 == 0 { [0, 100 + n, n, n] } else { [77, 500 + n, n, n] })
        .collect();
    let shader = emitted(&native, 32);
    // (the host's own read, `VICTORY_INACTIVE_LANES=0`, is another translation: this card's
    // driver happened to give 0 for those lanes too, which no D3D12 host promises)
    let was = lower::TEST_INACTIVE_LANES.replace(Some(false));
    let as_before = emitted(&native, 32);
    lower::TEST_INACTIVE_LANES.set(was);
    assert!(shader.dxil != as_before.dxil, "the source lane is tested");
    let _one = super::super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Some(got) = run(&shader, [0; 4], 128) else { return };
    same_as_console("ds_bpermute of lanes outside the branch", &got, &want);
    eprintln!("ran on the hardware device: the permute read 0 from lanes that left through a branch");
}

/// `v_mov_b32_dpp v4, v1 row_shr:1` with EXEC narrowed to the even lanes (a guard): lane n
/// reads lane n - 1, odd and outside EXEC. Without `fi` the source is invalid: with
/// `bound_ctrl` the lane reads 0, without it keeps v4 (1000); with `fi` it reads the lane's
/// v1 (3n + 1 of n - 1) wherever the row has one. The odd lanes do not run it (1000).
#[test]
fn dpp_reads_of_lanes_outside_exec_are_the_consoles() {
    let _one = super::super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    for (bound_ctrl, fi) in [(false, false), (true, false), (false, true)] {
        let mut w = vop3(0x169, 1, v(0), k(3)).to_vec(); // v_mul_lo_u32 v1, v0, 3
        w.extend([
            vop2(0x25, 1, k(1), 1), // v_add_nc_u32 v1, 1, v1
            vop1(0x01, 4, LIT),     // v_mov_b32 v4, 1000
            1000,
            vop2(0x1B, 3, k(1), 0), // v_and_b32 v3, 1, v0
            sop1(0x03, 12, EXEC),   // s_mov_b32 s12, exec_lo
            vopc(0xD2, k(0), 3),    // v_cmpx_eq_u32 exec_lo, 0, v3: the even lanes
            vop1(0x01, 4, 0xFA),    // v_mov_b32 v4, v1 row_shr:1 row_mask:0xf bank_mask:0xf
            0xF << 28 | 0xF << 24 | u32::from(bound_ctrl) << 19 | u32::from(fi) << 18 | 0x111 << 8 | 1,
            sop1(0x03, EXEC, 12), // s_mov_b32 exec_lo, s12
        ]);
        w.extend(stores([4, 1, 0, 0]));
        let native = native_of(&w, false, 32);
        let x = |n: u32| 3 * n + 1;
        let want: Vec<u32> = (0..32u32)
            .flat_map(|n| {
                let got = if n % 2 == 1 {
                    1000
                } else if fi && n % 16 != 0 {
                    x(n - 1)
                } else if bound_ctrl {
                    0
                } else {
                    1000
                };
                [got, x(n), n, n]
            })
            .collect();
        let shader = emitted(&native, 32);
        let Some(got) = run(&shader, [0; 4], 128) else { return };
        same_as_console(&format!("DPP, bound_ctrl {bound_ctrl}, fi {fi}"), &got, &want);
    }
    eprintln!("ran on the hardware device: DPP reads of lanes outside EXEC as on the console");
}
