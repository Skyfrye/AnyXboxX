//! The splash screen: shown for a moment at start-up and while a long job runs. Its pictures are
//! never in the repository (they may carry third-party marks); they are read at run time from
//! `ANYXBOXX_LAUNCHER_SPLASH` (a file or a folder), else the folder
//! `%LOCALAPPDATA%\AnyXboxX\launcher\splash\` (PNG and JPEG, by file name), else the single
//! `splash.png` next to it. Several pictures rotate as a slideshow with a cross-fade. Without a
//! picture of the player's, the launcher's own (`art::HERO`, made for this project) are shown. The painting is in
//! `ui_launch` and `ui_slides`; this module holds the pictures, the timing and the status line.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};

use crate::art::Source;
use crate::progress::Progress;

/// Environment variable that points at the splash picture or folder.
pub const SPLASH_ENV: &str = "ANYXBOXX_LAUNCHER_SPLASH";
/// How long the splash stays at start-up.
pub const STARTUP_DURATION: Duration = Duration::from_millis(2000);
/// A picture is shrunk to this many pixels on its longest side before it becomes a texture.
pub const MAX_TEXTURE_SIDE: u32 = 2560;
/// Seconds one picture stays before the next one fades in.
pub const SLIDE_PERIOD: f64 = 6.0;
/// Seconds of the cross-fade.
pub const FADE_DURATION: f64 = 0.8;
/// File extensions read from a splash folder (lower case).
const EXTENSIONS: [&str; 3] = ["png", "jpg", "jpeg"];

/// The pictures of a folder (PNG and JPEG), sorted by file name. An unreadable folder is empty.
pub fn list_images(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file() && has_image_extension(p))
        .collect();
    files.sort_by_key(|p| p.file_name().map(|n| n.to_ascii_lowercase()));
    files
}

fn has_image_extension(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// The pictures to show, in order: the override (a file, or a folder), else the `splash` folder
/// under `local_app_data`, else its single `splash.png`; empty when there is none.
pub fn resolve_sources(env_value: Option<&OsStr>, local_app_data: Option<&OsStr>) -> Vec<PathBuf> {
    if let Some(value) = env_value.filter(|v| !v.is_empty()) {
        let path = PathBuf::from(value);
        if path.is_file() {
            return vec![path];
        }
        let found = list_images(&path);
        if !found.is_empty() {
            return found;
        }
    }
    let Some(root) = local_app_data.filter(|v| !v.is_empty()) else {
        return Vec::new();
    };
    let launcher = PathBuf::from(root).join("AnyXboxX").join("launcher");
    let found = list_images(&launcher.join("splash"));
    if !found.is_empty() {
        return found;
    }
    let single = launcher.join("splash.png");
    if single.is_file() {
        vec![single]
    } else {
        Vec::new()
    }
}

/// The pictures as this machine has them.
pub fn current_sources() -> Vec<PathBuf> {
    let env_value: Option<OsString> = std::env::var_os(SPLASH_ENV);
    let local = std::env::var_os("LOCALAPPDATA");
    resolve_sources(env_value.as_deref(), local.as_deref())
}

/// One picture of the slideshow at a given time: which, how opaque, how far into its life.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Slide {
    pub index: usize,
    pub alpha: f32,
    /// 0 when it appears, 1 when it is gone (drives the Ken Burns drift).
    pub progress: f32,
}

/// The pictures on screen at one time: the current one, opaque, and during the last `fade`
/// seconds of its slot the next one fading in over it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Timeline {
    pub current: Slide,
    pub incoming: Option<Slide>,
    /// The picture to decode ahead (the one after the current).
    pub next_index: usize,
}

/// Which pictures show at `time` for `count` pictures, `period` seconds apart with a `fade`.
pub fn timeline(time: f64, count: usize, period: f64, fade: f64) -> Timeline {
    let count = count.max(1);
    let time = time.max(0.0);
    let slot = (time / period).floor();
    let life = period + fade;
    let progress = |slot: f64| -> f32 {
        let start = (slot * period - fade).max(0.0);
        ((time - start) / life).clamp(0.0, 1.0) as f32
    };
    let index = |slot: f64| (slot as usize) % count;
    let current = Slide {
        index: index(slot),
        alpha: 1.0,
        progress: progress(slot),
    };
    let into_fade = time - ((slot + 1.0) * period - fade);
    let incoming = (count > 1 && into_fade > 0.0).then(|| Slide {
        index: index(slot + 1.0),
        alpha: (into_fade / fade).clamp(0.0, 1.0) as f32,
        progress: progress(slot + 1.0),
    });
    Timeline {
        current,
        incoming,
        next_index: index(slot + 1.0),
    }
}

/// The size a picture of `width` x `height` shrinks to so its longest side is at most `max_side`
/// (unchanged when it already fits; never below 1 pixel).
pub fn fit_size(width: u32, height: u32, max_side: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= max_side || longest == 0 {
        return (width, height);
    }
    let scale = f64::from(max_side) / f64::from(longest);
    let shrink = |side: u32| ((f64::from(side) * scale).round() as u32).max(1);
    (shrink(width), shrink(height))
}

/// Whether the splash is on screen: at start-up for its duration, then while a job runs unless the
/// user hid it for this job.
pub fn is_visible(elapsed: Duration, job_running: bool, hidden_for_job: bool) -> bool {
    elapsed < STARTUP_DURATION || (job_running && !hidden_for_job)
}

/// The line under the picture: the translate progress, else the last log line, else `idle`.
pub fn status_text(progress: Option<&Progress>, last_line: Option<&str>, idle: &str) -> String {
    if let Some(p) = progress {
        return format!("{} of {} {} ({}%)", p.done, p.total, p.what, p.percent);
    }
    match last_line.map(str::trim).filter(|l| !l.is_empty()) {
        Some(line) => line.chars().take(110).collect(),
        None => idle.to_string(),
    }
}

/// Decodes a PNG or JPEG into a colour image, shrunk so its longest side is at most `max_side`.
pub fn load_image(path: &Path, max_side: u32) -> Result<ColorImage, String> {
    let decoded = image::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(to_color_image(decoded, max_side))
}

/// A decoded picture as a colour image, shrunk so its longest side is at most `max_side`.
pub fn to_color_image(decoded: image::DynamicImage, max_side: u32) -> ColorImage {
    let (width, height) = fit_size(decoded.width(), decoded.height(), max_side);
    let shrunk = if (width, height) == (decoded.width(), decoded.height()) {
        decoded
    } else {
        decoded.resize_exact(width, height, image::imageops::FilterType::Triangle)
    };
    let rgba = shrunk.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    ColorImage::from_rgba_unmultiplied(size, rgba.as_raw())
}

/// A picture ready to paint: its texture, opacity and how far into its life it is.
pub struct Layer {
    pub texture: TextureHandle,
    pub index: usize,
    pub alpha: f32,
    pub progress: f32,
}

type Decoded = (usize, Option<ColorImage>);

/// The splash's state: when it started, its pictures (decoded one at a time on a thread, at most
/// two textures alive) and the job it was hidden for.
pub struct Splash {
    started: Instant,
    slides_started: Instant,
    count: usize,
    requests: Sender<(usize, Source)>,
    results: Receiver<Decoded>,
    paths: Vec<Source>,
    requested: HashSet<usize>,
    failed: HashSet<usize>,
    textures: HashMap<usize, TextureHandle>,
    hidden_for_job: bool,
    startup_skipped: bool,
    was_running: bool,
}

impl Splash {
    pub fn new() -> Self {
        let paths = crate::art::sources(current_sources());
        let (request_tx, request_rx) = mpsc::channel::<(usize, Source)>();
        let (result_tx, result_rx) = mpsc::channel::<Decoded>();
        std::thread::spawn(move || {
            while let Ok((index, source)) = request_rx.recv() {
                let image = match crate::art::load(&source, MAX_TEXTURE_SIDE) {
                    Ok(image) => Some(image),
                    Err(error) => {
                        eprintln!("splash picture ignored: {error}");
                        None
                    }
                };
                if result_tx.send((index, image)).is_err() {
                    break;
                }
            }
        });
        let now = Instant::now();
        Splash {
            started: now,
            slides_started: now,
            count: paths.len(),
            requests: request_tx,
            results: result_rx,
            paths,
            requested: HashSet::new(),
            failed: HashSet::new(),
            textures: HashMap::new(),
            hidden_for_job: false,
            startup_skipped: false,
            was_running: false,
        }
    }

    /// Takes the pictures the thread finished, asks for the ones now needed and drops the others,
    /// then returns the layers to paint (the current picture first, the fading-in one over it).
    pub fn layers(&mut self, ctx: &egui::Context) -> Vec<Layer> {
        if self.count == 0 {
            return Vec::new();
        }
        while let Ok((index, image)) = self.results.try_recv() {
            match image {
                Some(image) if self.requested.contains(&index) => {
                    let name = format!("splash{index}");
                    let texture = ctx.load_texture(name, image, TextureOptions::LINEAR);
                    self.textures.insert(index, texture);
                }
                Some(_) => {}
                None => {
                    self.failed.insert(index);
                }
            }
        }
        let line = timeline(
            self.slides_started.elapsed().as_secs_f64(),
            self.count,
            SLIDE_PERIOD,
            FADE_DURATION,
        );
        let wanted: HashSet<usize> = [line.current.index, line.next_index].into();
        self.textures.retain(|index, _| wanted.contains(index));
        self.requested.retain(|index| wanted.contains(index));
        for index in &wanted {
            if self.requested.contains(index) || self.failed.contains(index) {
                continue;
            }
            self.requested.insert(*index);
            let _ = self.requests.send((*index, self.paths[*index].clone()));
        }
        let slides = std::iter::once(line.current).chain(line.incoming);
        slides
            .filter_map(|slide| {
                let texture = self.textures.get(&slide.index)?.clone();
                Some(Layer {
                    texture,
                    index: slide.index,
                    alpha: slide.alpha,
                    progress: slide.progress,
                })
            })
            .collect()
    }

    /// Whether to draw it this frame; also re-arms it when a new job starts.
    pub fn visible(&mut self, job_running: bool) -> bool {
        if job_running && !self.was_running {
            self.hidden_for_job = false;
            self.slides_started = Instant::now();
        }
        self.was_running = job_running;
        let elapsed = if self.startup_skipped {
            STARTUP_DURATION
        } else {
            self.started.elapsed()
        };
        is_visible(elapsed, job_running, self.hidden_for_job)
    }

    /// Ends the start-up moment now (a click on the splash).
    pub fn skip_startup(&mut self) {
        self.startup_skipped = true;
    }

    /// Hides the launch screen until the next job ("Show details").
    pub fn hide_for_job(&mut self) {
        self.hidden_for_job = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), b"x").expect("write");
    }

    fn names(paths: &[PathBuf]) -> Vec<String> {
        paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_folder_lists_png_and_jpeg_sorted_by_name() {
        let dir = tempfile::tempdir().expect("temp dir");
        for n in [
            "04.jpg",
            "02.JPEG",
            "01-a.png",
            "03.jpg",
            "notes.txt",
            "x.gif",
        ] {
            touch(dir.path(), n);
        }
        std::fs::create_dir(dir.path().join("sub.png")).expect("dir");
        let got = list_images(dir.path());
        assert_eq!(names(&got), ["01-a.png", "02.JPEG", "03.jpg", "04.jpg"]);
        assert!(list_images(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn the_override_file_comes_first() {
        let dir = tempfile::tempdir().expect("temp dir");
        touch(dir.path(), "mine.png");
        let file = dir.path().join("mine.png");
        let got = resolve_sources(Some(file.as_os_str()), Some(OsStr::new("C:\\none")));
        assert_eq!(got, vec![file]);
    }

    #[test]
    fn the_override_folder_beats_the_local_folder() {
        let env_dir = tempfile::tempdir().expect("temp dir");
        touch(env_dir.path(), "b.jpg");
        touch(env_dir.path(), "a.png");
        let local = tempfile::tempdir().expect("temp dir");
        let splash = local
            .path()
            .join("AnyXboxX")
            .join("launcher")
            .join("splash");
        std::fs::create_dir_all(&splash).expect("dirs");
        touch(&splash, "z.png");
        let got = resolve_sources(
            Some(env_dir.path().as_os_str()),
            Some(local.path().as_os_str()),
        );
        assert_eq!(names(&got), ["a.png", "b.jpg"]);
        let got = resolve_sources(None, Some(local.path().as_os_str()));
        assert_eq!(names(&got), ["z.png"]);
    }

    #[test]
    fn the_single_splash_png_is_the_last_resort() {
        let local = tempfile::tempdir().expect("temp dir");
        let launcher = local.path().join("AnyXboxX").join("launcher");
        std::fs::create_dir_all(&launcher).expect("dirs");
        touch(&launcher, "splash.png");
        let got = resolve_sources(
            Some(OsStr::new("D:\\gone.png")),
            Some(local.path().as_os_str()),
        );
        assert_eq!(got, vec![launcher.join("splash.png")]);
    }

    #[test]
    fn empty_or_missing_inputs_give_nothing() {
        assert!(resolve_sources(Some(OsStr::new("")), Some(OsStr::new(""))).is_empty());
        assert!(resolve_sources(None, None).is_empty());
        let empty = tempfile::tempdir().expect("temp dir");
        assert!(resolve_sources(None, Some(empty.path().as_os_str())).is_empty());
    }

    #[test]
    fn the_timeline_holds_then_cross_fades() {
        let (p, f) = (SLIDE_PERIOD, FADE_DURATION);
        let start = timeline(0.0, 4, p, f);
        assert_eq!(start.current.index, 0);
        assert!(start.incoming.is_none());
        assert_eq!(start.next_index, 1);
        let held = timeline(p - f - 0.01, 4, p, f);
        assert!(held.incoming.is_none());
        let half = timeline(p - f / 2.0, 4, p, f);
        assert_eq!(half.current.index, 0);
        let incoming = half.incoming.expect("fading in");
        assert_eq!(incoming.index, 1);
        assert!((incoming.alpha - 0.5).abs() < 1e-4);
        let next = timeline(p + 0.1, 4, p, f);
        assert_eq!(next.current.index, 1);
        assert!(next.incoming.is_none());
        assert_eq!(timeline(4.0 * p + 0.1, 4, p, f).current.index, 0);
    }

    #[test]
    fn one_picture_never_fades_and_progress_stays_in_range() {
        let one = timeline(100.0, 1, SLIDE_PERIOD, FADE_DURATION);
        assert_eq!(one.current.index, 0);
        assert!(one.incoming.is_none());
        let none = timeline(5.9, 0, SLIDE_PERIOD, FADE_DURATION);
        assert_eq!(none.current.index, 0);
        for t in [0.0, 3.0, 5.5, 6.0, 11.7, 600.0] {
            let line = timeline(t, 3, SLIDE_PERIOD, FADE_DURATION);
            assert!((0.0..=1.0).contains(&line.current.progress));
            if let Some(i) = line.incoming {
                assert!((0.0..=1.0).contains(&i.alpha));
                assert!((0.0..=1.0).contains(&i.progress));
            }
        }
    }

    #[test]
    fn large_pictures_shrink_to_the_limit() {
        assert_eq!(fit_size(1280, 720, 2560), (1280, 720));
        assert_eq!(fit_size(2560, 2560, 2560), (2560, 2560));
        assert_eq!(fit_size(5120, 2880, 2560), (2560, 1440));
        assert_eq!(fit_size(1932, 5000, 2500), (966, 2500));
        assert_eq!(fit_size(10000, 1, 2560), (2560, 1));
        assert_eq!(fit_size(0, 0, 2560), (0, 0));
    }

    #[test]
    fn visibility_follows_startup_then_jobs() {
        let short = Duration::from_millis(500);
        let long = Duration::from_secs(5);
        assert!(is_visible(short, false, false));
        assert!(!is_visible(long, false, false));
        assert!(is_visible(long, true, false));
        assert!(!is_visible(long, true, true));
    }

    #[test]
    fn skip_startup_is_idempotent_and_ends_startup_visibility() {
        let mut splash = Splash::new();
        assert!(splash.visible(false));
        for _ in 0..5 {
            splash.skip_startup();
        }
        assert!(!splash.visible(false));
        assert!(splash.visible(true));
    }

    #[test]
    fn status_prefers_progress_then_log_then_idle() {
        let p = Progress {
            file: "f".into(),
            done: 3,
            total: 10,
            percent: 30,
            what: "containers".into(),
        };
        assert_eq!(
            status_text(Some(&p), Some("log"), "idle"),
            "3 of 10 containers (30%)"
        );
        assert_eq!(status_text(None, Some("  building  "), "idle"), "building");
        assert_eq!(status_text(None, Some("   "), "idle"), "idle");
        assert_eq!(status_text(None, None, "idle"), "idle");
    }

    #[test]
    fn a_png_and_a_jpeg_are_decoded_and_shrunk() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("s.png");
        image::RgbaImage::from_pixel(40, 20, image::Rgba([255, 0, 128, 255]))
            .save(&path)
            .expect("write png");
        let small = load_image(&path, 10).expect("decode");
        assert_eq!(small.size, [10, 5]);
        let full = load_image(&path, 100).expect("decode");
        assert_eq!(full.size, [40, 20]);
        let jpg = dir.path().join("s.jpg");
        image::RgbImage::from_pixel(40, 20, image::Rgb([10, 200, 30]))
            .save(&jpg)
            .expect("write jpeg");
        assert_eq!(load_image(&jpg, 10).expect("decode jpeg").size, [10, 5]);
        assert!(load_image(&dir.path().join("none.png"), 10).is_err());
    }
}
