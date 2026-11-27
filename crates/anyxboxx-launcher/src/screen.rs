//! The main screen's size in pixels, as Windows has it set now (`EnumDisplaySettingsW` on the
//! primary display), whatever the launcher's DPI scaling. `None` where it cannot be read.

#[cfg(windows)]
pub fn primary_size() -> Option<(u32, u32)> {
    use windows::Win32::Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW};
    let mut mode = DEVMODEW {
        dmSize: u16::try_from(std::mem::size_of::<DEVMODEW>()).ok()?,
        ..DEVMODEW::default()
    };
    // SAFETY: `mode` is a valid DEVMODEW whose dmSize is set; a null device name is the primary
    // display.
    let ok = unsafe { EnumDisplaySettingsW(None, ENUM_CURRENT_SETTINGS, &mut mode) };
    let size = (mode.dmPelsWidth, mode.dmPelsHeight);
    (ok.as_bool() && size.0 > 0 && size.1 > 0).then_some(size)
}

#[cfg(not(windows))]
pub fn primary_size() -> Option<(u32, u32)> {
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_size_read_is_never_zero() {
        if let Some((w, h)) = super::primary_size() {
            assert!(w > 0 && h > 0);
        }
    }
}
