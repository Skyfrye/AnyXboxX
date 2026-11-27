//! The landing page: the picture across a wide hero with a slow zoom and a gentle lean toward the
//! pointer, a dark gradient on its left carrying the name, the status chips and the big PLAY.

use eframe::egui::{self, Color32, Rect, Stroke, Ui, pos2, vec2};

use crate::app::{App, Tab, Verify};
use crate::home::{self, Readiness};
use crate::host::GpuVendor;
use crate::widgets::{self, Status};
use crate::{cover, shade, splash, theme, tokens, ui_states};

/// Height of the hero: a share of the page, between these bounds.
const HERO_MIN: f32 = 420.0;
const HERO_MAX: f32 = 760.0;
const HERO_SHARE: f32 = 1.0;
/// Height of the text block, to centre it in the hero.
const TEXT_HEIGHT: f32 = 400.0;
/// Share of the hero's width the text column takes, and the least it takes in points.
const COLUMN_SHARE: f32 = 0.46;
const COLUMN_MIN: f32 = 340.0;
/// How far the picture leans away from the pointer (in pan units, see `cover`).
const PARALLAX: f32 = 0.35;
/// Overlay opacity where the picture shows through, on the right.
const CLEAR_ALPHA: u8 = 36;

pub fn home(app: &mut App, ui: &mut Ui) {
    ui_states::banner(app, ui);
    let readiness = home::folder_readiness(app.settings.game_path().is_some(), &app.verify);
    let layers = app.splash.layers(ui.ctx());
    let height = (ui.available_height() * HERO_SHARE).clamp(HERO_MIN, HERO_MAX);
    let (hero, _) =
        ui.allocate_exact_size(vec2(ui.available_width(), height), egui::Sense::hover());
    let column = text_column(hero);
    paint_hero(ui, hero, column, &layers);
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(column));
    hero_text(app, &mut child, readiness);
}

/// Where the text sits: the left part of the hero, inside its padding, centred vertically.
fn text_column(hero: Rect) -> Rect {
    let pad = tokens::XXL;
    let width = (hero.width() * COLUMN_SHARE)
        .max(COLUMN_MIN)
        .min(hero.width() - 2.0 * pad);
    Rect::from_min_max(
        pos2(hero.left() + pad, hero.top() + pad),
        pos2(hero.left() + pad + width, hero.bottom() - pad),
    )
}

/// The picture (or the gradient when there is none), drifting, under a left-to-right shade.
fn paint_hero(ui: &Ui, hero: Rect, column: Rect, layers: &[splash::Layer]) {
    let radius = tokens::radius(tokens::R_HERO);
    if layers.is_empty() {
        ui.painter()
            .add(egui::Shape::mesh(theme::gradient_mesh(hero)));
    } else {
        let pointer = ui
            .input(|i| i.pointer.hover_pos())
            .filter(|p| hero.contains(*p));
        let lean = cover::parallax(pointer, hero, PARALLAX);
        let ctx = ui.ctx();
        let lean = vec2(
            ctx.animate_value_with_time(egui::Id::new("hero_pan_x"), lean.x, 0.45),
            ctx.animate_value_with_time(egui::Id::new("hero_pan_y"), lean.y, 0.45),
        );
        for layer in layers {
            let (zoom, drift) = cover::slide_view(layer.index, layer.progress, 0.5);
            let uv = cover::cover_uv(layer.texture.size_vec2(), hero.size(), zoom, drift + lean);
            egui::Image::new(&layer.texture)
                .uv(uv)
                .corner_radius(radius)
                .tint(Color32::WHITE.gamma_multiply(layer.alpha))
                .paint_at(ui, hero);
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(50));
    }
    let solid = (column.right() + tokens::L - hero.left()) / hero.width();
    let clear = (solid + 0.38).min(1.0);
    let [r, g, b] = theme::OVERLAY_RGB;
    let shade = shade::rounded_horizontal(hero, f32::from(tokens::R_HERO), |t| {
        let k = ((t - solid) / (clear - solid).max(1e-3)).clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - k) * (1.0 - k);
        let alpha = f32::from(theme::OVERLAY_ALPHA)
            + (f32::from(CLEAR_ALPHA) - f32::from(theme::OVERLAY_ALPHA)) * eased;
        Color32::from_rgba_unmultiplied(r, g, b, alpha.round() as u8)
    });
    let painter = ui.painter();
    painter.add(egui::Shape::mesh(shade));
    painter.rect_stroke(
        hero,
        radius,
        Stroke::new(1.0_f32, Color32::WHITE.gamma_multiply(0.12)),
        egui::StrokeKind::Inside,
    );
}

fn hero_text(app: &mut App, ui: &mut Ui, readiness: Readiness) {
    ui.add_space(((ui.available_height() - TEXT_HEIGHT) / 2.0).max(0.0));
    ui.label(tokens::display("AnyXboxX"));
    ui.label(
        egui::RichText::new("DIFFBASE")
            .size(tokens::SUBTITLE)
            .strong()
            .color(theme::GLOW),
    );
    ui.add_space(tokens::XS);
    ui.label(tokens::muted(
        "Run your own Xbox Series build on this PC with our VIctory line: card budgets, RAM relief and a lighter submitting thread. Nothing leaves your machine.",
    ));
    ui.add_space(tokens::L);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(tokens::S, tokens::S);
        status_chips(app, ui, readiness);
    });
    ui.add_space(tokens::XL);
    let busy = app.is_busy();
    let verifying = matches!(app.verify, Verify::Running);
    let label = home::play_label(readiness, busy, verifying);
    let size = vec2(280.0_f32.min(ui.available_width()), 64.0);
    let play = widgets::glow_button(ui, !busy, label, size, busy);
    if play.clicked() {
        press_play(app, readiness);
    }
    ui.add_space(tokens::M);
    let options = &app.settings.options;
    let size = options
        .size(app.screen)
        .map_or("title's own size".to_string(), crate::output::format_size);
    ui.label(tokens::caption(format!(
        "{size}{}. Preset {}. Keyboard: {}. Change them on the Play page.",
        if options.features.dlss { ", DLSS" } else { "" },
        options.preset.label(),
        options.keyboard.label()
    )));
    ui.label(tokens::caption(
        "In game: click the picture to use the mouse, F9 lets it go, F3 shows the stats.",
    ));
}

fn status_chips(app: &App, ui: &mut Ui, readiness: Readiness) {
    match app.gpu {
        GpuVendor::Unknown => widgets::chip(
            ui,
            Status::Warn,
            "GPU not detected",
            "DXGI listed no graphics card. Update the GPU driver, then press Check again on Setup.",
        ),
        gpu => widgets::chip(
            ui,
            Status::Good,
            &format!("GPU: {}", gpu.label()),
            app.adapter_name.as_deref().unwrap_or("Graphics card found"),
        ),
    };
    if app.cache_ready {
        widgets::chip(
            ui,
            Status::Good,
            "Shader cache filled",
            "The first launch will not wait for shaders.",
        );
    } else {
        widgets::chip(
            ui,
            Status::Warn,
            "Shader cache empty",
            "PLAY fills it before the game starts (minutes the first time).",
        );
    }
    let (status, text, tip) = match readiness {
        Readiness::Ready => (
            Status::Good,
            "Game folder ready",
            "Every required file is present.",
        ),
        Readiness::NeedsFolder => (
            Status::Warn,
            "No game folder",
            "Choose the folder that holds game_scarlett_master.exe.",
        ),
        Readiness::NeedsCheck => (
            Status::Warn,
            "Folder not checked",
            "PLAY checks the folder first (a few seconds).",
        ),
        Readiness::Problem => (
            Status::Bad,
            "Folder has problems",
            "Open the Game folder page to see which files are missing.",
        ),
    };
    widgets::chip(ui, status, text, tip);
}

/// What the big button does: launch when everything is ready, else go to the step that is not.
fn press_play(app: &mut App, readiness: Readiness) {
    match readiness {
        Readiness::Ready => app.start(false),
        Readiness::NeedsCheck => {
            app.launch_when_ready = true;
            app.start_verify();
        }
        Readiness::NeedsFolder => app.pick_game_folder(),
        Readiness::Problem => app.tab = Tab::Files,
    }
}
