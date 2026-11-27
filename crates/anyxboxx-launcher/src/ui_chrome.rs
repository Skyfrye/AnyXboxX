//! The window's frame: the left navigation, the console drawer at the bottom and the toasts.

use eframe::egui::{self, Align2, Color32, CornerRadius, Rect, RichText, Stroke, Ui, pos2, vec2};

use crate::app::{App, Tab};
use crate::loglevel::{self, Level};
use crate::toast::Kind;
use crate::widgets::{self, Status};
use crate::{about, anim, theme, tokens};

/// Width of the left navigation.
pub const SIDEBAR_WIDTH: f32 = 212.0;
const CONSOLE_HEIGHT: f32 = 232.0;
/// Width of a toast.
const TOAST_WIDTH: f32 = 360.0;
/// Distance a toast slides in from the right edge.
const TOAST_SLIDE: f32 = 40.0;

/// The pages in the navigation, with their icon and name.
const PAGES: [(Tab, &str, &str); 5] = [
    (Tab::Home, "\u{1F3E0}", "Home"),
    (Tab::Setup, "\u{1F527}", "Setup"),
    (Tab::Files, "\u{1F4C1}", "Game folder"),
    (Tab::Shaders, "\u{2728}", "Shaders"),
    (Tab::Play, "\u{1F3AE}", "Play"),
];

/// The navigation: the name on top, one row per page, and the About line at the bottom.
pub fn sidebar(app: &mut App, ui: &mut Ui) {
    ui.add_space(tokens::S);
    ui.label(RichText::new("AnyXboxX").size(tokens::TITLE + 2.0).strong());
    ui.label(
        RichText::new("DIFFBASE")
            .size(tokens::CAPTION)
            .strong()
            .color(crate::theme::GLOW),
    );
    ui.label(tokens::caption("Our VIctory line, tuned on NVIDIA"));
    ui.add_space(tokens::L);
    for (tab, icon, label) in PAGES {
        if nav_row(ui, app.tab == tab, icon, label) {
            app.tab = tab;
        }
        ui.add_space(tokens::XS);
    }
    ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
        ui.add_space(tokens::S);
        ui.label(tokens::caption(format!("Commit {}", about::COMMIT)))
            .on_hover_text(about::line());
        ui.label(tokens::caption(format!("Launcher {}", about::VERSION)));
        ui.label(tokens::caption("GPL-3.0-or-later"));
    });
}

fn nav_row(ui: &mut Ui, selected: bool, icon: &str, label: &str) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), 44.0), egui::Sense::click());
    let hover = ui.ctx().animate_bool_with_time(
        response.id,
        response.hovered() || response.has_focus(),
        tokens::HOVER_TIME,
    );
    let radius = CornerRadius::same(tokens::R_CONTROL + 2);
    let lift = if selected { 0.0 } else { 3.0 * hover };
    let body = rect.translate(vec2(lift, 0.0));
    let painter = ui.painter();
    if selected {
        painter.rect_filled(body, radius, theme::BUTTON_PINK);
        painter.rect_filled(
            Rect::from_min_size(
                pos2(body.left() - 10.0, body.center().y - 10.0),
                vec2(3.0, 20.0),
            ),
            CornerRadius::same(2),
            theme::GLOW,
        );
    } else if hover > 0.0 {
        painter.rect_filled(
            body,
            radius,
            Color32::from_rgba_unmultiplied(0xff, 0x2d, 0x95, (56.0 * hover) as u8),
        );
    }
    widgets::focus_ring(ui, &response, body, radius);
    let color = if selected {
        Color32::WHITE
    } else {
        theme::TEXT
    };
    let painter = ui.painter();
    painter.text(
        pos2(body.left() + 24.0, body.center().y),
        Align2::CENTER_CENTER,
        icon,
        egui::FontId::proportional(18.0),
        color,
    );
    painter.text(
        pos2(body.left() + 48.0, body.center().y),
        Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(16.0),
        color,
    );
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
}

fn level_color(level: Level) -> Color32 {
    match level {
        Level::Step => theme::TEXT,
        Level::Error => theme::BAD,
        Level::Warn => theme::WARN,
        Level::Info => theme::TEXT_MUTED,
    }
}

/// The drawer's header row, always on screen: a button that opens the log, and "Clear".
pub fn console_header(app: &mut App, ui: &mut Ui) {
    ui.horizontal(|ui| {
        let verb = if app.drawer_open { "Hide" } else { "Show" };
        let label = format!("{verb} console ({} lines)", app.log.len());
        if widgets::pill(ui, app.drawer_open, &label) {
            app.drawer_open = !app.drawer_open;
        }
        if app.drawer_open {
            if widgets::secondary_button(ui, !app.log.is_empty(), "Copy log").clicked() {
                ui.ctx().copy_text(app.log_text());
                app.notice = Some("Log copied to the clipboard.".to_string());
            }
            if widgets::secondary_button(ui, !app.log.is_empty(), "Clear").clicked() {
                app.log.clear();
            }
        }
    });
}

/// The log, one colour per level, newest at the bottom.
pub fn console_body(app: &App, ui: &mut Ui) {
    theme::card(ui, |ui| {
        egui::ScrollArea::vertical()
            .id_salt("log")
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .max_height(CONSOLE_HEIGHT - 60.0)
            .show(ui, |ui| {
                if app.log.is_empty() {
                    ui.label(
                        RichText::new(
                            "Nothing yet: the output of the build and of the game lands here.",
                        )
                        .color(theme::TEXT_MUTED),
                    );
                }
                for line in &app.log {
                    let level = loglevel::classify_line(line);
                    let mut text = RichText::new(line).monospace().color(level_color(level));
                    if level == Level::Step {
                        text = text.strong();
                    }
                    ui.label(text);
                }
            });
    });
}

/// Height of the console drawer's body.
pub const fn console_height() -> f32 {
    CONSOLE_HEIGHT
}

fn kind_status(kind: Kind) -> Status {
    match kind {
        Kind::Success => Status::Good,
        Kind::Error => Status::Bad,
        Kind::Info => Status::Warn,
    }
}

/// The toasts, stacked at the top right: each slides in from the right edge and fades out.
pub fn toasts(app: &mut App, ctx: &egui::Context) {
    let now = ctx.input(|i| i.time);
    app.toasts.prune(now);
    if app.toasts.is_empty() {
        return;
    }
    let screen = ctx.content_rect();
    let mut y = screen.top() + tokens::M;
    for toast in app.toasts.visible() {
        let entered = anim::ease_out_cubic(((now - toast.born) / tokens::TOAST_IN) as f32);
        let x = screen.right() - tokens::M - TOAST_WIDTH + (1.0 - entered) * TOAST_SLIDE;
        let shown = egui::Area::new(egui::Id::new(("toast", toast.text.as_str())))
            .order(egui::Order::Tooltip)
            .fixed_pos(pos2(x, y))
            .interactable(false)
            .show(ctx, |ui| {
                ui.set_opacity(toast.alpha(now));
                toast_card(ui, kind_status(toast.kind), &toast.text);
            });
        y += shown.response.rect.height() + tokens::S;
    }
    ctx.request_repaint_after(std::time::Duration::from_millis(16));
}

fn toast_card(ui: &mut Ui, status: Status, text: &str) {
    egui::Frame::new()
        .fill(theme::CHIP_FILL)
        .stroke(Stroke::new(1.0_f32, status.color().gamma_multiply(0.8)))
        .corner_radius(CornerRadius::same(tokens::R_CONTROL + 4))
        .inner_margin(egui::Margin::symmetric(16, 12))
        .shadow(egui::Shadow {
            offset: [0, 6],
            blur: 18,
            spread: 0,
            color: Color32::from_black_alpha(120),
        })
        .show(ui, |ui| {
            ui.set_width(TOAST_WIDTH - 32.0);
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(vec2(20.0, 20.0), egui::Sense::hover());
                widgets::status_icon(ui.painter(), rect.center(), 9.0, status);
                ui.add(egui::Label::new(tokens::body(text)).wrap());
            });
        });
}
