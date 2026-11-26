//! 16-bit float results on hosts of 32-lane waves (NVIDIA, whose `f32tof16` rounds toward
//! zero, as WARP's does): rounded to nearest, ties to even, as the console rounds them, from
//! the f32's bits (`Lower::f16_nearest_bits`). Lowered, validated by `dxil.dll` when present,
//! and run on the hardware device and WARP.

use super::host::{Dispatch, Host, Root, View};
use super::{HostShader, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;

/// Lowers `native` for a host whose waves have at most `host_wave` lanes.
fn lowered(native: &NativeProgram, host_wave: u32) -> HostShader {
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, native).expect("io::raise");
    let was = lower::TEST_HOST_WAVE.replace(Some(host_wave));
    let lowered = lower::lower(&lower::Parts {
        native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    });
    lower::TEST_HOST_WAVE.set(was);
    let lowered = lowered.expect("lower");
    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        user_data: lowered.user_data,
        samplers: lowered.samplers,
        static_samplers: Vec::new(),
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
        root_uavs: lowered.root_uavs,
        root_indexed_views: res.root_indexed_views,
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut shader.dxil.clone()).expect("DXIL validation failed");
    }
    shader
}

/// The console's `v_cvt_f16_f32` of the f32 with bits `x` (round to nearest, ties to even,
/// subnormal halves kept, 65520 and up infinity), worked out on its exponent and mantissa
/// alone; NaN as a quiet NaN of its sign (the test only asks a NaN for a NaN).
fn rne(x: u32) -> u16 {
    let sign = (x >> 16 & 0x8000) as u16;
    let exp = (x >> 23 & 0xFF) as i32;
    let mant = x & 0x7F_FFFF;
    if exp == 0xFF {
        return sign | if mant == 0 { 0x7C00 } else { 0x7E00 };
    }
    // the value is m * 2^(e - 23), m with its implicit bit
    let (m, e) = if exp == 0 { (u64::from(mant), -126) } else { (u64::from(mant | 0x80_0000), exp - 127) };
    // the half's step: 2^-24 below 2^-14, else 2^(e - 10)
    let step = (e.max(-14) - 10) as i64;
    // m * 2^(e - 23) / 2^step = m >> (step - e + 23)
    let shift = step - i64::from(e) + 23;
    let q = if shift <= 0 {
        m << -shift
    } else if shift >= 64 {
        0
    } else {
        let q = m >> shift;
        let rest = m & ((1u64 << shift) - 1);
        let half = 1u64 << (shift - 1);
        q + u64::from(rest > half || rest == half && q & 1 == 1)
    };
    // q steps of 2^step: the half's bits are the exponent field (from 2^-14 on) and q's
    // fraction, which carries into the exponent where it rounds up
    let bits = if e < -14 { q } else { (((e + 15) as u64) << 10) + (q - 1024) };
    sign | bits.min(0x7C00) as u16
}

fn is_nan(h: u32) -> bool {
    h & 0x7C00 == 0x7C00 && h & 0x3FF != 0
}

#[test]
fn the_reference_rounds_as_ieee_does() {
    assert_eq!(rne(0x3F80_0000), 0x3C00); // 1.0
    assert_eq!(rne(0x3F80_1000), 0x3C00); // 1 + half a step: ties to even
    assert_eq!(rne(0x3F80_3000), 0x3C02); // 1 + 1.5 steps: ties to even, up
    assert_eq!(rne(0x3F80_1001), 0x3C01);
    assert_eq!(rne(0x477F_E000), 0x7BFF); // 65504
    assert_eq!(rne(0x477F_EFFF), 0x7BFF);
    assert_eq!(rne(0x477F_F000), 0x7C00); // 65520: infinity
    assert_eq!(rne(0x3380_0000), 0x0001); // 2^-24, the least subnormal half
    assert_eq!(rne(0x3300_0000), 0x0000); // 2^-25: a tie, to even (zero)
    assert_eq!(rne(0x3300_0001), 0x0001);
    assert_eq!(rne(0x387F_FFFF), 0x0400); // just below 2^-14: up to the least normal
    assert_eq!(rne(0xC000_0000), 0xC000); // -2.0
    assert_eq!(rne(0x8000_0001), 0x8000);
    assert_eq!(rne(0xFF80_0000), 0xFC00);
}

/// 64 groups of 1024 threads of a console wave32 compute program: thread g (of the whole
/// dispatch) converts the f32 with bits s0 + g * s1 (`v_cvt_f16_f32`) and stores the half at
/// dword g of the raw UAV.
fn sweep_program() -> NativeProgram {
    let vop1 = |op: u32, vdst: u32, src0: u32| 0x7E00_0000 | vdst << 17 | op << 9 | src0;
    let vop2 = |op: u32, vdst: u32, src0: u32, vsrc1: u32| op << 25 | vdst << 17 | vsrc1 << 9 | src0;
    let v = |n: u32| 256 + n;
    let words = [
        0xBE87_0305, // s_mov_b32 s7, s5: the group
        0xBE85_0380, // s_mov_b32 s5, 0
        0x3D << 26 | 0x02 << 18 | 8 << 6 | 2,
        125 << 25,              // s_load_dwordx4 s[8:11], s[4:5], 0
        vop1(0x01, 4, 7),       // v_mov_b32 v4, s7
        vop2(0x1A, 4, 128 + 10, 4), // v_lshlrev_b32 v4, 10, v4
        vop2(0x25, 4, v(0), 4), // v_add_nc_u32 v4, v0, v4: g
        0xD400_0000 | 0x169 << 16 | 1,
        v(4) | 1 << 9,          // v_mul_lo_u32 v1, v4, s1
        vop2(0x25, 1, 0, 1),    // v_add_nc_u32 v1, s0, v1
        vop1(0x0A, 2, v(1)),    // v_cvt_f16_f32 v2, v1
        vop2(0x1A, 3, 128 + 2, 4), // v_lshlrev_b32 v3, 2, v4
        0xE070_1000,
        0x8002_0000 | 2 << 8 | 3, // buffer_store_dword v2, v3, s[8:11], 0 offen
        S_ENDPGM,
    ];
    NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 8,
        layout: vec![
            RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 4,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
        ],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 4,
            slot: 1,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 5 << 1 | 1 << 7, // 5 user SGPRs (s0..s4), the group in s5
            threads: [1024, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// `shader` on the device (`warp`) for each pair of root constants `(s0, s1)`: the UAV of
/// `dwords` each time; None without the device.
fn dispatched(shader: &HostShader, warp: bool, constants: &[(u32, u32)], dwords: u32, groups: u32) -> Option<Vec<Vec<u32>>> {
    let host = Host::new(warp).ok()?;
    let (root, pso) = host.compute_pipeline(shader).unwrap_or_else(|e| panic!("compute_pipeline: {e}"));
    let mut out = Vec::new();
    for &(s0, s1) in constants {
        let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        for u in &shader.user_data {
            if let (RootType::Constants, 0, d @ 0..=1) = (u.root, u.slot, u.dword) {
                ud[u.index as usize] = [s0, s1][d as usize];
            }
        }
        let got = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &[View::RawUav(dwords)],
                    samplers: &[],
                    groups: [groups, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
        out.push(got[0].clone());
    }
    Some(out)
}

/// Every f32 whose low 16 bits are one of the patterns that decide a rounding (none of the
/// dropped bits, all of them but the last, exactly half a step, just over, ...), with each of
/// the 65,536 high halves: every half's neighbourhood, subnormals, the overflow to infinity,
/// infinities and NaNs, of both signs. Converted on 32-lane hosts from the bits, as the
/// console rounds them, and on others as before (the exact form through the host's own
/// conversion): the same halves.
#[test]
fn f16_results_from_the_bits_round_as_the_console_for_every_kind_of_f32() {
    let native = sweep_program();
    let (bits, exact) = (lowered(&native, 32), lowered(&native, 64));
    assert!(bits.dxil != exact.dxil, "the 32-lane host's form is its own");
    let lows = [0x0000u32, 0x0FFF, 0x1000, 0x1001, 0x1FFF, 0x2000, 0x2FFF, 0x3000, 0x8000, 0xFFFF];
    let constants: Vec<(u32, u32)> = lows.iter().map(|&low| (low, 0x1_0000)).collect();
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut ran = 0;
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let (Some(a), Some(b)) = (
            dispatched(&bits, warp, &constants, 65536, 64),
            dispatched(&exact, warp, &constants, 65536, 64),
        ) else {
            eprintln!("no SM 6.6 {label} device; not run");
            continue;
        };
        for (n, &low) in lows.iter().enumerate() {
            for g in 0..65536u32 {
                let x = g << 16 | low;
                let (got, theirs) = (a[n][g as usize], b[n][g as usize]);
                let want = u32::from(rne(x));
                if is_nan(want) {
                    assert!(is_nan(got & 0xFFFF), "{label}: {x:#010x} gave {got:#x}, no NaN");
                    assert_eq!(got, theirs, "{label}: NaN {x:#010x} as the host's conversion makes it");
                } else {
                    assert_eq!(got, want, "{label}: {x:#010x} gave {got:#06x}, the console {want:#06x}");
                    assert_eq!(theirs, want, "{label}: the exact form, {x:#010x}");
                }
            }
        }
        ran += 1;
    }
    assert!(ran > 0, "no SM 6.6 device");
    eprintln!("ran on {ran} device(s): 655,360 f32s rounded to halves as the console rounds them");
}
