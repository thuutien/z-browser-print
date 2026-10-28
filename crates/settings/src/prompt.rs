//! The "Allow this website to print?" window that the agent opens when an unknown
//! website tries to print. Its answer goes into the config and the agent reloads.

use std::path::PathBuf;

use eframe::egui;
use zprint_core::config::Config;

use crate::{agent, compact_style, APP_NAME, ICON_RGBA};

pub fn run(config_path: PathBuf, origin: String) -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_app_id("zprint")
            .with_inner_size([440.0, 150.0])
            .with_resizable(false)
            .with_always_on_top()
            .with_icon(egui::IconData { rgba: ICON_RGBA.to_vec(), width: 64, height: 64 }),
        ..Default::default()
    };
    eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |cc| {
            cc.egui_ctx.all_styles_mut(compact_style);
            Ok(Box::new(Prompt { config_path, origin, error: None, raised: false }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("could not open the window: {e}"))
}

struct Prompt {
    config_path: PathBuf,
    origin: String,
    error: Option<String>,
    /// Brought to the front once it is on screen.
    raised: bool,
}

impl Prompt {
    fn answer(&self, allow: bool) -> anyhow::Result<()> {
        let mut config = Config::load(&self.config_path)?;
        let origin = &self.origin;
        config.allowed_origins.retain(|o| o != origin);
        config.denied_origins.retain(|o| o != origin);
        if allow {
            config.allowed_origins.push(origin.clone());
        } else {
            config.denied_origins.push(origin.clone());
        }
        config.save(&self.config_path)?;
        agent::reload(config.port).map_err(|e| anyhow::anyhow!(e))
    }
}

impl eframe::App for Prompt {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Window managers often ignore "on top" until the window is shown, and the
        // browser that triggered us usually has focus, so ask again after mapping.
        if !self.raised {
            self.raised = true;
            let ctx = ui.ctx();
            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::AlwaysOnTop));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                egui::UserAttentionType::Informational,
            ));
        }
        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(4.0);
            ui.heading("Allow this website to print?");
            ui.add_space(4.0);
            ui.label(egui::RichText::new(&self.origin).monospace().strong());
            ui.label(egui::RichText::new("It wants to use your Zebra printers.").small().weak());
            if let Some(error) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let allow = ui.add(egui::Button::new(egui::RichText::new("  Allow  ").strong()));
                let block = ui.button("Block").on_hover_text("Never let this website print");
                // When the window isn't focused yet (the browser usually is), the
                // window manager uses the press to focus us and egui never sees it.
                // Count a release over the button as a click too, so one click works.
                let released = ui.input(|i| i.pointer.primary_released());
                let pressed = |b: &egui::Response| b.clicked() || (released && b.hovered());
                let choice = if pressed(&allow) {
                    Some(true)
                } else if pressed(&block) {
                    Some(false)
                } else {
                    None
                };
                if let Some(allow) = choice {
                    match self.answer(allow) {
                        Ok(()) => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
                        Err(e) => self.error = Some(format!("Could not save: {e:#}")),
                    }
                }
            });
        });
    }
}
