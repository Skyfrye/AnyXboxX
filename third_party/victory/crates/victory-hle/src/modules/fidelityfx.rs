//! `amd_fidelityfx_loader_xbox.dll`, answered here when NVIDIA DLSS can be
//! tried (`VICTORY_DLSS=1`, `victory_gpu::dlss::readiness`): the title's
//! FidelityFX upscaler calls (`ffxCreateContext`, `ffxDispatch`, `ffxQuery`,
//! `ffxDestroyContext`, and `ffxConfigure`) come here with their
//! descriptions ([`desc`]).
//!
//! An upscale context the title makes is ours if DLSS makes a feature for
//! it: its dispatches record DLSS's evaluation where FidelityFX would have
//! recorded its passes, with the same textures, jitter, motion vector scale
//! and reset; its jitter queries are answered here. Any other context (DLSS
//! could not be made: no NGX on this card, no feature) goes to the title's
//! FidelityFX, its upscaler DLL, which the loader would have loaded. A
//! context of ours whose dispatch DLSS cannot take gets a FidelityFX
//! context of its own from then on, made from a copy of the title's
//! description.
//!
//! The jitter phase count the title asks for is never 0: the title divides
//! by it (`victory_loader::resolution`'s notes). Ours is FidelityFX's own
//! count; one FidelityFX gives as 0 or less is replaced by that count.
//!
//! Source: Neptune (VIctory Edit.) 2026-11-25c, `crates/victory-hle/src/
//! modules/fidelityfx.rs` (the FidelityFX redirect patch received for
//! DexNom Runtime, there to AMD's PC DLL with `VICTORY_FSR_DLL`): the
//! interception of the loader, its description walk and log. Adapted: the
//! backend is DLSS, the fallback the title's own FidelityFX.

#![allow(non_snake_case)]

mod desc;

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use victory_gpu::{dlss, external};

use crate::HleModule;
use desc::*;

/// The title's loader, which we answer for.
const LOADER: &str = "amd_fidelityfx_loader_xbox.dll";
/// The title's upscaler, which its loader loads: where other contexts go.
const UPSCALER: &str = "amd_fidelityfx_upscaler_xbox.dll";
const DLLS: [&str; 1] = [LOADER];

/// Whether the loader is answered here: DLSS can be tried.
pub fn enabled() -> bool {
    matches!(dlss::readiness(), dlss::Readiness::Ready(_))
}

/// The render scale the title's FidelityFX is to render at, if DLSS can be
/// tried (`victory_loader::resolution::apply_asked`).
pub fn render_scale() -> Option<f32> {
    match dlss::readiness() {
        dlss::Readiness::Ready(setup) => Some(setup.mode.scale()),
        _ => None,
    }
}

fn mode() -> dlss::Mode {
    match dlss::readiness() {
        dlss::Readiness::Ready(setup) => setup.mode,
        _ => dlss::Mode::Quality,
    }
}

fn tweaks() -> &'static Tweaks {
    static TWEAKS: OnceLock<Tweaks> = OnceLock::new();
    TWEAKS.get_or_init(|| {
        let var = |n| std::env::var(n).ok();
        let t = Tweaks::parse(
            var("VICTORY_DLSS_JITTER_SCALE").as_deref(),
            var("VICTORY_DLSS_MV_SCALE").as_deref(),
            var("VICTORY_DLSS_REACTIVE").as_deref(),
        );
        if t != Tweaks::default() {
            tracing::info!(?t, "DLSS: the title's jitter, motion vector scale or reactive mask changed by the switches");
        }
        t
    })
}

type Create = unsafe extern "system" fn(*mut *mut c_void, *mut Header, *const c_void) -> u32;
type Destroy = unsafe extern "system" fn(*mut *mut c_void, *const c_void) -> u32;
type Call = unsafe extern "system" fn(*mut *mut c_void, *mut Header) -> u32;

/// The title's FidelityFX upscaler.
struct Console {
    create: Create,
    destroy: Destroy,
    configure: Call,
    query: Call,
    dispatch: Call,
}

/// The title's upscaler DLL, mapped on first use as its loader would.
fn console() -> Option<&'static Console> {
    static CONSOLE: OnceLock<Option<Console>> = OnceLock::new();
    CONSOLE
        .get_or_init(|| match crate::runtime::load_title_dll(UPSCALER) {
            Ok(Some(base)) => {
                let found = exports(base);
                if found.is_none() {
                    tracing::error!(dll = UPSCALER, "FidelityFX: the title's upscaler lacks an export");
                }
                found
            }
            Ok(None) => {
                tracing::error!(dll = UPSCALER, "FidelityFX: the title's upscaler is not in its folder");
                None
            }
            Err(error) => {
                tracing::error!(dll = UPSCALER, %error, "FidelityFX: the title's upscaler did not load");
                None
            }
        })
        .as_ref()
}

fn exports(base: usize) -> Option<Console> {
    use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
    let export = |name: &[u8]| {
        // SAFETY: a mapped title image and a NUL-terminated name.
        unsafe { GetProcAddress(base as *mut c_void, name.as_ptr()) }.map(|f| f as usize)
    };
    // SAFETY: the FidelityFX API's five exports (`ffx_api.h`).
    unsafe {
        Some(Console {
            create: std::mem::transmute::<usize, Create>(export(b"ffxCreateContext\0")?),
            destroy: std::mem::transmute::<usize, Destroy>(export(b"ffxDestroyContext\0")?),
            configure: std::mem::transmute::<usize, Call>(export(b"ffxConfigure\0")?),
            query: std::mem::transmute::<usize, Call>(export(b"ffxQuery\0")?),
            dispatch: std::mem::transmute::<usize, Call>(export(b"ffxDispatch\0")?),
        })
    }
}

/// A copy of the title's create descriptions, to make a FidelityFX context
/// from later: the known ones, linked again ([`create_size`]).
struct Saved {
    nodes: Vec<Vec<u64>>,
    /// Every description of the chain was copied.
    whole: bool,
}

impl Saved {
    /// # Safety
    /// `head` is the title's chain of descriptions.
    unsafe fn of(head: *mut Header) -> Saved {
        let mut saved = Saved {
            nodes: Vec::new(),
            whole: true,
        };
        for node in chain(head) {
            // SAFETY: a description of the title's.
            let ty = unsafe { (*node).ty };
            match create_size(ty) {
                Some(bytes) => {
                    let mut words = vec![0u64; bytes / 8];
                    // SAFETY: a description of that type has that size.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            node.cast::<u8>(),
                            words.as_mut_ptr().cast(),
                            bytes,
                        )
                    };
                    saved.nodes.push(words);
                }
                None => saved.whole = false,
            }
        }
        saved
    }

    /// The copy, linked: valid while `self` is.
    fn linked(&mut self) -> *mut Header {
        let mut next: *mut Header = std::ptr::null_mut();
        for node in self.nodes.iter_mut().rev() {
            node[1] = next as u64;
            next = node.as_mut_ptr().cast();
        }
        next
    }
}

/// A context of ours: the DLSS feature made for the title's context.
struct Ours {
    feature: Arc<dlss::Feature>,
    auto_exposure: bool,
    saved: Mutex<Saved>,
    /// DLSS cannot take this context's dispatches: FidelityFX does.
    broken: Arc<AtomicBool>,
    /// The FidelityFX context made for it then (0: none yet).
    console: Mutex<usize>,
}

impl Ours {
    /// The FidelityFX context to go on with, made now if needed.
    fn console_context(&self) -> Option<usize> {
        let mut context = self.console.lock().unwrap_or_else(|e| e.into_inner());
        if *context != 0 {
            return Some(*context);
        }
        let console = console()?;
        let mut saved = self.saved.lock().unwrap_or_else(|e| e.into_inner());
        if !saved.whole {
            tracing::warn!(
                "FidelityFX: the title's context had descriptions not known here; none is made in place of DLSS"
            );
            return None;
        }
        let mut made: *mut c_void = std::ptr::null_mut();
        // SAFETY: the title's upscaler, with a copy of the title's
        // descriptions; its own allocation.
        let code = unsafe { (console.create)(&mut made, saved.linked(), std::ptr::null()) };
        tracing::info!(code, "FidelityFX: a context made in place of DLSS's");
        (code == OK && !made.is_null()).then(|| {
            *context = made as usize;
            *context
        })
    }

    fn give_up(&self, why: &str) {
        if !self.broken.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                why,
                "DLSS: stops for this upscaler; the title's FidelityFX upscales from now on"
            );
            println!("dlss           stopped: {why}; FidelityFX upscales from now on");
        }
    }
}

/// Our contexts, by the handle the title holds.
static OURS: Mutex<Vec<(usize, Arc<Ours>)>> = Mutex::new(Vec::new());

/// The context of ours `context` points at, if it is one.
fn ours(context: *mut *mut c_void) -> Option<Arc<Ours>> {
    // SAFETY: the title passes a context pointer (null tolerated).
    let handle = unsafe { context.as_ref() }.map(|c| *c as usize)?;
    let ours = OURS.lock().unwrap_or_else(|e| e.into_inner());
    ours.iter()
        .find(|(h, _)| *h == handle)
        .map(|(_, o)| o.clone())
}

/// The descriptions of a chain, at most 32.
fn chain(head: *mut Header) -> impl Iterator<Item = *mut Header> {
    let mut node = head;
    std::iter::from_fn(move || {
        if node.is_null() {
            return None;
        }
        let this = node;
        // SAFETY: a description of the title's: its type, then the next.
        node = unsafe { (*this).next };
        Some(this)
    })
    .take(32)
}

/// The first description of type `ty` in a chain.
fn find(head: *mut Header, ty: u64) -> Option<*mut Header> {
    // SAFETY: descriptions of the title's.
    chain(head).find(|&n| unsafe { (*n).ty } == ty)
}

/// Logs the description types a call carries, once per type, with their
/// first words: the console build's are not documented.
fn note(call: &'static str, head: *mut Header) {
    static SEEN: Mutex<Vec<(&str, u64)>> = Mutex::new(Vec::new());
    for node in chain(head) {
        // SAFETY: a description of the title's.
        let ty = unsafe { (*node).ty };
        let fresh = {
            let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
            let fresh = !seen.contains(&(call, ty));
            if fresh {
                seen.push((call, ty));
            }
            fresh
        };
        if fresh {
            // SAFETY: the first words of a description, to learn its layout.
            let words: Vec<String> = (0..12)
                .map(|i| {
                    format!("{:#x}", unsafe {
                        node.cast::<u64>().add(i).read_unaligned()
                    })
                })
                .collect();
            tracing::info!(
                call,
                ty = format_args!("{ty:#x}"),
                ?words,
                "FidelityFX description"
            );
        }
    }
}

/// Says `what` once per text.
fn once(what: String) {
    static SAID: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if !said.contains(&what) {
        tracing::info!("{what}");
        println!("dlss           {what}");
        said.push(what);
    }
}

/// `ffxCreateContext(context, desc, allocation callbacks)`: an upscale
/// context becomes ours if DLSS makes a feature for it; anything else is
/// the title's FidelityFX's.
extern "system" fn ffxCreateContext(
    context: *mut *mut c_void,
    desc: *mut Header,
    alloc: *const c_void,
) -> u32 {
    if context.is_null() || desc.is_null() {
        return ERROR_PARAMETER;
    }
    note("create", desc);
    if let Some(upscale) = find(desc, CREATE_UPSCALE) {
        // SAFETY: a description of this type.
        let c = unsafe { &*upscale.cast::<CreateUpscale>() };
        let asked = desc::create(c, mode());
        match dlss::Feature::new(asked) {
            Ok(feature) => {
                let ours = Arc::new(Ours {
                    feature,
                    auto_exposure: asked.flags.auto_exposure,
                    // SAFETY: the title's chain.
                    saved: Mutex::new(unsafe { Saved::of(desc) }),
                    broken: Arc::new(AtomicBool::new(false)),
                    console: Mutex::new(0),
                });
                let handle = Arc::as_ptr(&ours) as usize;
                OURS.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((handle, ours));
                // SAFETY: checked non-null; the title's out value.
                unsafe { context.write(handle as *mut c_void) };
                once(format!(
                    "on: the title's upscaler is DLSS {} ({}x{} to {}x{})",
                    asked.mode.name(),
                    asked.render.0,
                    asked.render.1,
                    asked.display.0,
                    asked.display.1
                ));
                return OK;
            }
            Err(why) => once(format!("not used: {why}; the title's FidelityFX upscales")),
        }
    }
    let Some(console) = console() else {
        return ERROR;
    };
    // SAFETY: the title's upscaler, with the title's own arguments.
    unsafe { (console.create)(context, desc, alloc) }
}

/// `ffxDestroyContext(context, allocation callbacks)`.
extern "system" fn ffxDestroyContext(context: *mut *mut c_void, alloc: *const c_void) -> u32 {
    let Some(ours) = ours(context) else {
        let Some(console) = console() else {
            return ERROR;
        };
        // SAFETY: the title's upscaler, with the title's own arguments.
        return unsafe { (console.destroy)(context, alloc) };
    };
    let handle = Arc::as_ptr(&ours) as usize;
    OURS.lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(h, _)| *h != handle);
    let mut made = *ours.console.lock().unwrap_or_else(|e| e.into_inner()) as *mut c_void;
    if !made.is_null()
        && let Some(console) = console()
    {
        // SAFETY: the context the title's upscaler made for ours.
        unsafe { (console.destroy)(&mut made, std::ptr::null()) };
    }
    // SAFETY: the title's context pointer (checked by `ours`).
    unsafe { context.write(std::ptr::null_mut()) };
    // The feature goes when the lists that use it have run.
    OK
}

/// `ffxConfigure(context, desc)`: FidelityFX's settings mean nothing to
/// DLSS.
extern "system" fn ffxConfigure(context: *mut *mut c_void, desc: *mut Header) -> u32 {
    note("configure", desc);
    match ours(context) {
        Some(ours) => match *ours.console.lock().unwrap_or_else(|e| e.into_inner()) {
            0 => OK,
            made => forward(|c| c.configure, made, desc),
        },
        None => {
            let Some(console) = console() else {
                return ERROR;
            };
            // SAFETY: the title's upscaler, with the title's own arguments.
            unsafe { (console.configure)(context, desc) }
        }
    }
}

/// A call to the title's upscaler for a context it made for one of ours.
fn forward(call: impl FnOnce(&Console) -> Call, made: usize, desc: *mut Header) -> u32 {
    let Some(console) = console() else {
        return ERROR;
    };
    let mut made = made as *mut c_void;
    // SAFETY: the title's upscaler, its own context, the title's description.
    unsafe { call(console)(&mut made, desc) }
}

/// The last jitter phase count answered, for an offset asked with none.
static PHASES: AtomicI32 = AtomicI32::new(8);

/// `ffxQuery(context, desc)`: of ours (or of none, while DLSS is on), the
/// jitter and render size are answered here; of others the title's
/// FidelityFX answers, with its phase count kept above 0.
extern "system" fn ffxQuery(context: *mut *mut c_void, desc: *mut Header) -> u32 {
    if desc.is_null() {
        return ERROR_PARAMETER;
    }
    note("query", desc);
    // SAFETY: the title passes a context pointer or null.
    let none = unsafe { context.as_ref() }.is_none_or(|c| c.is_null());
    let ours = ours(context);
    if ours.is_some() || none {
        // SAFETY: a description of the title's.
        if let Some(code) = unsafe { answer(desc) } {
            return code;
        }
        if let Some(ours) = &ours {
            // FidelityFX's, once it took over this context.
            return match ours
                .broken
                .load(Ordering::Relaxed)
                .then(|| ours.console_context())
                .flatten()
            {
                Some(made) => forward(|c| c.query, made, desc),
                None => UNKNOWN_DESCTYPE,
            };
        }
    }
    let Some(console) = console() else {
        return ERROR;
    };
    // SAFETY: the title's upscaler, with the title's own arguments.
    let code = unsafe { (console.query)(context, desc) };
    // SAFETY: as above.
    unsafe { keep_phases(desc) };
    code
}

/// What we answer of a query, if we do.
///
/// # Safety
/// `desc` is a description of the title's.
unsafe fn answer(desc: *mut Header) -> Option<u32> {
    // SAFETY: the caller's; each cast is to the type the description says.
    unsafe {
        match (*desc).ty {
            QUERY_PHASE_COUNT => {
                let q = &*desc.cast::<QueryPhaseCount>();
                let count = dlss::jitter_phase_count(q.render_width, q.display_width);
                PHASES.store(count, Ordering::Relaxed);
                if let Some(out) = q.out.as_mut() {
                    *out = count;
                }
            }
            QUERY_JITTER_OFFSET => {
                let q = &*desc.cast::<QueryJitterOffset>();
                let count = if q.phase_count > 0 {
                    q.phase_count
                } else {
                    PHASES.load(Ordering::Relaxed)
                };
                let (x, y) = dlss::jitter_offset(q.index, count);
                if let Some(out) = q.out_x.as_mut() {
                    *out = x;
                }
                if let Some(out) = q.out_y.as_mut() {
                    *out = y;
                }
            }
            QUERY_RENDER_SIZE => {
                let q = &*desc.cast::<QueryRenderSize>();
                let (w, h) = dlss::render_size((q.display[0], q.display[1]), mode().scale());
                if let (Some(ow), Some(oh)) = (q.out_width.as_mut(), q.out_height.as_mut()) {
                    (*ow, *oh) = (w, h);
                }
            }
            QUERY_RATIO => {
                let q = &*desc.cast::<QueryRatio>();
                if let Some(out) = q.out.as_mut() {
                    *out = 1.0 / mode().scale();
                }
            }
            QUERY_PROVIDER_VERSION => {
                let q = &mut *desc.cast::<QueryProviderVersion>();
                q.version_id = 0;
                q.version_name = c"NVIDIA DLSS (VIctory)".as_ptr();
            }
            QUERY_MEMORY => {
                if let Some(out) = (*desc.cast::<QueryMemory>()).usage.as_mut() {
                    *out = [0, 0];
                }
            }
            QUERY_MEMORY_V2 => {
                // The usage pointer, after the sizes and flags.
                if let Some(out) = desc
                    .cast::<u8>()
                    .add(56)
                    .cast::<*mut [u64; 2]>()
                    .read_unaligned()
                    .as_mut()
                {
                    *out = [0, 0];
                }
            }
            QUERY_RESOURCES => {
                let q = &mut *desc.cast::<QueryResources>();
                // Colour, depth and motion vectors; exposure and the
                // reactive mask if there are.
                (q.required, q.optional) = (0b111, 0b11000);
            }
            _ => return None,
        }
    }
    Some(OK)
}

/// After FidelityFX answered a phase count: one below 1 is replaced by
/// FidelityFX's own count (the title divides by it).
///
/// # Safety
/// `desc` is a description of the title's.
unsafe fn keep_phases(desc: *mut Header) {
    // SAFETY: the caller's; a description of the type it says.
    unsafe {
        if (*desc).ty != QUERY_PHASE_COUNT {
            return;
        }
        let q = &*desc.cast::<QueryPhaseCount>();
        let Some(out) = q.out.as_mut() else { return };
        if *out < 1 {
            let count = dlss::jitter_phase_count(q.render_width, q.display_width);
            once(format!(
                "FidelityFX gave a jitter phase count of {} for {} to {} pixels wide: {count} instead",
                *out, q.render_width, q.display_width
            ));
            *out = count;
        }
        PHASES.store(*out, Ordering::Relaxed);
    }
}

/// Why a dispatch cannot go to DLSS, if it cannot.
fn refused(d: &DispatchUpscale) -> Option<String> {
    if !external::is_title_list(d.command_list) {
        return Some("the dispatch's command list is not one of ours".into());
    }
    for (what, r) in [
        ("colour", &d.color),
        ("depth", &d.depth),
        ("motion vectors", &d.motion),
    ] {
        if external::host_texture(r.resource).is_none() {
            return Some(format!("the {what} is not a texture of the title"));
        }
    }
    match external::host_texture(d.output.resource) {
        None => Some("the output is not a texture of the title".into()),
        Some(t) if !t.unordered => Some(format!(
            "the output (format {}) takes no unordered access on the host",
            t.format
        )),
        Some(_) => None,
    }
}

/// `ffxDispatch(context, desc)`: an upscale of ours records DLSS's
/// evaluation in the title's list.
extern "system" fn ffxDispatch(context: *mut *mut c_void, desc: *mut Header) -> u32 {
    if desc.is_null() {
        return ERROR_PARAMETER;
    }
    note("dispatch", desc);
    let Some(ours) = ours(context) else {
        let Some(console) = console() else {
            return ERROR;
        };
        // SAFETY: the title's upscaler, with the title's own arguments.
        return unsafe { (console.dispatch)(context, desc) };
    };
    // SAFETY: a description of the title's: its type first.
    let ty = unsafe { (*desc).ty };
    let broken = ours.broken.load(Ordering::Relaxed);
    match ty {
        DISPATCH_UPSCALE if !broken => {
            // SAFETY: a description of this type.
            let d = unsafe { &*desc.cast::<DispatchUpscale>() };
            if upscale(&ours, d) {
                return OK;
            }
        }
        // DLSS needs none made; the mask is not given to it then.
        DISPATCH_REACTIVE_MASK if !broken => {
            once("the title's reactive mask generation is left out (DLSS)".into());
            return OK;
        }
        DISPATCH_UPSCALE | DISPATCH_REACTIVE_MASK => {}
        _ => return UNKNOWN_DESCTYPE,
    }
    match ours.console_context() {
        Some(made) => forward(|c| c.dispatch, made, desc),
        None => ERROR,
    }
}

/// Records DLSS's evaluation for dispatch `d` of context `ours`; false
/// (and the context given up) if DLSS cannot take it.
fn upscale(ours: &Ours, d: &DispatchUpscale) -> bool {
    if let Some(why) = refused(d) {
        ours.give_up(&why);
        return false;
    }
    let t = tweaks();
    let broken = ours.broken.clone();
    let failed = move |why: String| {
        if !broken.swap(true, Ordering::Relaxed) {
            tracing::warn!(%why, "DLSS: stops for this upscaler; the title's FidelityFX upscales from the next frame");
            println!("dlss           stopped: {why}; FidelityFX upscales from the next frame");
        }
    };
    let inputs = desc::inputs(d, ours.auto_exposure, t);
    if dlss::dispatch(
        &ours.feature,
        d.command_list,
        inputs,
        desc::frame(d, t),
        failed,
    ) {
        return true;
    }
    ours.give_up("the dispatch's command list records nothing");
    false
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "fidelityfx",
        dlls: &DLLS,
        exports: vec![
            crate::hle_export!(ffxCreateContext),
            crate::hle_export!(ffxDestroyContext),
            crate::hle_export!(ffxConfigure),
            crate::hle_export!(ffxQuery),
            crate::hle_export!(ffxDispatch),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(ty: u64) -> Header {
        Header {
            ty,
            next: std::ptr::null_mut(),
        }
    }

    #[test]
    fn the_phase_count_answered_is_never_zero() {
        let mut out = -1;
        let mut q = QueryPhaseCount {
            header: header(QUERY_PHASE_COUNT),
            render_width: 1707,
            display_width: 2560,
            out: &mut out,
        };
        // SAFETY: our description.
        assert_eq!(unsafe { answer(&raw mut q.header) }, Some(OK));
        assert_eq!(out, 17);
        // FidelityFX's zero (run res-6) is replaced.
        out = 0;
        q.render_width = 1280;
        q.display_width = 1920;
        // SAFETY: as above.
        unsafe { keep_phases(&raw mut q.header) };
        assert_eq!(out, 18);
        // An answer above zero stays.
        out = 4;
        // SAFETY: as above.
        unsafe { keep_phases(&raw mut q.header) };
        assert_eq!(out, 4);
    }

    #[test]
    fn jitter_offsets_without_a_count_use_the_last_one() {
        let (mut x, mut y) = (9.0, 9.0);
        PHASES.store(8, Ordering::Relaxed);
        let mut q = QueryJitterOffset {
            header: header(QUERY_JITTER_OFFSET),
            index: 9,
            phase_count: 0,
            out_x: &mut x,
            out_y: &mut y,
        };
        // SAFETY: our description.
        assert_eq!(unsafe { answer(&raw mut q.header) }, Some(OK));
        assert_eq!((x, y), dlss::jitter_offset(1, 8));
    }

    #[test]
    fn render_size_and_ratio_follow_the_dlss_mode() {
        let (mut w, mut h, mut ratio) = (0, 0, 0.0);
        let mut q = QueryRenderSize {
            header: header(QUERY_RENDER_SIZE),
            display: [2560, 1440],
            quality: 0,
            out_width: &mut w,
            out_height: &mut h,
        };
        // SAFETY: our descriptions.
        unsafe {
            assert_eq!(answer(&raw mut q.header), Some(OK));
            let mut r = QueryRatio {
                header: header(QUERY_RATIO),
                quality: 0,
                out: &mut ratio,
            };
            assert_eq!(answer(&raw mut r.header), Some(OK));
            let mut other = header(DISPATCH_UPSCALE);
            assert_eq!(answer(&raw mut other), None, "not a query of ours");
        }
        // (VICTORY_DLSS is not set in tests: the default mode.)
        assert_eq!((w, h), (1707, 960));
        assert!((ratio - 1.0 / 0.667).abs() < 1e-6);
    }

    #[test]
    fn a_saved_chain_is_linked_again() {
        let mut version = [CREATE_VERSION, 0, 0x100_1001];
        let mut backend = [0x100_0002u64, version.as_mut_ptr() as u64, 0x1234];
        let backend = backend.as_mut_ptr();
        let mut upscale = CreateUpscale {
            header: Header {
                ty: CREATE_UPSCALE,
                next: backend.cast(),
            },
            flags: 0x29,
            max_render: [1280, 720],
            max_upscale: [1920, 1080],
            message: std::ptr::null(),
        };
        // SAFETY: our chain.
        let mut saved = unsafe { Saved::of(&raw mut upscale.header) };
        assert!(saved.whole);
        let head = saved.linked();
        let types: Vec<u64> = chain(head).map(|n| unsafe { (*n).ty }).collect();
        assert_eq!(types, [CREATE_UPSCALE, 0x100_0002, CREATE_VERSION]);
        // SAFETY: the copy's nodes.
        let copy = unsafe { &*head.cast::<CreateUpscale>() };
        assert_eq!(
            (copy.flags, copy.max_render, copy.max_upscale),
            (0x29, [1280, 720], [1920, 1080])
        );
        assert_ne!(head, &raw mut upscale.header, "a copy");
        // One of a type not known here makes the copy partial.
        // SAFETY: our array; then as above.
        unsafe { backend.write(0x2_0000) };
        // SAFETY: as above.
        assert!(!unsafe { Saved::of(&raw mut upscale.header) }.whole);
    }

    #[test]
    fn the_loader_alone_is_answered() {
        let m = module();
        assert!(m.serves("AMD_FidelityFX_Loader_Xbox.dll"));
        assert!(
            !m.serves(UPSCALER),
            "the title's upscaler stays the title's"
        );
        let names: Vec<_> = m.exports.iter().map(|e| e.name).collect();
        assert_eq!(
            names,
            [
                "ffxCreateContext",
                "ffxDestroyContext",
                "ffxConfigure",
                "ffxQuery",
                "ffxDispatch"
            ]
        );
    }
}
