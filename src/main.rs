#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod auth;
mod config;
mod deepseek;
mod pow;

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, Sender};

#[derive(Debug, Clone)]
enum TokenStatus {
    Ok { key: String, masked: String },
    Failed { error: String },
    Pending,
}

#[derive(Debug)]
enum ChatEvent {
    Delta { text: String },
    Finished { result: Result<(), String> },
}

struct FireCodeApp {
    config: config::Config,
    config_path: std::path::PathBuf,
    tokens: BTreeMap<String, TokenStatus>,
    default_token: Option<String>,
    messages: Vec<(&'static str, String)>,
    input: String,
    streaming_text: Option<String>,
    busy: bool,
    event_rx: Receiver<ChatEvent>,
    prompt_tx: Option<Sender<String>>,
}

fn spawn_worker(base_url: String, token: String) -> (Sender<String>, Receiver<ChatEvent>) {
    let (prompt_tx, prompt_rx) = std::sync::mpsc::channel::<String>();
    let (event_tx, event_rx) = std::sync::mpsc::channel::<ChatEvent>();

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = event_tx.send(ChatEvent::Finished {
                    result: Err(format!("tokio runtime: {e}")),
                });
                return;
            }
        };
        rt.block_on(async move {
            let client = match deepseek::DeepSeekClient::new(&base_url, &token) {
                Ok(c) => c,
                Err(e) => {
                    let _ = event_tx.send(ChatEvent::Finished {
                        result: Err(format!("{e:#}")),
                    });
                    return;
                }
            };
            let mut session: Option<String> = None;
            let mut parent_message_id: Option<i64> = None;

            while let Ok(prompt) = prompt_rx.recv() {
                if session.is_none() {
                    match client.create_session().await {
                        Ok(id) => session = Some(id),
                        Err(e) => {
                            let _ = event_tx.send(ChatEvent::Finished {
                                result: Err(format!("create session: {e:#}")),
                            });
                            continue;
                        }
                    }
                }
                let session_id = session.clone().expect("session checked above");
                let result = try_prompt(
                    &client,
                    &session_id,
                    &prompt,
                    parent_message_id,
                    &event_tx,
                )
                .await;
                if let Ok(Some(id)) = &result {
                    parent_message_id = Some(*id);
                }
                let _ = event_tx.send(ChatEvent::Finished {
                    result: match &result {
                        Ok(_) => Ok(()),
                        Err(e) => Err(format!("{e:#}")),
                    },
                });
            }
        });
    });

    (prompt_tx, event_rx)
}

async fn try_prompt(
    client: &deepseek::DeepSeekClient,
    session_id: &str,
    prompt: &str,
    parent_message_id: Option<i64>,
    event_tx: &Sender<ChatEvent>,
) -> Result<Option<i64>, anyhow::Error> {
    let resp = client
        .completion(session_id, prompt, parent_message_id, false)
        .await?;
    let mut acc = deepseek::ResponseAccumulator::default();
    let last = deepseek::pump_sse(resp, |payload| {
        if acc.feed(payload) {
            let _ = event_tx.send(ChatEvent::Delta {
                text: acc.text.clone(),
            });
        }
    })
    .await?;
    Ok(last)
}

impl FireCodeApp {
    fn new(config: config::Config, config_path: std::path::PathBuf) -> Self {
        let mut app = Self {
            config,
            config_path,
            tokens: BTreeMap::new(),
            default_token: None,
            messages: Vec::new(),
            input: String::new(),
            streaming_text: None,
            busy: false,
            event_rx: std::sync::mpsc::channel().1,
            prompt_tx: None,
        };
        app.reread_tokens();
        app
    }

    fn reread_tokens(&mut self) {
        self.default_token = None;
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
            match auth::read_token(std::path::Path::new(&self.config.profile.path), &site.token)
            {
                Ok(token) => {
                    if id == self.config.default_site {
                        self.default_token = Some(token.value.clone());
                    }
                    let masked = token.masked();
                    self.tokens.insert(
                        id,
                        TokenStatus::Ok { key: token.key, masked },
                    );
                }
                Err(e) => {
                    self.tokens.insert(
                        id,
                        TokenStatus::Failed {
                            error: format!("{e:#}"),
                        },
                    );
                }
            }
        }

        match (&self.default_token, self.config.site(&self.config.default_site)) {
            (Some(token), Ok(site)) => {
                let (prompt_tx, event_rx) = spawn_worker(site.api_base.clone(), token.clone());
                self.prompt_tx = Some(prompt_tx);
                self.event_rx = event_rx;
            }
            (None, _) => self.prompt_tx = None,
            (_, Err(e)) => {
                eprintln!("default site missing: {e}");
                self.prompt_tx = None;
            }
        }
    }

    fn send_message(&mut self) {
        let text = self.input.trim().to_owned();
        if text.is_empty() || self.busy {
            return;
        }
        let Some(prompt_tx) = &self.prompt_tx else {
            return;
        };
        self.messages.push(("user", text.clone()));
        self.input.clear();
        self.streaming_text = Some(String::new());
        self.busy = true;
        let _ = prompt_tx.send(text);
    }

    fn poll_events(&mut self, ctx: &egui::Context) {
        let mut repaint = false;
        while let Ok(event) = self.event_rx.try_recv() {
            repaint = true;
            match event {
                ChatEvent::Delta { text } => {
                    self.streaming_text = Some(text);
                }
                ChatEvent::Finished { result } => {
                    self.busy = false;
                    let text = self.streaming_text.take().unwrap_or_default();
                    match result {
                        Ok(()) => self.messages.push(("assistant", text)),
                        Err(e) => {
                            if !text.is_empty() {
                                self.messages.push(("assistant", text));
                            }
                            self.messages.push(("error", e));
                        }
                    }
                }
            }
        }
        if repaint {
            ctx.request_repaint();
        }
    }
}

impl eframe::App for FireCodeApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_events(&ctx);

        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("FireCode");
                ui.label(format!("v{}", env!("CARGO_PKG_VERSION")));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    egui::CollapsingHeader::new("⚙ auth")
                        .id_salt("auth_panel")
                        .show(ui, |ui| {
                            ui.monospace(self.config_path.display().to_string());
                            if self.config.profile.path.is_empty() {
                                ui.colored_label(
                                    egui::Color32::YELLOW,
                                    "Firefox profile: not detected",
                                );
                            } else {
                                ui.label(format!("profile: {}", self.config.profile.path));
                            }
                            for (id, status) in &self.tokens {
                                match status {
                                    TokenStatus::Ok { key, masked } => {
                                        ui.colored_label(
                                            egui::Color32::LIGHT_GREEN,
                                            format!("{id}: token '{key}': {masked}"),
                                        );
                                    }
                                    TokenStatus::Failed { error } => {
                                        ui.colored_label(
                                            egui::Color32::RED,
                                            format!("{id}: token error: {error}"),
                                        );
                                    }
                                    TokenStatus::Pending => {
                                        ui.label(format!("{id}: reading..."));
                                    }
                                }
                            }
                            if ui.button("Re-read tokens").clicked() {
                                self.reread_tokens();
                            }
                        });
                });
            });
            ui.separator();

            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    ui.add_space(4.0);
                    if self.messages.is_empty() && !self.busy {
                        ui.colored_label(
                            egui::Color32::GRAY,
                            "Chat with the default site. Messages go straight to the API.",
                        );
                    }
                    for (role, text) in &self.messages {
                        match *role {
                            "user" => {
                                ui.horizontal_wrapped(|ui| {
                                    ui.strong("You:");
                                    ui.label(text);
                                });
                            }
                            "assistant" => {
                                ui.horizontal_wrapped(|ui| {
                                    ui.strong("AI:");
                                    ui.label(text);
                                });
                            }
                            _ => {
                                ui.colored_label(egui::Color32::RED, format!("error: {text}"));
                            }
                        }
                        ui.add_space(2.0);
                    }
                    if let Some(partial) = &self.streaming_text {
                        ui.horizontal_wrapped(|ui| {
                            ui.strong("AI:");
                            if partial.is_empty() {
                                ui.spinner();
                                ui.label("waiting for stream...");
                            } else {
                                ui.label(format!("{partial}\u{258c}"));
                            }
                        });
                    }
                    ui.add_space(8.0);
                });

            ui.separator();
            ui.horizontal(|ui| {
                let input_width = (ui.available_width() - 90.0).max(120.0);
                let response = ui.add_sized(
                    [input_width, 22.0],
                    egui::TextEdit::singleline(&mut self.input)
                        .hint_text("Type a message... (Enter to send)")
                        .id_salt("chat_input"),
                );
                let enter_pressed =
                    response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let can_send = !self.busy
                    && !self.input.trim().is_empty()
                    && self.prompt_tx.is_some();
                let send_clicked = ui
                    .add_enabled(can_send, egui::Button::new("Send"))
                    .clicked();
                if (send_clicked || enter_pressed) && can_send {
                    self.send_message();
                    response.request_focus();
                }
                if self.busy {
                    ui.spinner();
                }
            });
            if self.prompt_tx.is_none() {
                ui.colored_label(
                    egui::Color32::RED,
                    "No valid token for the default site - chat is disabled.",
                );
            }
        });
    }
}

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
