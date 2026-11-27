//! The "Launch options" card grid of the Play page. The choices are plain data in `options` and
//! `features`; the settings file keeps them between runs. The features card is in `ui_features`.

use eframe::egui::{self, RichText, Ui};

use crate::app::App;
use crate::options::{Keyboard, LogLevel, Preset, WindowMode};
use crate::output::{self, Output};
use crate::{theme, tokens, ui_features, widgets};

pub fn cards(app: &mut App, ui: &mut Ui) {
    ui.columns(2, |columns| {
        display_card(app, &mut columns[0]);
        controls_card(app, &mut columns[1]);
    });
    ui.add_space(tokens::M);
    ui.columns(2, |columns| {
        ui_features::card(app, &mut columns[0]);
        performance_card(app, &mut columns[1]);
    });
    ui.add_space(tokens::M);
    diagnostics_card(app, ui);
}

pub fn card_title(ui: &mut Ui, text: &str) {
    ui.label(tokens::subtitle(text));
    ui.add_space(tokens::S);
}

pub fn hint(ui: &mut Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .color(theme::TEXT_MUTED)
            .size(tokens::CAPTION),
    );
}

/// One pill per choice; the chosen one is lit.
pub fn pills<T: Copy + PartialEq>(
    ui: &mut Ui,
    value: &mut T,
    all: &[T],
    label: impl Fn(T) -> &'static str,
) {
    ui.horizontal_wrapped(|ui| {
        for &choice in all {
            if widgets::pill(ui, *value == choice, label(choice)) {
                *value = choice;
            }
        }
    });
}

pub fn toggle_row(ui: &mut Ui, text: &str, value: &mut bool) {
    ui.horizontal_top(|ui| {
        widgets::toggle(ui, value);
        ui.add(egui::Label::new(tokens::body(text)).wrap());
    });
}

fn display_card(app: &mut App, ui: &mut Ui) {
    theme::card(ui, |ui| {
        card_title(ui, "Display");
        let screen = app.screen;
        let options = &mut app.settings.options;
        pills(ui, &mut options.output, &Output::ALL, Output::label);
        let size = options.output.size(screen);
        let said = match (options.output, size, screen) {
            (Output::Screen, Some(size), Some(screen)) => format!(
                "Your screen is {}: the game draws at {}.",
                output::format_size(screen),
                output::format_size(size)
            ),
            (_, Some(size), _) => format!("The game draws at {}.", output::format_size(size)),
            (Output::Screen, None, None) => {
                "The screen size could not be read: the title's own 3840x2160 is used.".to_string()
            }
            _ => "The title's own 3840x2160, rendered at two thirds and upscaled.".to_string(),
        };
        hint(ui, &said);
        ui.add_space(tokens::S);
        ui.add_enabled_ui(size.is_some(), |ui| {
            toggle_row(
                ui,
                "Native render: draw at the output size, not at two thirds of it",
                &mut options.native_render,
            );
        });
        hint(
            ui,
            "The title only takes the sizes of its own list; with DLSS on, DLSS picks the render size.",
        );
        ui.add_space(tokens::S);
        pills(ui, &mut options.window, &WindowMode::ALL, WindowMode::label);
    });
}

fn controls_card(app: &mut App, ui: &mut Ui) {
    theme::card(ui, |ui| {
        card_title(ui, "Controls");
        let options = &mut app.settings.options;
        pills(ui, &mut options.keyboard, &Keyboard::ALL, Keyboard::label);
        ui.add_space(tokens::S);
        ui.label(format!("Move: {}", options.keyboard.move_keys()));
        ui.label("Look: the mouse, or I J K L");
        ui.label(options.keyboard.shoulder_keys());
        ui.label("Confirm: Enter. Back: Backspace. Menu: Esc.");
        ui.add_space(tokens::S);
        ui.label(tokens::body("Mouse").strong());
        hint(
            ui,
            "Click the picture to take the mouse: it turns the camera, left button is the right trigger, right button the left one. F9 takes it or lets it go; switching windows lets it go.",
        );
        ui.add_space(tokens::XS);
        ui.label(tokens::body("F3: stats overlay").strong());
        hint(
            ui,
            "Shows or hides the numbers on the picture (frame rate, memory) when the overlay is on.",
        );
    });
}

fn performance_card(app: &mut App, ui: &mut Ui) {
    theme::card(ui, |ui| {
        card_title(ui, "Performance");
        let settings = &mut app.settings;
        pills(
            ui,
            &mut settings.options.preset,
            &Preset::ALL,
            Preset::label,
        );
        hint(ui, settings.options.preset.description());
        ui.add_space(tokens::S);
        toggle_row(
            ui,
            "Translate the shaders before playing (long, once per build and settings)",
            &mut settings.warm_before_play,
        );
        hint(
            ui,
            "Off: the game translates them as it meets them, so the first minutes are slow.",
        );
        ui.add_enabled_ui(settings.warm_before_play, |ui| {
            ui.add(
                egui::Slider::new(&mut settings.jobs, 0..=32)
                    .text("shader workers (0 = every core)"),
            );
            toggle_row(
                ui,
                "Also preload shaders.rpf and the game DLLs",
                &mut settings.options.warm_first,
            );
            toggle_row(
                ui,
                "Always re-warm (otherwise only when the cache is stale)",
                &mut settings.always_rewarm,
            );
        });
        ui.add_space(tokens::S);
        toggle_row(
            ui,
            "Tap Enter (and A) through the start screens for 150 seconds",
            &mut settings.options.auto_a,
        );
        toggle_row(
            ui,
            "NVIDIA Smooth Motion: driver frame generation (RTX 40 and 50 series)",
            &mut settings.options.smooth_motion,
        );
        hint(
            ui,
            "Twice the pictures shown, not more responsive. Written into the NVIDIA driver's profile for victory.exe at each launch.",
        );
    });
}

fn diagnostics_card(app: &mut App, ui: &mut Ui) {
    theme::card(ui, |ui| {
        card_title(ui, "Diagnostics");
        let logs = app.log_folder();
        let settings = &mut app.settings;
        ui.horizontal_wrapped(|ui| {
            ui.label("Log level of VIctory");
            pills(
                ui,
                &mut settings.options.log,
                &LogLevel::ALL,
                LogLevel::label,
            );
        });
        ui.add_space(tokens::S);
        toggle_row(
            ui,
            "Keep the launcher open after the game starts",
            &mut settings.keep_open_after_launch,
        );
        toggle_row(
            ui,
            "Skip the running-game check (only if you are sure no game runs)",
            &mut settings.skip_process_check,
        );
        ui.add_space(tokens::S);
        ui.horizontal_wrapped(|ui| {
            let label = logs
                .as_ref()
                .map_or("unknown".to_string(), |d| d.display().to_string());
            ui.label(tokens::caption(format!("Game logs: {label}")));
            if let Some(dir) = logs
                && widgets::secondary_button(ui, true, "Open logs folder").clicked()
            {
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::process::Command::new("explorer").arg(&dir).spawn();
            }
        });
    });
}
