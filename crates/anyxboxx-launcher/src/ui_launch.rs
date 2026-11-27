//! The full-window launch screen: the splash picture covering the window with a slow Ken Burns
//! drift, a dark gradient at the bottom, the stages of the launch, a large progress bar fed by
//! VIctory's progress lines, the elapsed time and the time left, and Cancel. At start-up the same
//! picture shows for a moment with the name alone.

use eframe::egui::{self, Align2, Color32, FontId, Rect, Stroke, Ui, epaint::Mesh, pos2, vec2};

use crate::app::App;
use crate::launch_progress::{self, Stage, Tracker};
use crate::progress::Progress;
use crate::widgets::{self, Status};
use crate::{bar, cover, splash, theme, tokens};

/// Widest the text column gets; it sits left, on the gradient.
const COLUMN_MAX: f32 = 880.0;
/// Height of the launch bar.
const BAR: f32 = 18.0;
/// Room the text block takes at the bottom of the window.
const BLOCK_HEIGHT: f32 = 300.0;

/// What one frame of the launch screen shows.
struct View<'a> {
    stages: &'a [Stage],
    tracker: &'a Tracker,
    status: String,
    now: f64,
    can_cancel: bool,
}

/// Draws the launch screen over the window when a job runs (or the preview is on), else the
/// start-up splash for its moment.
pub fn show(app: &mut App, ctx: &egui::Context) {
    let busy = app.running.is_some();
    let preview = app.preview_launch && !busy;
    if !app.splash.visible(busy) && !preview {
        return;
    }
    let layers = app.splash.layers(ctx);
    let now = app.now_secs();
    let screen = ctx.content_rect();
    let mut cancel = false;
    let mut hide = false;
    egui::Area::new(egui::Id::new("launch_screen"))
        .order(egui::Order::Foreground)
        .fixed_pos(screen.min)
        .show(ctx, |ui| {
            ui.set_clip_rect(screen);
            let (rect, response) = ui.allocate_exact_size(screen.size(), egui::Sense::click());
            paint_picture(ui, rect, &layers);
            if !busy && !preview {
                startup(ui, rect);
                if response.clicked() {
                    app.splash.skip_startup();
                }
                return;
            }
            let preview_tracker;
            let (stages, tracker, status, can_cancel) = if busy {
                let running = app.running.as_ref().map(|r| r.stages.clone());
                let status = match app.tracker.progress() {
                    Some(progress) => launch_progress::count_line(progress),
                    None => {
                        splash::status_text(None, app.log.back().map(String::as_str), "Working...")
                    }
                };
                let stage = app.tracker.stage();
                (
                    running.unwrap_or_default(),
                    &app.tracker,
                    status,
                    stage != Some(Stage::Start),
                )
            } else {
                preview_tracker = preview_model(now);
                let status = launch_progress::count_line(
                    preview_tracker.progress().expect("the preview has a count"),
                );
                (Stage::ALL.to_vec(), &preview_tracker, status, true)
            };
            let view = View {
                stages: &stages,
                tracker,
                status,
                now,
                can_cancel,
            };
            (cancel, hide) = block(ui, rect, &view);
        });
    if cancel {
        app.cancel_job();
    }
    if hide {
        app.splash.hide_for_job();
        app.preview_launch = false;
    }
    ctx.request_repaint_after(std::time::Duration::from_millis(33));
}

/// The picture covering `rect`, drifting and leaning with the pointer, under the dark overlay.
fn paint_picture(ui: &Ui, rect: Rect, layers: &[splash::Layer]) {
    let painter = ui.painter();
    painter.add(egui::Shape::mesh(theme::gradient_mesh(rect)));
    let pointer = ui.input(|i| i.pointer.hover_pos());
    let lean = cover::parallax(pointer, rect, 0.15);
    for layer in layers {
        let (zoom, drift) = cover::slide_view(layer.index, layer.progress, 1.0);
        let uv = cover::cover_uv(layer.texture.size_vec2(), rect.size(), zoom, drift + lean);
        let tint = Color32::WHITE.gamma_multiply(layer.alpha);
        painter.image(layer.texture.id(), rect, uv, tint);
    }
    painter.rect_filled(rect, 0.0, Color32::from_rgba_unmultiplied(12, 4, 24, 60));
    let solid_from = (rect.bottom() - BLOCK_HEIGHT).max(rect.top());
    painter.add(egui::Shape::mesh(fade_down(
        Rect::from_min_max(
            pos2(rect.left(), solid_from - 160.0),
            pos2(rect.right(), solid_from),
        ),
        0,
        theme::OVERLAY_ALPHA,
    )));
    painter.add(egui::Shape::mesh(fade_down(
        Rect::from_min_max(pos2(rect.left(), solid_from), rect.max),
        theme::OVERLAY_ALPHA,
        240,
    )));
}

/// A band of the overlay colour whose opacity runs from `top` to `bottom`.
fn fade_down(band: Rect, top: u8, bottom: u8) -> Mesh {
    let [r, g, b] = theme::OVERLAY_RGB;
    let at = |alpha| Color32::from_rgba_unmultiplied(r, g, b, alpha);
    let mut mesh = Mesh::default();
    mesh.colored_vertex(band.left_top(), at(top));
    mesh.colored_vertex(band.right_top(), at(top));
    mesh.colored_vertex(band.left_bottom(), at(bottom));
    mesh.colored_vertex(band.right_bottom(), at(bottom));
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 3, 2);
    mesh
}

/// The text column's rectangle at the bottom left of the window.
fn column(rect: Rect) -> Rect {
    let side = tokens::XXL.min(rect.width() * 0.06).max(tokens::L);
    let width = (rect.width() - 2.0 * side).min(COLUMN_MAX);
    let top = (rect.bottom() - BLOCK_HEIGHT + tokens::L).max(rect.top() + tokens::L);
    Rect::from_min_max(
        pos2(rect.left() + side, top),
        pos2(rect.left() + side + width, rect.bottom() - tokens::XL),
    )
}

/// Start-up: the name over the picture; a click skips it.
fn startup(ui: &Ui, rect: Rect) {
    let col = column(rect);
    let painter = ui.painter();
    painter.text(
        pos2(col.left(), col.bottom() - 64.0),
        Align2::LEFT_BOTTOM,
        "AnyXboxX",
        FontId::proportional(64.0),
        theme::TEXT,
    );
    painter.text(
        pos2(col.left(), col.bottom() - 36.0),
        Align2::LEFT_BOTTOM,
        "Diffbase: our VIctory line. Starting...",
        FontId::proportional(tokens::SUBTITLE),
        theme::TEXT_MUTED,
    );
}

/// The stages, the title, the bar, the timing and the buttons. Returns (cancel, hide) clicks.
fn block(ui: &mut Ui, rect: Rect, view: &View) -> (bool, bool) {
    let col = column(rect);
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(col));
    let ui = &mut child;
    stepper(ui, view.stages, view.tracker.stage());
    ui.add_space(tokens::M);
    let title = view.tracker.stage().map_or("Getting ready", Stage::label);
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(title)
                .size(40.0)
                .strong()
                .color(theme::TEXT),
        );
        if let Some(progress) = view.tracker.progress() {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(format!("{}%", progress.percent))
                        .size(tokens::TITLE)
                        .strong()
                        .color(theme::TEXT),
                );
            });
        }
    });
    ui.label(tokens::muted(view.status.as_str()));
    ui.add_space(tokens::M);
    bar::sized_bar(ui, view.tracker.fraction(), "", BAR);
    ui.add_space(tokens::M);
    let mut clicks = (false, false);
    ui.horizontal(|ui| {
        let eta = view.tracker.eta(view.now);
        let timing = launch_progress::timing_line(view.tracker.elapsed(view.now), eta);
        ui.label(tokens::body(timing));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if view.can_cancel {
                clicks.0 = widgets::secondary_button(ui, true, "Cancel").clicked();
            }
            clicks.1 = widgets::secondary_button(ui, true, "Show details").clicked();
        });
    });
    clicks
}

/// The stages as dots on a line: done (a check), current (a pulsing ring), still to come.
fn stepper(ui: &mut Ui, stages: &[Stage], current: Option<Stage>) {
    let time = ui.input(|i| i.time);
    ui.horizontal(|ui| {
        for (i, &stage) in stages.iter().enumerate() {
            if i > 0 {
                let (line, _) = ui.allocate_exact_size(vec2(40.0, 20.0), egui::Sense::hover());
                let done = current.is_some_and(|c| stage <= c);
                let color = if done {
                    theme::HOT_PINK
                } else {
                    theme::TEXT_MUTED.gamma_multiply(0.4)
                };
                ui.painter()
                    .hline(line.x_range(), line.center().y, Stroke::new(2.0_f32, color));
            }
            let (dot, _) = ui.allocate_exact_size(vec2(20.0, 20.0), egui::Sense::hover());
            let c = dot.center();
            let painter = ui.painter();
            match current {
                Some(now) if stage < now => widgets::status_icon(painter, c, 9.0, Status::Good),
                Some(now) if stage == now => {
                    let glow = crate::anim::pulse(time, 1.6);
                    painter.circle_filled(c, 9.0 + 3.0 * glow, theme::HOT_PINK.gamma_multiply(0.3));
                    painter.circle_filled(c, 7.0, theme::HOT_PINK);
                }
                _ => {
                    painter.circle_stroke(
                        c,
                        7.0,
                        Stroke::new(1.5_f32, theme::TEXT_MUTED.gamma_multiply(0.7)),
                    );
                }
            }
            let active = current == Some(stage);
            let label = egui::RichText::new(stage.label()).size(tokens::CAPTION + 1.0);
            ui.label(if active {
                label.color(theme::TEXT).strong()
            } else {
                label.color(theme::TEXT_MUTED)
            });
        }
    });
}

/// The preview's made-up progress: a shader warm moving at a steady pace (debug builds only reach
/// this, through `--preview-launch`).
fn preview_model(now: f64) -> Tracker {
    const TOTAL: u64 = 25_689;
    const CYCLE: f64 = 90.0;
    let started = now - now.rem_euclid(CYCLE);
    let make = |t: f64| {
        let done = (((t - started) / CYCLE) * TOTAL as f64) as u64;
        Progress {
            file: "shaders.rpf".to_string(),
            done,
            total: TOTAL,
            percent: done * 100 / TOTAL,
            what: "containers".to_string(),
        }
    };
    let mut tracker = Tracker::start(started - 20.0);
    tracker.step("Building VIctory", started - 20.0);
    tracker.step("Warming the shader cache", started);
    tracker.observe(&make(started), started);
    tracker.observe(&make(now), now);
    tracker
}
