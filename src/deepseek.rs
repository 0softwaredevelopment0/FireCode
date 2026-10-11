use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::Value;

use crate::pow::{PowChallenge, pow_header_value, solve_pow};

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:145.0) Gecko/20100101 Firefox/145.0";

pub struct DeepSeekClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

#[derive(Debug, Deserialize)]
struct ApiResponse {
    code: i64,
    msg: String,
    data: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ChallengeEnvelope {
    #[serde(default)]
    data: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct CompletionOptions {
    pub model_class: String,
    pub thinking_enabled: bool,
    pub search_enabled: bool,
}

impl DeepSeekClient {
    pub fn new(base_url: &str, token: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .build()
            .context("failed to build http client")?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
        })
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{}{path}", self.base_url))
            .header("Authorization", format!("Bearer {}", self.token))
            .header("x-client-platform", "web")
    }

    pub async fn create_session(&self) -> Result<String> {
        let resp: ApiResponse = self
            .post("/api/v0/chat_session/create")
            .json(&serde_json::json!({ "agent": "chat" }))
            .send()
            .await?
            .json()
            .await?;
        if resp.code != 0 {
            bail!("chat_session/create failed: {} ({})", resp.msg, resp.code);
        }
        let id = resp
            .data
            .as_ref()
            .and_then(|d| d["biz_data"]["id"].as_str())
            .ok_or_else(|| anyhow!("chat_session/create: missing session id"))?
            .to_owned();
        Ok(id)
    }

    pub async fn create_pow_challenge(&self, target_path: &str) -> Result<PowChallenge> {
        let body: ChallengeEnvelope = self
            .post("/api/v0/chat/create_pow_challenge")
            .json(&serde_json::json!({ "target_path": target_path }))
            .send()
            .await?
            .json()
            .await?;
        let ch: PowChallenge = serde_json::from_value(
            body.data
                .ok_or_else(|| anyhow!("create_pow_challenge: no data"))?["biz_data"]["challenge"]
                .clone(),
        )
        .context("failed to parse pow challenge")?;
        Ok(ch)
    }

    /// Sends a prompt and returns the raw SSE response on success.
    pub async fn completion(
        &self,
        session_id: &str,
        prompt: &str,
        parent_message_id: Option<i64>,
        options: &CompletionOptions,
    ) -> Result<reqwest::Response> {
        let target_path = "/api/v0/chat/completion";
        let challenge = self.create_pow_challenge(target_path).await?;
        let nonce = solve_pow(&challenge)
            .ok_or_else(|| anyhow!("pow solver failed to find a nonce"))?;
        let pow = pow_header_value(&challenge, nonce);

        let body = serde_json::json!({
            "chat_session_id": session_id,
            "prompt": prompt,
            "ref_file_ids": [],
            "parent_message_id": parent_message_id,
            "thinking_enabled": options.thinking_enabled,
            "search_enabled": options.search_enabled,
            "model_class": options.model_class,
        });

        let resp = self
            .post(target_path)
            .header("x-ds-pow-response", pow)
            .json(&body)
            .send()
            .await
            .context("completion request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            bail!("completion failed: HTTP {status}: {}", text.trim());
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        if content_type.starts_with("application/json") {
            let api: ApiResponse = resp.json().await?;
            bail!("completion failed: {} ({})", api.msg, api.code);
        }
        Ok(resp)
    }

    /// Lists recent chats (the ones shown at chat.deepseek.com,
    /// share links like /a/chat/s/<uuid>).
    pub async fn list_sessions(&self, limit: u32) -> Result<Vec<SessionInfo>> {
        let resp: ApiResponse = self
            .http
            .get(format!("{}/api/v0/chat_session/fetch_page", self.base_url))
            .query(&[
                ("lte_cursor.pinned", "false"),
                ("limit", &limit.to_string()),
            ])
            .header("Authorization", format!("Bearer {}", self.token))
            .header("x-client-platform", "web")
            .send()
            .await?
            .json()
            .await?;
        if resp.code != 0 {
            bail!("chat_session/fetch_page failed: {} ({})", resp.msg, resp.code);
        }
        let empty = Vec::new();
        let items = resp
            .data
            .as_ref()
            .and_then(|d| d["biz_data"]["chat_sessions"].as_array())
            .unwrap_or(&empty);
        Ok(items
            .iter()
            .filter_map(|it| {
                Some(SessionInfo {
                    id: it["id"].as_str()?.to_owned(),
                    title: it["title"]
                        .as_str()
                        .unwrap_or("New chat")
                        .to_owned(),
                })
            })
            .collect())
    }

    /// Loads the message history of a session.
    pub async fn history(&self, session_id: &str) -> Result<Vec<HistoryMessage>> {
        let resp: ApiResponse = self
            .http
            .get(format!("{}/api/v0/chat/history_messages", self.base_url))
            .query(&[("chat_session_id", session_id)])
            .header("Authorization", format!("Bearer {}", self.token))
            .header("x-client-platform", "web")
            .send()
            .await?
            .json()
            .await?;
        if resp.code != 0 {
            bail!("chat/history_messages failed: {} ({})", resp.msg, resp.code);
        }
        let empty = Vec::new();
        let items = resp
            .data
            .as_ref()
            .and_then(|d| d["biz_data"]["chat_messages"].as_array())
            .unwrap_or(&empty);
        Ok(items
            .iter()
            .map(|m| {
                let role = match m["role"].as_str() {
                    Some("USER") => "user",
                    _ => "assistant",
                };
                let mut thinking = String::new();
                let mut text = String::new();
                if let Some(frags) = m["fragments"].as_array() {
                    for frag in frags {
                        let content = frag["content"].as_str().unwrap_or("");
                        match frag["type"].as_str() {
                            Some("THINK") | Some("THINKING") => thinking.push_str(content),
                            Some("RESPONSE") | Some("USER") => text.push_str(content),
                            _ => {}
                        }
                    }
                }
                let thinking = if thinking.trim().is_empty() {
                    None
                } else {
                    Some(thinking)
                };
                let message_id = m["message_id"].as_i64();
                HistoryMessage {
                    role: role.to_owned(),
                    thinking,
                    text,
                    message_id,
                }
            })
            .collect())
    }
}

/// Accumulates the assistant answer from SSE `data:` payloads.
///
/// The assistant message is a list of fragments (`THINKING`, `RESPONSE`,
/// search-result fragments). Stream payload kinds seen in the wild:
/// - full snapshot: `{"v": {"response": {"fragments": [...]}}}`
/// - fragment creation: `{"p": "response/fragments", "o": "APPEND", "v": [frags]}`
/// - patch: `{"p": "response/fragments/-1/content", "o": "APPEND", "v": "..."}`
/// - continuation: `{"v": "..."}` (no p/o) - appends to the last fragment;
///   this is how most tokens arrive, dropping them truncates the answer
/// - batch: `{"p": "response", "o": "BATCH", "v": [...]}` - items carry
///   paths relative to `response` (e.g. `"fragments"`)
#[derive(Default)]
pub struct ResponseAccumulator {
    fragments: Vec<Fragment>,
    /// Content that arrived before the server created any fragment. Such
    /// streams (e.g. DeepThink) start with content patches while the fragment
    /// list is still empty; the implicit fragment is classified once the
    /// server reveals fragment types.
    implicit: Option<String>,
}

#[derive(Default, Clone)]
struct Fragment {
    ftype: String,
    content: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AccumulatedAnswer {
    pub thinking: Option<String>,
    pub text: String,
}

impl ResponseAccumulator {
    pub fn answer(&self) -> AccumulatedAnswer {
        let mut out = AccumulatedAnswer::default();
        let mut thinking = String::new();
        for frag in &self.fragments {
            match frag.ftype.as_str() {
                // the real wire type is "THINK"; accept THINKING defensively
                "THINK" | "THINKING" => thinking.push_str(&frag.content),
                "RESPONSE" => out.text.push_str(&frag.content),
                _ => {}
            }
        }
        // Unresolved implicit content: it preceded every known fragment, so
        // if a RESPONSE fragment exists the implicit part was reasoning.
        if let Some(imp) = &self.implicit {
            if !imp.is_empty() {
                if self.fragments.iter().any(|f| f.ftype == "RESPONSE") {
                    thinking.insert_str(0, imp);
                } else {
                    out.text.insert_str(0, imp);
                }
            }
        }
        if !thinking.trim().is_empty() {
            out.thinking = Some(thinking);
        }
        out
    }

    /// Feeds one `data:` payload; returns true when the answer changed.
    pub fn feed(&mut self, payload: &str) -> bool {
        let Ok(v) = serde_json::from_str::<Value>(payload) else {
            return false;
        };
        if let Some(snapshot) = v.get("v") {
            if snapshot.is_object() && snapshot.get("response").is_some() {
                return self.apply_snapshot(snapshot);
            }
        }
        if let (Some(p), Some(o)) = (
            v.get("p").and_then(Value::as_str),
            v.get("o").and_then(Value::as_str),
        ) {
            return match o {
                "APPEND" => match p {
                    "response/fragments" | "fragments" => {
                        self.append_fragments(v.get("v"))
                    }
                    _ if p.ends_with("/content") => {
                        self.append_to_last_fragment(v.get("v").and_then(Value::as_str))
                    }
                    _ => false,
                },
                "BATCH" => {
                    if let Some(items) = v.get("v").and_then(Value::as_array) {
                        let mut changed = false;
                        for item in items {
                            changed |= self.feed_nested(item);
                        }
                        return changed;
                    }
                    false
                }
                _ => false,
            };
        }
        // Bare `{"v": "..."}` (no p at all): continuation of the last fragment.
        // Payloads carrying a `p` but no `o` (results, quasi_status) are NOT
        // continuations.
        if v.get("p").is_none() {
            if let Some(add) = v.get("v").and_then(Value::as_str) {
                return self.append_to_last_fragment(Some(add));
            }
        }
        false
    }

    /// BATCH items carry paths relative to `response`: only fragment-list
    /// patches are meaningful here; content paths inside batches belong to
    /// search fragments and other non-answer fragments, so they are skipped.
    fn feed_nested(&mut self, item: &Value) -> bool {
        let p = item.get("p").and_then(Value::as_str);
        let o = item.get("o").and_then(Value::as_str);
        match (p, o) {
            (Some("response/fragments") | Some("fragments"), Some("APPEND")) => {
                self.append_fragments(item.get("v"))
            }
            _ => false,
        }
    }

    fn append_fragments(&mut self, v: Option<&Value>) -> bool {
        let Some(items) = v.and_then(Value::as_array) else {
            return false;
        };
        let before = self.answer();
        self.reconcile_implicit();
        for frag in items {
            self.fragments.push(Fragment {
                ftype: frag["type"].as_str().unwrap_or("").to_owned(),
                content: frag["content"].as_str().unwrap_or("").to_owned(),
            });
        }
        self.answer() != before
    }

    fn append_to_last_fragment(&mut self, add: Option<&str>) -> bool {
        let Some(add) = add else {
            return false;
        };
        if add.is_empty() {
            return false;
        }
        let before = self.answer();
        if let Some(last) = self.fragments.last_mut() {
            last.content.push_str(add);
        } else {
            self.implicit.get_or_insert_default().push_str(add);
        }
        self.answer() != before
    }

    /// The implicit fragment always precedes every server-declared fragment,
    /// and content that streams before a RESPONSE fragment is reasoning.
    fn reconcile_implicit(&mut self) {
        let Some(imp) = self.implicit.take() else {
            return;
        };
        if imp.is_empty() || self.fragments.is_empty() {
            self.implicit = Some(imp);
            return;
        }
        self.fragments.insert(
            0,
            Fragment {
                ftype: "THINKING".to_owned(),
                content: imp,
            },
        );
    }

    fn apply_snapshot(&mut self, snapshot: &Value) -> bool {
        let Some(frags) = snapshot["response"]["fragments"].as_array() else {
            return false;
        };
        let before = self.answer();
        // Server snapshots are authoritative state; drop any implicit content
        // (it is already included in the snapshot's fragments).
        self.implicit = None;
        self.fragments = frags
            .iter()
            .map(|frag| Fragment {
                ftype: frag["type"].as_str().unwrap_or("").to_owned(),
                content: frag["content"].as_str().unwrap_or("").to_owned(),
            })
            .collect();
        self.answer() != before
    }
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct HistoryMessage {
    pub role: String,
    pub thinking: Option<String>,
    pub text: String,
    pub message_id: Option<i64>,
}

/// Splits an SSE byte stream into `(event_name, data_payload)` pairs and
/// invokes `on_data` for every payload. Returns the `response_message_id`
/// from the `ready` event, if seen.
pub async fn pump_sse<F>(resp: reqwest::Response, mut on_data: F) -> Result<Option<i64>>
where
    F: FnMut(&str, &str),
{
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut response_message_id: Option<i64> = None;
    let mut event = String::new();
    loop {
        let chunk = match stream.next().await {
            Some(Ok(c)) => c,
            Some(Err(e)) => bail!("stream error: {e}"),
            None => break,
        };
        buf.extend_from_slice(&chunk);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let mut line = String::from_utf8_lossy(&line).into_owned();
            while line.ends_with('\n') || line.ends_with('\r') {
                line.pop();
            }
            if let Some(name) = line.strip_prefix("event:") {
                event = name.trim().to_owned();
                continue;
            }
            let Some(payload) = line.strip_prefix("data:") else {
                continue;
            };
            let payload = payload.trim();
            if payload.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(payload) {
                if let Some(id) = v.get("response_message_id").and_then(Value::as_i64) {
                    response_message_id = Some(id);
                }
            }
            on_data(&event, payload);
            if event == "close" {
                return Ok(response_message_id);
            }
        }
    }
    Ok(response_message_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    fn snapshot(thinking: &str, response: &str) -> String {
        format!(
            r#"{{"v":{{"response":{{"fragments":[
            {{"id":1,"type":"THINK","content":"{thinking}"}},
            {{"id":2,"type":"RESPONSE","content":"{response}"}}]}}}}}}"#
        )
    }

    #[test]
    fn accumulator_handles_snapshot_then_appends() {
        let mut acc = ResponseAccumulator::default();
        assert!(acc.feed(&snapshot("hmm", "p")));
        let a = acc.answer();
        assert_eq!(a.text, "p");
        assert_eq!(a.thinking.as_deref(), Some("hmm"));
        assert!(acc.feed(r#"{"p":"response/fragments/-1/content","o":"APPEND","v":"ong"}"#));
        assert_eq!(acc.answer().text, "pong");
        assert!(!acc.feed(r#"{"p":"response/fragments/-1/content","o":"APPEND","v":""}"#));
    }

    #[test]
    fn accumulator_appends_bare_v_continuations() {
        let mut acc = ResponseAccumulator::default();
        assert!(acc.feed(
            r#"{"p":"response/fragments/-1/content","o":"APPEND","v":"Я —"}"#
        ));
        assert!(acc.feed(r#"{"v":" Deep"}"#));
        assert!(acc.feed(r#"{"v":"Seek"}"#));
        assert_eq!(acc.answer().text, "Я — DeepSeek");
        // title payloads have neither v nor p - must not touch the text
        assert!(!acc.feed(r#"{"content":"DeepSeek модель"}"#));
        assert_eq!(acc.answer().text, "Я — DeepSeek");
    }

    #[test]
    fn accumulator_bare_v_goes_to_implicit_text_without_fragments() {
        let mut acc = ResponseAccumulator::default();
        // content before any fragment exists is captured as implicit
        assert!(acc.feed(r#"{"v":"orphan"}"#));
        assert_eq!(acc.answer().text, "orphan");
        // payloads without a string "v" change nothing
        assert!(!acc.feed(r#"{"content":"title"}"#));
        assert_eq!(acc.answer().text, "orphan");
    }

    #[test]
    fn accumulator_ignores_status_values_after_content() {
        let mut acc = ResponseAccumulator::default();
        assert!(acc.feed(r#"{"p":"response/fragments/-1/content","o":"APPEND","v":"pong"}"#));
        // server finish payloads must not leak into the text
        assert!(!acc.feed(
            r#"{"p":"response","o":"BATCH","v":[{"p":"accumulated_token_usage","v":99},{"p":"quasi_status","v":"FINISHED"}]}"#
        ));
        assert!(!acc.feed(r#"{"p":"response/status","o":"SET","v":"FINISHED"}"#));
        assert_eq!(acc.answer().text, "pong");
    }

    #[test]
    fn accumulator_batch_creates_fragments_and_ignores_scalars() {
        let mut acc = ResponseAccumulator::default();
        assert!(acc.feed(r#"{"p":"response","o":"BATCH","v":[
            {"p":"fragments","o":"APPEND","v":[{"id":3,"type":"RESPONSE","content":"he","references":[],"stage_id":2}]},
            {"p":"has_pending_fragment","o":"SET","v":false}]}"#));
        assert!(acc.feed(r#"{"v":"y"}"#));
        assert_eq!(acc.answer().text, "hey");
    }

    #[test]
    fn accumulator_replaces_text_on_late_snapshot() {
        let mut acc = ResponseAccumulator::default();
        assert!(acc.feed(r#"{"p":"response/fragments/-1/content","o":"APPEND","v":"par"}"#));
        assert!(acc.feed(&snapshot("", "partial text")));
        assert_eq!(acc.answer().text, "partial text");
    }

    #[test]
    fn accumulator_ignores_garbage_and_non_text_patches() {
        let mut acc = ResponseAccumulator::default();
        assert!(!acc.feed("not json"));
        assert!(!acc.feed(r#"{"p":"accumulated_token_usage","o":"BATCH","v":[]}"#));
        assert_eq!(acc.answer().text, "");
    }

    #[test]
    fn accumulator_thinking_flow_from_real_dump() {
        let mut acc = ResponseAccumulator::default();
        // the initial snapshot carries the first THINK tokens
        assert!(acc.feed(
            r#"{"v":{"response":{"message_id":2,"thinking_enabled":true,"fragments":[{"id":2,"type":"THINK","content":"We","elapsed_secs":null,"stage_id":1}]}}}"#
        ));
        assert_eq!(acc.answer().thinking.as_deref(), Some("We"));
        assert!(acc.feed(r#"{"p":"response/fragments/-1/content","o":"APPEND","v":" need"}"#));
        assert!(acc.feed(r#"{"v":" answer."}"#));
        assert!(!acc.feed(
            r#"{"p":"response/fragments/-1/elapsed_secs","o":"SET","v":0.78}"#
        ));
        assert_eq!(
            acc.answer().thinking.as_deref(),
            Some("We need answer.")
        );
        assert!(acc.feed(
            r#"{"p":"response/fragments","o":"APPEND","v":[{"id":3,"type":"RESPONSE","content":"255","references":[],"stage_id":1}]}"#
        ));
        let a = acc.answer();
        assert_eq!(a.text, "255");
        assert_eq!(a.thinking.as_deref(), Some("We need answer."));
    }

    #[test]
    fn accumulator_search_flow_from_real_dump() {
        let mut acc = ResponseAccumulator::default();
        // search results patch: p without o - not a continuation
        assert!(!acc.feed(
            r#"{"p":"response/fragments/-1/results","v":[{"url":"https://x","title":"t","snippet":"s"}]}"#
        ));
        // search-fragment content arrives in a nested fragment BATCH - ignored
        assert!(!acc.feed(
            r#"{"p":"response/fragments/-1","o":"BATCH","v":[{"p":"status","v":"FINISHED"},{"p":"content","v":"Found 12 web pages"}]}"#
        ));
        assert!(!acc.feed(r#"{"p":"response/conversation_mode","o":"SET","v":"SEARCH"}"#));
        assert!(acc.feed(r#"{"p":"response","o":"BATCH","v":[
            {"p":"fragments","o":"APPEND","v":[{"id":3,"type":"RESPONSE","content":"The","references":[],"stage_id":2}]},
            {"p":"has_pending_fragment","o":"SET","v":false}]}"#));
        assert!(acc.feed(r#"{"p":"response/fragments/-1/content","o":"APPEND","v":" current"}"#));
        assert!(acc.feed(r#"{"v":" stable **1.96.1**[citation:2]."}"#));
        let a = acc.answer();
        assert_eq!(a.text, "The current stable **1.96.1**[citation:2].");
        assert_eq!(a.thinking, None);
    }

    fn real_client() -> (DeepSeekClient, config::Config) {
        let (config, _) = crate::config::load_or_create().expect("config");
        let site = config.site(&config.default_site).expect("site");
        let preset = crate::settings::BrowserPreset::Firefox;
        let profile = crate::config::resolve_profile_dir(&config, preset);
        let token = crate::auth::read_token(&profile, preset, &site.token)
            .expect("token")
            .value;
        let client = DeepSeekClient::new(&site.api_base, &token).expect("client");
        (client, config)
    }

    /// Full round trip against the real DeepSeek account on this machine.
    /// Run explicitly: cargo test --release -- --ignored
    #[tokio::test]
    #[ignore = "requires the real Firefox profile and sends real messages"]
    async fn real_round_trip_replies_pong() {
        let (client, config) = real_client();
        let session = client.create_session().await.expect("session");
        let options = CompletionOptions {
            model_class: config
                .preset(&config.default_preset)
                .expect("preset")
                .model_class
                .clone(),
            thinking_enabled: false,
            search_enabled: false,
        };
        let resp = client
            .completion(&session, "Reply with exactly: pong", None, &options)
            .await
            .expect("completion");
        assert_eq!(
            resp.headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or(""),
            "text/event-stream; charset=utf-8"
        );

        let mut acc = ResponseAccumulator::default();
        let _ = pump_sse(resp, |_event, payload| {
            acc.feed(payload);
        })
        .await
        .expect("stream");
        println!("assistant: {:?}", acc.answer().text);
        assert_eq!(acc.answer().text.trim(), "pong");
    }

    /// Round trip with DeepThink enabled: reasoning must arrive separately.
    /// Run explicitly: cargo test --release -- --ignored
    #[tokio::test]
    #[ignore = "requires the real Firefox profile and sends real messages"]
    async fn real_thinking_round_trip_separates_reasoning() {
        let (client, _config) = real_client();
        let session = client.create_session().await.expect("session");
        let options = CompletionOptions {
            model_class: "deepseek_chat".to_owned(),
            thinking_enabled: true,
            search_enabled: false,
        };
        let resp = client
            .completion(
                &session,
                "What is 15*17? Think briefly, then answer with just the number.",
                None,
                &options,
            )
            .await
            .expect("completion");

        let mut acc = ResponseAccumulator::default();
        let _ = pump_sse(resp, |_event, payload| {
            acc.feed(payload);
        })
        .await
        .expect("stream");
        let a = acc.answer();
        println!("thinking: {:?}", a.thinking.as_deref().map(str::len));
        println!("text: {:?}", a.text);
        assert!(a.thinking.is_some(), "reasoning must arrive for DeepThink");
        assert!(!a.text.trim().is_empty(), "answer text must be present");
    }

    /// Verifies session listing and history against the real account.
    /// Run explicitly: cargo test --release -- --ignored
    #[tokio::test]
    #[ignore = "requires the real Firefox profile and a logged-in account"]
    async fn real_sessions_list_and_history() {
        let (client, _config) = real_client();
        let sessions = client.list_sessions(20).await.expect("list");
        println!("sessions: {}", sessions.len());
        for s in sessions.iter().take(5) {
            println!("  {} | {}", s.id, s.title);
        }
        assert!(!sessions.is_empty(), "account has at least one chat");

        let first = &sessions[0];
        let history = client.history(&first.id).await.expect("history");
        println!(
            "history of '{}' ({}): {} messages",
            first.title,
            first.id,
            history.len()
        );
        assert!(
            history.iter().any(|m| m.role == "user"),
            "history contains a user message"
        );
        assert!(
            history
                .iter()
                .any(|m| m.role == "assistant" && !m.text.trim().is_empty()),
            "history contains a non-empty assistant message"
        );
    }
}