//! `kind = "openai-compatible"`: the chat-completions API that llama.cpp, Ollama, vLLM, LM Studio,
//! and many hosted services speak.
//!
//! Structured output goes in `response_format` as a JSON schema (tags constrained by an `enum`).
//! Servers that reject that get JSON mode (`{"type": "json_object"}`) with the schema in the
//! prompt instead; once one rejects it, this provider stops trying for the rest of the run.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use super::{Llm, ProviderConfig, Request, Usage, error_message};

pub struct OpenAi {
    agent: ureq::Agent,
    url: String,
    model: String,
    key: Option<String>,
    /// Cleared when the server rejects `json_schema`.
    schema_ok: AtomicBool,
    usage: Mutex<Usage>,
}

impl OpenAi {
    pub fn new(config: &ProviderConfig, key: Option<String>) -> Self {
        let base = config.base_url.as_deref().unwrap_or_default();
        OpenAi {
            agent: agent(config),
            url: format!("{}/chat/completions", base.trim_end_matches('/')),
            model: config.model.clone(),
            key,
            schema_ok: AtomicBool::new(true),
            usage: Mutex::new(Usage::default()),
        }
    }

    fn body(&self, req: &Request, schema: bool) -> Value {
        let mut system = req.system.clone();
        if !schema {
            system.push_str(&format!(
                "\n\nThe reply must be a JSON object matching this JSON Schema:\n{}",
                req.schema
            ));
        }
        let mut messages = vec![
            json!({"role": "system", "content": system}),
            json!({"role": "user", "content": req.user}),
        ];
        if let Some((prev, why)) = &req.retry {
            messages.push(json!({"role": "assistant", "content": prev}));
            messages.push(json!({
                "role": "user",
                "content": format!("That reply was not valid: {why}. Reply again with only the \
                                    JSON object."),
            }));
        }
        let format = if schema {
            json!({
                "type": "json_schema",
                "json_schema": {"name": req.schema_name, "strict": true, "schema": req.schema},
            })
        } else {
            json!({"type": "json_object"})
        };
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "temperature": 0,
            "response_format": format,
        });
        if let Some(n) = req.max_tokens {
            body["max_tokens"] = json!(n);
        }
        body
    }

    /// POSTs `body`; returns the status and response body.
    fn post(&self, body: &Value) -> Result<(u16, String)> {
        let mut r = self
            .agent
            .post(&self.url)
            .header("content-type", "application/json");
        if let Some(k) = &self.key {
            r = r.header("authorization", &format!("Bearer {k}"));
        }
        let mut resp = r.send(body.to_string()).map_err(transport_error)?;
        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .with_config()
            .limit(4 << 20)
            .read_to_string()
            .map_err(transport_error)?;
        Ok((status, text))
    }
}

impl Llm for OpenAi {
    fn complete(&self, req: &Request) -> Result<String> {
        let schema = self.schema_ok.load(Ordering::Relaxed);
        let (mut status, mut text) = self.post(&self.body(req, schema))?;
        if schema && matches!(status, 400 | 422) {
            // Most likely `json_schema` isn't supported; JSON mode is the fallback.
            self.schema_ok.store(false, Ordering::Relaxed);
            (status, text) = self.post(&self.body(req, false))?;
        }
        if !(200..300).contains(&status) {
            bail!(
                "{} returned HTTP {status}: {}",
                self.url,
                error_message(&text)
            );
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| anyhow!("{} returned a response that isn't JSON: {e}", self.url))?;
        if let Some(u) = v.get("usage") {
            let n = |p: &str| u.pointer(p).and_then(Value::as_u64).unwrap_or(0);
            let cached = n("/prompt_tokens_details/cached_tokens");
            let mut usage = self.usage.lock().unwrap();
            usage.requests += 1;
            usage.input += n("/prompt_tokens").saturating_sub(cached);
            usage.cache_read += cached;
            usage.output += n("/completion_tokens");
        }
        let choice = v
            .pointer("/choices/0")
            .ok_or_else(|| anyhow!("{} returned no choices", self.url))?;
        if choice.get("finish_reason").and_then(Value::as_str) == Some("length") {
            bail!("the reply was cut off at the model's output limit");
        }
        let msg = choice.get("message");
        if let Some(r) = msg.and_then(|m| m.get("refusal")).and_then(Value::as_str) {
            bail!("the model refused: {r}");
        }
        msg.and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("{} returned no message content", self.url))
    }

    fn usage(&self) -> Usage {
        *self.usage.lock().unwrap()
    }
}

/// An agent for provider requests: the provider's timeout, status codes returned rather than
/// raised (so error bodies can be read).
pub(crate) fn agent(config: &ProviderConfig) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(std::time::Duration::from_secs(10)))
        .timeout_global(Some(config.timeout()))
        .http_status_as_error(false)
        .user_agent(crate::fetch::USER_AGENT)
        .tls_config(crate::fetch::tls())
        .build()
        .new_agent()
}

pub(crate) fn transport_error(e: ureq::Error) -> anyhow::Error {
    match crate::fetch::FetchError::from(e) {
        crate::fetch::FetchError::Timeout => anyhow!("timed out waiting for the reply"),
        e => anyhow!("{e}"),
    }
}
