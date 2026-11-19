//! The `anthropic` provider against the local test server with recorded responses.

mod common;

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use common::server::{Response, Server};
use serde_json::{Value, json};
use shlv::ai::{Kind, Provider, ProviderConfig, Usage};
use shlv::choose::{Item, TagOption};

fn scripted(replies: Vec<Response>) -> Server {
    let q = Mutex::new(VecDeque::from(replies));
    Server::start(move |_| {
        q.lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Response::new(500).body("script ran out"))
    })
}

/// A Messages API reply: a thinking block, then `text`.
fn message(text: &str, stop: &str, usage: Value) -> Response {
    Response::json(&json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-5-5",
        "content": [
            {"type": "thinking", "thinking": "", "signature": "x"},
            {"type": "text", "text": text},
        ],
        "stop_reason": stop,
        "stop_details": null,
        "usage": usage,
    }))
}

fn ok(text: &str) -> Response {
    message(
        text,
        "end_turn",
        json!({"input_tokens": 40, "output_tokens": 12, "cache_creation_input_tokens": 900,
               "cache_read_input_tokens": 0}),
    )
}

fn api_error(status: u16, kind: &str, msg: &str) -> Response {
    Response::new(status)
        .header("content-type", "application/json")
        .body(json!({"type": "error", "error": {"type": kind, "message": msg}}).to_string())
}

fn config(server: &Server) -> ProviderConfig {
    // Safety: no test here reads this variable concurrently with a write.
    unsafe { std::env::set_var("SHLV_TEST_ANTHROPIC_KEY", "sk-ant-test") };
    ProviderConfig {
        kind: Kind::Anthropic,
        model: "claude-opus-5-5".into(),
        base_url: Some(server.url("")),
        api_key_env: Some("SHLV_TEST_ANTHROPIC_KEY".into()),
        api_key_file: None,
        api_key_cmd: None,
        timeout: Some(5),
        effort: Some("low".into()),
    }
}

fn provider(s: &Server) -> Provider {
    Provider::new("claude", &config(s)).unwrap()
}

fn item() -> Item {
    Item {
        url: "https://tokio.rs/tokio/tutorial".into(),
        title: Some("Tokio tutorial".into()),
        ..Default::default()
    }
}

fn list() -> Vec<TagOption> {
    ["rust", "async", "python"]
        .iter()
        .map(|n| TagOption {
            name: n.to_string(),
            ..Default::default()
        })
        .collect()
}

#[test]
fn sends_a_cached_structured_request() {
    let s = scripted(vec![ok(r#"{"tags": ["rust", "async", "go"]}"#)]);
    let p = provider(&s);
    let tags = p.choose_tags(&item(), &list()).unwrap();
    let names: Vec<_> = tags.iter().map(|t| t.tag.as_str()).collect();
    assert_eq!(names, ["rust", "async"]);

    let r = &s.requests()[0];
    assert_eq!(r.path, "/v1/messages");
    assert_eq!(r.header("x-api-key"), Some("sk-ant-test"));
    assert_eq!(r.header("anthropic-version"), Some("2026-10-21"));
    assert_eq!(r.header("authorization"), None);
    let b = r.json();
    assert_eq!(b["model"], "claude-opus-5-5");
    assert!(
        b.get("temperature").is_none(),
        "2026-10-21 rejects sampling parameters"
    );
    assert_eq!(
        b["system"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
    assert!(
        b["system"][0]["text"]
            .as_str()
            .unwrap()
            .ends_with("- rust\n- async\n- python")
    );
    assert_eq!(b["output_config"]["effort"], "low");
    let format = &b["output_config"]["format"];
    assert_eq!(format["type"], "json_schema");
    assert_eq!(
        format["schema"]["properties"]["tags"]["items"]["enum"],
        json!(["rust", "async", "python"])
    );
    assert_eq!(b["messages"].as_array().unwrap().len(), 1);
}

#[test]
fn counts_cache_hits() {
    let hit = json!({"input_tokens": 40, "output_tokens": 10, "cache_creation_input_tokens": 0,
                     "cache_read_input_tokens": 900});
    let s = scripted(vec![
        ok(r#"{"summary": "One."}"#),
        message(r#"{"summary": "Two."}"#, "end_turn", hit),
    ]);
    let p = provider(&s);
    p.summarize(&item()).unwrap();
    p.summarize(&item()).unwrap();
    assert_eq!(
        p.usage(),
        Usage {
            requests: 2,
            input: 80,
            output: 22,
            cache_read: 900,
            cache_write: 900
        }
    );
}

#[test]
fn retries_rate_limits_and_overload() {
    let s = scripted(vec![
        api_error(429, "rate_limit_error", "slow down").header("retry-after", "0"),
        api_error(529, "overloaded_error", "busy").header("retry-after", "0.2"),
        ok(r#"{"summary": "Done."}"#),
    ]);
    let t = Instant::now();
    assert_eq!(provider(&s).summarize(&item()).unwrap(), "Done.");
    assert!(t.elapsed() >= Duration::from_millis(200));
    assert_eq!(s.requests().len(), 3);

    // Gives up after three retries and says why.
    let busy = || api_error(529, "overloaded_error", "Overloaded").header("retry-after", "0");
    let s = scripted(vec![busy(), busy(), busy(), busy(), ok("{}")]);
    let e = format!("{:#}", provider(&s).summarize(&item()).unwrap_err());
    assert_eq!(
        e,
        "provider claude (summarize): HTTP 529 (overloaded; retried): Overloaded"
    );
    assert_eq!(s.requests().len(), 4);
}

#[test]
fn shows_api_errors() {
    let s = scripted(vec![api_error(
        401,
        "authentication_error",
        "invalid x-api-key",
    )]);
    let e = format!("{:#}", provider(&s).summarize(&item()).unwrap_err());
    assert_eq!(
        e,
        "provider claude (summarize): HTTP 401: invalid x-api-key"
    );
    assert_eq!(s.requests().len(), 1, "4xx is not retried");
}

#[test]
fn refusals_and_truncation_are_errors() {
    let refusal = Response::json(&json!({
        "type": "message", "role": "assistant", "content": [],
        "stop_reason": "refusal",
        "stop_details": {"type": "refusal", "category": "cyber", "explanation": "Not allowed."},
        "usage": {"input_tokens": 5, "output_tokens": 0},
    }));
    let cut = message(
        "{\"summ",
        "max_tokens",
        json!({"input_tokens": 5, "output_tokens": 8192}),
    );
    let s = scripted(vec![refusal, cut]);
    let p = provider(&s);
    let e = format!("{:#}", p.summarize(&item()).unwrap_err());
    assert!(
        e.ends_with("the model declined (cyber): Not allowed."),
        "{e}"
    );
    let e = format!("{:#}", p.summarize(&item()).unwrap_err());
    assert!(
        e.ends_with("the reply was cut off at max_tokens (8192)"),
        "{e}"
    );
    assert_eq!(
        s.requests().len(),
        2,
        "neither is retried as malformed output"
    );
}

#[test]
fn malformed_output_is_retried_with_feedback() {
    let s = scripted(vec![ok("Here you go"), ok(r#"{"tags": ["python"]}"#)]);
    provider(&s).choose_tags(&item(), &list()).unwrap();
    let m = s.requests()[1].json()["messages"].clone();
    assert_eq!(m[1]["role"], "assistant");
    assert_eq!(m[2]["role"], "user");
    assert!(
        m[2]["content"]
            .as_str()
            .unwrap()
            .contains("not valid: not JSON")
    );
}

#[test]
fn needs_a_key_and_checks_effort() {
    let s = scripted(vec![]);
    let mut c = config(&s);
    c.api_key_env = None;
    let e = format!("{:#}", Provider::new("claude", &c).err().unwrap());
    assert!(e.contains("needs an API key"), "{e}");
    let mut c = config(&s);
    c.effort = Some("extreme".into());
    let e = format!("{:#}", Provider::new("claude", &c).err().unwrap());
    assert!(
        e.contains("`effort` must be low, medium, high, xhigh, or max"),
        "{e}"
    );
}

/// Live, when `ANTHROPIC_API_KEY` is set:
/// `cargo test --test anthropic -- --ignored`. Costs a few cents at most.
#[test]
#[ignore]
fn live_anthropic() {
    assert!(
        std::env::var("ANTHROPIC_API_KEY").is_ok(),
        "set ANTHROPIC_API_KEY"
    );
    let c = ProviderConfig {
        kind: Kind::Anthropic,
        model: std::env::var("SHLV_TEST_ANTHROPIC_MODEL").unwrap_or("claude-opus-5-5".into()),
        base_url: None,
        api_key_env: Some("ANTHROPIC_API_KEY".into()),
        api_key_file: None,
        api_key_cmd: None,
        timeout: Some(120),
        effort: Some("low".into()),
    };
    let p = Provider::new("live", &c).unwrap();
    let tags = p.choose_tags(&item(), &list()).unwrap();
    assert!(tags.iter().any(|t| t.tag == "rust"), "{tags:?}");
    let summary = p
        .summarize_and_tag(&item(), &list())
        .unwrap()
        .summary
        .unwrap();
    eprintln!("summary: {summary}\nusage: {:?}", p.usage());
}
