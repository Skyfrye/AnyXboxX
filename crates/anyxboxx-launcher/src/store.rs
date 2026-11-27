//! The settings file: `%LOCALAPPDATA%\AnyXboxX-diffbase\launcher\settings.toml` (apart from the
//! upstream AnyXboxX launcher's). It keeps the game folder,
//! the thread count and the Play page's options between runs. A missing or broken
//! file is not an error: the launcher starts with its defaults.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::options::LaunchOptions;
use crate::session::Settings;

/// What is written to the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Stored {
    pub game_dir: String,
    pub jobs: u32,
    pub skip_process_check: bool,
    /// Keep the launcher open after the game started.
    pub keep_open_after_launch: bool,
    pub always_rewarm: bool,
    pub warm_before_play: bool,
    pub options: LaunchOptions,
}

/// The folder of this launcher's files under `%LOCALAPPDATA%`.
pub const APP_DIR: &str = "AnyXboxX-diffbase";

impl Stored {
    pub fn from_settings(settings: &Settings) -> Self {
        Stored {
            game_dir: settings.game_dir.clone(),
            jobs: settings.jobs,
            skip_process_check: settings.skip_process_check,
            keep_open_after_launch: settings.keep_open_after_launch,
            always_rewarm: settings.always_rewarm,
            warm_before_play: settings.warm_before_play,
            options: settings.options.clone(),
        }
    }

    /// The settings these values describe; the overrides text is not kept.
    pub fn into_settings(self) -> Settings {
        Settings {
            game_dir: self.game_dir,
            jobs: self.jobs,
            skip_process_check: self.skip_process_check,
            keep_open_after_launch: self.keep_open_after_launch,
            always_rewarm: self.always_rewarm,
            warm_before_play: self.warm_before_play,
            options: self.options,
            ..Settings::default()
        }
    }
}

/// Where the file lives, given `%LOCALAPPDATA%`.
pub fn path_in(local_app_data: Option<&OsStr>) -> Option<PathBuf> {
    local_app_data.filter(|v| !v.is_empty()).map(|root| {
        PathBuf::from(root)
            .join(APP_DIR)
            .join("launcher")
            .join("settings.toml")
    })
}

pub fn current_path() -> Option<PathBuf> {
    path_in(std::env::var_os("LOCALAPPDATA").as_deref())
}

/// Reads the file; `None` when it is missing or cannot be read as settings.
pub fn load(path: &Path) -> Option<Stored> {
    let text = std::fs::read_to_string(path).ok()?;
    toml::from_str(&text).ok()
}

/// Writes the file, creating its folder.
pub fn save(path: &Path, stored: &Stored) -> Result<(), String> {
    let text = toml::to_string_pretty(stored).map_err(|e| e.to_string())?;
    if let Some(folder) = path.parent() {
        std::fs::create_dir_all(folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    }
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::{Keyboard, Preset, WindowMode};
    use crate::output::Output;

    fn sample() -> Settings {
        Settings {
            game_dir: "D:\\Game".to_string(),
            jobs: 6,
            skip_process_check: true,
            keep_open_after_launch: true,
            always_rewarm: true,
            options: LaunchOptions {
                preset: Preset::Quality,
                output: Output::P1080,
                window: WindowMode::Borderless,
                keyboard: Keyboard::Azerty,
                warm_first: true,
                ..LaunchOptions::default()
            },
            ..Settings::default()
        }
    }

    #[test]
    fn settings_survive_a_save_and_a_load() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("a").join("b").join("settings.toml");
        let stored = Stored::from_settings(&sample());
        save(&path, &stored).expect("save");
        let back = load(&path).expect("load");
        assert_eq!(back, stored);
        let settings = back.into_settings();
        assert_eq!(settings.game_dir, "D:\\Game");
        assert_eq!(settings.options.keyboard, Keyboard::Azerty);
        assert!(settings.options.warm_first);
        assert_eq!(settings.options.output, Output::P1080);
    }

    #[test]
    fn a_missing_or_broken_file_gives_none() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(load(&dir.path().join("nope.toml")).is_none());
        let broken = dir.path().join("broken.toml");
        std::fs::write(&broken, "game_dir = [").expect("write");
        assert!(load(&broken).is_none());
    }

    #[test]
    fn a_partial_file_fills_the_rest_with_defaults() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("partial.toml");
        std::fs::write(
            &path,
            "game_dir = \"E:\\\\G\"\n[options]\nkeyboard = \"qwertz\"\n",
        )
        .expect("write");
        let settings = load(&path).expect("load").into_settings();
        assert_eq!(settings.game_dir, "E:\\G");
        assert_eq!(settings.options.keyboard, Keyboard::Qwertz);
        assert_eq!(settings.options.preset, Preset::Fast);
    }

    #[test]
    fn a_file_from_the_neptune_launcher_still_loads() {
        // Its XG library fields (gone with the Neptune base) and old options are ignored.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("old.toml");
        std::fs::write(
            &path,
            "game_dir = \"G\"\nxg_dll = \"D:\\\\xg_xs.dll\"\nxg_mode = \"compare\"\n[options]\noverlay = false\nframetime_dump = true\n",
        )
        .expect("write");
        let settings = load(&path).expect("load").into_settings();
        assert_eq!(settings.game_dir, "G");
        assert_eq!(settings.options, LaunchOptions::default());
    }

    #[test]
    fn the_path_is_under_local_app_data() {
        let path = path_in(Some(OsStr::new("C:\\L"))).expect("path");
        assert!(
            path.ends_with("AnyXboxX-diffbase/launcher/settings.toml")
                || path.ends_with("AnyXboxX-diffbase\\launcher\\settings.toml")
        );
        assert_eq!(path_in(None), None);
        assert_eq!(path_in(Some(OsStr::new(""))), None);
    }
}
