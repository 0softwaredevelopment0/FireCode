#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agent;
mod auth;
mod chatcache;
mod chromium;
mod config;
mod deepseek;
mod fonts;
mod markdown;
mod pow;
mod settings;

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, Sender};

use egui::FontFamily;
use markdown::{Block, InlineSpan};

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
        agent: bool,
        allow_sudo: bool,
        response_delay_enabled: bool,
        response_delay_secs: u64,
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

/// Turns an owned String into a `&'static str` for [`ChatMessage::role`]:
/// the leak is a few bytes per restored message and keeps every
/// construction site literal ("user", "assistant", ...). Deliberate.
fn leak_static(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

#[derive(Default, Clone)]
struct StreamingAnswer {
    thinking: Option<String>,
    text: String,
}

enum ChatEvent {
    StreamStart,
    Delta(StreamingAnswer),
    ResponseDone {
        result: Result<(), String>,
        last_message_id: Option<i64>,
        /// In agent mode: message text with command/continue lines removed.
        visible_text: Option<String>,
    },
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
    CommandStarted { cmd: String },
    CommandFinished { cmd: String },
    AgentMessage { role: &'static str, text: String },
}

fn spawn_worker(
    base_url: String,
    token: String,
    agent_config: config::AgentConfig,
    workdir: std::path::PathBuf,
) -> (
    Sender<WorkerCommand>,
    Receiver<ChatEvent>,
    Sender<()>,
) {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<WorkerCommand>();
    let (event_tx, event_rx) = std::sync::mpsc::channel::<ChatEvent>();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();

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
            let shell = agent::Shell::parse(&agent_config.shell);
            let timeout = std::time::Duration::from_secs(agent_config.timeout_secs);
            let max_steps = agent_config.max_steps;

            while let Ok(cmd) = cmd_rx.recv() {
                match cmd {
                    WorkerCommand::Prompt {
                        session_id,
                        parent_message_id,
                        text,
                        options,
                        agent,
                        allow_sudo,
                        response_delay_enabled,
                        response_delay_secs,
                    } => {
                        let mut session_id = session_id;
                        let mut parent_message_id = parent_message_id;
                        let mut next_text = text;
                        // armed before the loop's automatic sends: the next
                        // request waits out the rate-limit guard delay
                        let mut delay_next = false;

                        // the agent loop: response -> commands -> feedback -> response
                        let mut step = 0u32;
                        let send = |event_tx: &Sender<ChatEvent>, event: ChatEvent| {
                            let _ = event_tx.send(event);
                        };
                        loop {
                            step += 1;
                            if agent && step > max_steps {
                                send(
                                    &event_tx,
                                    ChatEvent::AgentMessage {
                                        role: "error",
                                        text: format!(
                                            "agent step limit reached ({max_steps}), stopping"
                                        ),
                                    },
                                );
                                break;
                            }

                            // rate-limit guard: pause before each automatic
                            // request (feedback / Continue), never before the
                            // user's own first prompt; the flag stays armed
                            // so every subsequent automatic request waits too
                            if delay_next && response_delay_enabled && response_delay_secs > 0 {
                                tokio::time::sleep(std::time::Duration::from_secs(
                                    response_delay_secs,
                                ))
                                .await;
                                // a Stop pressed during the delay ends the turn
                                if stop_rx.try_recv().is_ok() {
                                    send(
                                        &event_tx,
                                        ChatEvent::Finished {
                                            result: Ok(()),
                                            last_message_id: parent_message_id,
                                        },
                                    );
                                    break;
                                }
                            }

                            if session_id.is_none() {
                                match client.create_session().await {
                                    Ok(id) => {
                                        send(&event_tx, ChatEvent::SessionCreated { id: id.clone() });
                                        session_id = Some(id);
                                    }
                                    Err(e) => {
                                        send(
                                            &event_tx,
                                            ChatEvent::Finished {
                                                result: Err(format!("create session: {e:#}")),
                                                last_message_id: None,
                                            },
                                        );
                                        return;
                                    }
                                }
                            }
                            let sid = session_id.clone().expect("session checked above");

                            send(&event_tx, ChatEvent::StreamStart);
                            let result = try_prompt(
                                &client,
                                &sid,
                                &next_text,
                                parent_message_id,
                                &options,
                                &event_tx,
                                &stop_rx,
                            )
                            .await;
                            let (full_text, last_id, manually_stopped) = match result {
                                Ok((text, id, stopped)) => (text, id, stopped),
                                Err(e) => {
                                    send(
                                        &event_tx,
                                        ChatEvent::ResponseDone {
                                            result: Err(format!("{e:#}")),
                                            last_message_id: None,
                                            visible_text: None,
                                        },
                                    );
                                    break;
                                }
                            };
                            let parsed = if agent {
                                Some(agent::parse_response(&full_text))
                            } else {
                                None
                            };
                            if let Some(id) = last_id {
                                parent_message_id = Some(id);
                            }
                            send(
                                &event_tx,
                                ChatEvent::ResponseDone {
                                    result: Ok(()),
                                    last_message_id: last_id,
                                    visible_text: parsed
                                        .as_ref()
                                        .map(|parsed| parsed.text.clone()),
                                },
                            );

                            if !agent {
                                break;
                            }

                            // a manual Stop must end the whole turn: the
                            // partial reply is kept as-is, no commands run
                            // and no auto-continue prompt goes out
                            if manually_stopped {
                                break;
                            }

                            let parsed = parsed.expect("parsed in agent mode");
                            // Commands share the message with the loop
                            // markers: they run first and their feedback
                            // output re-triggers the agent on its own.
                            if !parsed.commands.is_empty() {
                                let mut results = Vec::new();
                                for cmd in &parsed.commands {
                                    send(&event_tx, ChatEvent::CommandStarted { cmd: cmd.clone() });
                                    let output = if !allow_sudo && agent::looks_like_sudo(cmd) {
                                        agent::CommandOutput {
                                            output: "(blocked: sudo commands are disabled in FireCode settings)".to_owned(),
                                            exit_code: None,
                                            timed_out: false,
                                        }
                                    } else {
                                        agent::execute_command(cmd, &workdir, shell, timeout)
                                            .await
                                            .unwrap_or_else(|e| agent::CommandOutput {
                                                output: format!("(execution error: {e:#})"),
                                                exit_code: None,
                                                timed_out: false,
                                            })
                                    };
                                    send(
                                        &event_tx,
                                        ChatEvent::CommandFinished { cmd: cmd.clone() },
                                    );
                                    results.push((cmd.clone(), output));
                                }
                                next_text = agent::build_feedback(&results);
                                send(
                                    &event_tx,
                                    ChatEvent::AgentMessage {
                                        role: "feedback",
                                        text: next_text.clone(),
                                    },
                                );
                                delay_next = true;
                                continue;
                            }
                            // the auto-continue loop: until the model writes
                            // <stop> on its own line, nudge it with the
                            // standard Continue prompt after every reply
                            if parsed.stop {
                                break;
                            }
                            next_text =
                                "Continue, to stop this response, use <stop> in your message on new line."
                                    .to_owned();
                            send(
                                &event_tx,
                                ChatEvent::AgentMessage {
                                    role: "user",
                                    text: next_text.clone(),
                                },
                            );
                            delay_next = true;
                            continue;
                        }
                        send(
                            &event_tx,
                            ChatEvent::Finished {
                                result: Ok(()),
                                last_message_id: parent_message_id,
                            },
                        );
                    }
                    WorkerCommand::LoadHistory { session_id } => {
                        let event = match client.history(&session_id).await {
                            Ok(messages) => {
                                let last_message_id =
                                    messages.iter().rev().find_map(|m| m.message_id);
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

    (cmd_tx, event_rx, stop_tx)
}
#[allow(clippy::too_many_arguments)]
async fn try_prompt(
    client: &deepseek::DeepSeekClient,
    session_id: &str,
    prompt: &str,
    parent_message_id: Option<i64>,
    options: &deepseek::CompletionOptions,
    event_tx: &Sender<ChatEvent>,
    stop_rx: &std::sync::mpsc::Receiver<()>,
) -> Result<(String, Option<i64>, bool), anyhow::Error> {
    // drop stop signals from previous phases - only a stop pressed during
    // this stream cancels it
    while stop_rx.try_recv().is_ok() {}
    let resp = client
        .completion(session_id, prompt, parent_message_id, options)
        .await?;
    let mut acc = deepseek::ResponseAccumulator::default();
    let mut response_id: Option<i64> = None;
    let mut stop_requested = false;
    let sid = session_id.to_owned();
    let tx = event_tx.clone();
    let last = deepseek::pump_sse(
        resp,
        |event, payload| {
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
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) {
                if let Some(id) = v.get("response_message_id").and_then(|i| i.as_i64()) {
                    response_id = Some(id);
                }
            }
            if acc.feed(payload) {
                let a = acc.answer();
                let _ = tx.send(ChatEvent::Delta(StreamingAnswer {
                    thinking: a.thinking,
                    text: a.text,
                }));
            }
        },
        || {
            if stop_rx.try_recv().is_ok() {
                stop_requested = true;
                true
            } else {
                false
            }
        },
    )
    .await?;
    if stop_requested {
        if let Some(id) = response_id.or(last) {
            let _ = client.stop_stream(session_id, id).await;
        }
    }
    // the third value tells the agent loop that the user stopped the stream
    // manually - the partial text must not trigger the auto-continue loop
    Ok((acc.answer().text, last, stop_requested))
}
struct ActiveSession {
    id: Option<String>,
    title: String,
    messages: Vec<ChatMessage>,
    last_message_id: Option<i64>,
    deepthink: bool,
    search: bool,
    code: bool,
}

/// Remembered DeepThink/Search/Code toggles per chat id (like the web app,
/// they are per-chat settings). New chats start from the preset default.
fn default_toggles(config: &config::Config) -> (bool, bool, bool) {
    let thinking = config
        .preset(&config.default_preset)
        .map(|p| p.thinking_enabled)
        .unwrap_or(false);
    (thinking, false, false)
}

struct FireCodeApp {
    config: config::Config,
    config_path: std::path::PathBuf,
    settings: settings::Settings,
    settings_draft: settings::Settings,
    settings_open: bool,
    settings_error: Option<String>,
    resolved_profile: String,
    tokens: BTreeMap<String, TokenStatus>,
    default_token: Option<String>,
    sessions: Vec<deepseek::SessionInfo>,
    active: ActiveSession,
    toggles: BTreeMap<String, (bool, bool, bool)>,
    busy: bool,
    input: String,
    streaming: Option<StreamingAnswer>,
    copied_code: Option<(egui::Id, std::time::Instant)>,
    running_command_index: Option<usize>,
    preset_ids: Vec<String>,
    preset_selected: usize,
    event_rx: Receiver<ChatEvent>,
    cmd_tx: Option<Sender<WorkerCommand>>,
    stop_tx: Option<Sender<()>>,
}
fn spans_to_job(
    ui: &mut egui::Ui,
    spans: &[InlineSpan],
    size: f32,
    bright: bool,
    force_bold_family: bool,
) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let text_color = ui.visuals().text_color();
    let strong = ui.visuals().strong_text_color();
    let weak = ui.visuals().weak_text_color();
    let plain_family = FontFamily::Proportional;
    let bold_family = FontFamily::Name("bold".into());
    let italic_family = FontFamily::Name("italic".into());
    let plain_font = if force_bold_family {
        bold_family.clone()
    } else {
        plain_family.clone()
    };
    for span in spans {
        let (text, font_id, color, bg) = match span {
            InlineSpan::Plain(t) => (
                t,
                egui::FontId::new(size, plain_font.clone()),
                if bright {
                    text_color
                } else {
                    text_color.gamma_multiply(0.92)
                },
                egui::Color32::TRANSPARENT,
            ),
            InlineSpan::Bold(t) => (
                t,
                egui::FontId::new(size, bold_family.clone()),
                strong,
                egui::Color32::TRANSPARENT,
            ),
            InlineSpan::Italic(t) => (
                t,
                egui::FontId::new(size, italic_family.clone()),
                weak,
                egui::Color32::TRANSPARENT,
            ),
            InlineSpan::Code(t) => (
                t,
                egui::FontId::monospace(size * 0.92),
                egui::Color32::from_rgb(225, 225, 230),
                egui::Color32::from_rgb(45, 45, 50),
            ),
        };
        let mut format = egui::TextFormat::simple(font_id, color);
        format.background = bg;
        job.append(text, 0.0, format);
    }
    job
}
fn markdown_blocks_ui(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    text: &str,
    id_salt: &str,
    copied: &mut Option<(egui::Id, std::time::Instant)>,
) {
    let body = egui::TextStyle::Body.resolve(ui.style()).size;
    for (bi, block) in markdown::parse_blocks(text).iter().enumerate() {
        match block {
            Block::Paragraph(p) => {
                let job = spans_to_job(ui, &markdown::parse_inline(p), body, false, false);
                ui.horizontal_wrapped(|ui| {
                    ui.label(job);
                });
            }
            Block::Heading { level, text: h } => {
                let size = body * match level {
                    1 => 1.5,
                    2 => 1.3,
                    3 => 1.15,
                    _ => 1.05,
                };
                let job = spans_to_job(ui, &markdown::parse_inline(h), size, true, true);
                ui.horizontal_wrapped(|ui| {
                    ui.label(job);
                });
                ui.add_space(2.0);
            }
            Block::BulletItem(t) => {
                let job = spans_to_job(ui, &markdown::parse_inline(t), body, false, false);
                ui.horizontal_wrapped(|ui| {
                    ui.weak("  •  ");
                    ui.label(job);
                });
            }
            Block::OrderedItem { num, text: t } => {
                let job = spans_to_job(ui, &markdown::parse_inline(t), body, false, false);
                ui.horizontal_wrapped(|ui| {
                    ui.weak(format!("  {num}. "));
                    ui.label(job);
                });
            }
            Block::Table { header, rows } => {
                let cols = header
                    .len()
                    .max(rows.iter().map(|r| r.len()).max().unwrap_or(0))
                    .max(1);
                egui::Grid::new(egui::Id::new((id_salt, bi)))
                    .striped(true)
                    .num_columns(cols)
                    .spacing([16.0, 4.0])
                    .show(ui, |ui| {
                        for cell in header {
                            let job = spans_to_job(
                                ui,
                                &markdown::parse_inline(cell),
                                body,
                                true,
                                true,
                            );
                            ui.label(job);
                        }
                        ui.end_row();
                        for row in rows {
                            for ci in 0..cols {
                                let cell = row.get(ci).map(String::as_str).unwrap_or("");
                                let job = spans_to_job(
                                    ui,
                                    &markdown::parse_inline(cell),
                                    body,
                                    false,
                                    false,
                                );
                                ui.label(job);
                            }
                            ui.end_row();
                        }
                    });
                ui.add_space(4.0);
            }
            Block::Code { lang, code, closed } => code_block_ui(
                ui,
                ctx,
                lang,
                code,
                *closed,
                egui::Id::new((id_salt, bi)),
                copied,
            ),
        }
    }
}

/// Command execution output: ONE window sized to its content - it
/// stretches right only as far as the widest text line (capped by the
/// chat width, where long lines wrap) instead of the full chat width.
/// A single dark frame containing the clickable header row (triangle +
/// command, lighter on hover) and the body below. Collapsed by default
/// to a fixed ~10-real-line scrollable preview (mouse wheel or
/// scrollbar); clicking the header row toggles the full output. An empty
/// body renders the header row only.
fn command_output_ui(ui: &mut egui::Ui, id: egui::Id, header: &str, body: &str) {
    let expanded = ui.memory_mut(|mem| *mem.data.get_temp_mut_or(id, false));

    let mono_size = egui::TextStyle::Monospace.resolve(ui.style()).size;
    let measure = |text: String| -> egui::Vec2 {
        ui.fonts_mut(|fonts| {
            fonts
                .layout_job(egui::text::LayoutJob::simple(
                    text,
                    egui::FontId::monospace(mono_size),
                    egui::Color32::WHITE,
                    f32::INFINITY,
                ))
                .size()
        })
    };
    let row_h = measure("Ag".to_owned()).y;
    let char_w = measure("0".to_owned()).x;

    // multiline feedback headers (multiline commands span lines inside
    // `Command "...":`) must not leak into the single-line header row -
    // show the first line with an ellipsis
    let header_line: String = {
        let mut lines = header.lines();
        let first = lines.next().unwrap_or("").to_owned();
        if lines.next().is_some() {
            format!("{first} …")
        } else {
            first
        }
    };

    // window width = widest of the header and body text (monospace
    // char-count estimate) plus frame paddings, capped by the chat width
    let body_w = body
        .lines()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(0) as f32
        * char_w;
    let header_w = measure(format!("▸ {header_line}")).x;
    let content_w = (body_w.max(header_w) + 18.0).min(ui.available_width());

    egui::Frame::default()
        .fill(egui::Color32::from_rgb(30, 30, 34))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(70, 70, 76)))
        .corner_radius(6)
        .inner_margin(egui::Margin::same(0))
        .show(ui, |ui| {
            ui.set_max_width(content_w);

            // header row: click target + background fill, text as a real
            // widget (painter text is what caused the frozen-copy artifact)
            let header_h = ui
                .style()
                .spacing
                .interact_size
                .y
                .max(row_h * 1.4);
            let (rect, response) = ui.allocate_exact_size(
                egui::vec2(content_w, header_h),
                egui::Sense::click(),
            );
            if response.clicked() {
                ui.memory_mut(|mem| {
                    let flag = mem.data.get_temp_mut_or(id, false);
                    *flag = !*flag;
                });
            }
            if response.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            ui.painter().rect_filled(
                rect,
                0,
                if response.hovered() {
                    egui::Color32::from_rgb(48, 48, 55)
                } else {
                    egui::Color32::from_rgb(38, 38, 44)
                },
            );
            let tri = if expanded { "▾" } else { "▸" };
            ui.put(
                rect.shrink2(egui::vec2(8.0, 0.0)),
                egui::Label::new(
                    egui::RichText::new(format!("{tri} {header_line}"))
                        .monospace()
                        .color(egui::Color32::from_rgb(225, 225, 230)),
                )
                // truncate (not extend): a longer-than-window header must
                // not widen the chat content - that was shifting every
                // wrapped line left and making it unreadable
                .wrap_mode(egui::TextWrapMode::Truncate),
            );

            if !body.is_empty() {
                // thin divider under the header, content width only
                let (sep_rect, _) =
                    ui.allocate_exact_size(egui::vec2(content_w, 1.0), egui::Sense::hover());
                ui.painter().line_segment(
                    [sep_rect.left_top(), sep_rect.right_top()],
                    egui::Stroke::new(1.0, egui::Color32::from_rgb(70, 70, 76)),
                );
                egui::Frame::default()
                    .inner_margin(egui::Margin::same(8))
                    .show(ui, |ui| {
                        if expanded {
                            ui.monospace(body);
                        } else {
                            // fixed ~10-real-line preview; the scrollbar
                            // hugs this window's right edge
                            let preview_h = row_h * 20.0;
                            ui.allocate_ui(
                                egui::vec2((content_w - 16.0).max(0.0), preview_h),
                                |ui| {
                                    egui::ScrollArea::vertical()
                                        .id_salt(id.with("scroll"))
                                        .max_height(preview_h)
                                        .show(ui, |ui| {
                                            ui.monospace(body);
                                        });
                                },
                            );
                        }
                    });
            }
        });
    ui.add_space(2.0);
}

fn code_block_ui(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    lang: &str,
    code: &str,
    closed: bool,
    id: egui::Id,
    copied: &mut Option<(egui::Id, std::time::Instant)>,
) {
    let is_copied = copied
        .as_ref()
        .is_some_and(|(cid, at)| *cid == id && at.elapsed().as_secs_f32() < 1.5);
    let mut header = lang.to_owned();
    if !closed {
        if !header.is_empty() {
            header.push(' ');
        }
        header.push('\u{258c}');
    }
    egui::Frame::default()
        .fill(egui::Color32::from_rgb(30, 30, 34))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(70, 70, 76)))
        .corner_radius(6)
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let copy_label = if is_copied { "✓ Copied" } else { "📋 Copy" };
                if ui.small_button(copy_label).clicked() {
                    ctx.copy_text(code.to_owned());
                    *copied = Some((id, std::time::Instant::now()));
                }
                if !header.is_empty() {
                    ui.weak(header);
                }
            });
            ui.add_space(2.0);
            egui::ScrollArea::horizontal().id_salt(id).show(ui, |ui| {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(code.trim_end_matches('\n'))
                            .monospace()
                            .color(egui::Color32::from_rgb(220, 220, 225)),
                    )
                    .wrap_mode(egui::TextWrapMode::Extend),
                );
            });
        });
    ui.add_space(2.0);
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
        if let Err(e) = agent::ensure_prompt_md() {
            eprintln!("PROMPT.md: {e:#}");
        }
        let settings = settings::load_or_create().unwrap_or_else(|e| {
            eprintln!("settings: {e:#}");
            settings::Settings::default()
        });
        let mut app = Self {
            config,
            config_path,
            settings_draft: settings.clone(),
            settings,
            settings_open: false,
            settings_error: None,
            resolved_profile: String::new(),
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
                code: false,
            },
            toggles: BTreeMap::new(),
            busy: false,
            input: String::new(),
            streaming: None,
            copied_code: None,
            running_command_index: None,
            preset_ids,
            preset_selected,
            event_rx: std::sync::mpsc::channel().1,
            cmd_tx: None,
            stop_tx: None,
        };
        app.reread_tokens();
        if app.cmd_tx.is_some() {
            app.refresh_sessions();
        }
        app
    }

    fn reread_tokens(&mut self) {
        self.default_token = None;
        let preset = self.settings.browser.preset;
        let profile_dir = config::resolve_profile_dir(&self.config, preset);
        self.resolved_profile = profile_dir.display().to_string();
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
            match auth::read_token(&profile_dir, preset, &site.token) {
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
                let workdir =
                    agent::resolve_working_dir(&self.settings.agent.working_dir);
                let (cmd_tx, event_rx, stop_tx) = spawn_worker(
                    site.api_base.clone(),
                    token.clone(),
                    self.config.agent.clone(),
                    workdir,
                );
                self.cmd_tx = Some(cmd_tx);
                self.event_rx = event_rx;
                self.stop_tx = Some(stop_tx);
            }
            _ => {
                self.cmd_tx = None;
                self.stop_tx = None;
            }
        }
    }

    fn refresh_sessions(&mut self) {
        if let Some(cmd_tx) = self.cmd_tx.clone() {
            let _ = cmd_tx.send(WorkerCommand::RefreshSessions);
        }
    }

    fn save_settings(&mut self) {
        match settings::save(&self.settings_draft) {
            Ok(()) => {
                self.settings = self.settings_draft.clone();
                self.settings_error = None;
                self.reread_tokens();
            }
            Err(e) => {
                self.settings_error = Some(format!("{e:#}"));
            }
        }
    }

    fn reset_settings(&mut self) {
        match settings::load_or_create() {
            Ok(saved) => {
                self.settings = saved.clone();
                self.settings_draft = saved;
                self.settings_error = None;
                self.reread_tokens();
            }
            Err(e) => {
                self.settings_error = Some(format!("{e:#}"));
            }
        }
    }

    fn open_session(&mut self, id: String, title: String) {
        if self.busy {
            return;
        }
        let (deepthink, search, code) = self
            .toggles
            .get(&id)
            .copied()
            .unwrap_or_else(|| default_toggles(&self.config));
        self.active = ActiveSession {
            id: Some(id.clone()),
            title,
            messages: Vec::new(),
            last_message_id: None,
            deepthink,
            search,
            code,
        };
        // local cache first: it holds the local-only command/feedback
        // windows the server history does not know about; a stuck
        // "...running" command line (app closed mid-run) is dropped
        if let Some(cached) = chatcache::load(&id) {
            self.active.messages = cached
                .messages
                .into_iter()
                .filter(|m| !(m.role == "command" && m.text.contains("...running")))
                .map(|m| ChatMessage {
                    role: leak_static(m.role),
                    thinking: m.thinking,
                    text: m.text,
                })
                .collect();
            self.active.last_message_id = cached.last_message_id;
            return;
        }
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
        let (deepthink, search, code) = default_toggles(&self.config);
        self.active = ActiveSession {
            id: None,
            title: "New chat".to_owned(),
            messages: Vec::new(),
            last_message_id: None,
            deepthink,
            search,
            code,
        };
        self.streaming = None;
    }

    fn toggle_active(&mut self, which: u8) {
        let (mut deepthink, mut search, mut code) =
            (self.active.deepthink, self.active.search, self.active.code);
        match which {
            0 => deepthink = !deepthink,
            1 => search = !search,
            _ => code = !code,
        }
        self.active.deepthink = deepthink;
        self.active.search = search;
        self.active.code = code;
        if let Some(id) = self.active.id.clone() {
            self.toggles.insert(id, (deepthink, search, code));
        }
    }

    fn send_message(&mut self) {
        let text = self.input.trim().to_owned();
        if text.is_empty() || self.busy {
            return;
        }
        self.input.clear();
        self.dispatch_prompt(text, None);
    }

    fn send_prompt_from_agents_md(&mut self) {
        if self.busy {
            return;
        }
        let content = match agent::load_prompt_md() {
            Ok(c) => c,
            Err(e) => {
                self.messages_push(ChatMessage {
                    role: "error",
                    thinking: None,
                    text: format!("PROMPT.md: {e:#}"),
                });
                return;
            }
        };
        let chars = content.chars().count();
        self.dispatch_prompt(content, Some(format!("(PROMPT.md, {chars} chars)")));
    }

    fn dispatch_prompt(&mut self, text: String, display_text: Option<String>) {
        let Some(cmd_tx) = self.cmd_tx.clone() else {
            return;
        };
        let model_class = self
            .config
            .preset(self.selected_preset_id())
            .map(|p| p.model_class.clone())
            .unwrap_or_else(|_| "deepseek_chat".to_owned());
        let display = display_text.clone().unwrap_or_else(|| text.clone());
        self.messages_push(ChatMessage {
            role: if display_text.is_some() { "prompt" } else { "user" },
            thinking: None,
            text: display,
        });
        // Include AGENTS.md is re-checked on every send.
        let text = match (
            self.settings.agent.include_agents_md,
            agent::load_agents_md_opt(),
        ) {
            (true, Some(instructions)) => {
                format!("Project instructions (AGENTS.md):\n{instructions}\n\nUser message:\n{text}")
            }
            _ => text,
        };
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
            agent: self.active.code,
            allow_sudo: self.settings.agent.allow_sudo,
            response_delay_enabled: self.settings.agent.response_delay_enabled,
            response_delay_secs: self.settings.agent.response_delay_secs,
        });
    }

    fn messages_push(&mut self, message: ChatMessage) {
        self.active.messages.push(message);
        self.persist_active();
    }

    /// Writes the active session's messages to the local cache so a
    /// reopened chat (also after an app restart) shows the same
    /// command/feedback windows. No-op for chats without an id yet.
    fn persist_active(&self) {
        if let Some(id) = &self.active.id {
            chatcache::save(
                id,
                &chatcache::CachedChat {
                    messages: self
                        .active
                        .messages
                        .iter()
                        .map(|m| chatcache::CachedMessage {
                            role: m.role.to_owned(),
                            thinking: m.thinking.clone(),
                            text: m.text.clone(),
                        })
                        .collect(),
                    last_message_id: self.active.last_message_id,
                },
            );
        }
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
                ChatEvent::StreamStart => {
                    self.streaming = Some(StreamingAnswer::default());
                }
                ChatEvent::Delta(streaming) => {
                    self.streaming = Some(streaming);
                }
                ChatEvent::ResponseDone {
                    result,
                    last_message_id,
                    visible_text,
                } => {
                    let streaming = self.streaming.take().unwrap_or_default();
                    match result {
                        Ok(()) => {
                            if let Some(id) = last_message_id {
                                self.active.last_message_id = Some(id);
                            }
                            self.messages_push(ChatMessage {
                                role: "assistant",
                                thinking: streaming.thinking,
                                text: visible_text.unwrap_or(streaming.text),
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
                ChatEvent::Finished {
                    result,
                    last_message_id,
                } => {
                    self.busy = false;
                    self.running_command_index = None;
                    self.streaming = None;
                    match result {
                        Ok(()) => {
                            if let Some(id) = last_message_id {
                                self.active.last_message_id = Some(id);
                            }
                        }
                        Err(e) => {
                            self.messages_push(ChatMessage {
                                role: "error",
                                thinking: None,
                                text: e,
                            });
                        }
                    }
                }
                ChatEvent::CommandStarted { cmd } => {
                    self.messages_push(ChatMessage {
                        role: "command",
                        thinking: None,
                        text: format!("$ {cmd}\n...running"),
                    });
                    self.running_command_index = Some(self.active.messages.len() - 1);
                }
                ChatEvent::CommandFinished { cmd } => {
                    // the window stays "just the command": the output is shown
                    // by the following feedback message, not duplicated here
                    let text = format!("$ {cmd}");
                    if let Some(idx) = self.running_command_index {
                        if let Some(msg) = self.active.messages.get_mut(idx) {
                            msg.text = text;
                            self.running_command_index = None;
                            self.persist_active();
                            continue;
                        }
                    }
                    self.messages_push(ChatMessage {
                        role: "command",
                        thinking: None,
                        text,
                    });
                }
                ChatEvent::AgentMessage { role, text } => {
                    self.messages_push(ChatMessage {
                        role,
                        thinking: None,
                        text,
                    });
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
                        .insert(0, deepseek::SessionInfo { id: id.clone(), title });
                    // the chat just got its id - messages pushed before it
                    // (the user prompt) are not cached yet
                    self.persist_active();
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
                        // seed the local cache from the server history so
                        // this chat is served locally from now on
                        self.persist_active();
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
                    egui::CollapsingHeader::new("⚙ auth")
                        .id_salt("auth_panel")
                        .show(ui, |ui| {
                            ui.monospace(self.config_path.display().to_string());
                            if self.resolved_profile.is_empty() {
                                ui.colored_label(
                                    egui::Color32::YELLOW,
                                    "profile: not detected",
                                );
                            } else {
                                ui.label(format!("profile: {}", self.resolved_profile));
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
                        if ui.add_enabled(!self.busy, egui::Button::new("⟳")).clicked() {
                            self.refresh_sessions();
                        }
                    });
                    ui.separator();
                    egui::Panel::bottom("sessions_bottom").show(ui, |ui| {
                        if ui
                            .add_sized(
                                [ui.available_width(), 22.0],
                                egui::Button::new("⚙ Settings"),
                            )
                            .clicked()
                        {
                            self.settings_draft = self.settings.clone();
                            self.settings_error = None;
                            self.settings_open = true;
                        }
                    });
                    egui::ScrollArea::vertical().id_salt("sessions_scroll").show(ui, |ui| {
                        let active_id = self.active.id.clone();
                        let mut open_request: Option<(String, String)> = None;
                        if self.sessions.is_empty() && !self.busy {
                            ui.colored_label(
                                egui::Color32::GRAY,
                                "No chats found.",
                            );
                        }
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
                    let font_id = egui::TextStyle::Body.resolve(ui.style()).clone();
                    let text_color = ui.visuals().text_color();
                    let input_text = self.input.clone();
                    let rows = ui
                        .fonts_mut(|fonts| {
                            fonts
                                .layout_job(egui::text::LayoutJob::simple(
                                    input_text,
                                    font_id,
                                    text_color,
                                    input_width,
                                ))
                                .rows
                                .len()
                        })
                        .clamp(1, 6);
                    let response = ui.add_sized(
                        [input_width, 22.0 * rows as f32],
                        egui::TextEdit::multiline(&mut self.input)
                            .desired_rows(rows)
                            .desired_width(input_width)
                            .hint_text("Type a message... (Ctrl+Enter to send)")
                            .id_salt("chat_input"),
                    );
                    let send_hotkey = response.has_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter) && i.modifiers.ctrl);
                    let can_send = !self.busy
                        && !self.input.trim().is_empty()
                        && self.cmd_tx.is_some();
                    if self.streaming.is_some() {
                        if ui
                            .add_sized(
                                [72.0, 30.0],
                                egui::Button::new(
                                    egui::RichText::new("■ Stop").size(15.0),
                                ),
                            )
                            .clicked()
                        {
                            if let Some(stop_tx) = self.stop_tx.clone() {
                                let _ = stop_tx.send(());
                            }
                        }
                    } else {
                        let send_clicked = ui
                            .add_enabled(
                                can_send,
                                egui::Button::new(egui::RichText::new("▶ Send").size(15.0)),
                            )
                            .clicked();
                        if (send_clicked || send_hotkey) && can_send {
                            self.send_message();
                            response.request_focus();
                        }
                    }
                });
                if self.cmd_tx.is_none() {
                    ui.colored_label(
                        egui::Color32::RED,
                        "No valid token for the default site - chat is disabled.",
                    );
                }
            });

            if self.settings_open {
                let mut open = self.settings_open;
                egui::Window::new("Settings")
                    .open(&mut open)
                    .resizable(false)
                    .show(&ctx, |ui| {
                        ui.strong("Agent");
                        ui.checkbox(
                            &mut self.settings_draft.agent.allow_sudo,
                            "Allow sudo commands",
                        );
                        ui.weak("When off, sudo commands are blocked and the agent is told so in feedback.");
                        ui.checkbox(
                            &mut self.settings_draft.agent.include_agents_md,
                            "Include AGENTS.md",
                        );
                        ui.weak("When on, AGENTS.md from the exe directory is prepended to every message (checked on each send).");
                        ui.horizontal(|ui| {
                            ui.label("Working directory:");
                            ui.add(
                                egui::TextEdit::singleline(
                                    &mut self.settings_draft.agent.working_dir,
                                )
                                .hint_text("local")
                                .desired_width(220.0),
                            );
                        });
                        ui.weak("\"local\" = the folder the app was launched from; anything else is used as a custom path.");
                        ui.add_space(6.0);
                        ui.checkbox(
                            &mut self.settings_draft.agent.response_delay_enabled,
                            "Response delay (rate-limit guard)",
                        );
                        ui.horizontal(|ui| {
                            ui.label("Delay before automatic agent requests:");
                            ui.add_enabled(
                                self.settings_draft.agent.response_delay_enabled,
                                egui::DragValue::new(
                                    &mut self.settings_draft.agent.response_delay_secs,
                                )
                                .range(0..=600)
                                .suffix(" s"),
                            );
                        });
                        ui.weak("Before each automatic agent-loop message (command feedback, Continue prompt) FireCode waits this many seconds. The user's own prompt is never delayed.");
                        ui.add_space(6.0);
                        ui.separator();
                        ui.strong("Browser (token source)");
                        let selected = self.settings_draft.browser.preset;
                        egui::ComboBox::from_id_salt("browser_preset_combo")
                            .selected_text(selected.display_name())
                            .show_ui(ui, |ui| {
                                for preset in settings::BrowserPreset::all() {
                                    ui.selectable_value(
                                        &mut self.settings_draft.browser.preset,
                                        preset,
                                        preset.display_name(),
                                    );
                                }
                            });
                        let resolved = config::resolve_profile_dir(
                            &self.config,
                            self.settings_draft.browser.preset,
                        );
                        ui.monospace(resolved.display().to_string());
                        if let Some(error) = &self.settings_error {
                            ui.colored_label(egui::Color32::RED, error);
                        }
                        ui.separator();
                        ui.horizontal(|ui| {
                            if ui.button("Save").clicked() {
                                self.save_settings();
                            }
                            if ui.button("Reset").clicked() {
                                self.reset_settings();
                            }
                        });
                    });
                self.settings_open = open;
            }

            egui::CentralPanel::default().show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.strong(&self.active.title);
                    ui.separator();
                    let think_btn = egui::Button::selectable(
                        self.active.deepthink,
                        egui::RichText::new("▶ DeepThink"),
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
                    let code_btn = egui::Button::selectable(
                        self.active.code,
                        egui::RichText::new("<> Code"),
                    );
                    if ui.add_enabled(!self.busy, code_btn).clicked() {
                        self.toggle_active(2);
                    }
                    if ui
                        .add_enabled(!self.busy && self.cmd_tx.is_some(), egui::Button::new("📄 Prompt"))
                        .clicked()
                    {
                        self.send_prompt_from_agents_md();
                    }
                    if self.busy {
                        ui.spinner();
                    }
                });
                ui.separator();

                // both directions: the horizontal scrollbar sits at the
                // bottom of the chat area, right above the input panel,
                // and pans the chat left/right when content is wider
                // than the viewport
                egui::ScrollArea::both()
                    .id_salt("chat_scroll")
                    .auto_shrink(false)
                    .stick_to_bottom(self.busy)
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
                    .show(ui, |ui| {
                        ui.add_space(4.0);
                        if self.active.messages.is_empty() && !self.busy {
                            ui.colored_label(
                                egui::Color32::GRAY,
                                "Select a chat on the left or start a new one.",
                            );
                        }
                        let ctx2 = ui.ctx().clone();
                        if let Some((_, at)) = &self.copied_code {
                            if at.elapsed().as_secs_f32() >= 1.5 {
                                self.copied_code = None;
                            }
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
                                        egui::CollapsingHeader::new("▶ Thinking")
                                            .id_salt(("thinking_header", mi))
                                            .default_open(false)
                                            .show(ui, |ui| {
                                                ui.horizontal_wrapped(|ui| {
                                                    ui.weak(thinking.clone());
                                                });
                                            });
                                    }
                                    markdown_blocks_ui(
                                        ui,
                                        &ctx2,
                                        &message.text,
                                        format!("blocks{mi}").as_str(),
                                        &mut self.copied_code,
                                    );
                                }
                                "command" => {
                                    // live indicator only: while the command
                                    // runs show the dim `$ cmd ...running`
                                    // line; when finished the feedback window
                                    // header already carries the command, so
                                    // rendering it again would duplicate it
                                    if message.text.contains("...running") {
                                        ui.horizontal_wrapped(|ui| {
                                            ui.weak(
                                                egui::RichText::new(&message.text)
                                                    .monospace(),
                                            );
                                        });
                                    }
                                }
                                "feedback" => {
                                    ui.strong("⚙ Feedback:");
                                    let blocks =
                                        agent::parse_feedback_blocks(&message.text);
                                    if blocks.is_empty() {
                                        markdown_blocks_ui(
                                            ui,
                                            &ctx2,
                                            &message.text,
                                            format!("feedback{mi}").as_str(),
                                            &mut self.copied_code,
                                        );
                                    } else {
                                        for (bi, (header, body)) in
                                            blocks.iter().enumerate()
                                        {
                                            command_output_ui(
                                                ui,
                                                egui::Id::new(("fblock", mi, bi)),
                                                header,
                                                body,
                                            );
                                        }
                                    }
                                }
                                "prompt" => {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.strong("📄 Prompt:");
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
                                    ui.weak(format!("▶ {thinking}\u{258c}"));
                                });
                            }
                            markdown_blocks_ui(
                                ui,
                                &ctx2,
                                &streaming.text,
                                "streamblocks",
                                &mut self.copied_code,
                            );
                            if streaming.text.is_empty() && streaming.thinking.is_none() {
                                ui.horizontal_wrapped(|ui| {
                                    ui.spinner();
                                    ui.label("waiting for stream...");
                                });
                            }
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
        Box::new(|cc| {
            fonts::install(&cc.egui_ctx);
            Ok(Box::new(FireCodeApp::new(config, config_path)))
        }),
    )
}
