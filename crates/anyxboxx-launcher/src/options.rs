//! The launch options of the Play page and the environment each one sets. Every option that
//! reaches VIctory goes through the one table in `env_pairs`, and `ENV_NAMES` lists the variable
//! names it may use: a test checks each of them against VIctory's own sources in this tree, so an
//! option never names a variable nothing reads. The output size is `output`'s, the optional
//! features `features`'s.
//!
//! Not offered: the Series S console profile (`victory run --console series-s`, an argument, not a
//! variable).

use serde::{Deserialize, Serialize};

use crate::env::Pair;
use crate::features::Features;
use crate::output::{self, Output};

pub const WINDOW_MODE: &str = "VICTORY_WINDOW_MODE";
pub const KEYBOARD: &str = "VICTORY_KEYBOARD";
pub const LOG: &str = "VICTORY_LOG";
/// Taps Enter (also the first pad's A) on the start screens for 150 seconds: the same script as
/// `--auto-a` (`anyxboxx::run::auto_a_keys`).
pub const KEYS: &str = "VICTORY_KEYS";
/// Every variable an option may set (the preset's own are in `Preset::overrides`); the tests
/// check them against VIctory's sources.
#[cfg(test)]
pub const ENV_NAMES: [&str; 6] = [
    crate::output::OUTPUT,
    crate::output::RENDER_SCALE,
    WINDOW_MODE,
    KEYBOARD,
    LOG,
    KEYS,
];

/// VIctory's own room left for textures when nothing says otherwise (`VICTORY_TEXTURE_ROOM_PERCENT`,
/// `victory-gpu` `resource.rs`).
#[cfg(test)]
pub const DEFAULT_TEXTURE_ROOM_PERCENT: u32 = 90;

/// The "8 GB card" preset: on top of the NVIDIA profile's reservations (1536 MB of buffers and
/// 1024 MB of draw-data windows on the card), buffer copies capped at 512 MB, no draw-data windows on
/// the card (read from the PC's memory each frame) and textures given 85 % of the card's budget
/// instead of 90 %, so that the card stays under the budget Windows gives the game. Not measured.
pub const LOW_VRAM: &[(&str, &str)] = &[
    ("VICTORY_CARD_BUFFERS_MB", "512"),
    ("VICTORY_CARD_WINDOWS_MB", "0"),
    ("VICTORY_TEXTURE_ROOM_PERCENT", "85"),
];

/// The "12 GB card" preset: buffer copies on the card capped at 1024 MB, draw-data windows at
/// 512 MB, textures at VIctory's default room. Not measured on a 12 GB card yet.
pub const MID_VRAM: &[(&str, &str)] = &[
    ("VICTORY_CARD_BUFFERS_MB", "1024"),
    ("VICTORY_CARD_WINDOWS_MB", "512"),
    ("VICTORY_TEXTURE_ROOM_PERCENT", "90"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    #[default]
    Fast,
    Quality,
    /// For cards with 8 GB of video memory or less.
    LowVram,
    /// For cards with 12 GB of video memory.
    MidVram,
}

impl Preset {
    pub const ALL: [Preset; 4] = [
        Preset::Fast,
        Preset::Quality,
        Preset::MidVram,
        Preset::LowVram,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Preset::Fast => "Fast",
            Preset::Quality => "Quality",
            Preset::LowVram => "8 GB card",
            Preset::MidVram => "12 GB card",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Preset::Fast => "The profile as measured: the best speed known so far (16 GB cards).",
            Preset::Quality => {
                "Picture first: windows and buffers read fresh each frame, fewer copies on the card. Slower; not measured on this VIctory base yet."
            }
            Preset::LowVram => {
                "For 8 GB cards: smaller reservations on the card and less room for textures, so the card stays within its budget instead of spilling into the PC's memory. Pair it with 1920x1080. Not measured."
            }
            Preset::MidVram => {
                "For 12 GB cards: reservations on the card capped at 1024 MB of buffers and 512 MB of windows. Not measured on a 12 GB card yet."
            }
        }
    }

    /// The variables this preset adds on top of the profile. Fast is the profile itself; Quality
    /// is main's `-Quality` set (`anyxboxx::run::QUALITY`).
    pub fn overrides(self) -> Vec<Pair> {
        match self {
            Preset::Fast => Vec::new(),
            Preset::Quality => anyxboxx::run::QUALITY
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            Preset::LowVram => LOW_VRAM
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            Preset::MidVram => MID_VRAM
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowMode {
    #[default]
    Windowed,
    Borderless,
}

impl WindowMode {
    pub const ALL: [WindowMode; 2] = [WindowMode::Windowed, WindowMode::Borderless];

    pub fn label(self) -> &'static str {
        match self {
            WindowMode::Windowed => "Windowed",
            WindowMode::Borderless => "Borderless full screen",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Keyboard {
    /// By physical key position: ZQSD works on AZERTY with nothing to set.
    #[default]
    Auto,
    Qwerty,
    Azerty,
    Qwertz,
}

impl Keyboard {
    pub const ALL: [Keyboard; 4] = [
        Keyboard::Auto,
        Keyboard::Qwerty,
        Keyboard::Azerty,
        Keyboard::Qwertz,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Keyboard::Auto => "Auto (physical keys)",
            Keyboard::Qwerty => "QWERTY",
            Keyboard::Azerty => "AZERTY",
            Keyboard::Qwertz => "QWERTZ",
        }
    }

    /// The letters of the left stick, up/left/down/right, as printed on that keyboard.
    pub fn move_keys(self) -> &'static str {
        match self {
            Keyboard::Azerty => "Z Q S D",
            Keyboard::Auto => "W A S D positions (Z Q S D on AZERTY)",
            Keyboard::Qwerty | Keyboard::Qwertz => "W A S D",
        }
    }

    /// The letters of the bumper and trigger keys (left/right) as printed on that keyboard.
    pub fn shoulder_keys(self) -> &'static str {
        match self {
            Keyboard::Azerty => "Bumpers A E, triggers W C",
            Keyboard::Qwertz => "Bumpers Q E, triggers Y C",
            Keyboard::Auto | Keyboard::Qwerty => "Bumpers Q E, triggers Z C",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    #[default]
    Default,
    Warn,
    Info,
    Debug,
}

impl LogLevel {
    pub const ALL: [LogLevel; 4] = [
        LogLevel::Default,
        LogLevel::Warn,
        LogLevel::Info,
        LogLevel::Debug,
    ];

    pub fn label(self) -> &'static str {
        match self {
            LogLevel::Default => "Default",
            LogLevel::Warn => "Warnings only",
            LogLevel::Info => "Info",
            LogLevel::Debug => "Debug (slow)",
        }
    }
}

/// What the Play page's cards choose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LaunchOptions {
    pub preset: Preset,
    /// The size the title draws at (`VICTORY_OUTPUT`).
    pub output: Output,
    /// Render at the output size (`VICTORY_RENDER_SCALE=1.0`) instead of the title's two thirds.
    pub native_render: bool,
    pub window: WindowMode,
    pub keyboard: Keyboard,
    pub log: LogLevel,
    /// Translate the shaders into the cache before starting the game.
    pub warm_first: bool,
    /// Taps Enter (and A) through the start screens for 150 seconds.
    pub auto_a: bool,
    /// NVIDIA Smooth Motion (driver frame generation) for victory.exe: written into the driver's
    /// profile before each launch (`smooth_motion`), not into VIctory's environment.
    pub smooth_motion: bool,
    /// The optional features (overlay, DLSS, ray tracing, natural eyes, confetti fix).
    pub features: Features,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        LaunchOptions {
            preset: Preset::Fast,
            output: Output::Screen,
            native_render: true,
            window: WindowMode::Windowed,
            keyboard: Keyboard::Auto,
            log: LogLevel::Default,
            warm_first: false,
            auto_a: false,
            smooth_motion: false,
            features: Features::default(),
        }
    }
}

impl LaunchOptions {
    /// The output size to ask for given the main screen's size, `None` for the title's own.
    pub fn size(&self, screen: Option<(u32, u32)>) -> Option<(u32, u32)> {
        self.output.size(screen)
    }

    /// The variables the options set: the preset's, the output size (and the native render scale,
    /// only with an output size: without one the title's 3840x2160 would be rendered whole), then
    /// one table row per option that left its default, then the features. The user's own overrides
    /// go after these and win.
    pub fn env_pairs(&self, screen: Option<(u32, u32)>) -> Vec<Pair> {
        let mut pairs = self.preset.overrides();
        if let Some(size) = self.size(screen) {
            pairs.push((output::OUTPUT.to_string(), output::format_size(size)));
            if self.native_render {
                pairs.push((
                    output::RENDER_SCALE.to_string(),
                    output::NATIVE_SCALE.to_string(),
                ));
            }
        }
        if self.window == WindowMode::Borderless {
            pairs.push((WINDOW_MODE.to_string(), "borderless".to_string()));
        }
        let keyboard = match self.keyboard {
            Keyboard::Auto => None,
            Keyboard::Qwerty => Some("qwerty"),
            Keyboard::Azerty => Some("azerty"),
            Keyboard::Qwertz => Some("qwertz"),
        };
        if let Some(value) = keyboard {
            pairs.push((KEYBOARD.to_string(), value.to_string()));
        }
        let log = match self.log {
            LogLevel::Default => None,
            LogLevel::Warn => Some("warn"),
            LogLevel::Info => Some("info"),
            LogLevel::Debug => Some("debug"),
        };
        if let Some(value) = log {
            pairs.push((LOG.to_string(), value.to_string()));
        }
        if self.auto_a {
            pairs.push((KEYS.to_string(), anyxboxx::run::auto_a_keys()));
        }
        pairs.extend(self.features.env_pairs());
        pairs
    }
}

#[cfg(test)]
#[path = "options_tests.rs"]
mod tests;
