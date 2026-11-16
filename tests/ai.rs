//! AI providers against the local test server with recorded responses (feature `ai`).
#![cfg(feature = "ai")]

mod common;

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use common::server::{Request, Response, Server};
use serde_json::{Value, json};
use shlv::ai::{Kind, Provider, ProviderConfig};
use shlv::choose::{Choice, Item, TagOption};

/// A server that answers with `replies` in order (then 500s).
fn scripted(replies: Vec<Response>) -> Server {
    let q = Mutex::new(VecDeque::from(replies));
    Server::start(move |_| {
        q.lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Response::new(500).body("script ran out"))
    })
}

/// A chat-completions reply whose message content is `content`.
fn chat(content: &str) -> Response {
    Response::json(&json!({
        "id": "x",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": {"role": "assistant", "content": content},
        }],
    }))
}

fn config(server: &Server) -> ProviderConfig {
    ProviderConfig {
        kind: Kind::OpenaiCompatible,
        model: "test-model".into(),
        base_url: Some(server.url("/v1")),
        api_key_env: None,
        api_key_file: None,
        effort: None,
        timeout: Some(5),
    }
}

fn provider(server: &Server) -> Provider {
    Provider::new("local", &config(server)).unwrap()
}

fn item() -> Item {
    Item {
        url: "https://tokio.rs/tokio/tutorial".into(),
        title: Some("Tokio tutorial".into()),
        note: Some("async intro".into()),
        summary: None,
        text: Some("Tokio is an asynchronous runtime for Rust.".into()),
    }
}

fn list() -> Vec<TagOption> {
    let t = |name: &str, aliases: &[&str], d: Option<&str>| TagOption {
        name: name.into(),
        aliases: aliases.iter().map(|a| a.to_string()).collect(),
        description: d.map(str::to_string),
    };
    vec![
        t("rust", &["rs"], Some("the Rust language")),
        t("async", &[], None),
        t("python", &["py"], None),
    ]
}

fn names(c: &[Choice]) -> Vec<&str> {
    c.iter().map(|c| c.tag.as_str()).collect()
}

fn messages(r: &Request) -> Vec<Value> {
    r.json()["messages"].as_array().unwrap().clone()
}

#[test]
fn chooses_tags_with_a_schema() {
    let s = scripted(vec![chat(r#"{"tags": ["rust", "rs", "Async", "golang"]}"#)]);
    let tags = provider(&s).choose_tags(&item(), &list()).unwrap();
    // Aliases resolve, duplicates fold, unknown names drop.
    assert_eq!(names(&tags), ["rust", "async"]);
    assert!(tags.iter().all(|t| t.confidence.is_none()));

    let reqs = s.requests();
    assert_eq!(reqs.len(), 1);
    let r = &reqs[0];
    assert_eq!(r.path, "/v1/chat/completions");
    assert_eq!(r.header("authorization"), None);
    let body = r.json();
    assert_eq!(body["model"], "test-model");
    let format = &body["response_format"];
    assert_eq!(format["type"], "json_schema");
    assert_eq!(
        format["json_schema"]["schema"]["properties"]["tags"]["items"]["enum"],
        json!(["rust", "async", "python"])
    );
    let m = messages(r);
    let system = m[0]["content"].as_str().unwrap();
    assert!(system.contains("- rust: the Rust language (also: rs)\n- async\n- python (also: py)"));
    let user = m[1]["content"].as_str().unwrap();
    assert_eq!(
        user,
        "URL: https://tokio.rs/tokio/tutorial\nTitle: Tokio tutorial\nNote: async intro\n\n\
         Page text:\nTokio is an asynchronous runtime for Rust."
    );
}

#[test]
fn summarizes() {
    let s = scripted(vec![
        chat(r#"{"summary": "  The Tokio tutorial.\n Async Rust. "}"#),
        chat("```json\n{\"summary\": \"Both.\", \"tags\": [\"py\"]}\n```"),
    ]);
    let p = provider(&s);
    assert_eq!(
        p.summarize(&item()).unwrap(),
        "The Tokio tutorial. Async Rust."
    );
    let a = p.summarize_and_tag(&item(), &list()).unwrap();
    assert_eq!(a.summary.as_deref(), Some("Both."));
    assert_eq!(names(&a.tags), ["python"]);
    let both = s.requests()[1].json();
    let props = &both["response_format"]["json_schema"]["schema"]["properties"];
    assert!(props["summary"].is_object() && props["tags"].is_object());
}

#[test]
fn falls_back_to_json_mode_when_the_schema_is_rejected() {
    let s = scripted(vec![
        Response::new(400)
            .body(r#"{"error": {"message": "response_format json_schema unsupported"}}"#),
        chat(r#"{"tags": ["rust"]}"#),
        chat(r#"{"tags": ["async"]}"#),
    ]);
    let p = provider(&s);
    assert_eq!(names(&p.choose_tags(&item(), &list()).unwrap()), ["rust"]);
    // The next call goes straight to JSON mode.
    assert_eq!(names(&p.choose_tags(&item(), &list()).unwrap()), ["async"]);
    let reqs = s.requests();
    assert_eq!(reqs.len(), 3);
    for r in &reqs[1..] {
        let body = r.json();
        assert_eq!(body["response_format"], json!({"type": "json_object"}));
        let system = messages(r)[0]["content"].as_str().unwrap().to_string();
        assert!(system.contains("matching this JSON Schema"), "{system}");
        assert!(
            system.contains(r#""enum":["rust","async","python"]"#),
            "{system}"
        );
    }
}

#[test]
fn retries_malformed_output_once() {
    let s = scripted(vec![
        chat("Sure! Here are the tags: rust"),
        chat(r#"{"tags": ["rust"]}"#),
    ]);
    assert_eq!(
        names(&provider(&s).choose_tags(&item(), &list()).unwrap()),
        ["rust"]
    );
    let m = messages(&s.requests()[1]);
    assert_eq!(m.len(), 4);
    assert_eq!(m[2]["content"], "Sure! Here are the tags: rust");
    assert!(
        m[3]["content"]
            .as_str()
            .unwrap()
            .starts_with("That reply was not valid: not JSON")
    );

    let s = scripted(vec![chat("nope"), chat(r#"{"summary": ""}"#)]);
    let e = format!("{:#}", provider(&s).summarize(&item()).unwrap_err());
    assert_eq!(
        e,
        "provider local (summarize): invalid reply twice: the summary is empty"
    );
    assert_eq!(s.requests().len(), 2);

    let long = json!({"summary": "x".repeat(1001)}).to_string();
    let s = scripted(vec![chat(&long), chat(&long)]);
    let e = format!("{:#}", provider(&s).summarize(&item()).unwrap_err());
    assert!(e.contains("1001 characters; at most 1000"), "{e}");
}

#[test]
fn http_errors_name_provider_and_task() {
    let s = scripted(vec![
        Response::new(500).body(r#"{"error": {"message": "model crashed"}}"#),
        Response::new(401).body(r#"{"error": "invalid api key"}"#),
    ]);
    let p = provider(&s);
    let e = format!("{:#}", p.choose_tags(&item(), &list()).unwrap_err());
    assert_eq!(
        e,
        format!(
            "provider local (tag): {} returned HTTP 500: model crashed",
            s.url("/v1/chat/completions")
        )
    );
    let e = format!("{:#}", p.summarize(&item()).unwrap_err());
    assert!(e.starts_with("provider local (summarize): "), "{e}");
    assert!(e.ends_with("HTTP 401: invalid api key"), "{e}");
}

#[test]
fn truncation_and_refusal_are_errors_not_retries() {
    let cut = Response::json(&json!({"choices": [{
        "finish_reason": "length",
        "message": {"role": "assistant", "content": "{\"summ"},
    }]}));
    let refused = Response::json(&json!({"choices": [{
        "finish_reason": "stop",
        "message": {"role": "assistant", "content": null, "refusal": "I can't help with that."},
    }]}));
    let s = scripted(vec![cut, refused]);
    let p = provider(&s);
    let e = format!("{:#}", p.summarize(&item()).unwrap_err());
    assert!(
        e.ends_with("the reply was cut off at the model's output limit"),
        "{e}"
    );
    let e = format!("{:#}", p.summarize(&item()).unwrap_err());
    assert!(
        e.ends_with("the model refused: I can't help with that."),
        "{e}"
    );
    assert_eq!(s.requests().len(), 2);
}

#[test]
fn times_out() {
    let s = scripted(vec![chat(r#"{"tags": []}"#).delay(Duration::from_secs(4))]);
    let mut c = config(&s);
    c.timeout = Some(1);
    let t = Instant::now();
    let e = format!(
        "{:#}",
        Provider::new("slow", &c)
            .unwrap()
            .choose_tags(&item(), &list())
            .unwrap_err()
    );
    assert_eq!(e, "provider slow (tag): timed out waiting for the reply");
    assert!(t.elapsed() < Duration::from_secs(3));
}

#[test]
fn api_keys_come_from_the_environment() {
    let s = scripted(vec![chat(r#"{"tags": []}"#)]);
    let mut c = config(&s);
    c.api_key_env = Some("SHLV_TEST_KEY_SET".into());
    // Safety: tests in this file don't read this variable concurrently.
    unsafe { std::env::set_var("SHLV_TEST_KEY_SET", "sk-test\n") };
    Provider::new("remote", &c)
        .unwrap()
        .choose_tags(&item(), &list())
        .unwrap();
    assert_eq!(
        s.requests()[0].header("authorization"),
        Some("Bearer sk-test")
    );
    c.api_key_env = Some("SHLV_TEST_KEY_UNSET".into());
    let e = format!("{:#}", Provider::new("remote", &c).err().unwrap());
    assert_eq!(
        e,
        "provider remote: environment variable SHLV_TEST_KEY_UNSET (api_key_env) is not set"
    );
}

#[test]
fn empty_canonical_list_is_an_error() {
    let s = scripted(vec![]);
    let e = format!("{:#}", provider(&s).choose_tags(&item(), &[]).unwrap_err());
    assert!(e.contains("the canonical tag list is empty"), "{e}");
    assert!(s.requests().is_empty());
}

/// Against a real local server: `SHLV_TEST_OPENAI_URL=http://localhost:8080/v1
/// SHLV_TEST_OPENAI_MODEL=... cargo test --all-features --test ai -- --ignored`.
#[test]
#[ignore]
fn live_openai_compatible() {
    let (Ok(url), Ok(model)) = (
        std::env::var("SHLV_TEST_OPENAI_URL"),
        std::env::var("SHLV_TEST_OPENAI_MODEL"),
    ) else {
        panic!("set SHLV_TEST_OPENAI_URL and SHLV_TEST_OPENAI_MODEL");
    };
    let c = ProviderConfig {
        kind: Kind::OpenaiCompatible,
        model,
        base_url: Some(url),
        api_key_env: std::env::var("SHLV_TEST_OPENAI_KEY_ENV").ok(),
        api_key_file: None,
        effort: None,
        timeout: Some(300),
    };
    let p = Provider::new("live", &c).unwrap();
    let t = Instant::now();
    let tags = p.choose_tags(&item(), &list()).unwrap();
    eprintln!("tags {:?} in {:?}", names(&tags), t.elapsed());
    assert!(names(&tags).contains(&"rust"), "{tags:?}");
    let t = Instant::now();
    let summary = p.summarize(&item()).unwrap();
    eprintln!("summary {summary:?} in {:?}", t.elapsed());
    assert!(!summary.is_empty());
}

#[test]
fn config_show_prints_providers() {
    let dir = std::env::temp_dir().join(format!("shlv-ai-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        "[ai]\ntag = \"local\"\n\n[provider.local]\nkind = \"openai-compatible\"\n\
         base_url = \"http://localhost:8080/v1\"\nmodel = \"m\"\napi_key_env = \"MY_KEY\"\n",
    )
    .unwrap();
    let o = std::process::Command::new(env!("CARGO_BIN_EXE_shlv"))
        .args(["config", "show"])
        .env("SHLV_CONFIG", &path)
        .env("MY_KEY", "sk-secret")
        .output()
        .unwrap();
    let out = String::from_utf8(o.stdout).unwrap();
    assert!(o.status.success());
    assert!(out.contains("[ai]\ntag = \"local\""), "{out}");
    assert!(out.contains("[provider.local]"), "{out}");
    assert!(out.contains("api_key_env = \"MY_KEY\""), "{out}");
    assert!(
        !out.contains("sk-secret") && !out.contains("ignored"),
        "{out}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn api_keys_from_a_file() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("shlv-keyfile-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let key = dir.join("anthropic");
    std::fs::write(&key, "sk-from-file\n").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();

    let s = scripted(vec![chat(r#"{"tags": []}"#)]);
    let mut c = config(&s);
    c.api_key_file = Some(key.display().to_string());
    Provider::new("secret", &c)
        .unwrap()
        .choose_tags(&item(), &list())
        .unwrap();
    assert_eq!(
        s.requests()[0].header("authorization"),
        Some("Bearer sk-from-file")
    );

    c.api_key_file = Some(dir.join("missing").display().to_string());
    let e = format!("{:#}", Provider::new("secret", &c).err().unwrap());
    assert!(
        e.starts_with(&format!(
            "provider secret: cannot read api_key_file {}",
            dir.join("missing").display()
        )),
        "{e}"
    );
    std::fs::write(&key, "\n").unwrap();
    c.api_key_file = Some(key.display().to_string());
    let e = format!("{:#}", Provider::new("secret", &c).err().unwrap());
    assert!(e.ends_with("is empty"), "{e}");

    c.api_key_env = Some("X".into());
    let e = format!("{:#}", Provider::new("secret", &c).err().unwrap());
    assert!(
        e.contains("set `api_key_env` or `api_key_file`, not both"),
        "{e}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn proposes_new_names_alongside_list_tags() {
    use shlv::ai::Proposing;
    let s = scripted(vec![
        chat(
            r#"{"tags": ["rust"], "new_tags": [
                {"name": "Tokio", "description": " the Tokio\n runtime "},
                {"name": "py", "description": "an alias: a list tag after all"},
                "runtime",
                {"name": "x", "description": "over the cap"},
                {"description": "no name"}]}"#,
        ),
        // JSON mode may leave new_tags out: none.
        chat(r#"{"tags": ["async"]}"#),
        // Not an array: invalid, retried.
        chat(r#"{"tags": [], "new_tags": "tokio"}"#),
        chat(r#"{"tags": [], "new_tags": []}"#),
        // Only new names (a request sent while the list was empty): fine.
        chat(r#"{"new_tags": [{"name": "tokio", "description": ""}]}"#),
    ]);
    let mut p = provider(&s);
    p.set_proposing(Proposing {
        max: 2,
        known: vec!["tokio".into(), "wifi".into()],
    });
    let a = p.tag(&item(), &list()).unwrap();
    assert_eq!(names(&a.tags), ["rust", "python"]);
    let new: Vec<(&str, Option<&str>)> = a
        .proposed
        .iter()
        .map(|t| (t.name.as_str(), t.description.as_deref()))
        .collect();
    assert_eq!(
        new,
        [("tokio", Some("the Tokio runtime")), ("runtime", None)]
    );

    let body = s.requests()[0].json();
    let schema = &body["response_format"]["json_schema"]["schema"];
    assert_eq!(schema["required"], json!(["new_tags", "tags"]));
    assert_eq!(
        schema["properties"]["new_tags"]["items"]["required"],
        json!(["name", "description"])
    );
    let system = messages(&s.requests()[0])[0]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        system.contains("propose up to 2 new tags for the list"),
        "{system}"
    );
    assert!(
        system.contains("- python (also: py)\n\nNames already proposed"),
        "{system}"
    );
    assert!(system.ends_with("when it fits: tokio, wifi"), "{system}");

    let a = p.tag(&item(), &list()).unwrap();
    assert_eq!((names(&a.tags), a.proposed.len()), (vec!["async"], 0));
    let a = p.tag(&item(), &list()).unwrap();
    assert!(a.tags.is_empty() && a.proposed.is_empty());
    let retry = messages(&s.requests()[3]);
    assert!(
        retry[3]["content"]
            .as_str()
            .unwrap()
            .contains("\"new_tags\" is not an array"),
        "{retry:?}"
    );
    let a = p.tag(&item(), &list()).unwrap();
    assert_eq!((a.tags.len(), a.proposed[0].name.as_str()), (0, "tokio"));

    // Without proposing, an empty list is still an error.
    p.set_proposing(Proposing::default());
    assert!(p.tag(&item(), &[]).is_err());
}
