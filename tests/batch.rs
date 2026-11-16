//! Message Batches for `summarize --batch` and `suggest --batch`, against the local test server
//! playing the Batches API (feature `ai`).
#![cfg(feature = "ai")]

mod common;

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use common::server::{Request, Response, Server};
use serde_json::{Value, json};

#[derive(Default)]
struct Api {
    /// Submitted batches' requests, by batch number.
    batches: Vec<Vec<Value>>,
    /// Status checks so far per batch; the first says "in progress".
    checks: Vec<usize>,
}

fn message(text: &str) -> Value {
    json!({"type": "message", "role": "assistant", "stop_reason": "end_turn",
           "content": [{"type": "text", "text": text}],
           "usage": {"input_tokens": 50, "output_tokens": 5}})
}

/// The result line for one request, decided by the bookmark's note.
fn result(req: &Value) -> Value {
    let id = req["custom_id"].as_str().unwrap();
    let user = req["params"]["messages"][0]["content"].as_str().unwrap();
    let r = if user.contains("EXPIRE") {
        json!({"type": "expired"})
    } else if user.contains("ERR") {
        json!({"type": "errored", "error": {"type": "error",
               "error": {"type": "invalid_request_error", "message": "bad request"}}})
    } else {
        let mut out = serde_json::Map::new();
        if id.ends_with('s') || id.ends_with("st") {
            out.insert("summary".into(), json!("A batched summary."));
        }
        if id.ends_with('t') {
            out.insert("tags".into(), json!(["rust"]));
        }
        json!({"type": "succeeded", "message": message(&Value::Object(out).to_string())})
    };
    json!({"custom_id": id, "result": r})
}

fn handle(api: &Mutex<Api>, base: &str, r: &Request) -> Response {
    let path = r.path.split('?').next().unwrap();
    let mut api = api.lock().unwrap();
    if path.starts_with("/p/") {
        return Response::html(format!(
            "<main><p>Tokio is an asynchronous runtime for Rust. {}</p></main>",
            "Words of the page. ".repeat(10)
        ));
    }
    if r.method == "POST" && path == "/v1/messages/batches" {
        assert_eq!(r.header("x-api-key"), Some("sk-ant-test"));
        let reqs = r.json()["requests"].as_array().unwrap().clone();
        api.batches.push(reqs);
        api.checks.push(0);
        let n = api.batches.len();
        return Response::json(
            &json!({"id": format!("msgbatch_{n}"), "type": "message_batch",
                                      "processing_status": "in_progress"}),
        );
    }
    if let Some(n) = path.strip_prefix("/v1/messages/batches/msgbatch_") {
        let i: usize = n.parse::<usize>().unwrap() - 1;
        api.checks[i] += 1;
        let total = api.batches[i].len() as u64;
        if api.checks[i] == 1 {
            return Response::json(&json!({"id": format!("msgbatch_{n}"),
                "processing_status": "in_progress",
                "request_counts": {"processing": total, "succeeded": 0, "errored": 0,
                                   "canceled": 0, "expired": 0},
                "results_url": null}));
        }
        let results: Vec<Value> = api.batches[i].iter().map(result).collect();
        let count = |t: &str| results.iter().filter(|r| r["result"]["type"] == t).count();
        return Response::json(&json!({"id": format!("msgbatch_{n}"),
            "processing_status": "ended",
            "request_counts": {"processing": 0, "succeeded": count("succeeded"),
                               "errored": count("errored"), "canceled": 0,
                               "expired": count("expired")},
            "results_url": format!("{base}/results/{n}")}));
    }
    if let Some(n) = path.strip_prefix("/results/") {
        let i: usize = n.parse::<usize>().unwrap() - 1;
        let body: Vec<String> = api.batches[i]
            .iter()
            .map(|r| result(r).to_string())
            .collect();
        return Response::new(200).body(body.join("\n"));
    }
    Response::new(404)
}

struct Fixture {
    server: Server,
    api: Arc<Mutex<Api>>,
    dir: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let api: Arc<Mutex<Api>> = Arc::default();
        let base: Arc<Mutex<String>> = Arc::default();
        let (a, b) = (api.clone(), base.clone());
        let server = Server::start(move |r| handle(&a, &b.lock().unwrap(), r));
        *base.lock().unwrap() = server.url("");
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "shlv-batch-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            format!(
                "[ai]\nsummarize = \"claude\"\ntag = \"claude\"\n\n\
                 [tags.canonical]\nrust = []\npython = []\n\n\
                 [provider.claude]\nkind = \"anthropic\"\nmodel = \"claude-opus-5-5\"\n\
                 base_url = \"{}\"\napi_key_env = \"SHLV_TEST_KEY\"\n\n\
                 [provider.local]\nkind = \"openai-compatible\"\nbase_url = \"{}\"\nmodel = \"m\"\n",
                server.url(""),
                server.url("/v1")
            ),
        )
        .unwrap();
        Fixture { server, api, dir }
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let o = Command::new(env!("CARGO_BIN_EXE_shlv"))
            .args(args)
            .env("SHLV_DB", self.dir.join("shlv.db"))
            .env("SHLV_CONFIG", self.dir.join("config.toml"))
            .env("SHLV_TEST_KEY", "sk-ant-test")
            .output()
            .unwrap();
        (
            o.status.code().unwrap_or(-1),
            String::from_utf8(o.stdout).unwrap(),
            String::from_utf8(o.stderr).unwrap(),
        )
    }

    fn ok(&self, args: &[&str]) -> String {
        let (code, out, err) = self.run(args);
        assert_eq!(code, 0, "shlv {args:?}: {err}");
        out
    }

    fn summary(&self, id: &str) -> Value {
        let v: Value = serde_json::from_str(&self.ok(&["show", id, "--json"])).unwrap();
        v["summary"].clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn summaries_by_batch() {
    let f = Fixture::new();
    f.ok(&["add", &f.server.url("/p/a")]);
    f.ok(&["add", &f.server.url("/p/b"), "--note", "ERR"]);
    f.ok(&["add", &f.server.url("/p/c"), "--note", "EXPIRE"]);
    let out = f.ok(&["summarize", "--all", "--batch"]);
    assert_eq!(
        out,
        "submitted batch msgbatch_1 (3 requests); collect the results with `shlv batch collect`\n"
    );
    let reqs = f.api.lock().unwrap().batches[0].clone();
    let ids: Vec<&str> = reqs
        .iter()
        .map(|r| r["custom_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["b3-s", "b2-s", "b1-s"]);
    let params = &reqs[0]["params"];
    assert_eq!(params["model"], "claude-opus-5-5");
    assert_eq!(params["output_config"]["format"]["type"], "json_schema");
    assert!(
        params["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("Page text:")
    );

    // Not ended yet: status and collect report progress, change nothing.
    assert!(
        f.ok(&["batch", "status"])
            .starts_with("msgbatch_1  claude (3 requests): in progress")
    );
    // (`status` used the first check; this collect sees it ended.)
    let (code, out, err) = f.run(&["batch", "collect"]);
    assert_eq!(code, 1, "partial failures fail the command: {err}");
    assert_eq!(
        out,
        "collected batch msgbatch_1: 1 summaries, 0 tag suggestions, 2 failed\n"
    );
    assert!(err.contains("bookmark 2: errored: bad request"), "{err}");
    assert!(err.contains("bookmark 3: expired before it ran"), "{err}");
    assert!(err.contains("to retry the failed ones live"), "{err}");
    assert_eq!(f.summary("1"), "A batched summary.");
    assert!(f.summary("2").is_null() && f.summary("3").is_null());
    assert_eq!(f.ok(&["batch", "collect"]), "no batches waiting\n");
}

#[test]
fn tag_suggestions_by_batch() {
    let f = Fixture::new();
    f.ok(&["add", &f.server.url("/p/a")]);
    f.ok(&["add", &f.server.url("/p/b"), "-t", "python"]);
    let out = f.ok(&["suggest", "1", "2", "--batch"]);
    assert!(
        out.contains("submitted batch msgbatch_1 (2 requests)"),
        "{out}"
    );
    let reqs = f.api.lock().unwrap().batches[0].clone();
    // Same provider for both tasks and no summary yet: one request asks for both.
    assert_eq!(reqs[0]["custom_id"], "b1-st");
    let (code, out, _) = f.run(&["batch", "collect"]);
    assert_eq!(code, 0);
    assert!(out.contains("in progress"), "{out}");
    let out = f.ok(&["batch", "collect"]);
    assert_eq!(
        out,
        "collected batch msgbatch_1: 2 summaries, 2 tag suggestions, 0 failed\n"
    );
    let pending = f.ok(&["suggest", "ls"]);
    assert!(pending.contains("#rust (llm)"), "{pending}");
}

#[test]
fn other_kinds_refuse_batches() {
    let f = Fixture::new();
    let cfg = std::fs::read_to_string(f.dir.join("config.toml")).unwrap();
    std::fs::write(
        f.dir.join("config.toml"),
        cfg.replace("summarize = \"claude\"", "summarize = \"local\""),
    )
    .unwrap();
    f.ok(&["add", &f.server.url("/p/a")]);
    let (code, _, err) = f.run(&["summarize", "1", "--batch"]);
    assert_eq!(code, 1);
    assert!(
        err.contains("--batch needs a provider of kind \"anthropic\" (the Message Batches API); local isn't one"),
        "{err}"
    );
    assert!(f.api.lock().unwrap().batches.is_empty());
}
