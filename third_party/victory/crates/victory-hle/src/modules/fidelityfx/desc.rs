//! The FidelityFX API's descriptions the title passes, laid out as AMD's
//! public FidelityFX SDK declares them (v2.4.0: `Kits/FidelityFX/api/
//! include/ffx_api.h`, `ffx_api_types.h`, `upscalers/include/
//! ffx_upscale.h`; MIT), and what DLSS is given of them. The title's
//! descriptions were logged on Series S (Neptune's `VICTORY_FSR_DLL` route,
//! runs of 2026-11-26): an upscale context made with flags 0x29, 1280x720
//! to 1920x1080, a backend description of type 0x1000002 naming the device,
//! and an upscaler version 4.1.1; one dispatch type, 0x10001.

use std::ffi::{c_char, c_void};
use std::mem::{offset_of, size_of};

use victory_gpu::dlss;

pub(super) const CREATE_UPSCALE: u64 = 0x1_0000;
pub(super) const DISPATCH_UPSCALE: u64 = 0x1_0001;
pub(super) const QUERY_RATIO: u64 = 0x1_0002;
pub(super) const QUERY_RENDER_SIZE: u64 = 0x1_0003;
pub(super) const QUERY_PHASE_COUNT: u64 = 0x1_0004;
pub(super) const QUERY_JITTER_OFFSET: u64 = 0x1_0005;
pub(super) const DISPATCH_REACTIVE_MASK: u64 = 0x1_0006;
pub(super) const QUERY_MEMORY: u64 = 0x1_0008;
pub(super) const QUERY_MEMORY_V2: u64 = 0x1_0009;
pub(super) const QUERY_RESOURCES: u64 = 0x1_000a;
pub(super) const CREATE_VERSION: u64 = 0x1_000b;
pub(super) const QUERY_PROVIDER_VERSION: u64 = 6;
/// A backend's description: its id in the top byte, the device after the
/// header (DX12's is 2; the title's is 0x1000002).
pub(super) fn is_backend(ty: u64) -> bool {
    ty & 0x00ff_0000 == 0 && ty & 0xffff == 2
}

/// `ffxReturnCode_t`.
pub(super) const OK: u32 = 0;
pub(super) const ERROR: u32 = 1;
pub(super) const UNKNOWN_DESCTYPE: u32 = 2;
pub(super) const ERROR_PARAMETER: u32 = 6;

/// `FfxApiCreateContextUpscaleFlags`.
const HDR: u32 = 1 << 0;
const DISPLAY_MOTION: u32 = 1 << 1;
const MOTION_JITTERED: u32 = 1 << 2;
const DEPTH_INVERTED: u32 = 1 << 3;
const AUTO_EXPOSURE: u32 = 1 << 5;

/// `ffxApiHeader`: every description starts with its type and the next one.
#[repr(C)]
pub(super) struct Header {
    pub ty: u64,
    pub next: *mut Header,
}

/// `FfxApiResourceDescription`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct ResourceDesc {
    pub ty: u32,
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub mips: u32,
    pub flags: u32,
    pub usage: u32,
}

/// `FfxApiResource`: the title's resource, its description and the state
/// it is in.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(super) struct Resource {
    pub resource: *mut c_void,
    pub desc: ResourceDesc,
    pub state: u32,
}

/// `ffxCreateContextDescUpscale`.
#[repr(C)]
pub(super) struct CreateUpscale {
    pub header: Header,
    pub flags: u32,
    pub max_render: [u32; 2],
    pub max_upscale: [u32; 2],
    pub message: *const c_void,
}

/// `ffxDispatchDescUpscale`. Its `bool`s are bytes here.
#[repr(C)]
pub(super) struct DispatchUpscale {
    pub header: Header,
    pub command_list: *mut c_void,
    pub color: Resource,
    pub depth: Resource,
    pub motion: Resource,
    pub exposure: Resource,
    pub reactive: Resource,
    pub transparency: Resource,
    pub output: Resource,
    pub jitter: [f32; 2],
    pub motion_scale: [f32; 2],
    pub render: [u32; 2],
    pub upscale: [u32; 2],
    pub sharpen: u8,
    pub sharpness: f32,
    pub frame_ms: f32,
    pub pre_exposure: f32,
    pub reset: u8,
    pub near: f32,
    pub far: f32,
    pub fov: f32,
    pub meters: f32,
    pub flags: u32,
}

/// `ffxQueryDescUpscaleGetJitterPhaseCount`.
#[repr(C)]
pub(super) struct QueryPhaseCount {
    pub header: Header,
    pub render_width: u32,
    pub display_width: u32,
    pub out: *mut i32,
}

/// `ffxQueryDescUpscaleGetJitterOffset`.
#[repr(C)]
pub(super) struct QueryJitterOffset {
    pub header: Header,
    pub index: i32,
    pub phase_count: i32,
    pub out_x: *mut f32,
    pub out_y: *mut f32,
}

/// `ffxQueryDescUpscaleGetRenderResolutionFromQualityMode`.
#[repr(C)]
pub(super) struct QueryRenderSize {
    pub header: Header,
    pub display: [u32; 2],
    pub quality: u32,
    pub out_width: *mut u32,
    pub out_height: *mut u32,
}

/// `ffxQueryDescUpscaleGetUpscaleRatioFromQualityMode`.
#[repr(C)]
pub(super) struct QueryRatio {
    pub header: Header,
    pub quality: u32,
    pub out: *mut f32,
}

/// `ffxQueryGetProviderVersion`.
#[repr(C)]
pub(super) struct QueryProviderVersion {
    pub header: Header,
    pub version_id: u64,
    pub version_name: *const c_char,
}

/// `ffxQueryDescUpscaleGetGPUMemoryUsage` (the V2's pointer is at 56).
#[repr(C)]
pub(super) struct QueryMemory {
    pub header: Header,
    pub usage: *mut [u64; 2],
}

/// `ffxQueryDescUpscaleGetResourceRequirements`.
#[repr(C)]
pub(super) struct QueryResources {
    pub header: Header,
    pub required: u64,
    pub optional: u64,
}

// The SDK's layout, on 64-bit Windows.
const _: () = {
    assert!(size_of::<Header>() == 16);
    assert!(size_of::<Resource>() == 48 && offset_of!(Resource, state) == 40);
    assert!(size_of::<CreateUpscale>() == 48 && offset_of!(CreateUpscale, max_render) == 20);
    assert!(offset_of!(DispatchUpscale, command_list) == 16);
    assert!(offset_of!(DispatchUpscale, color) == 24);
    assert!(offset_of!(DispatchUpscale, output) == 24 + 6 * 48);
    assert!(offset_of!(DispatchUpscale, jitter) == 360);
    assert!(offset_of!(DispatchUpscale, render) == 376);
    assert!(offset_of!(DispatchUpscale, sharpen) == 392);
    assert!(offset_of!(DispatchUpscale, reset) == 408);
    assert!(offset_of!(DispatchUpscale, flags) == 428);
    assert!(size_of::<DispatchUpscale>() == 432);
    assert!(offset_of!(QueryPhaseCount, out) == 24);
    assert!(offset_of!(QueryJitterOffset, out_y) == 32);
    assert!(offset_of!(QueryRenderSize, out_width) == 32);
    assert!(offset_of!(QueryRatio, out) == 24);
};

/// The bytes a copy of a known create description takes (`None` for one
/// of a type not known here).
pub(super) fn create_size(ty: u64) -> Option<usize> {
    match ty {
        CREATE_UPSCALE => Some(size_of::<CreateUpscale>()),
        // Header and one word (the device, or the version and padding).
        CREATE_VERSION => Some(24),
        t if is_backend(t) => Some(24),
        _ => None,
    }
}

/// DLSS's flags from the title's create flags.
pub(super) fn flags(create: u32) -> dlss::Flags {
    dlss::Flags {
        hdr: create & HDR != 0,
        motion_at_display: create & DISPLAY_MOTION != 0,
        motion_jittered: create & MOTION_JITTERED != 0,
        depth_inverted: create & DEPTH_INVERTED != 0,
        auto_exposure: create & AUTO_EXPOSURE != 0,
    }
}

/// The DLSS feature for the title's upscale context: its largest render
/// size and its output size.
pub(super) fn create(c: &CreateUpscale, mode: dlss::Mode) -> dlss::Create {
    dlss::Create {
        render: (c.max_render[0], c.max_render[1]),
        display: (c.max_upscale[0], c.max_upscale[1]),
        flags: flags(c.flags),
        mode,
    }
}

/// What the user may change of the title's values (`VICTORY_DLSS_JITTER_SCALE`,
/// `VICTORY_DLSS_MV_SCALE`: `x,y` or one number for both, each multiplying
/// the title's; `VICTORY_DLSS_REACTIVE=0` gives DLSS no reactive mask).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Tweaks {
    pub jitter: [f32; 2],
    pub motion: [f32; 2],
    pub reactive: bool,
}

impl Default for Tweaks {
    fn default() -> Self {
        Tweaks {
            jitter: [1.0; 2],
            motion: [1.0; 2],
            reactive: true,
        }
    }
}

impl Tweaks {
    pub(super) fn parse(
        jitter: Option<&str>,
        motion: Option<&str>,
        reactive: Option<&str>,
    ) -> Tweaks {
        let pair = |s: Option<&str>| {
            let s = s?;
            let v: Vec<f32> = s
                .split(',')
                .map(|v| v.trim().parse().ok())
                .collect::<Option<_>>()?;
            match v[..] {
                [a] => Some([a, a]),
                [a, b] => Some([a, b]),
                _ => None,
            }
        };
        Tweaks {
            jitter: pair(jitter).unwrap_or([1.0; 2]),
            motion: pair(motion).unwrap_or([1.0; 2]),
            reactive: reactive.is_none_or(|v| v.trim() != "0"),
        }
    }
}

/// One frame's values for DLSS from the title's dispatch.
pub(super) fn frame(d: &DispatchUpscale, t: &Tweaks) -> dlss::Frame {
    dlss::Frame {
        jitter: [d.jitter[0] * t.jitter[0], d.jitter[1] * t.jitter[1]],
        motion_scale: [
            d.motion_scale[0] * t.motion[0],
            d.motion_scale[1] * t.motion[1],
        ],
        render: (d.render[0], d.render[1]),
        reset: d.reset != 0,
        pre_exposure: d.pre_exposure,
        frame_ms: d.frame_ms,
    }
}

/// The title's textures for DLSS: the exposure only without automatic
/// exposure, the reactive mask (as DLSS's mask of pixels to take from this
/// frame) only if it covers the rendered part.
pub(super) fn inputs(d: &DispatchUpscale, auto_exposure: bool, t: &Tweaks) -> dlss::Inputs {
    let covers = |r: &Resource| r.desc.width >= d.render[0] && r.desc.height >= d.render[1];
    let reactive = if t.reactive && covers(&d.reactive) {
        d.reactive.resource
    } else {
        std::ptr::null_mut()
    };
    dlss::Inputs {
        color: d.color.resource,
        depth: d.depth.resource,
        motion: d.motion.resource,
        exposure: if auto_exposure {
            std::ptr::null()
        } else {
            d.exposure.resource
        },
        reactive,
        output: d.output.resource,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first 12 words of the title's dispatch as logged on Series S
    /// (pointers made up): 1280x720 colour in 16-bit float RGBA, read by
    /// pixel and compute shaders, then the 32-bit float depth.
    const LOGGED: [u64; 12] = [
        0x10001,
        0,
        0x1000,
        0x2000,
        0x4_0000_0002,
        0x2d0_0000_0500,
        0x1_0000_0001,
        0x2_0000_0000,
        0xc,
        0x3000,
        0x1c_0000_0002,
        0x2d0_0000_0500,
    ];

    fn logged() -> DispatchUpscale {
        let mut words = [0u64; 432 / 8];
        words[..12].copy_from_slice(&LOGGED);
        // SAFETY: plain data of the description's size; the bytes read as
        // `u8` flags are zero.
        unsafe { std::mem::transmute(words) }
    }

    #[test]
    fn the_logged_dispatch_reads_as_the_sdk_declares() {
        let d = logged();
        assert_eq!(d.header.ty, DISPATCH_UPSCALE);
        assert_eq!(d.command_list as usize, 0x1000);
        assert_eq!(d.color.resource as usize, 0x2000);
        // Texture 2D, R16G16B16A16_FLOAT, 1280x720, one mip, UAV usage,
        // PIXEL_COMPUTE_READ.
        let c = d.color.desc;
        assert_eq!(
            (c.ty, c.format, c.width, c.height, c.depth, c.mips, c.usage),
            (2, 4, 1280, 720, 1, 1, 2)
        );
        assert_eq!(d.color.state, 0xc);
        assert_eq!(d.depth.resource as usize, 0x3000);
        assert_eq!(
            (d.depth.desc.format, d.depth.desc.width, d.depth.desc.height),
            (28, 1280, 720)
        );
    }

    #[test]
    fn the_titles_create_flags_are_dlss_s() {
        // Logged: flags 0x29.
        let f = flags(0x29);
        assert!(f.hdr && f.depth_inverted && f.auto_exposure);
        assert!(!f.motion_at_display && !f.motion_jittered);
        // HDR, motion vectors at the render size, inverted depth, auto
        // exposure.
        assert_eq!(f.ngx(), 1 | 2 | 8 | 64);
        assert_eq!(flags(DISPLAY_MOTION | MOTION_JITTERED).ngx(), 4);
    }

    #[test]
    fn create_takes_the_largest_render_and_the_output() {
        let c = CreateUpscale {
            header: Header {
                ty: CREATE_UPSCALE,
                next: std::ptr::null_mut(),
            },
            flags: 0x29,
            max_render: [1707, 960],
            max_upscale: [2560, 1440],
            message: std::ptr::null(),
        };
        let d = create(&c, dlss::Mode::Quality);
        assert_eq!(
            (d.render, d.display, d.mode),
            ((1707, 960), (2560, 1440), dlss::Mode::Quality)
        );
        assert_eq!(d.flags, flags(0x29));
    }

    #[test]
    fn dispatch_values_go_to_dlss_as_they_are() {
        let mut d = logged();
        d.jitter = [0.25, -0.125];
        d.motion_scale = [1280.0, -720.0];
        d.render = [1280, 720];
        d.reset = 1;
        d.pre_exposure = 0.5;
        d.frame_ms = 33.3;
        let f = frame(&d, &Tweaks::default());
        assert_eq!(f.jitter, [0.25, -0.125]);
        assert_eq!(f.motion_scale, [1280.0, -720.0]);
        assert_eq!(f.render, (1280, 720));
        assert!(f.reset);
        assert_eq!((f.pre_exposure, f.frame_ms), (0.5, 33.3));
        let flipped = Tweaks::parse(Some("1,-1"), Some("-1"), None);
        let g = frame(&d, &flipped);
        assert_eq!(g.jitter, [0.25, 0.125]);
        assert_eq!(g.motion_scale, [-1280.0, 720.0]);
    }

    #[test]
    fn inputs_follow_the_flags() {
        let mut d = logged();
        d.render = [1280, 720];
        d.motion.resource = 0x4000 as *mut c_void;
        d.exposure.resource = 0x5000 as *mut c_void;
        d.reactive.resource = 0x6000 as *mut c_void;
        d.reactive.desc.width = 1280;
        d.reactive.desc.height = 720;
        d.output.resource = 0x7000 as *mut c_void;
        let i = inputs(&d, true, &Tweaks::default());
        assert_eq!(
            [i.color, i.depth, i.motion, i.output].map(|p| p as usize),
            [0x2000, 0x3000, 0x4000, 0x7000]
        );
        assert!(
            i.exposure.is_null(),
            "automatic exposure: no exposure texture"
        );
        assert_eq!(i.reactive as usize, 0x6000);
        assert_eq!(
            inputs(&d, false, &Tweaks::default()).exposure as usize,
            0x5000
        );
        let off = Tweaks::parse(None, None, Some("0"));
        assert!(inputs(&d, true, &off).reactive.is_null());
        // A mask smaller than the rendered part is not used.
        d.reactive.desc.width = 640;
        assert!(inputs(&d, true, &Tweaks::default()).reactive.is_null());
    }

    #[test]
    fn tweaks_parse() {
        assert_eq!(Tweaks::parse(None, None, None), Tweaks::default());
        let t = Tweaks::parse(Some(" -1 , 1 "), Some("x"), Some("1"));
        assert_eq!(
            (t.jitter, t.motion, t.reactive),
            ([-1.0, 1.0], [1.0, 1.0], true)
        );
        assert_eq!(Tweaks::parse(Some("1,2,3"), None, None).jitter, [1.0, 1.0]);
    }

    #[test]
    fn known_create_descriptions_have_sizes() {
        assert_eq!(create_size(CREATE_UPSCALE), Some(48));
        assert_eq!(create_size(0x100_0002), Some(24), "the title's backend");
        assert_eq!(create_size(2), Some(24), "DX12's");
        assert_eq!(create_size(CREATE_VERSION), Some(24));
        assert_eq!(create_size(0x2_0000), None);
        assert!(!is_backend(QUERY_RATIO));
    }
}
