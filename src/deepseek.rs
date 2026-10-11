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
        model_class: &str,
        thinking_enabled: bool,
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
            "thinking_enabled": thinking_enabled,
            "model_class": model_class,
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
                let mut text = String::new();
                if let Some(frags) = m["fragments"].as_array() {
                    for frag in frags {
                        if frag["type"] == Value::String("RESPONSE".into())
                            || frag["type"] == Value::String("USER".into())
                        {
                            text.push_str(frag["content"].as_str().unwrap_or(""));
                        }
                    }
                }
                let message_id = m["message_id"].as_i64();
                HistoryMessage {
                    role: role.to_owned(),
                    text,
                    message_id,
                }
            })
            .collect())
    }
}

/// Accumulates the assistant answer from SSE `data:` payloads.
///
/// Stream payload kinds seen in the wild:
/// - full snapshot: `{"v": {"response": {"fragments": [...]}}}`
/// - patch: `{"p": "response/fragments/-1/content", "o": "APPEND", "v": "..."}`
/// - continuation: `{"v": "..."}` (no p/o) - appends to the last APPEND target;
///   this is how most tokens arrive, dropping them truncates the answer
/// - batch: `{"p": "response", "o": "BATCH", "v": [patches or continuations]}`
#[derive(Default)]
pub struct ResponseAccumulator {
    pub text: String,
    last_append_is_content: bool,
}

impl ResponseAccumulator {
    /// Feeds one `data:` payload; returns true when the accumulated text changed.
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
            match o {
                "APPEND" => {
                    self.last_append_is_content = p.ends_with("content");
                    if self.last_append_is_content {
                        if let Some(add) = v.get("v").and_then(Value::as_str) {
                            self.text.push_str(add);
                            return !add.is_empty();
                        }
                    }
                }
                "BATCH" => {
                    if let Some(items) = v.get("v").and_then(Value::as_array) {
                        let mut changed = false;
                        for item in items {
                            changed |= self.feed(&item.to_string());
                        }
                        return changed;
                    }
                }
                _ => {}
            }
            return false;
        }
        // Bare `{"v": "..."}` (no p at all): continuation of the last APPEND
        // target. Payloads carrying a `p` but no `o` (e.g. quasi_status) are
        // NOT continuations.
        if self.last_append_is_content && v.get("p").is_none() {
            if let Some(add) = v.get("v").and_then(Value::as_str) {
                self.text.push_str(add);
                return !add.is_empty();
            }
        }
        false
    }

    fn apply_snapshot(&mut self, snapshot: &Value) -> bool {
        let Some(frags) = snapshot["response"]["fragments"].as_array() else {
            return false;
        };
        for frag in frags.iter().rev() {
            if frag["type"] == Value::String("RESPONSE".into()) {
                let content = frag["content"].as_str().unwrap_or("");
                if self.text != content {
                    self.text = content.to_owned();
                    return true;
                }
                return false;
            }
        }
        false
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

    #[test]
    fn accumulator_handles_snapshot_then_appends() {
        let mut acc = ResponseAccumulator::default();
        let snapshot = r#"{"v":{"response":{"fragments":[
            {"id":1,"type":"THINKING","content":"hmm"},
            {"id":2,"type":"RESPONSE","content":"p"}]}}}"#;
        assert!(acc.feed(snapshot));
        assert_eq!(acc.text, "p");
        assert!(acc.feed(r#"{"p":"response/fragments/-1/content","o":"APPEND","v":"ong"}"#));
        assert_eq!(acc.text, "pong");
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
        assert_eq!(acc.text, "Я — DeepSeek");
        // title/close payloads must not touch the text
        assert!(!acc.feed(r#"{"content":"DeepSeek модель"}"#));
        assert_eq!(acc.text, "Я — DeepSeek");
    }

    #[test]
    fn accumulator_bare_v_ignored_without_preceding_append() {
        let mut acc = ResponseAccumulator::default();
        assert!(!acc.feed(r#"{"v":"orphan"}"#));
        assert_eq!(acc.text, "");
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
        assert_eq!(acc.text, "pong");
    }

    #[test]
    fn accumulator_handles_batch_patches_and_continuations() {
        let mut acc = ResponseAccumulator::default();
        let batch = r#"{"p":"response","o":"BATCH","v":[
            {"p":"response/fragments/-1/content","o":"APPEND","v":"he"},
            {"v":"y"},
            {"p":"accumulated_token_usage","v":42}]}"#;
        assert!(acc.feed(batch));
        assert_eq!(acc.text, "hey");
    }

    #[test]
    fn accumulator_replaces_text_on_late_snapshot() {
        let mut acc = ResponseAccumulator::default();
        assert!(acc.feed(r#"{"p":"response/fragments/-1/content","o":"APPEND","v":"par"}"#));
        let snapshot = r#"{"v":{"response":{"fragments":[
            {"id":2,"type":"RESPONSE","content":"partial text"}]}}}"#;
        assert!(acc.feed(snapshot));
        assert_eq!(acc.text, "partial text");
    }

    #[test]
    fn accumulator_ignores_garbage_and_non_text_patches() {
        let mut acc = ResponseAccumulator::default();
        assert!(!acc.feed("not json"));
        assert!(!acc.feed(r#"{"p":"accumulated_token_usage","o":"BATCH","v":[]}"#));
        assert_eq!(acc.text, "");
    }

    /// Full round trip against the real DeepSeek account on this machine.
    /// Run explicitly: cargo test --release -- --ignored
    #[tokio::test]
    #[ignore = "requires the real Firefox profile and sends real messages"]
    async fn real_round_trip_replies_pong() {
        let (config, _) = crate::config::load_or_create().expect("config");
        let site = config.site(&config.default_site).expect("site");
        let token = crate::auth::read_token(
            std::path::Path::new(&config.profile.path),
            &site.token,
        )
        .expect("token")
        .value;

        let client = DeepSeekClient::new(&site.api_base, &token).expect("client");
        let session = client.create_session().await.expect("session");
        let resp = client
            .completion(&session, "Reply with exactly: pong", None, "deepseek_chat", false)
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
        println!("assistant: {:?}", acc.text);
        assert_eq!(acc.text.trim(), "pong");
    }

    /// Verifies session listing and history against the real account.
    /// Run explicitly: cargo test --release -- --ignored
    #[tokio::test]
    #[ignore = "requires the real Firefox profile and a logged-in account"]
    async fn real_sessions_list_and_history() {
        let (config, _) = crate::config::load_or_create().expect("config");
        let site = config.site(&config.default_site).expect("site");
        let token = crate::auth::read_token(
            std::path::Path::new(&config.profile.path),
            &site.token,
        )
        .expect("token")
        .value;

        let client = DeepSeekClient::new(&site.api_base, &token).expect("client");
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
            history.iter().any(|m| m.role == "assistant" && !m.text.trim().is_empty()),
            "history contains a non-empty assistant message"
        );
    }
}
