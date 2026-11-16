//! `kind = "anthropic"`: the Claude Messages API over raw HTTPS (no SDK for Rust).
//!
//! - `POST {base_url}/v1/messages` with `x-api-key` and `anthropic-version`.
//! - Structured output through `output_config.format` (`json_schema`); tags are an `enum` of the
//!   canonical list, so the model can't invent one.
//! - The system prompt (instructions plus the canonical list) is the stable prefix and carries
//!   `cache_control`, so bulk runs read it from the cache. It only caches above the model's
//!   minimum (512 tokens on current models, 1024 to 4096 on older ones).
//! - 429 and 529 are retried with backoff that honors `retry-after`; refusals and `max_tokens`
//!   stops are errors, not malformed-output retries.

use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use super::openai::{agent, transport_error};
use super::{Llm, ProviderConfig, Request, Usage, error_message};

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const API_VERSION: &str = "2026-10-21";

/// Room for adaptive thinking plus a short JSON reply.
const MAX_TOKENS: u32 = 8192;

/// Retries of 429/529 responses, and the longest wait honored between them.
const RETRIES: u32 = 3;
const MAX_WAIT: Duration = Duration::from_secs(60);

pub struct Anthropic {
    agent: ureq::Agent,
    url: String,
    model: String,
    key: String,
    effort: Option<String>,
    usage: Mutex<Usage>,
}

impl Anthropic {
    pub fn new(config: &ProviderConfig, key: String) -> Self {
        let base = config.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL);
        Anthropic {
            agent: agent(config),
            url: format!("{}/v1/messages", base.trim_end_matches('/')),
            model: config.model.clone(),
            key,
            effort: config.effort.clone(),
            usage: Mutex::new(Usage::default()),
        }
    }

    pub fn body(&self, req: &Request) -> Value {
        let mut messages = vec![json!({"role": "user", "content": req.user})];
        if let Some((prev, why)) = &req.retry {
            messages.push(json!({"role": "assistant", "content": prev}));
            messages.push(json!({
                "role": "user",
                "content": format!("That reply was not valid: {why}. Reply again with only the \
                                    JSON object."),
            }));
        }
        let mut output = json!({"format": {"type": "json_schema", "schema": req.schema}});
        if let Some(e) = &self.effort {
            output["effort"] = json!(e);
        }
        json!({
            "model": self.model,
            "max_tokens": MAX_TOKENS,
            "system": [{
                "type": "text",
                "text": req.system,
                "cache_control": {"type": "ephemeral"},
            }],
            "messages": messages,
            "output_config": output,
        })
    }

    fn post(&self, body: &str) -> Result<(u16, Option<String>, String)> {
        let mut resp = self
            .agent
            .post(&self.url)
            .header("content-type", "application/json")
            .header("x-api-key", &self.key)
            .header("anthropic-version", API_VERSION)
            .send(body)
            .map_err(transport_error)?;
        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let text = resp
            .body_mut()
            .with_config()
            .limit(4 << 20)
            .read_to_string()
            .map_err(transport_error)?;
        Ok((status, retry_after, text))
    }
}

impl Llm for Anthropic {
    fn complete(&self, req: &Request) -> Result<String> {
        let body = self.body(req).to_string();
        let mut attempt = 0;
        let (status, text) = loop {
            let (status, retry_after, text) = self.post(&body)?;
            if matches!(status, 429 | 529) && attempt < RETRIES {
                std::thread::sleep(backoff(attempt, retry_after.as_deref()));
                attempt += 1;
                continue;
            }
            break (status, text);
        };
        if !(200..300).contains(&status) {
            let what = match status {
                429 => " (rate limited; retried)",
                529 => " (overloaded; retried)",
                _ => "",
            };
            bail!("HTTP {status}{what}: {}", error_message(&text));
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| anyhow!("the API returned a response that isn't JSON: {e}"))?;
        if let Some(u) = v.get("usage") {
            let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
            let mut usage = self.usage.lock().unwrap();
            usage.requests += 1;
            usage.input += n("input_tokens");
            usage.output += n("output_tokens");
            usage.cache_read += n("cache_read_input_tokens");
            usage.cache_write += n("cache_creation_input_tokens");
        }
        match v.get("stop_reason").and_then(Value::as_str) {
            Some("refusal") => {
                let d = v.get("stop_details");
                let cat = d
                    .and_then(|d| d.get("category"))
                    .and_then(Value::as_str)
                    .map(|c| format!(" ({c})"))
                    .unwrap_or_default();
                let why = d
                    .and_then(|d| d.get("explanation"))
                    .and_then(Value::as_str)
                    .map(|e| format!(": {e}"))
                    .unwrap_or_default();
                bail!("the model declined{cat}{why}");
            }
            Some("max_tokens") => bail!("the reply was cut off at max_tokens ({MAX_TOKENS})"),
            _ => {}
        }
        let text: String = v
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect();
        if text.is_empty() {
            bail!("the reply has no text");
        }
        Ok(text)
    }

    fn usage(&self) -> Usage {
        *self.usage.lock().unwrap()
    }
}

/// How long to wait before retry `attempt` (0-based): `retry-after` seconds when the API sends
/// it, else 1, 2, 4 s; never more than a minute.
fn backoff(attempt: u32, retry_after: Option<&str>) -> Duration {
    let d = retry_after
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|s| s.is_finite() && *s >= 0.0)
        .map(Duration::from_secs_f64)
        .unwrap_or(Duration::from_secs(1 << attempt));
    d.min(MAX_WAIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backs_off() {
        assert_eq!(backoff(0, None), Duration::from_secs(1));
        assert_eq!(backoff(2, None), Duration::from_secs(4));
        assert_eq!(backoff(0, Some("7")), Duration::from_secs(7));
        assert_eq!(backoff(0, Some("0.5")), Duration::from_millis(500));
        assert_eq!(backoff(0, Some("9999")), MAX_WAIT);
        assert_eq!(backoff(1, Some("soon")), Duration::from_secs(2));
    }
}
