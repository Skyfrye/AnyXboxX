//! The size the title draws its picture at (`VICTORY_OUTPUT`) and whether it renders at that size
//! (`VICTORY_RENDER_SCALE=1.0`) or at its own two thirds. The title takes only the sizes of its own
//! mode list (`victory_loader::resolution::MODES`); "Screen" picks the largest of them that fits the
//! main screen, so a 2560x1440 screen gets 2560x1440 and a 3440x1440 one gets 2560x1440 too.

use serde::{Deserialize, Serialize};

pub const OUTPUT: &str = "VICTORY_OUTPUT";
pub const RENDER_SCALE: &str = "VICTORY_RENDER_SCALE";
/// The render scale of "Native": the picture is rendered at the output size, not upscaled.
pub const NATIVE_SCALE: &str = "1.0";

/// The title's own output sizes, smallest first (its display mode list).
pub const MODES: [(u32, u32); 5] = [
    (1280, 720),
    (1664, 936),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Output {
    /// The largest of the title's sizes that fits the main screen.
    #[default]
    Screen,
    P720,
    P936,
    P1080,
    P1440,
    P2160,
    /// No `VICTORY_OUTPUT`: the title's own 3840x2160, rendered at two thirds.
    TitleDefault,
}

impl Output {
    pub const ALL: [Output; 7] = [
        Output::Screen,
        Output::P720,
        Output::P936,
        Output::P1080,
        Output::P1440,
        Output::P2160,
        Output::TitleDefault,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Output::Screen => "Screen size",
            Output::P720 => "1280x720",
            Output::P936 => "1664x936",
            Output::P1080 => "1920x1080",
            Output::P1440 => "2560x1440",
            Output::P2160 => "3840x2160",
            Output::TitleDefault => "Title's own",
        }
    }

    /// The size to ask for, `None` for the title's own (or "Screen" with no screen size known).
    pub fn size(self, screen: Option<(u32, u32)>) -> Option<(u32, u32)> {
        match self {
            Output::Screen => screen.map(fit),
            Output::P720 => Some(MODES[0]),
            Output::P936 => Some(MODES[1]),
            Output::P1080 => Some(MODES[2]),
            Output::P1440 => Some(MODES[3]),
            Output::P2160 => Some(MODES[4]),
            Output::TitleDefault => None,
        }
    }
}

/// The largest of [`MODES`] no wider and no taller than `screen`; the smallest mode for a screen
/// smaller than all of them.
pub fn fit(screen: (u32, u32)) -> (u32, u32) {
    MODES
        .iter()
        .rev()
        .copied()
        .find(|&(w, h)| w <= screen.0 && h <= screen.1)
        .unwrap_or(MODES[0])
}

/// `WxH`, as VIctory reads it.
pub fn format_size((w, h): (u32, u32)) -> String {
    format!("{w}x{h}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_screen_gets_the_largest_mode_that_fits() {
        assert_eq!(fit((2560, 1440)), (2560, 1440));
        assert_eq!(fit((3440, 1440)), (2560, 1440));
        assert_eq!(fit((1920, 1200)), (1920, 1080));
        assert_eq!(fit((3840, 2160)), (3840, 2160));
        assert_eq!(fit((5120, 2880)), (3840, 2160));
        assert_eq!(fit((1366, 768)), (1280, 720));
        assert_eq!(fit((800, 600)), (1280, 720));
    }

    #[test]
    fn screen_without_a_known_size_leaves_the_title_its_own() {
        assert_eq!(Output::Screen.size(None), None);
        assert_eq!(Output::Screen.size(Some((2560, 1440))), Some((2560, 1440)));
        assert_eq!(Output::TitleDefault.size(Some((2560, 1440))), None);
        assert_eq!(Output::P1080.size(None), Some((1920, 1080)));
    }

    #[test]
    fn every_fixed_choice_is_one_of_the_titles_modes() {
        for output in Output::ALL {
            if let Some(size) = output.size(Some((7680, 4320))) {
                assert!(MODES.contains(&size), "{output:?}");
            }
        }
        assert_eq!(format_size((2560, 1440)), "2560x1440");
    }

    /// The engine's own list: a size it does not know is refused there (said why) and ignored.
    #[test]
    fn the_modes_are_the_engines_list() {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../third_party/victory/crates/victory-loader/src/resolution.rs");
        let Ok(text) = std::fs::read_to_string(source) else {
            return;
        };
        let wanted = MODES
            .iter()
            .map(|(w, h)| format!("({w}, {h})"))
            .collect::<Vec<_>>()
            .join(", ");
        assert!(text.contains(&format!("[{wanted}]")), "{wanted}");
    }
}
