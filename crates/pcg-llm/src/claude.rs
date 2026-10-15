//! The Claude API (`POST /v1/messages`, streamed) as a [`Backend`].
//!
//! Rust has no official Anthropic SDK, so this speaks the HTTP API directly:
//! one streaming request per call, server-sent events parsed line by line.
//! Credentials come from the environment like the official SDKs read them:
//! `ANTHROPIC_API_KEY` (sent as `x-api-key`), else `ANTHROPIC_AUTH_TOKEN`
//! (an OAuth bearer token). `ANTHROPIC_BASE_URL` overrides the endpoint,
//! `PCG_LLM_MODEL` the model.
//!
//! Thinking is left at the model's default (adaptive); the request's
//! [`Effort`](crate::Effort) sets how much of it to spend. Declined requests
//! are re-run server-side on Anthropic's recommended fallback model
//! (`fallbacks: "default"`) where the model supports it; a decline that
//! survives that is [`Error::Refused`].

use crate::backend::{Backend, Completion, Error, Request};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read};
use std::time::Duration;

pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
const DEFAULT_URL: &str = "https://api.anthropic.com";
const VERSION: &str = "2023-06-01";
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const OAUTH_BETA: &str = "oauth-2025-04-20";
/// Retries of overloaded / rate-limited / failed requests before any text arrived.
const RETRIES: u32 = 3;

enum Auth {
    ApiKey(String),
    Bearer(String),
}

pub struct Claude {
    auth: Auth,
    model: String,
    url: String,
    agent: ureq::Agent,
}

/// Models that accept `fallbacks: "default"`.
fn takes_fallbacks(model: &str) -> bool {
    matches!(model, "claude-opus-5-5" | "claude-opus-5" | "claude-fable-5-1" | "claude-sonnet-5-5")
}

impl Claude {
    /// From the environment; `Err` says what is missing.
    pub fn from_env() -> Result<Self, Error> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let auth = match (var("ANTHROPIC_API_KEY"), var("ANTHROPIC_AUTH_TOKEN")) {
            (Some(k), _) => Auth::ApiKey(k.trim().to_string()),
            (None, Some(t)) => Auth::Bearer(t.trim().to_string()),
            (None, None) => {
                return Err(Error::Config("set ANTHROPIC_API_KEY to use Claude".into()));
            }
        };
        let model = var("PCG_LLM_MODEL").unwrap_or_else(|| DEFAULT_MODEL.into());
        let url = var("ANTHROPIC_BASE_URL").unwrap_or_else(|| DEFAULT_URL.into());
        Ok(Self::new(auth, model, url))
    }

    pub fn with_api_key(key: &str, model: &str) -> Self {
        Self::new(Auth::ApiKey(key.into()), model.into(), DEFAULT_URL.into())
    }

    fn new(auth: Auth, model: String, url: String) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(20)))
            // A stream may legitimately stay open for minutes; this only bounds silence.
            .timeout_recv_body(Some(Duration::from_secs(300)))
            .http_status_as_error(false)
            .build()
            .into();
        Self { auth, model, url: url.trim_end_matches('/').to_string(), agent }
    }

    fn body(&self, req: &Request) -> Value {
        let mut b = json!({
            "model": self.model,
            "max_tokens": req.max_tokens,
            "stream": true,
            "system": req.system,
            "messages": [{ "role": "user", "content": req.prompt }],
            "output_config": { "effort": req.effort.as_str() },
        });
        if takes_fallbacks(&self.model) {
            b["fallbacks"] = json!("default");
        }
        b
    }

    fn send(&self, body: &str) -> Result<ureq::http::Response<ureq::Body>, Error> {
        let mut betas: Vec<&str> = Vec::new();
        if takes_fallbacks(&self.model) {
            betas.push(FALLBACK_BETA);
        }
        let mut r = self
            .agent
            .post(format!("{}/v1/messages", self.url))
            .header("content-type", "application/json")
            .header("anthropic-version", VERSION);
        r = match &self.auth {
            Auth::ApiKey(k) => r.header("x-api-key", k),
            Auth::Bearer(t) => {
                betas.push(OAUTH_BETA);
                r.header("authorization", format!("Bearer {t}"))
            }
        };
        if !betas.is_empty() {
            r = r.header("anthropic-beta", betas.join(","));
        }
        r.send(body).map_err(|e| Error::Network(e.to_string()))
    }
}

/// The message of an API error body (`{"error":{"message":…}}`), else the body.
fn error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| body.chars().take(300).collect())
}

impl Backend for Claude {
    fn name(&self) -> &str {
        &self.model
    }

    fn complete(&self, req: &Request, on_text: &mut dyn FnMut(&str) -> bool) -> Result<Completion, Error> {
        let body = self.body(req).to_string();
        let mut attempt = 0;
        loop {
            attempt += 1;
            let retry = std::thread::sleep;
            let mut resp = match self.send(&body) {
                Ok(r) => r,
                Err(_) if attempt <= RETRIES => {
                    retry(Duration::from_secs(1 << attempt));
                    continue;
                }
                Err(e) => return Err(e),
            };
            let status = resp.status().as_u16();
            if status != 200 {
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .map_or(Duration::from_secs(1 << attempt), Duration::from_secs);
                let text = resp.body_mut().read_to_string().unwrap_or_default();
                if matches!(status, 429 | 500..=599) && attempt <= RETRIES {
                    retry(wait.min(Duration::from_secs(60)));
                    continue;
                }
                return Err(Error::Http { status, message: error_message(&text) });
            }
            let mut streamed = false;
            let res = read_stream(resp.body_mut().as_reader(), &mut |t| {
                streamed = true;
                on_text(t)
            });
            match res {
                // A stream that broke before any text is as good as a failed request.
                Err(Error::Network(_) | Error::Http { status: 429 | 500..=599, .. })
                    if !streamed && attempt <= RETRIES =>
                {
                    retry(Duration::from_secs(1 << attempt));
                }
                r => return r,
            }
        }
    }
}

/// Read a Messages API event stream to its end: text deltas go to `on_text`,
/// the stop reason decides the result.
pub fn read_stream(r: impl Read, on_text: &mut dyn FnMut(&str) -> bool) -> Result<Completion, Error> {
    let mut out = Completion::default();
    let mut stop: Option<String> = None;
    let mut refusal: Option<String> = None;
    let mut data = String::new();
    let mut lines = BufReader::new(r).lines();
    loop {
        let line = match lines.next() {
            Some(Ok(l)) => Some(l),
            Some(Err(e)) => return Err(Error::Network(e.to_string())),
            None => None,
        };
        match line.as_deref() {
            Some(l) if l.starts_with("data:") => {
                data.push_str(l["data:".len()..].trim_start());
                continue;
            }
            Some(l) if !l.is_empty() => continue, // `event:`, comments
            _ => {}
        }
        // Blank line or end of stream: dispatch the event.
        if !data.is_empty() {
            let v: Value = serde_json::from_str(&data).map_err(|e| Error::Protocol(e.to_string()))?;
            data.clear();
            match v["type"].as_str().unwrap_or("") {
                "message_start" => {
                    let u = &v["message"]["usage"];
                    out.input_tokens = u["input_tokens"].as_u64().unwrap_or(0)
                        + u["cache_read_input_tokens"].as_u64().unwrap_or(0)
                        + u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                }
                "content_block_delta" if v["delta"]["type"] == "text_delta" => {
                    let t = v["delta"]["text"].as_str().unwrap_or("");
                    out.text.push_str(t);
                    if !on_text(t) {
                        return Err(Error::Cancelled);
                    }
                }
                "message_delta" => {
                    if let Some(s) = v["delta"]["stop_reason"].as_str() {
                        stop = Some(s.to_string());
                    }
                    if let Some(c) = v["delta"]["stop_details"]["category"].as_str() {
                        refusal = Some(c.to_string());
                    }
                    if let Some(n) = v["usage"]["output_tokens"].as_u64() {
                        out.output_tokens = n;
                    }
                }
                "error" => {
                    let kind = v["error"]["type"].as_str().unwrap_or("");
                    let status = match kind {
                        "overloaded_error" => 529,
                        "rate_limit_error" => 429,
                        "api_error" => 500,
                        _ => 400,
                    };
                    let message = v["error"]["message"].as_str().unwrap_or(kind).to_string();
                    return Err(Error::Http { status, message });
                }
                _ => {} // ping, thinking deltas, block start/stop, fallback markers
            }
        }
        if line.is_none() {
            break;
        }
    }
    match stop.as_deref() {
        Some("end_turn" | "stop_sequence") => Ok(out),
        Some("max_tokens") => Err(Error::Truncated),
        Some("refusal") => Err(Error::Refused(refusal)),
        Some(other) => Err(Error::Protocol(format!("unexpected stop reason {other}"))),
        None => Err(Error::Network("stream ended early".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sse(events: &[&str]) -> String {
        events.iter().map(|e| format!("event: x\ndata: {e}\n\n")).collect()
    }

    #[test]
    fn stream_text_and_usage() {
        let s = sse(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":12}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}}"#,
            r#"{"type":"ping"}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hel"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"lo"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let mut seen = String::new();
        let c = read_stream(s.as_bytes(), &mut |t| {
            seen.push_str(t);
            true
        })
        .unwrap();
        assert_eq!((c.text.as_str(), seen.as_str(), c.input_tokens, c.output_tokens), ("Hello", "Hello", 12, 7));
    }

    #[test]
    fn stream_failures() {
        let refused =
            sse(&[r#"{"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":{"category":"cyber"}}}"#]);
        assert_eq!(read_stream(refused.as_bytes(), &mut |_| true), Err(Error::Refused(Some("cyber".into()))));
        let cut = sse(&[r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#]);
        assert_eq!(read_stream(cut.as_bytes(), &mut |_| true), Err(Error::Truncated));
        let err = sse(&[r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#]);
        assert!(matches!(read_stream(err.as_bytes(), &mut |_| true), Err(Error::Http { status: 529, .. })));
        let early = sse(&[r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"a"}}"#]);
        assert!(matches!(read_stream(early.as_bytes(), &mut |_| true), Err(Error::Network(_))));
        let text = sse(&[r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"a"}}"#]);
        assert_eq!(read_stream(text.as_bytes(), &mut |_| false), Err(Error::Cancelled));
    }

    #[test]
    fn request_body() {
        let c = Claude::with_api_key("k", DEFAULT_MODEL);
        let b =
            c.body(&Request { system: "s".into(), prompt: "p".into(), max_tokens: 100, effort: crate::Effort::Low });
        assert_eq!(b["model"], DEFAULT_MODEL);
        assert_eq!(b["stream"], true);
        assert_eq!(b["output_config"]["effort"], "low");
        assert_eq!(b["fallbacks"], "default");
        assert!(b.get("thinking").is_none(), "adaptive thinking is the default");
        assert_eq!(b["messages"][0]["content"], "p");
    }
}
