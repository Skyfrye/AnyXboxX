//! The optional features of this line, each one VIctory switch (or a few), off unless said:
//! the stats overlay (on by default), NVIDIA DLSS, the title's ray-traced light, the reflection bound
//! that gives natural eyes without ray tracing, and the skin confetti fix. `FEATURE_NAMES` lists
//! every variable this module may set; a test checks each against VIctory's sources.

use serde::{Deserialize, Serialize};

use crate::env::Pair;

/// The numbers on the picture, drawn in release builds too; F3 shows and hides them in game.
pub const PANEL: &str = "VICTORY_PANEL";
pub const DLSS: &str = "VICTORY_DLSS";
/// The user's own `nvngx_dlss.dll`, or the folder holding it: never shipped here.
pub const DLSS_DLL: &str = "VICTORY_DLSS_DLL";
pub const DLSS_MODE: &str = "VICTORY_DLSS_MODE";
/// The title's own BVH builder: ray-traced indirect light (interiors lit right), slower.
pub const BVH_BUILDER: &str = "VICTORY_BVH_BUILDER";
/// The ambient stand-in's bound on the reflections: natural eyes with ray tracing off.
pub const AMBIENT_SPECULAR: &str = "VICTORY_AMBIENT_SPECULAR";
pub const AMBIENT_SPECULAR_VALUE: &str = "1.5";
/// Uploads sent after the card wrote a texture keep what the card wrote: the skin "confetti" fix.
pub const FIRST_UPLOAD_FRONT: &str = "VICTORY_FIRST_UPLOAD_FRONT";
/// The file NGX looks for (VIctory's `dlss::MODEL_FILE`).
pub const DLSS_MODEL_FILE: &str = "nvngx_dlss.dll";

#[cfg(test)]
pub const FEATURE_NAMES: [&str; 7] = [
    PANEL,
    DLSS,
    DLSS_DLL,
    DLSS_MODE,
    BVH_BUILDER,
    AMBIENT_SPECULAR,
    FIRST_UPLOAD_FRONT,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DlssMode {
    #[default]
    Quality,
    Balanced,
    Performance,
    UltraPerformance,
    Dlaa,
}

impl DlssMode {
    pub const ALL: [DlssMode; 5] = [
        DlssMode::Quality,
        DlssMode::Balanced,
        DlssMode::Performance,
        DlssMode::UltraPerformance,
        DlssMode::Dlaa,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DlssMode::Quality => "Quality",
            DlssMode::Balanced => "Balanced",
            DlssMode::Performance => "Performance",
            DlssMode::UltraPerformance => "Ultra perf.",
            DlssMode::Dlaa => "DLAA",
        }
    }

    /// The value VIctory reads; `None` for Quality, its default.
    fn value(self) -> Option<&'static str> {
        match self {
            DlssMode::Quality => None,
            DlssMode::Balanced => Some("balanced"),
            DlssMode::Performance => Some("performance"),
            DlssMode::UltraPerformance => Some("ultra-performance"),
            DlssMode::Dlaa => Some("dlaa"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Features {
    pub overlay: bool,
    pub dlss: bool,
    /// The DLSS model file or its folder, as picked or typed.
    pub dlss_dll: String,
    pub dlss_mode: DlssMode,
    pub ray_tracing: bool,
    /// Only applied with ray tracing off (the bound is for the ambient stand-in).
    pub natural_eyes: bool,
    pub confetti_fix: bool,
}

impl Default for Features {
    fn default() -> Self {
        Features {
            overlay: true,
            dlss: false,
            dlss_dll: String::new(),
            dlss_mode: DlssMode::Quality,
            ray_tracing: false,
            natural_eyes: false,
            confetti_fix: false,
        }
    }
}

/// Whether `text` names a DLSS model VIctory can use: the file itself or a folder holding it.
pub fn dlss_model_found(text: &str) -> bool {
    let path = std::path::Path::new(text.trim().trim_matches('"'));
    if text.trim().is_empty() {
        return false;
    }
    if path.is_dir() {
        return path.join(DLSS_MODEL_FILE).is_file();
    }
    path.is_file()
        && path
            .file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case(DLSS_MODEL_FILE))
}

impl Features {
    /// Natural eyes applies: asked for, and ray tracing off.
    pub fn eyes_apply(&self) -> bool {
        self.natural_eyes && !self.ray_tracing
    }

    /// The variables of the features that are on, in this order.
    pub fn env_pairs(&self) -> Vec<Pair> {
        let mut pairs: Vec<Pair> = Vec::new();
        let mut push = |k: &str, v: &str| pairs.push((k.to_string(), v.to_string()));
        if self.overlay {
            push(PANEL, "1");
        }
        if self.dlss {
            push(DLSS, "1");
            let dll = self.dlss_dll.trim().trim_matches('"');
            if !dll.is_empty() {
                push(DLSS_DLL, dll);
            }
            if let Some(mode) = self.dlss_mode.value() {
                push(DLSS_MODE, mode);
            }
        }
        if self.ray_tracing {
            push(BVH_BUILDER, "1");
        }
        if self.eyes_apply() {
            push(AMBIENT_SPECULAR, AMBIENT_SPECULAR_VALUE);
        }
        if self.confetti_fix {
            push(FIRST_UPLOAD_FRONT, "1");
        }
        pairs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get<'a>(pairs: &'a [Pair], name: &str) -> Option<&'a str> {
        pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn by_default_only_the_overlay_is_on() {
        let pairs = Features::default().env_pairs();
        assert_eq!(pairs, vec![(PANEL.to_string(), "1".to_string())]);
    }

    #[test]
    fn dlss_names_the_users_model_and_its_mode() {
        let features = Features {
            overlay: false,
            dlss: true,
            dlss_dll: "\"C:\\Games\\Other\\nvngx_dlss.dll\"".to_string(),
            dlss_mode: DlssMode::Performance,
            ..Features::default()
        };
        let pairs = features.env_pairs();
        assert_eq!(get(&pairs, DLSS), Some("1"));
        assert_eq!(
            get(&pairs, DLSS_DLL),
            Some("C:\\Games\\Other\\nvngx_dlss.dll")
        );
        assert_eq!(get(&pairs, DLSS_MODE), Some("performance"));
        let quality = Features {
            dlss: true,
            ..Features::default()
        };
        assert_eq!(get(&quality.env_pairs(), DLSS_MODE), None);
        assert_eq!(get(&quality.env_pairs(), DLSS_DLL), None);
    }

    #[test]
    fn natural_eyes_only_without_ray_tracing() {
        let mut features = Features {
            natural_eyes: true,
            ..Features::default()
        };
        assert_eq!(get(&features.env_pairs(), AMBIENT_SPECULAR), Some("1.5"));
        features.ray_tracing = true;
        let pairs = features.env_pairs();
        assert_eq!(get(&pairs, AMBIENT_SPECULAR), None);
        assert_eq!(get(&pairs, BVH_BUILDER), Some("1"));
    }

    #[test]
    fn the_confetti_fix_is_its_switch() {
        let features = Features {
            confetti_fix: true,
            overlay: false,
            ..Features::default()
        };
        assert_eq!(
            features.env_pairs(),
            vec![(FIRST_UPLOAD_FRONT.to_string(), "1".to_string())]
        );
    }

    #[test]
    fn a_dlss_model_is_the_file_or_a_folder_holding_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!dlss_model_found(&dir.path().display().to_string()));
        let model = dir.path().join(DLSS_MODEL_FILE);
        std::fs::write(&model, b"MZ").expect("write");
        assert!(dlss_model_found(&dir.path().display().to_string()));
        assert!(dlss_model_found(&model.display().to_string()));
        let other = dir.path().join("other.dll");
        std::fs::write(&other, b"MZ").expect("write");
        assert!(!dlss_model_found(&other.display().to_string()));
        assert!(!dlss_model_found(""));
    }
}
