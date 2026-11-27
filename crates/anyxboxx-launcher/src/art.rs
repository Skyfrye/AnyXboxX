//! The launcher's own pictures, made for this project (no game art, logos or marks): the hero
//! slideshow of the Home page and the launch screen, used when the player put no picture of their
//! own where `splash` looks, and the window icon. How each was made: `assets/README.md`.

use std::path::PathBuf;

use eframe::egui::{self, ColorImage};

/// The built-in hero pictures, in slideshow order.
pub const HERO: [&[u8]; 2] = [
    include_bytes!("../assets/hero-1.jpg"),
    include_bytes!("../assets/hero-2.jpg"),
];
/// The window and taskbar icon (256x256 PNG; the executable's own icon is `assets/icon.ico`).
pub const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");

/// Where a slideshow picture comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The player's own file (`splash::current_sources`).
    File(PathBuf),
    /// One of [`HERO`].
    Builtin(usize),
}

/// The player's pictures when there are any, else the built-in ones.
pub fn sources(files: Vec<PathBuf>) -> Vec<Source> {
    if files.is_empty() {
        (0..HERO.len()).map(Source::Builtin).collect()
    } else {
        files.into_iter().map(Source::File).collect()
    }
}

/// Decodes `source`, shrunk so its longest side is at most `max_side`.
pub fn load(source: &Source, max_side: u32) -> Result<ColorImage, String> {
    match source {
        Source::File(path) => crate::splash::load_image(path, max_side),
        Source::Builtin(index) => {
            let bytes = HERO
                .get(*index)
                .ok_or_else(|| format!("no built-in picture {index}"))?;
            let decoded = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
            Ok(crate::splash::to_color_image(decoded, max_side))
        }
    }
}

/// The window icon, `None` if it does not decode.
pub fn window_icon() -> Option<egui::IconData> {
    let rgba = image::load_from_memory(ICON_PNG).ok()?.to_rgba8();
    Some(egui::IconData {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_pictures_of_the_players_the_built_in_ones_show() {
        assert_eq!(
            sources(Vec::new()),
            vec![Source::Builtin(0), Source::Builtin(1)]
        );
        let mine = vec![PathBuf::from("a.png")];
        assert_eq!(sources(mine), vec![Source::File(PathBuf::from("a.png"))]);
    }

    #[test]
    fn the_built_in_pictures_decode_and_shrink() {
        for index in 0..HERO.len() {
            let image = load(&Source::Builtin(index), 640).expect("decodes");
            assert!(image.size[0] <= 640 && image.size[1] <= 640);
            assert!(image.size[0] > image.size[1], "landscape");
        }
        assert!(load(&Source::Builtin(HERO.len()), 640).is_err());
    }

    #[test]
    fn the_icon_is_square() {
        let icon = window_icon().expect("icon");
        assert_eq!(icon.width, icon.height);
        assert!(icon.width >= 64);
    }
}
