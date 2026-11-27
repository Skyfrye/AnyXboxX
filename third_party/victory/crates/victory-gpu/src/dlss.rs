//! NVIDIA DLSS in place of the title's FidelityFX upscaler (`VICTORY_DLSS=1`;
//! off by default).
//!
//! The title's FidelityFX calls reach `victory_hle::modules::fidelityfx`,
//! which makes a DLSS [`Feature`] where the title makes an upscaling
//! context, and records its evaluation where the title dispatches the
//! upscale, with the same textures FidelityFX gets ([`dispatch`]). DLSS runs
//! on the host GPU through NVIDIA's NGX runtime, which the driver installs
//! (`_nvngx.dll`); the DLSS model (`nvngx_dlss.dll`) is the user's
//! (`VICTORY_DLSS_DLL`), never shipped here. Without either, or on a card
//! NGX does not run DLSS on, FidelityFX goes on as before ([`readiness`],
//! [`Feature::new`]).
//!
//! `VICTORY_DLSS_MODE` picks the render scale ([`Mode`], Quality by default);
//! the title is made to render at it (`victory_loader::resolution`).

#[cfg(test)]
mod device_tests;
mod ngx;

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::{ID3D12GraphicsCommandList, ID3D12Resource};
use windows::core::Interface;

use crate::external::{READ, WRITE};

/// DLSS's quality modes: the render scale (of the output's width and
/// height) and NGX's `PerfQualityValue`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Quality,
    Balanced,
    Performance,
    UltraPerformance,
    /// Antialiasing only, at the output size.
    Dlaa,
}

impl Mode {
    /// `VICTORY_DLSS_MODE`'s values.
    pub fn parse(spec: &str) -> Option<Mode> {
        Some(
            match spec
                .trim()
                .to_ascii_lowercase()
                .replace(['-', '_', ' '], "")
                .as_str()
            {
                "quality" | "q" => Mode::Quality,
                "balanced" | "b" => Mode::Balanced,
                "performance" | "perf" | "p" => Mode::Performance,
                "ultraperformance" | "up" => Mode::UltraPerformance,
                "dlaa" | "native" => Mode::Dlaa,
                _ => return None,
            },
        )
    }

    /// The render scale. The title truncates output x scale, so these give
    /// DLSS's own render sizes at 2560x1440 (1707x960 for Quality; checked
    /// against DLSS's optimal settings on an RTX 5080, `device_tests`).
    pub fn scale(self) -> f32 {
        match self {
            Mode::Quality => 0.667,
            Mode::Balanced => 0.5801,
            Mode::Performance => 0.5,
            Mode::UltraPerformance => 0.3334,
            Mode::Dlaa => 1.0,
        }
    }

    /// `NVSDK_NGX_PerfQuality_Value`.
    fn perf_quality(self) -> i32 {
        match self {
            Mode::Performance => 0,
            Mode::Balanced => 1,
            Mode::Quality => 2,
            Mode::UltraPerformance => 3,
            Mode::Dlaa => 5,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Mode::Quality => "Quality",
            Mode::Balanced => "Balanced",
            Mode::Performance => "Performance",
            Mode::UltraPerformance => "Ultra Performance",
            Mode::Dlaa => "DLAA",
        }
    }
}

/// The size the title renders at for `output` and `scale`: it multiplies in
/// single precision and truncates.
pub fn render_size(output: (u32, u32), scale: f32) -> (u32, u32) {
    (
        (output.0 as f32 * scale) as u32,
        (output.1 as f32 * scale) as u32,
    )
}

/// How many jitter phases a sequence has for `render_width` and
/// `display_width`: FidelityFX's own count (8 x the scale factor squared,
/// truncated; `ffx_fsr3upscaler.cpp`, MIT), which DLSS's guide asks for too,
/// but never below 1: the title divides by it (`victory_loader::resolution`).
pub fn jitter_phase_count(render_width: u32, display_width: u32) -> i32 {
    if render_width == 0 || display_width == 0 {
        return 8;
    }
    let ratio = display_width as f32 / render_width as f32;
    ((8.0 * ratio * ratio) as i32).max(1)
}

/// The jitter offset of frame `index` in a sequence of `phase_count`
/// (FidelityFX's: Halton bases 2 and 3, centred on the pixel); a count below
/// 1 is taken as 1.
pub fn jitter_offset(index: i32, phase_count: i32) -> (f32, f32) {
    let at = index % phase_count.max(1) + 1;
    (halton(at, 2) - 0.5, halton(at, 3) - 0.5)
}

fn halton(index: i32, base: i32) -> f32 {
    let (mut f, mut result, mut i) = (1.0f32, 0.0f32, index);
    while i > 0 {
        f /= base as f32;
        result += f * (i % base) as f32;
        i /= base;
    }
    result
}

/// What the title says of its inputs when it makes the upscaler.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flags {
    /// The colour is in linear high dynamic range.
    pub hdr: bool,
    /// Motion vectors are at the output size (else at the render size).
    pub motion_at_display: bool,
    /// Motion vectors include the jitter.
    pub motion_jittered: bool,
    /// Depth goes from 1 (near) to 0 (far).
    pub depth_inverted: bool,
    /// No exposure is given: the upscaler works it out.
    pub auto_exposure: bool,
}

impl Flags {
    /// `NVSDK_NGX_DLSS_Feature_Flags`.
    pub fn ngx(self) -> i32 {
        const IS_HDR: i32 = 1 << 0;
        const MV_LOW_RES: i32 = 1 << 1;
        const MV_JITTERED: i32 = 1 << 2;
        const DEPTH_INVERTED: i32 = 1 << 3;
        const AUTO_EXPOSURE: i32 = 1 << 6;
        [
            (self.hdr, IS_HDR),
            (!self.motion_at_display, MV_LOW_RES),
            (self.motion_jittered, MV_JITTERED),
            (self.depth_inverted, DEPTH_INVERTED),
            (self.auto_exposure, AUTO_EXPOSURE),
        ]
        .iter()
        .filter(|(on, _)| *on)
        .fold(0, |flags, (_, bit)| flags | bit)
    }
}

/// A DLSS feature to make: the largest render size, the output size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Create {
    pub render: (u32, u32),
    pub display: (u32, u32),
    pub flags: Flags,
    pub mode: Mode,
}

/// One frame's values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    /// The camera's jitter, in render pixels.
    pub jitter: [f32; 2],
    /// What motion vectors are multiplied by to be in pixels.
    pub motion_scale: [f32; 2],
    /// The part of the inputs rendered this frame.
    pub render: (u32, u32),
    pub reset: bool,
    /// What the colour was multiplied by (1 for none).
    pub pre_exposure: f32,
    pub frame_ms: f32,
}

/// The title's textures for one evaluation (null for an optional one left
/// out): our console objects, which the title keeps alive until the list it
/// records the upscale into has run.
#[derive(Debug, Clone, Copy)]
pub struct Inputs {
    pub color: *const c_void,
    pub depth: *const c_void,
    pub motion: *const c_void,
    pub exposure: *const c_void,
    pub reactive: *const c_void,
    pub output: *const c_void,
}

// SAFETY: pointers to our console objects, which live while lists name them.
unsafe impl Send for Inputs {}

/// Whether DLSS can be tried, from what needs no device: the switch, the
/// model DLL, NGX's runtime from the driver.
#[derive(Debug, Clone, PartialEq)]
pub enum Readiness {
    /// `VICTORY_DLSS` is not `1`.
    Off,
    /// Asked for, and something is missing (why).
    Unusable(String),
    Ready(Setup),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Setup {
    pub mode: Mode,
    /// The user's `nvngx_dlss.dll`.
    pub model: PathBuf,
    /// The driver's `_nvngx.dll`.
    pub core: PathBuf,
}

/// The model's file name: NGX looks for no other.
pub const MODEL_FILE: &str = "nvngx_dlss.dll";

/// [`Readiness`] from the switches: `VICTORY_DLSS=1`, `VICTORY_DLSS_DLL`
/// (the model or its folder), `VICTORY_DLSS_MODE`; said once.
pub fn readiness() -> &'static Readiness {
    static READY: OnceLock<Readiness> = OnceLock::new();
    READY.get_or_init(|| {
        let env = |name| std::env::var_os(name);
        let mode = env("VICTORY_DLSS_MODE").map(|m| m.to_string_lossy().into_owned());
        let ready = check(
            env("VICTORY_DLSS").as_deref().and_then(|v| v.to_str()),
            mode.as_deref(),
            env("VICTORY_DLSS_DLL").map(PathBuf::from).as_deref(),
            ngx::core_path,
        );
        match &ready {
            Readiness::Off => {}
            Readiness::Unusable(why) => {
                tracing::warn!(%why, "DLSS: not used; the title's own upscaler runs as without VICTORY_DLSS");
                println!("dlss           off: {why}; the title's own upscaler runs as without VICTORY_DLSS");
            }
            Readiness::Ready(s) => {
                tracing::info!(mode = s.mode.name(), scale = s.mode.scale(), model = %s.model.display(), core = %s.core.display(), "DLSS: asked for");
                println!(
                    "dlss           asked: {} (render scale {}), model {}, NGX {}",
                    s.mode.name(),
                    s.mode.scale(),
                    s.model.display(),
                    s.core.display()
                );
            }
        }
        ready
    })
}

/// [`readiness`]'s decision, apart from the environment.
fn check(
    switch: Option<&str>,
    mode: Option<&str>,
    model: Option<&Path>,
    core: impl FnOnce() -> Result<PathBuf, String>,
) -> Readiness {
    if switch.map(str::trim) != Some("1") {
        return Readiness::Off;
    }
    let mode = match mode {
        None => Mode::Quality,
        Some(spec) => match Mode::parse(spec) {
            Some(mode) => mode,
            None => {
                return Readiness::Unusable(format!(
                    "VICTORY_DLSS_MODE={spec}: expected quality, balanced, performance, ultra-performance or dlaa"
                ));
            }
        },
    };
    let Some(model) = model else {
        return Readiness::Unusable(format!(
            "VICTORY_DLSS_DLL is not set: it names your {MODEL_FILE} (the DLSS model, from a game of yours) or its folder"
        ));
    };
    let model = if model.is_dir() {
        model.join(MODEL_FILE)
    } else {
        model.to_owned()
    };
    if !model
        .file_name()
        .is_some_and(|n| n.eq_ignore_ascii_case(MODEL_FILE))
    {
        return Readiness::Unusable(format!(
            "VICTORY_DLSS_DLL={}: NGX loads only a file named {MODEL_FILE}",
            model.display()
        ));
    }
    if !model.is_file() {
        return Readiness::Unusable(format!(
            "VICTORY_DLSS_DLL: {} is not there",
            model.display()
        ));
    }
    match core() {
        Ok(core) => Readiness::Ready(Setup { mode, model, core }),
        Err(why) => Readiness::Unusable(why),
    }
}

/// NGX started on the host device, and its parameter object (all NGX calls
/// hold `lock`: NGX is not thread-safe).
struct Ngx {
    runtime: ngx::Runtime,
    params: ngx::Params,
    lock: Mutex<()>,
}

static NGX: OnceLock<Result<Ngx, String>> = OnceLock::new();

/// NGX, started on first use with `setup`.
fn ngx_with(setup: &Setup) -> Result<&'static Ngx, String> {
    NGX.get_or_init(|| start(setup))
        .as_ref()
        .map_err(Clone::clone)
}

fn ngx() -> Result<&'static Ngx, String> {
    match NGX.get() {
        Some(started) => started.as_ref().map_err(Clone::clone),
        None => match readiness() {
            Readiness::Ready(setup) => ngx_with(setup),
            _ => Err("DLSS is not asked for, or cannot be used".into()),
        },
    }
}

fn start(setup: &Setup) -> Result<Ngx, String> {
    let device = crate::external::host_device().ok_or("no host GPU")?;
    let runtime = ngx::Runtime::load(&setup.core)?;
    let models = setup.model.parent().unwrap_or(Path::new("."));
    let data = victory_core::Config::load()
        .ok()
        .and_then(|c| c.cache_subdir("ngx").ok())
        .unwrap_or_else(|| std::env::temp_dir().join("victory-ngx"));
    let _ = std::fs::create_dir_all(&data);
    runtime.init(device, models, &data)?;
    let params = runtime.capability()?;
    params.check()?;
    let available = params.get_i(ngx::name::AVAILABLE).unwrap_or(0);
    if available != 1 {
        let driver = params.get_i(ngx::name::NEEDS_UPDATED_DRIVER).unwrap_or(0);
        let (major, minor) = (
            params.get_ui(ngx::name::MIN_DRIVER_MAJOR).unwrap_or(0),
            params.get_ui(ngx::name::MIN_DRIVER_MINOR).unwrap_or(0),
        );
        let result = params.get_i(ngx::name::INIT_RESULT).unwrap_or(0);
        return Err(if driver != 0 {
            format!("DLSS needs a newer NVIDIA driver ({major}.{minor} or later)")
        } else {
            format!(
                "NGX runs no DLSS here (not an RTX card, or {} did not load: result {result:#x})",
                setup.model.display()
            )
        });
    }
    tracing::info!(core = %setup.core.display(), model = %setup.model.display(), data = %data.display(), "DLSS: NGX started on the host device");
    Ok(Ngx {
        runtime,
        params,
        lock: Mutex::new(()),
    })
}

fn held(m: &Mutex<()>) -> std::sync::MutexGuard<'_, ()> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A DLSS feature (NGX's handle) and what it was made for. Let go of once
/// nothing records with it, and released once the card has run what did.
pub struct Feature {
    handle: usize,
    pub create: Create,
    /// DLSS's own render size for the mode and output, for the log.
    pub optimal: Option<(u32, u32)>,
    /// The first evaluation was said.
    said: std::sync::atomic::AtomicBool,
}

impl Feature {
    /// Starts NGX if needed and makes DLSS's feature for `create`, waiting
    /// for the card to have run its setup. Why not, if not: then the title's
    /// own upscaler goes on.
    pub fn new(create: Create) -> Result<Arc<Feature>, String> {
        let ngx = ngx()?;
        Self::made(ngx, create)
    }

    fn made(ngx: &'static Ngx, create: Create) -> Result<Arc<Feature>, String> {
        let (w, h) = create.render;
        if w == 0 || h == 0 || create.display.0 < w || create.display.1 < h {
            return Err(format!(
                "render size {w}x{h} for an output of {}x{}",
                create.display.0, create.display.1
            ));
        }
        let optimal = optimal_size(ngx, create.display, create.mode);
        let made = crate::external::run_now(|pass| {
            use ngx::name::*;
            // (Released before the card runs it: other lists' evaluations
            // need the lock meanwhile.)
            let _held = held(&ngx.lock);
            let p = &ngx.params;
            p.set_ui(CREATION_NODE_MASK, 1);
            p.set_ui(VISIBILITY_NODE_MASK, 1);
            p.set_ui(WIDTH, w);
            p.set_ui(HEIGHT, h);
            p.set_ui(OUT_WIDTH, create.display.0);
            p.set_ui(OUT_HEIGHT, create.display.1);
            p.set_i(PERF_QUALITY, create.mode.perf_quality());
            p.set_i(CREATE_FLAGS, create.flags.ngx());
            p.set_i(OUTPUT_SUBRECTS, 0);
            ngx.runtime.create(pass.command_list(), p)
        })
        .ok_or("no host GPU")?;
        let handle = made
            .map_err(|result| format!("NGX did not make the DLSS feature (result {result:#x})"))?;
        tracing::info!(
            render = format_args!("{w}x{h}"),
            output = format_args!("{}x{}", create.display.0, create.display.1),
            mode = create.mode.name(),
            flags = format_args!("{:#x}", create.flags.ngx()),
            ?optimal,
            "DLSS: feature made"
        );
        Ok(Arc::new(Feature {
            handle: handle as usize,
            create,
            optimal,
            said: Default::default(),
        }))
    }

    /// Records the evaluation on `list` with host textures `t` and `frame`.
    fn record(
        &self,
        list: &ID3D12GraphicsCommandList,
        t: &Textures,
        frame: &Frame,
    ) -> Result<(), String> {
        use ngx::name::*;
        let ngx = ngx()?;
        let raw = |r: Option<&ID3D12Resource>| r.map_or(std::ptr::null_mut(), Interface::as_raw);
        let (w, h) = clamp_render(frame.render, self.create.render);
        let _held = held(&ngx.lock);
        let p = &ngx.params;
        p.set_resource(COLOR, t.color.as_raw());
        p.set_resource(OUTPUT, t.output.as_raw());
        p.set_resource(DEPTH, t.depth.as_raw());
        p.set_resource(MOTION_VECTORS, t.motion.as_raw());
        p.set_f(JITTER_X, frame.jitter[0]);
        p.set_f(JITTER_Y, frame.jitter[1]);
        p.set_f(SHARPNESS, 0.0);
        p.set_i(RESET, i32::from(frame.reset));
        p.set_f(MV_SCALE_X, nonzero(frame.motion_scale[0]));
        p.set_f(MV_SCALE_Y, nonzero(frame.motion_scale[1]));
        p.set_resource(TRANSPARENCY_MASK, std::ptr::null_mut());
        p.set_resource(EXPOSURE_TEXTURE, raw(t.exposure));
        p.set_resource(BIAS_COLOR_MASK, raw(t.reactive));
        for base in SUBRECT_BASES {
            p.set_ui(base, 0);
        }
        p.set_ui(RENDER_WIDTH, w);
        p.set_ui(RENDER_HEIGHT, h);
        p.set_f(PRE_EXPOSURE, nonzero(frame.pre_exposure));
        p.set_f(EXPOSURE_SCALE, 1.0);
        p.set_i(INVERT_X, 0);
        p.set_i(INVERT_Y, 0);
        p.set_f(FRAME_MS, frame.frame_ms);
        let result = ngx
            .runtime
            .evaluate(list.as_raw(), self.handle as *mut c_void, p);
        if ngx::succeeded(result) {
            Ok(())
        } else {
            Err(format!("NGX did not evaluate DLSS (result {result:#x})"))
        }
    }
}

impl Drop for Feature {
    fn drop(&mut self) {
        /// The handle, released when dropped.
        struct Release(usize);
        impl Drop for Release {
            fn drop(&mut self) {
                if let Ok(ngx) = ngx() {
                    let _held = held(&ngx.lock);
                    let result = ngx.runtime.release(self.0 as *mut c_void);
                    tracing::info!(
                        result = format_args!("{result:#x}"),
                        "DLSS: feature released"
                    );
                }
            }
        }
        // After what was submitted so far has run.
        crate::host::retire(Release(self.handle));
    }
}

/// DLSS's own render size for `display` and `mode` (its optimal-settings
/// call), for the log.
fn optimal_size(ngx: &Ngx, display: (u32, u32), mode: Mode) -> Option<(u32, u32)> {
    use ngx::name::*;
    type Callback = unsafe extern "C" fn(*mut ngx::Param) -> ngx::NgxResult;
    let _held = held(&ngx.lock);
    let p = &ngx.params;
    let callback = p.get_ptr(OPTIMAL_SETTINGS).filter(|f| !f.is_null())?;
    p.set_ui(WIDTH, display.0);
    p.set_ui(HEIGHT, display.1);
    p.set_i(PERF_QUALITY, mode.perf_quality());
    p.set_i(RTX_VALUE, 0);
    // SAFETY: NGX's callback of this signature, with its parameter object.
    let result = unsafe { std::mem::transmute::<*mut c_void, Callback>(callback)(p.0) };
    if !ngx::succeeded(result) {
        return None;
    }
    Some((p.get_ui(OUT_WIDTH)?, p.get_ui(OUT_HEIGHT)?))
}

/// NGX takes 0 as "not given" only in its helpers: 1 here.
fn nonzero(v: f32) -> f32 {
    if v == 0.0 { 1.0 } else { v }
}

/// The rendered part, at most the feature's render size (0: all of it).
fn clamp_render(frame: (u32, u32), feature: (u32, u32)) -> (u32, u32) {
    let pick = |f: u32, m: u32| if f == 0 { m } else { f.min(m) };
    (pick(frame.0, feature.0), pick(frame.1, feature.1))
}

/// The host textures of one evaluation.
struct Textures<'a> {
    color: &'a ID3D12Resource,
    depth: &'a ID3D12Resource,
    motion: &'a ID3D12Resource,
    exposure: Option<&'a ID3D12Resource>,
    reactive: Option<&'a ID3D12Resource>,
    output: &'a ID3D12Resource,
}

/// Records DLSS's evaluation with `feature` at this place of title list
/// `list`, with the textures FidelityFX would have been given. It runs when
/// the list's translation gets there; `failed` is called then if it could
/// not (a texture that is none of the title's, or NGX's refusal). False if
/// `list` records nothing now (not one of ours, or closed).
pub fn dispatch(
    feature: &Arc<Feature>,
    list: *const c_void,
    inputs: Inputs,
    frame: Frame,
    failed: impl FnOnce(String) + Send + 'static,
) -> bool {
    let feature = feature.clone();
    crate::external::record(list, move |pass| {
        if let Err(why) = evaluate_in(&feature, pass, &inputs, &frame) {
            failed(why);
        }
    })
}

fn evaluate_in(
    feature: &Arc<Feature>,
    pass: &mut crate::external::Pass,
    i: &Inputs,
    frame: &Frame,
) -> Result<(), String> {
    let mut need = |texture: *const c_void, what: &str, write: bool| {
        let state = if write { WRITE } else { READ };
        pass.resource(texture, state, write)
            .ok_or_else(|| format!("the {what} ({texture:?}) is not a texture of the title"))
    };
    let color = need(i.color, "colour", false)?;
    let depth = need(i.depth, "depth", false)?;
    let motion = need(i.motion, "motion vectors", false)?;
    let exposure = if i.exposure.is_null() {
        None
    } else {
        need(i.exposure, "exposure", false).ok()
    };
    let reactive = if i.reactive.is_null() {
        None
    } else {
        need(i.reactive, "reactive mask", false).ok()
    };
    let output = need(i.output, "output", true)?;
    let textures = Textures {
        color,
        depth,
        motion,
        exposure,
        reactive,
        output,
    };
    if !feature
        .said
        .swap(true, std::sync::atomic::Ordering::Relaxed)
    {
        say_first(feature, &textures, frame);
    }
    // The feature lives until the card has run this list.
    let keep = feature.clone();
    pass.host_list().defer_host(move || {
        let _ = &keep;
    });
    feature.record(pass.list(), &textures, frame)
}

/// The first evaluation of a feature, for the log: sizes and formats.
fn say_first(feature: &Feature, t: &Textures, frame: &Frame) {
    let of = |r: &ID3D12Resource| {
        // SAFETY: a live resource.
        let d = unsafe { r.GetDesc() };
        format!(
            "{}x{} format {} flags {:#x}",
            d.Width, d.Height, d.Format.0, d.Flags.0
        )
    };
    tracing::info!(
        color = of(t.color),
        depth = of(t.depth),
        motion = of(t.motion),
        output = of(t.output),
        exposure = t.exposure.is_some(),
        reactive = t.reactive.is_some(),
        render = format_args!("{}x{}", frame.render.0, frame.render.1),
        jitter = ?frame.jitter,
        motion_scale = ?frame.motion_scale,
        mode = feature.create.mode.name(),
        "DLSS: first evaluation"
    );
    println!(
        "dlss           upscaling {}x{} to {}x{} ({})",
        frame.render.0,
        frame.render.1,
        feature.create.display.0,
        feature.create.display.1,
        feature.create.mode.name()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_give_dlss_render_sizes_at_1440p() {
        let at = |m: Mode| render_size((2560, 1440), m.scale());
        assert_eq!(at(Mode::Quality), (1707, 960));
        assert_eq!(at(Mode::Balanced), (1485, 835));
        assert_eq!(at(Mode::Performance), (1280, 720));
        assert_eq!(at(Mode::UltraPerformance), (853, 480));
        assert_eq!(at(Mode::Dlaa), (2560, 1440));
        assert_eq!(
            render_size((3840, 2160), Mode::Quality.scale()),
            (2561, 1440)
        );
    }

    #[test]
    fn modes_parse() {
        assert_eq!(Mode::parse("Quality"), Some(Mode::Quality));
        assert_eq!(
            Mode::parse(" ultra-performance "),
            Some(Mode::UltraPerformance)
        );
        assert_eq!(
            Mode::parse("ultra_performance"),
            Some(Mode::UltraPerformance)
        );
        assert_eq!(Mode::parse("perf"), Some(Mode::Performance));
        assert_eq!(Mode::parse("DLAA"), Some(Mode::Dlaa));
        assert_eq!(Mode::parse("max"), None);
        assert_eq!(
            [
                Mode::Quality,
                Mode::Balanced,
                Mode::Performance,
                Mode::UltraPerformance,
                Mode::Dlaa
            ]
            .map(Mode::perf_quality),
            [2, 1, 0, 3, 5]
        );
    }

    #[test]
    fn the_phase_count_is_fidelityfx_s_and_never_zero() {
        assert_eq!(jitter_phase_count(1280, 1920), 18);
        assert_eq!(jitter_phase_count(1707, 2560), 17);
        assert_eq!(jitter_phase_count(1280, 2560), 32);
        assert_eq!(jitter_phase_count(2560, 2560), 8);
        // A display smaller than a third of the render size gave 0.
        assert_eq!(jitter_phase_count(2560, 800), 1);
        assert_eq!(jitter_phase_count(0, 2560), 8);
        assert_eq!(jitter_phase_count(1280, 0), 8);
    }

    #[test]
    fn jitter_offsets_follow_halton_2_3() {
        assert_eq!(jitter_offset(0, 8), (0.0, 1.0 / 3.0 - 0.5));
        assert_eq!(jitter_offset(1, 8), (-0.25, 2.0 / 3.0 - 0.5));
        assert_eq!(jitter_offset(8, 8), jitter_offset(0, 8));
        // A zero count is one phase, not a division by zero.
        assert_eq!(jitter_offset(5, 0), jitter_offset(0, 1));
        for i in 0..64 {
            let (x, y) = jitter_offset(i, 32);
            assert!((-0.5..0.5).contains(&x) && (-0.5..0.5).contains(&y));
        }
    }

    #[test]
    fn flags_map_to_ngx() {
        // The title's 0x29: HDR, inverted depth, auto exposure; motion at
        // render size without jitter.
        let title = Flags {
            hdr: true,
            depth_inverted: true,
            auto_exposure: true,
            ..Flags::default()
        };
        assert_eq!(title.ngx(), 1 | 2 | 8 | 64);
        let other = Flags {
            motion_at_display: true,
            motion_jittered: true,
            ..Flags::default()
        };
        assert_eq!(other.ngx(), 4);
    }

    #[test]
    fn render_is_clamped_to_the_feature() {
        assert_eq!(clamp_render((0, 0), (1707, 960)), (1707, 960));
        assert_eq!(clamp_render((1280, 720), (1707, 960)), (1280, 720));
        assert_eq!(clamp_render((2560, 1440), (1707, 960)), (1707, 960));
        assert_eq!(nonzero(0.0), 1.0);
        assert_eq!(nonzero(0.5), 0.5);
    }

    #[test]
    fn readiness_falls_back_when_something_is_missing() {
        let dir = std::env::temp_dir().join(format!("victory-dlss-ready-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let model = dir.join(MODEL_FILE);
        std::fs::write(&model, b"not a dll").unwrap();
        let wrong = dir.join("dlss.dll");
        std::fs::write(&wrong, b"not a dll").unwrap();
        let core = || Ok(PathBuf::from("C:/driver/_nvngx.dll"));
        let ready = |m: Mode, model: &Path| {
            Readiness::Ready(Setup {
                mode: m,
                model: model.to_owned(),
                core: "C:/driver/_nvngx.dll".into(),
            })
        };
        assert_eq!(check(None, None, Some(&model), core), Readiness::Off);
        assert_eq!(check(Some("0"), None, Some(&model), core), Readiness::Off);
        assert_eq!(
            check(Some("1"), None, Some(&model), core),
            ready(Mode::Quality, &model)
        );
        // The folder names the model too.
        assert_eq!(
            check(Some("1"), Some("performance"), Some(&dir), core),
            ready(Mode::Performance, &model)
        );
        let unusable = |r: Readiness, says: &str| match r {
            Readiness::Unusable(why) => assert!(why.contains(says), "{why}"),
            other => panic!("{other:?}"),
        };
        unusable(
            check(Some("1"), None, None, core),
            "VICTORY_DLSS_DLL is not set",
        );
        unusable(
            check(Some("1"), None, Some(&wrong), core),
            "only a file named nvngx_dlss.dll",
        );
        unusable(
            check(
                Some("1"),
                None,
                Some(&dir.join("gone").join(MODEL_FILE)),
                core,
            ),
            "is not there",
        );
        unusable(
            check(Some("1"), Some("fast"), Some(&model), core),
            "VICTORY_DLSS_MODE=fast",
        );
        unusable(
            check(Some("1"), None, Some(&model), || {
                Err("no NGXCore key".into())
            }),
            "no NGXCore key",
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
