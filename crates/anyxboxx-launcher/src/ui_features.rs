//! The "Features" card of the Play page: this line's optional switches (`features`), each with
//! what it costs and how far it is measured.

use eframe::egui::{self, Ui};

use crate::app::App;
use crate::features::{self, DlssMode};
use crate::host::GpuVendor;
use crate::ui_options::{card_title, hint, pills, toggle_row};
use crate::{theme, tokens, widgets};

pub fn card(app: &mut App, ui: &mut Ui) {
    theme::card(ui, |ui| {
        card_title(ui, "Features");
        let nvidia = app.gpu == GpuVendor::Nvidia;
        let mut pick_model = false;
        let features = &mut app.settings.options.features;
        toggle_row(ui, "Stats overlay (F3 in game)", &mut features.overlay);
        hint(
            ui,
            "Frame rate, frame time, memory and textures in the upper left corner.",
        );
        ui.add_space(tokens::S);
        toggle_row(
            ui,
            "Ray-traced light: the title's own, interiors lit right (about 14 fps)",
            &mut features.ray_tracing,
        );
        ui.add_enabled_ui(!features.ray_tracing, |ui| {
            toggle_row(
                ui,
                "Natural eyes without ray tracing (reflections bounded)",
                &mut features.natural_eyes,
            );
        });
        ui.add_space(tokens::S);
        toggle_row(
            ui,
            "Skin confetti fix (experimental: uploads keep what the card wrote)",
            &mut features.confetti_fix,
        );
        ui.add_space(tokens::S);
        ui.add_enabled_ui(nvidia, |ui| {
            toggle_row(
                ui,
                "NVIDIA DLSS in place of the title's upscaler (experimental)",
                &mut features.dlss,
            );
        });
        if !nvidia {
            hint(ui, "DLSS needs an NVIDIA RTX card.");
        }
        if features.dlss && nvidia {
            pills(ui, &mut features.dlss_mode, &DlssMode::ALL, DlssMode::label);
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut features.dlss_dll)
                        .hint_text("nvngx_dlss.dll or its folder")
                        .desired_width(ui.available_width() - 90.0),
                );
                pick_model = widgets::secondary_button(ui, true, "Browse").clicked();
            });
            if features::dlss_model_found(&features.dlss_dll) {
                hint(ui, "DLSS model found.");
            } else {
                ui.label(
                    tokens::caption(
                        "Pick the nvngx_dlss.dll of a game you own: it is never shipped here. Without it the title's own upscaler runs.",
                    )
                    .color(theme::WARN),
                );
            }
        }
        if pick_model {
            app.pick_dlss_model();
        }
    });
}
