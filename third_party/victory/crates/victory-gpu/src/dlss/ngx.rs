//! The NVIDIA NGX runtime that comes with the NVIDIA driver (`_nvngx.dll`,
//! in the driver store; the registry names its folder), loaded at run time.
//! The DLSS model it loads (`nvngx_dlss.dll`) comes from the user.
//!
//! The declarations here are ours, written from the interface that NVIDIA's
//! public DLSS SDK documents (github.com/NVIDIA/DLSS, `include/nvsdk_ngx.h`,
//! `nvsdk_ngx_defs.h`, `nvsdk_ngx_params.h`, release v310.9.16): the names
//! and arguments of the runtime's exports, parameter names and values, and
//! the parameter object's interface. No file of the SDK is copied, linked
//! or shipped. The order of two exports' last arguments (the API version
//! before the feature information) is the runtime's own, which differs
//! from the SDK's static library wrapper; OptiScaler's `Nvngx_Proxy.h`
//! (GPL-3.0) documents it the same way.

use std::ffi::{CStr, c_char, c_void};
use std::path::{Path, PathBuf};

/// `NVSDK_NGX_Result`: success unless the top 12 bits are 0xBAD.
pub(super) type NgxResult = u32;

pub(super) fn succeeded(result: NgxResult) -> bool {
    result & 0xfff0_0000 != 0xbad0_0000
}

/// The interface version asked for (`NVSDK_NGX_Version_API`).
const VERSION_API: u32 = 0x15;
/// `NVSDK_NGX_ENGINE_TYPE_CUSTOM`.
const ENGINE_CUSTOM: i32 = 0;
/// `NVSDK_NGX_Feature_SuperSampling`: DLSS.
pub(super) const FEATURE_SUPER_SAMPLING: u32 = 1;
/// The project id NGX is given (any GUID for a project without an id from
/// NVIDIA, as its programming guide says), and our version.
const PROJECT_ID: &CStr = c"1305440c-c937-4558-b3a8-6276764ffeb7";
const ENGINE_VERSION: &CStr = c"1.0";

/// `NVSDK_NGX_PathListInfo`.
#[repr(C)]
struct PathList {
    paths: *const *const u16,
    len: u32,
}

/// `NVSDK_NGX_LoggingInfo`.
#[repr(C)]
struct Logging {
    callback: Option<extern "C" fn(*const c_char, i32, i32)>,
    minimum: i32,
    other_sinks_off: bool,
}

/// `NVSDK_NGX_FeatureCommonInfo`: where the model DLL is looked for first.
#[repr(C)]
struct CommonInfo {
    paths: PathList,
    internal: *mut c_void,
    logging: Logging,
}

const _: () = assert!(size_of::<CommonInfo>() == 40);

/// The parameter object (`NVSDK_NGX_Parameter`), a C++ interface called
/// through its table. MSVC puts the overloads of one name together, in the
/// reverse of the order they are declared in: `Set` of `void*`,
/// `ID3D12Resource*`, `ID3D11Resource*`, `int`, `unsigned int`, `double`,
/// `float`, `unsigned long long`; then `Get` of the same; then `Reset`.
/// Checked against the runtime when it is started ([`Params::check`]).
#[repr(C)]
pub(super) struct Param {
    table: *const usize,
}

mod slot {
    pub(super) const SET_PTR: usize = 0;
    pub(super) const SET_D3D12: usize = 1;
    pub(super) const SET_I: usize = 3;
    pub(super) const SET_UI: usize = 4;
    pub(super) const SET_F: usize = 6;
    pub(super) const GET_PTR: usize = 8;
    pub(super) const GET_I: usize = 11;
    pub(super) const GET_UI: usize = 12;
    pub(super) const GET_F: usize = 14;
}

type SetFn<T> = unsafe extern "system" fn(*mut Param, *const c_char, T);
type GetFn = unsafe extern "system" fn(*const Param, *const c_char, *mut u64) -> NgxResult;

/// A parameter object of the runtime's.
pub(super) struct Params(pub(super) *mut Param);

// SAFETY: used only under the runtime's lock (`super::Ngx::lock`).
unsafe impl Send for Params {}
// SAFETY: as above.
unsafe impl Sync for Params {}

impl Params {
    /// The function in `slot` of the object's table.
    ///
    /// # Safety
    /// `F` is that slot's signature.
    unsafe fn at<F: Copy>(&self, slot: usize) -> F {
        // SAFETY: a live object of the runtime's; its table has 17 slots.
        unsafe {
            let address = *(*self.0).table.add(slot);
            std::mem::transmute_copy(&address)
        }
    }

    fn set<T>(&self, slot: usize, name: &CStr, value: T) {
        // SAFETY: the slot's signature (`slot`).
        unsafe { self.at::<SetFn<T>>(slot)(self.0, name.as_ptr(), value) }
    }

    /// What a `Get` writes, into eight bytes (a slot that turned out to be
    /// of another type writes at most as much).
    fn get(&self, slot: usize, name: &CStr) -> Option<u64> {
        let mut out = 0u64;
        // SAFETY: the slot's signature, an out value of eight bytes.
        let result = unsafe { self.at::<GetFn>(slot)(self.0, name.as_ptr(), &mut out) };
        succeeded(result).then_some(out)
    }

    pub(super) fn set_ui(&self, name: &CStr, value: u32) {
        self.set(slot::SET_UI, name, value);
    }

    pub(super) fn set_i(&self, name: &CStr, value: i32) {
        self.set(slot::SET_I, name, value);
    }

    pub(super) fn set_f(&self, name: &CStr, value: f32) {
        self.set(slot::SET_F, name, value);
    }

    pub(super) fn set_ptr(&self, name: &CStr, value: *mut c_void) {
        self.set(slot::SET_PTR, name, value);
    }

    /// A D3D12 resource (null for none).
    pub(super) fn set_resource(&self, name: &CStr, value: *mut c_void) {
        self.set(slot::SET_D3D12, name, value);
    }

    pub(super) fn get_ui(&self, name: &CStr) -> Option<u32> {
        self.get(slot::GET_UI, name).map(|v| v as u32)
    }

    pub(super) fn get_i(&self, name: &CStr) -> Option<i32> {
        self.get(slot::GET_I, name).map(|v| v as u32 as i32)
    }

    pub(super) fn get_f(&self, name: &CStr) -> Option<f32> {
        self.get(slot::GET_F, name)
            .map(|v| f32::from_bits(v as u32))
    }

    pub(super) fn get_ptr(&self, name: &CStr) -> Option<*mut c_void> {
        self.get(slot::GET_PTR, name)
            .map(|v| v as usize as *mut c_void)
    }

    /// Whether values come back as they were set through the slots above:
    /// the table is laid out as [`Param`] says.
    pub(super) fn check(&self) -> Result<(), String> {
        const PTR: usize = 0x5eed_0000_1234;
        self.set_ui(c"VIctory.Check.UI", 0x5eed_1234);
        self.set_i(c"VIctory.Check.I", -7);
        self.set_f(c"VIctory.Check.F", 1.25);
        self.set_ptr(c"VIctory.Check.P", PTR as *mut c_void);
        let got = (
            self.get_ui(c"VIctory.Check.UI"),
            self.get_i(c"VIctory.Check.I"),
            self.get_f(c"VIctory.Check.F"),
            self.get_ptr(c"VIctory.Check.P").map(|p| p as usize),
        );
        if got == (Some(0x5eed_1234), Some(-7), Some(1.25), Some(PTR)) {
            Ok(())
        } else {
            Err(format!(
                "the NGX parameter object is not laid out as expected (read back {got:x?})"
            ))
        }
    }
}

type InitProjectId = unsafe extern "C" fn(
    *const c_char,
    i32,
    *const c_char,
    *const u16,
    *mut c_void,
    u32,
    *const CommonInfo,
) -> NgxResult;
type ParamsFn = unsafe extern "C" fn(*mut *mut Param) -> NgxResult;
type CreateFeature =
    unsafe extern "C" fn(*mut c_void, u32, *mut Param, *mut *mut c_void) -> NgxResult;
type EvaluateFeature =
    unsafe extern "C" fn(*mut c_void, *const c_void, *const Param, *const c_void) -> NgxResult;
type ReleaseFeature = unsafe extern "C" fn(*mut c_void) -> NgxResult;

/// The runtime's exports we call.
pub(super) struct Runtime {
    init: InitProjectId,
    capability: ParamsFn,
    create: CreateFeature,
    evaluate: EvaluateFeature,
    release: ReleaseFeature,
}

/// The runtime's folder, as the driver writes it in the registry
/// (`HKLM\SYSTEM\CurrentControlSet\Services\nvlddmkm\NGXCore`, `NGXPath`),
/// and the runtime in it.
pub(super) fn core_path() -> Result<PathBuf, String> {
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
    let key = wide(Path::new(
        r"SYSTEM\CurrentControlSet\Services\nvlddmkm\NGXCore",
    ));
    let value = wide(Path::new("NGXPath"));
    let mut buffer = vec![0u16; 1024];
    let mut bytes = (buffer.len() * 2) as u32;
    // SAFETY: NUL-terminated names, a buffer of `bytes` bytes.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if status != 0 {
        return Err("the NVIDIA driver names no NGX runtime (no NGXCore key: no NVIDIA driver, or one without NGX)".into());
    }
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    let folder = PathBuf::from(String::from_utf16_lossy(&buffer[..len]));
    let core = folder.join("_nvngx.dll");
    if core.is_file() {
        Ok(core)
    } else {
        Err(format!(
            "the NVIDIA driver's NGX runtime {} is not there",
            core.display()
        ))
    }
}

/// A NUL-terminated wide string.
pub(super) fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain([0]).collect()
}

/// NGX's log lines (`VICTORY_DLSS_LOG=1`, `2` for more).
extern "C" fn log_line(message: *const c_char, level: i32, feature: i32) {
    if message.is_null() {
        return;
    }
    // SAFETY: NGX passes a NUL-terminated line, valid during the call.
    let line = unsafe { CStr::from_ptr(message) }.to_string_lossy();
    tracing::info!(level, feature, "NGX: {}", line.trim_end());
}

impl Runtime {
    /// Loads the runtime at `path`.
    pub(super) fn load(path: &Path) -> Result<Runtime, String> {
        use windows_sys::Win32::System::LibraryLoader::{
            GetProcAddress, LOAD_WITH_ALTERED_SEARCH_PATH, LoadLibraryExW,
        };
        let name = wide(path);
        // SAFETY: a NUL-terminated path; the runtime's folder is searched for
        // what it needs.
        let module = unsafe {
            LoadLibraryExW(
                name.as_ptr(),
                std::ptr::null_mut(),
                LOAD_WITH_ALTERED_SEARCH_PATH,
            )
        };
        if module.is_null() {
            return Err(format!(
                "{} did not load: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
        let export = |name: &CStr| {
            // SAFETY: a loaded module and a NUL-terminated name.
            unsafe { GetProcAddress(module, name.as_ptr().cast()) }
                .map(|f| f as usize)
                .ok_or_else(|| format!("{} has no {}", path.display(), name.to_string_lossy()))
        };
        // SAFETY: these exports have these signatures (see the module's notes).
        unsafe {
            Ok(Runtime {
                init: std::mem::transmute::<usize, InitProjectId>(export(
                    c"NVSDK_NGX_D3D12_Init_ProjectID",
                )?),
                capability: std::mem::transmute::<usize, ParamsFn>(export(
                    c"NVSDK_NGX_D3D12_GetCapabilityParameters",
                )?),
                create: std::mem::transmute::<usize, CreateFeature>(export(
                    c"NVSDK_NGX_D3D12_CreateFeature",
                )?),
                evaluate: std::mem::transmute::<usize, EvaluateFeature>(export(
                    c"NVSDK_NGX_D3D12_EvaluateFeature",
                )?),
                release: std::mem::transmute::<usize, ReleaseFeature>(export(
                    c"NVSDK_NGX_D3D12_ReleaseFeature",
                )?),
            })
        }
    }

    /// Starts the runtime on `device`, looking for the model in `models`
    /// first; NGX writes its own files into `data`.
    pub(super) fn init(
        &self,
        device: *mut c_void,
        models: &Path,
        data: &Path,
    ) -> Result<(), String> {
        // NGX may keep the pointers after the call: they live for the run.
        let folder: &'static [u16] = Box::leak(wide(models).into_boxed_slice());
        let paths: &'static [*const u16; 1] = Box::leak(Box::new([folder.as_ptr()]));
        let data = wide(data);
        let verbose = std::env::var("VICTORY_DLSS_LOG").ok();
        let minimum = match verbose.as_deref() {
            Some("2") => 2,
            Some("1") => 1,
            _ => 0,
        };
        let info = CommonInfo {
            paths: PathList {
                paths: paths.as_ptr(),
                len: 1,
            },
            internal: std::ptr::null_mut(),
            logging: Logging {
                callback: (minimum > 0)
                    .then_some(log_line as extern "C" fn(*const c_char, i32, i32)),
                minimum,
                other_sinks_off: false,
            },
        };
        // SAFETY: NUL-terminated strings, a live device, our description.
        let result = unsafe {
            (self.init)(
                PROJECT_ID.as_ptr(),
                ENGINE_CUSTOM,
                ENGINE_VERSION.as_ptr(),
                data.as_ptr(),
                device,
                VERSION_API,
                &info,
            )
        };
        if succeeded(result) {
            Ok(())
        } else {
            Err(format!(
                "NGX did not start on the host device (result {result:#x})"
            ))
        }
    }

    /// The parameter object NGX keeps what it can do in.
    pub(super) fn capability(&self) -> Result<Params, String> {
        let mut params = std::ptr::null_mut();
        // SAFETY: an out pointer.
        let result = unsafe { (self.capability)(&mut params) };
        if succeeded(result) && !params.is_null() {
            Ok(Params(params))
        } else {
            Err(format!(
                "NGX gave no capability parameters (result {result:#x})"
            ))
        }
    }

    /// `NVSDK_NGX_D3D12_CreateFeature` of DLSS on `list` with `params`.
    pub(super) fn create(
        &self,
        list: *mut c_void,
        params: &Params,
    ) -> Result<*mut c_void, NgxResult> {
        let mut handle = std::ptr::null_mut();
        // SAFETY: an open list of the device NGX runs on, its parameters.
        let result = unsafe { (self.create)(list, FEATURE_SUPER_SAMPLING, params.0, &mut handle) };
        if succeeded(result) && !handle.is_null() {
            Ok(handle)
        } else {
            Err(result)
        }
    }

    /// `NVSDK_NGX_D3D12_EvaluateFeature` of `handle` on `list`.
    pub(super) fn evaluate(
        &self,
        list: *mut c_void,
        handle: *mut c_void,
        params: &Params,
    ) -> NgxResult {
        // SAFETY: an open list, a feature of this runtime, its parameters;
        // no progress callback.
        unsafe { (self.evaluate)(list, handle, params.0, std::ptr::null()) }
    }

    pub(super) fn release(&self, handle: *mut c_void) -> NgxResult {
        // SAFETY: a feature of this runtime that nothing runs any more.
        unsafe { (self.release)(handle) }
    }
}

/// Names of the parameters used (`nvsdk_ngx_defs.h`).
pub(super) mod name {
    use std::ffi::CStr;
    pub(crate) const AVAILABLE: &CStr = c"SuperSampling.Available";
    pub(crate) const NEEDS_UPDATED_DRIVER: &CStr = c"SuperSampling.NeedsUpdatedDriver";
    pub(crate) const MIN_DRIVER_MAJOR: &CStr = c"SuperSampling.MinDriverVersionMajor";
    pub(crate) const MIN_DRIVER_MINOR: &CStr = c"SuperSampling.MinDriverVersionMinor";
    pub(crate) const INIT_RESULT: &CStr = c"SuperSampling.FeatureInitResult";
    pub(crate) const OPTIMAL_SETTINGS: &CStr = c"DLSSOptimalSettingsCallback";
    pub(crate) const CREATION_NODE_MASK: &CStr = c"CreationNodeMask";
    pub(crate) const VISIBILITY_NODE_MASK: &CStr = c"VisibilityNodeMask";
    pub(crate) const WIDTH: &CStr = c"Width";
    pub(crate) const HEIGHT: &CStr = c"Height";
    pub(crate) const OUT_WIDTH: &CStr = c"OutWidth";
    pub(crate) const OUT_HEIGHT: &CStr = c"OutHeight";
    pub(crate) const PERF_QUALITY: &CStr = c"PerfQualityValue";
    pub(crate) const RTX_VALUE: &CStr = c"RTXValue";
    pub(crate) const CREATE_FLAGS: &CStr = c"DLSS.Feature.Create.Flags";
    pub(crate) const OUTPUT_SUBRECTS: &CStr = c"DLSS.Enable.Output.Subrects";
    pub(crate) const COLOR: &CStr = c"Color";
    pub(crate) const OUTPUT: &CStr = c"Output";
    pub(crate) const DEPTH: &CStr = c"Depth";
    pub(crate) const MOTION_VECTORS: &CStr = c"MotionVectors";
    pub(crate) const JITTER_X: &CStr = c"Jitter.Offset.X";
    pub(crate) const JITTER_Y: &CStr = c"Jitter.Offset.Y";
    pub(crate) const SHARPNESS: &CStr = c"Sharpness";
    pub(crate) const RESET: &CStr = c"Reset";
    pub(crate) const MV_SCALE_X: &CStr = c"MV.Scale.X";
    pub(crate) const MV_SCALE_Y: &CStr = c"MV.Scale.Y";
    pub(crate) const TRANSPARENCY_MASK: &CStr = c"TransparencyMask";
    pub(crate) const EXPOSURE_TEXTURE: &CStr = c"ExposureTexture";
    pub(crate) const BIAS_COLOR_MASK: &CStr = c"DLSS.Input.Bias.Current.Color.Mask";
    pub(crate) const RENDER_WIDTH: &CStr = c"DLSS.Render.Subrect.Dimensions.Width";
    pub(crate) const RENDER_HEIGHT: &CStr = c"DLSS.Render.Subrect.Dimensions.Height";
    pub(crate) const PRE_EXPOSURE: &CStr = c"DLSS.Pre.Exposure";
    pub(crate) const EXPOSURE_SCALE: &CStr = c"DLSS.Exposure.Scale";
    pub(crate) const INVERT_X: &CStr = c"DLSS.Indicator.Invert.X.Axis";
    pub(crate) const INVERT_Y: &CStr = c"DLSS.Indicator.Invert.Y.Axis";
    pub(crate) const FRAME_MS: &CStr = c"FrameTimeDeltaInMsec";
    /// Where each input's part used begins (all at the origin here).
    pub(crate) const SUBRECT_BASES: [&CStr; 12] = [
        c"DLSS.Input.Color.Subrect.Base.X",
        c"DLSS.Input.Color.Subrect.Base.Y",
        c"DLSS.Input.Depth.Subrect.Base.X",
        c"DLSS.Input.Depth.Subrect.Base.Y",
        c"DLSS.Input.MV.Subrect.Base.X",
        c"DLSS.Input.MV.Subrect.Base.Y",
        c"DLSS.Input.Translucency.Subrect.Base.X",
        c"DLSS.Input.Translucency.Subrect.Base.Y",
        c"DLSS.Input.Bias.Current.Color.Subrect.Base.X",
        c"DLSS.Input.Bias.Current.Color.Subrect.Base.Y",
        c"DLSS.Output.Subrect.Base.X",
        c"DLSS.Output.Subrect.Base.Y",
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_follow_the_failure_prefix() {
        assert!(succeeded(1));
        assert!(!succeeded(0xbad0_0000));
        assert!(!succeeded(0xbad0_0005));
        assert!(succeeded(0x0010_0000));
    }

    #[test]
    fn the_overload_slots_are_reversed_per_name() {
        // Set: 8 overloads from slot 0, the last declared (void*) first.
        assert_eq!(
            (
                slot::SET_PTR,
                slot::SET_D3D12,
                slot::SET_I,
                slot::SET_UI,
                slot::SET_F
            ),
            (0, 1, 3, 4, 6)
        );
        // Get: the same 8, after them.
        assert_eq!(
            (slot::GET_PTR, slot::GET_I, slot::GET_UI, slot::GET_F),
            (8, 11, 12, 14)
        );
    }
}
