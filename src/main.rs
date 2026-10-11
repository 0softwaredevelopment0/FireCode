#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod auth;
mod config;

use std::collections::BTreeMap;

fn main() -> eframe::Result {
    let (config, config_path) =
        config::load_or_create().unwrap_or_else(|e| panic!("config error: {e:#}"));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("FireCode")
            .with_inner_size([1000.0, 650.0]),
        ..Default::default()
    };
    eframe::run_native(
        "FireCode",
        options,
        Box::new(|_cc| Ok(Box::new(FireCodeApp::new(config, config_path)))),
    )
}

#[derive(Debug, Clone)]
enum TokenStatus {
    Ok { key: String, masked: String },
    Failed { error: String },
    Pending,
}

struct FireCodeApp {
    config: config::Config,
    config_path: std::path::PathBuf,
    tokens: BTreeMap<String, TokenStatus>,
}

impl FireCodeApp {
    fn new(config: config::Config, config_path: std::path::PathBuf) -> Self {
        let mut app = Self {
            config,
            config_path,
            tokens: BTreeMap::new(),
        };
        app.reread_tokens();
        app
    }

    fn reread_tokens(&mut self) {
        let ids: Vec<String> = self.config.sites.keys().cloned().collect();
        for id in ids {
            self.tokens.insert(id.clone(), TokenStatus::Pending);
            let site = match self.config.site(&id) {
                Ok(site) => site.clone(),
                Err(e) => {
                    self.tokens.insert(
                        id,
                        TokenStatus::Failed {
                            error: format!("{e:#}"),
                        },
                    );
                    continue;
                }
            };
            let status = match auth::read_token(
                std::path::Path::new(&self.config.profile.path),
                &site.token,
            ) {
                Ok(token) => {
                    let masked = token.masked();
                    TokenStatus::Ok {
                        key: token.key,
                        masked,
                    }
                }
                Err(e) => TokenStatus::Failed {
                    error: format!("{e:#}"),
                },
            };
            self.tokens.insert(id, status);
        }
    }
}

impl eframe::App for FireCodeApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("FireCode");
            ui.label(format!(
                "v{} - browserless AI chat client",
                env!("CARGO_PKG_VERSION")
            ));
            ui.separator();

            ui.group(|ui| {
                ui.label("Configuration");
                ui.monospace(self.config_path.display().to_string());
                if self.config.profile.path.is_empty() {
                    ui.colored_label(egui::Color32::YELLOW, "Firefox profile: not detected");
                } else {
                    ui.label(format!("Firefox profile: {}", self.config.profile.path));
                }
            });

            ui.add_space(8.0);

            ui.group(|ui| {
                ui.label("Sites & auth tokens");
                for (id, site) in &self.config.sites {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let is_default = *id == self.config.default_site;
                        let name = if is_default {
                            format!("{} [default]", site.display_name)
                        } else {
                            site.display_name.clone()
                        };
                        ui.strong(name);
                        ui.label(format!("({})", site.token.kind));
                    });
                    match self.tokens.get(id) {
                        Some(TokenStatus::Ok { key, masked }) => {
                            ui.colored_label(
                                egui::Color32::LIGHT_GREEN,
                                format!("token '{key}': {masked}"),
                            );
                        }
                        Some(TokenStatus::Failed { error }) => {
                            ui.colored_label(egui::Color32::RED, format!("token error: {error}"));
                        }
                        _ => {
                            ui.label("token: reading...");
                        }
                    }
                }
                ui.add_space(4.0);
                if ui.button("Re-read tokens").clicked() {
                    self.reread_tokens();
                }
            });
        });
    }
}
