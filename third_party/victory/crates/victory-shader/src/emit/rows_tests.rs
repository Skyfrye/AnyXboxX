//! A compute program's group number when the host runs a dispatch of more groups along X than a
//! D3D12 dimension holds in rows (`USER_GROUP_ROWS`): the console's numbers, every group once,
//! and the last row's groups past the count leave at once; without the words, the host's own
//! numbers along both. Lowered (wave32 and wave64 programs on a host of 32-lane waves),
//! validated by `dxil.dll` when present, and run on the hardware device and WARP.

use super::host::{Dispatch, Host, Root, View};
use super::{HostShader, USER_GROUP_ROWS, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;

/// A console compute program of `threads` threads a group (wave64 for 64): every thread adds
/// its group's number along Y plus one to the dword of the raw UAV at its group's number along
/// X.
fn adder(threads: u32) -> NativeProgram {
    let vop1 = |op: u32, vdst: u32, src0: u32| 0x7E00_0000 | vdst << 17 | op << 9 | src0;
    let vop2 = |op: u32, vdst: u32, src0: u32, vsrc1: u32| op << 25 | vdst << 17 | vsrc1 << 9 | src0;
    let words = [
        0xBE8C_0305, // s_mov_b32 s12, s5: the group along X
        0xBE8D_0306, // s_mov_b32 s13, s6: along Y
        0xBE85_0380, // s_mov_b32 s5, 0
        0x3D << 26 | 0x02 << 18 | 8 << 6 | 2,
        125 << 25,                 // s_load_dwordx4 s[8:11], s[4:5], 0
        vop1(0x01, 4, 12),         // v_mov_b32 v4, s12
        vop2(0x1A, 3, 128 + 2, 4), // v_lshlrev_b32 v3, 2, v4
        vop1(0x01, 2, 13),         // v_mov_b32 v2, s13
        vop2(0x25, 2, 129, 2),     // v_add_nc_u32 v2, 1, v2
        0xE0C8_1000,
        0x8002_0203, // buffer_atomic_add v2, v3, s[8:11], 0 offen
        S_ENDPGM,
    ];
    NativeProgram {
        stage: HwStage::Cs,
        wave64: threads > 32,
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
            // 5 user SGPRs (s0..s4), the group along X in s5, along Y in s6
            rsrc2: 5 << 1 | 1 << 7 | 1 << 8,
            threads: [threads, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// Lowers `native` for a host of 32-lane waves.
fn lowered(native: &NativeProgram) -> HostShader {
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, native).expect("io::raise");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
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

/// Where `shader`'s user data has the two words of rows: the row's groups and the groups asked
/// for along X.
fn words(shader: &HostShader) -> [usize; 2] {
    [USER_GROUP_ROWS, USER_GROUP_ROWS + 1].map(|kind| {
        let word = shader.user_data.iter().find(|u| u.root == RootType::Other(kind));
        word.unwrap_or_else(|| panic!("a program that reads its group's number asks for word {kind:#x}")).index as usize
    })
}

/// The groups `groups` of `shader` on the device (`warp`) with the words `row` and `asked`:
/// the UAV's first `dwords` dwords; None without the device.
fn ran(shader: &HostShader, warp: bool, (row, asked): (u32, u32), groups: [u32; 3], dwords: u32) -> Option<Vec<u32>> {
    let host = Host::new(warp).ok()?;
    let (root, pso) = host.compute_pipeline(shader).unwrap_or_else(|e| panic!("compute_pipeline: {e}"));
    let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    let at = words(shader);
    (ud[at[0]], ud[at[1]]) = (row, asked);
    let out = host
        .dispatch(&root, &pso, &Dispatch {
            roots: vec![(0, Root::Constants(ud))],
            views: &[View::RawUav(dwords)],
            samplers: &[],
            groups,
        })
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    Some(out[0].clone())
}

#[test]
fn groups_in_rows_are_numbered_as_the_console_numbers_them() {
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut ran_on = 0;
    for (label, warp) in [("hardware", false), ("warp", true)] {
        if Host::new(warp).is_err() {
            eprintln!("no {label} device; not run");
            continue;
        }
        for threads in [1u32, 64] {
            let shader = lowered(&adder(threads));
            // (asked, row, rows): the title's tile lists' counts, more than a dimension holds,
            // in two rows (the second with a group past the count), and a few groups in rows of
            // three, two of them past the count. 64 threads a group: only the few on WARP.
            let mut cases = vec![(7u32, 3u32, 3u32), (1000, 334, 3)];
            if threads == 1 || !warp {
                cases.extend([(67_440, 33_720, 2), (70_001, 35_001, 2)]);
            }
            for (asked, row, rows) in cases {
                let dwords = asked + 64;
                let Some(out) = ran(&shader, warp, (row, asked), [row, rows, 1], dwords) else {
                    continue;
                };
                // Group x of the console's once each, numbered 0 along Y; none past the count.
                let wrong = (0..dwords as usize).find(|&x| out[x] != if (x as u32) < asked { threads } else { 0 });
                assert_eq!(
                    wrong.map(|x| (x, out[x])),
                    None,
                    "{label}, {threads} threads: {asked} groups in {rows} rows of {row}"
                );
            }
            // Not in rows (both words 0): the host's numbers along X and Y.
            let out = ran(&shader, warp, (0, 0), [5, 3, 1], 64).expect("the device ran before");
            let want = |x: usize| if x < 5 { threads * (1 + 2 + 3) } else { 0 };
            assert_eq!((0..64).find(|&x| out[x] != want(x)), None, "{label}, {threads} threads: as the host numbers them");
        }
        ran_on += 1;
    }
    assert!(ran_on > 0, "no device");
    eprintln!("ran on {ran_on} device(s): groups in rows numbered as the console's, those past the count left");
}

/// A program that does not read its group's number asks for no words: the host runs its
/// dispatches as before.
#[test]
fn programs_that_do_not_read_their_group_ask_for_no_rows() {
    let words = [
        0xBE85_0380, // s_mov_b32 s5, 0
        0x3D << 26 | 0x02 << 18 | 8 << 6 | 2,
        125 << 25,   // s_load_dwordx4 s[8:11], s[4:5], 0
        0x7E04_0281, // v_mov_b32 v2, 1
        0x7E06_0280, // v_mov_b32 v3, 0
        0xE0C8_1000,
        0x8002_0203, // buffer_atomic_add v2, v3, s[8:11], 0 offen
        S_ENDPGM,
    ];
    let mut native = adder(1);
    native.io = StageIo::Cs(ComputeIo { rsrc2: 5 << 1, threads: [1, 1, 1] });
    native.code = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let shader = lowered(&native);
    assert!(
        !shader.user_data.iter().any(|u| matches!(u.root, RootType::Other(k) if k == USER_GROUP_ROWS || k == USER_GROUP_ROWS + 1)),
        "{:?}",
        shader.user_data
    );
}
