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
            "model_class": "deepseek_chat",
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
}

/// Accumulates the assistant answer from SSE `data:` payloads.
///
/// The stream mixes full snapshots (`{"v": {"response": {"fragments": ...}}}`)
/// and JSON patches (`{"p": "response/fragments/-1/content", "o": "APPEND", "v": "..."}`).
#[derive(Default)]
pub struct ResponseAccumulator {
    pub text: String,
}

impl ResponseAccumulator {
    /// Feeds one `data:` payload; returns true when the accumulated text changed.
    pub fn feed(&mut self, payload: &str) -> bool {
        let Ok(v) = serde_json::from_str::<Value>(payload) else {
            return false;
        };
        if let Some(snapshot) = v.get("v") {
            if snapshot.is_object() {
                return self.apply_snapshot(snapshot);
            }
        }
        if let (Some(p), Some(o)) = (
            v.get("p").and_then(Value::as_str),
            v.get("o").and_then(Value::as_str),
        ) {
            match o {
                "APPEND" => {
                    if p.ends_with("content") {
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

/// Splits an SSE byte stream into `data:` payloads and invokes `on_data`.
/// Returns the `response_message_id` from the `ready` event, if seen.
pub async fn pump_sse<F>(resp: reqwest::Response, mut on_data: F) -> Result<Option<i64>>
where
    F: FnMut(&str),
{
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut response_message_id: Option<i64> = None;
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
            let _ = on_data(payload);
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
    fn accumulator_handles_batch_patches() {
        let mut acc = ResponseAccumulator::default();
        let batch = r#"{"p":"response","o":"BATCH","v":[
            {"p":"response/fragments/-1/content","o":"APPEND","v":"he"},
            {"p":"accumulated_token_usage","v":42},
            {"p":"response/fragments/-1/content","o":"APPEND","v":"y"}]}"#;
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
            .completion(&session, "Reply with exactly: pong", None, false)
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
        let _ = pump_sse(resp, |payload| {
            acc.feed(payload);
        })
        .await
        .expect("stream");
        println!("assistant: {:?}", acc.text);
        assert_eq!(acc.text.trim(), "pong");
    }
}
