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

enum WorkerCommand {
    Prompt {
        session_id: Option<String>,
        parent_message_id: Option<i64>,
        text: String,
        options: deepseek::CompletionOptions,
    },
    LoadHistory {
        session_id: String,
    },
    RefreshSessions,
}

#[derive(Debug, Clone)]
struct ChatMessage {
    role: &'static str,
    thinking: Option<String>,
    text: String,
}

#[derive(Default, Clone)]
struct StreamingAnswer {
    thinking: Option<String>,
    text: String,
}

enum ChatEvent {
    Delta(StreamingAnswer),
    Finished {
        result: Result<(), String>,
        last_message_id: Option<i64>,
    },
    SessionCreated { id: String },
    TitleSet { session_id: String, title: String },
    HistoryLoaded {
        session_id: String,
        messages: Vec<ChatMessage>,
        last_message_id: Option<i64>,
    },
    SessionsLoaded {
        result: Result<Vec<deepseek::SessionInfo>, String>,
    },
}

fn spawn_worker(base_url: String, token: String) -> (Sender<WorkerCommand>, Receiver<ChatEvent>) {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<WorkerCommand>();
    let (event_tx, event_rx) = std::sync::mpsc::channel::<ChatEvent>();

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = event_tx.send(ChatEvent::Finished {
                    result: Err(format!("tokio runtime: {e}")),
                    last_message_id: None,
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
                        last_message_id: None,
                    });
                    return;
                }
            };

            while let Ok(cmd) = cmd_rx.recv() {
                match cmd {
                    WorkerCommand::Prompt {
                        session_id,
                        parent_message_id,
                        text,
                        options,
                    } => {
                        let sid = match session_id {
                            Some(sid) => sid,
                            None => match client.create_session().await {
                                Ok(id) => {
                                    let _ =
                                        event_tx.send(ChatEvent::SessionCreated { id: id.clone() });
                                    id
                                }
                                Err(e) => {
                                    let _ = event_tx.send(ChatEvent::Finished {
                                        result: Err(format!("create session: {e:#}")),
                                        last_message_id: None,
                                    });
                                    continue;
                                }
                            },
                        };
                        let result = try_prompt(
                            &client,
                            &sid,
                            &text,
                            parent_message_id,
                            &options,
                            &event_tx,
                        )
                        .await;
                        let _ = event_tx.send(ChatEvent::Finished {
                            result: match &result {
                                Ok(_) => Ok(()),
                                Err(e) => Err(format!("{e:#}")),
                            },
                            last_message_id: result.ok().flatten(),
                        });
                    }
                    WorkerCommand::LoadHistory { session_id } => {
                        let event = match client.history(&session_id).await {
                            Ok(messages) => {
                                let last_message_id = messages
                                    .iter()
                                    .rev()
                                    .find_map(|m| m.message_id);
                                ChatEvent::HistoryLoaded {
                                    session_id,
                                    messages: messages
                                        .into_iter()
                                        .map(|m| ChatMessage {
                                            role: if m.role == "user" { "user" } else { "assistant" },
                                            thinking: m.thinking,
                                            text: m.text,
                                        })
                                        .collect(),
                                    last_message_id,
                                }
                            }
                            Err(e) => ChatEvent::Finished {
                                result: Err(format!("history: {e:#}")),
                                last_message_id: None,
                            },
                        };
                        let _ = event_tx.send(event);
                    }
                    WorkerCommand::RefreshSessions => {
                        let event = ChatEvent::SessionsLoaded {
                            result: client
                                .list_sessions(50)
                                .await
                                .map_err(|e| format!("{e:#}")),
                        };
                        let _ = event_tx.send(event);
                    }
                }
            }
        });
    });

    (cmd_tx, event_rx)
}

#[allow(clippy::too_many_arguments)]
async fn try_prompt(
    client: &deepseek::DeepSeekClient,
    session_id: &str,
    prompt: &str,
    parent_message_id: Option<i64>,
    options: &deepseek::CompletionOptions,
    event_tx: &Sender<ChatEvent>,
) -> Result<Option<i64>, anyhow::Error> {
    let resp = client
        .completion(session_id, prompt, parent_message_id, options)
        .await?;
    let mut acc = deepseek::ResponseAccumulator::default();
    let sid = session_id.to_owned();
    let tx = event_tx.clone();
    let last = deepseek::pump_sse(resp, |event, payload| {
        if event == "title" {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) {
                if let Some(title) = v.get("content").and_then(|c| c.as_str()) {
                    let _ = tx.send(ChatEvent::TitleSet {
                        session_id: sid.clone(),
                        title: title.to_owned(),
                    });
                }
            }
            return;
        }
        if acc.feed(payload) {
            let a = acc.answer();
            let _ = tx.send(ChatEvent::Delta(StreamingAnswer {
                thinking: a.thinking,
                text: a.text,
            }));
        }
    })
    .await?;
    Ok(last)
}

struct ActiveSession {
    id: Option<String>,
    title: String,
    messages: Vec<ChatMessage>,
    last_message_id: Option<i64>,
    deepthink: bool,
    search: bool,
}

/// Remembered DeepThink/Search toggles per chat id (like the web app,
/// they are per-chat settings). New chats start from the preset default.
fn default_toggles(config: &config::Config) -> (bool, bool) {
    let thinking = config
        .preset(&config.default_preset)
        .map(|p| p.thinking_enabled)
        .unwrap_or(false);
    (thinking, false)
}

struct FireCodeApp {
    config: config::Config,
    config_path: std::path::PathBuf,
    tokens: BTreeMap<String, TokenStatus>,
    default_token: Option<String>,
    sessions: Vec<deepseek::SessionInfo>,
    active: ActiveSession,
    toggles: BTreeMap<String, (bool, bool)>,
    busy: bool,
    input: String,
    streaming: Option<StreamingAnswer>,
    preset_ids: Vec<String>,
    preset_selected: usize,
    event_rx: Receiver<ChatEvent>,
    cmd_tx: Option<Sender<WorkerCommand>>,
}

impl FireCodeApp {
    fn new(config: config::Config, config_path: std::path::PathBuf) -> Self {
        let preset_ids: Vec<String> = config.presets.keys().cloned().collect();
        let preset_selected = preset_ids
            .iter()
            .position(|id| *id == config.default_preset)
            .unwrap_or(0);
        let default_deepthink = config
            .preset(&config.default_preset)
            .map(|p| p.thinking_enabled)
            .unwrap_or(false);
        let mut app = Self {
            config,
            config_path,
            tokens: BTreeMap::new(),
            default_token: None,
            sessions: Vec::new(),
            active: ActiveSession {
                id: None,
                title: "New chat".to_owned(),
                messages: Vec::new(),
                last_message_id: None,
                deepthink: default_deepthink,
                search: false,
            },
            toggles: BTreeMap::new(),
            busy: false,
            input: String::new(),
            streaming: None,
            preset_ids,
            preset_selected,
            event_rx: std::sync::mpsc::channel().1,
            cmd_tx: None,
        };
        app.reread_tokens();
        if app.cmd_tx.is_some() {
            app.refresh_sessions();
        }
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
            match auth::read_token(std::path::Path::new(&self.config.profile.path), &site.token) {
                Ok(token) => {
                    if id == self.config.default_site {
                        self.default_token = Some(token.value.clone());
                    }
                    let masked = token.masked();
                    self.tokens
                        .insert(id, TokenStatus::Ok { key: token.key, masked });
                }
                Err(e) => {
                    self.tokens
                        .insert(id, TokenStatus::Failed { error: format!("{e:#}") });
                }
            }
        }

        match (&self.default_token, self.config.site(&self.config.default_site)) {
            (Some(token), Ok(site)) => {
                let (cmd_tx, event_rx) = spawn_worker(site.api_base.clone(), token.clone());
                self.cmd_tx = Some(cmd_tx);
                self.event_rx = event_rx;
            }
            _ => self.cmd_tx = None,
        }
    }

    fn refresh_sessions(&mut self) {
        if let Some(cmd_tx) = &self.cmd_tx {
            let _ = cmd_tx.send(WorkerCommand::RefreshSessions);
        }
    }

    fn open_session(&mut self, id: String, title: String) {
        if self.busy {
            return;
        }
        let (deepthink, search) = self
            .toggles
            .get(&id)
            .copied()
            .unwrap_or_else(|| default_toggles(&self.config));
        self.active = ActiveSession {
            id: Some(id),
            title,
            messages: Vec::new(),
            last_message_id: None,
            deepthink,
            search,
        };
        if let Some(cmd_tx) = self.cmd_tx.clone() {
            self.busy = true;
            let _ = cmd_tx.send(WorkerCommand::LoadHistory {
                session_id: self.active.id.clone().expect("id set above"),
            });
        }
    }

    fn new_chat(&mut self) {
        if self.busy {
            return;
        }
        let (deepthink, search) = default_toggles(&self.config);
        self.active = ActiveSession {
            id: None,
            title: "New chat".to_owned(),
            messages: Vec::new(),
            last_message_id: None,
            deepthink,
            search,
        };
        self.streaming = None;
    }

    fn toggle_active(&mut self, which: u8) {
        let (mut deepthink, mut search) = (self.active.deepthink, self.active.search);
        match which {
            0 => deepthink = !deepthink,
            _ => search = !search,
        }
        self.active.deepthink = deepthink;
        self.active.search = search;
        if let Some(id) = self.active.id.clone() {
            self.toggles.insert(id, (deepthink, search));
        }
    }

    fn send_message(&mut self) {
        let text = self.input.trim().to_owned();
        if text.is_empty() || self.busy {
            return;
        }
        let Some(cmd_tx) = self.cmd_tx.clone() else {
            return;
        };
        let model_class = self
            .config
            .preset(self.selected_preset_id())
            .map(|p| p.model_class.clone())
            .unwrap_or_else(|_| "deepseek_chat".to_owned());
        self.messages_push(ChatMessage {
            role: "user",
            thinking: None,
            text: text.clone(),
        });
        self.input.clear();
        self.streaming = Some(StreamingAnswer::default());
        self.busy = true;
        let _ = cmd_tx.send(WorkerCommand::Prompt {
            session_id: self.active.id.clone(),
            parent_message_id: self.active.last_message_id,
            text,
            options: deepseek::CompletionOptions {
                model_class,
                thinking_enabled: self.active.deepthink,
                search_enabled: self.active.search,
            },
        });
    }

    fn messages_push(&mut self, message: ChatMessage) {
        self.active.messages.push(message);
    }

    fn selected_preset_id(&self) -> &str {
        self.preset_ids
            .get(self.preset_selected)
            .map(String::as_str)
            .unwrap_or(self.config.default_preset.as_str())
    }

    fn poll_events(&mut self, ctx: &egui::Context) {
        let mut repaint = false;
        while let Ok(event) = self.event_rx.try_recv() {
            repaint = true;
            match event {
                ChatEvent::Delta(streaming) => {
                    self.streaming = Some(streaming);
                }
                ChatEvent::Finished {
                    result,
                    last_message_id,
                } => {
                    self.busy = false;
                    let streaming = self.streaming.take().unwrap_or_default();
                    match result {
                        Ok(()) => {
                            if let Some(id) = last_message_id {
                                self.active.last_message_id = Some(id);
                            }
                            self.messages_push(ChatMessage {
                                role: "assistant",
                                thinking: streaming.thinking,
                                text: streaming.text,
                            });
                        }
                        Err(e) => {
                            if !streaming.text.is_empty() {
                                self.messages_push(ChatMessage {
                                    role: "assistant",
                                    thinking: streaming.thinking,
                                    text: streaming.text,
                                });
                            }
                            self.messages_push(ChatMessage {
                                role: "error",
                                thinking: None,
                                text: e,
                            });
                        }
                    }
                }
                ChatEvent::SessionCreated { id } => {
                    self.active.id = Some(id.clone());
                    let title = self
                        .active
                        .messages
                        .iter()
                        .rev()
                        .find(|m| m.role == "user")
                        .map(|m| m.text.chars().take(40).collect::<String>())
                        .unwrap_or_else(|| "New chat".to_owned());
                    self.active.title = title.clone();
                    self.sessions
                        .insert(0, deepseek::SessionInfo { id, title });
                }
                ChatEvent::TitleSet { session_id, title } => {
                    if self.active.id.as_deref() == Some(session_id.as_str()) {
                        self.active.title = title.clone();
                    }
                    if let Some(entry) = self.sessions.iter_mut().find(|s| s.id == session_id) {
                        entry.title = title;
                    }
                }
                ChatEvent::HistoryLoaded {
                    session_id,
                    messages,
                    last_message_id,
                } => {
                    self.busy = false;
                    if self.active.id.as_deref() == Some(session_id.as_str()) {
                        self.active.messages = messages;
                        self.active.last_message_id = last_message_id;
                    }
                }
                ChatEvent::SessionsLoaded { result } => match result {
                    Ok(sessions) => self.sessions = sessions,
                    Err(e) => self.messages_push(ChatMessage {
                        role: "error",
                        thinking: None,
                        text: format!("session list: {e}"),
                    }),
                },
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
                    let preset_names: Vec<String> = self
                        .preset_ids
                        .iter()
                        .map(|id| {
                            self.config
                                .preset(id)
                                .map(|p| p.display_name.clone())
                                .unwrap_or_else(|_| id.clone())
                        })
                        .collect();
                    egui::ComboBox::from_id_salt("preset_combo")
                        .selected_text(
                            preset_names
                                .get(self.preset_selected)
                                .map(String::as_str)
                                .unwrap_or("preset"),
                        )
                        .show_ui(ui, |ui| {
                            for (i, name) in preset_names.iter().enumerate() {
                                ui.selectable_value(
                                    &mut self.preset_selected,
                                    i,
                                    name.clone(),
                                );
                            }
                        });
                    egui::CollapsingHeader::new("вљ™ auth")
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

            egui::Panel::left("sessions_panel")
                .resizable(true)
                .default_size(230.0)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if ui.add_enabled(!self.busy, egui::Button::new("+ New chat")).clicked() {
                            self.new_chat();
                        }
                        if ui.add_enabled(!self.busy, egui::Button::new("вџі")).clicked() {
                            self.refresh_sessions();
                        }
                    });
                    ui.separator();
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        let active_id = self.active.id.clone();
                        let mut open_request: Option<(String, String)> = None;
                        for session in &self.sessions {
                            let is_active = active_id.as_deref() == Some(session.id.as_str());
                            let label = if session.title.is_empty() {
                                session.id.chars().take(12).collect::<String>()
                            } else {
                                session.title.clone()
                            };
                            let style = if is_active {
                                egui::TextStyle::Body
                            } else {
                                egui::TextStyle::Small
                            };
                            let response = ui.add(
                                egui::Label::new(
                                    egui::RichText::new(label)
                                        .text_style(style)
                                        .color(if is_active {
                                            ui.visuals().text_color()
                                        } else {
                                            ui.visuals().weak_text_color()
                                        }),
                                )
                                .truncate(),
                            );
                            if response.clicked() && !self.busy && open_request.is_none() {
                                open_request = Some((session.id.clone(), session.title.clone()));
                            }
                        }
                        if let Some((id, title)) = open_request {
                            self.open_session(id, title);
                        }
                    });
                });

            egui::Panel::bottom("input_panel").show(ui, |ui| {
                ui.add_space(2.0);
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
                        && self.cmd_tx.is_some();
                    let send_clicked = ui
                        .add_enabled(can_send, egui::Button::new("Send"))
                        .clicked();
                    if (send_clicked || enter_pressed) && can_send {
                        self.send_message();
                        response.request_focus();
                    }
                });
                if self.cmd_tx.is_none() {
                    ui.colored_label(
                        egui::Color32::RED,
                        "No valid token for the default site - chat is disabled.",
                    );
                }
            });

            egui::CentralPanel::default().show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.strong(&self.active.title);
                    ui.separator();
                    let think_btn = egui::Button::selectable(
                        self.active.deepthink,
                        egui::RichText::new("🧠 DeepThink"),
                    );
                    if ui.add_enabled(!self.busy, think_btn).clicked() {
                        self.toggle_active(0);
                    }
                    let search_btn = egui::Button::selectable(
                        self.active.search,
                        egui::RichText::new("🔎 Search"),
                    );
                    if ui.add_enabled(!self.busy, search_btn).clicked() {
                        self.toggle_active(1);
                    }
                    if self.busy {
                        ui.spinner();
                    }
                });
                ui.separator();

                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        ui.add_space(4.0);
                        if self.active.messages.is_empty() && !self.busy {
                            ui.colored_label(
                                egui::Color32::GRAY,
                                "Select a chat on the left or start a new one.",
                            );
                        }
                        for (mi, message) in self.active.messages.iter().enumerate() {
                            match message.role {
                                "user" => {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.strong("You:");
                                        ui.label(&message.text);
                                    });
                                }
                                "assistant" => {
                                    ui.strong("AI:");
                                    if let Some(thinking) = &message.thinking {
                                        egui::CollapsingHeader::new("🧠 Thinking")
                                            .id_salt(("thinking_header", mi))
                                            .default_open(false)
                                            .show(ui, |ui| {
                                                ui.horizontal_wrapped(|ui| {
                                                    ui.weak(thinking.clone());
                                                });
                                            });
                                    }
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label(&message.text);
                                    });
                                }
                                _ => {
                                    ui.colored_label(
                                        egui::Color32::RED,
                                        format!("error: {}", message.text),
                                    );
                                }
                            }
                            ui.add_space(2.0);
                        }
                        if let Some(streaming) = &self.streaming {
                            ui.strong("AI:");
                            if let Some(thinking) = &streaming.thinking {
                                ui.horizontal_wrapped(|ui| {
                                    ui.weak(format!("🧠 {thinking}\u{258c}"));
                                });
                            }
                            ui.horizontal_wrapped(|ui| {
                                if streaming.text.is_empty() && streaming.thinking.is_none() {
                                    ui.spinner();
                                    ui.label("waiting for stream...");
                                } else {
                                    ui.label(format!("{}\u{258c}", streaming.text));
                                }
                            });
                        }
                        ui.add_space(8.0);
                    });
            });
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
