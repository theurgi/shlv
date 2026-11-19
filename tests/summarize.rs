//! `shlv summarize` against the local test server: pages and a recorded openai-compatible
//! provider on the same port.

mod common;

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use common::server::{Response, Server};
use serde_json::{Value, json};

fn words(n: usize) -> String {
    (0..n)
        .map(|i| format!("word{i}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Pages under /p/, and a chat endpoint that summarizes by echoing the page's first words.
fn server() -> Server {
    Server::start(|r| match r.path.split('?').next().unwrap_or("") {
        "/p/tokio" => Response::html(format!(
            "<title>Tokio</title><main><p>Tokio is an asynchronous runtime for Rust. {}</p></main>",
            words(30)
        )),
        "/p/zebra" => Response::html(format!(
            "<main><p>Zebras are striped equids from Africa. {}</p></main>",
            words(30)
        )),
        "/p/shell" => Response::html("<div id=root></div><script src=app.js></script>"),
        "/v1/chat/completions" => {
            let user = r.json()["messages"][1]["content"]
                .as_str()
                .unwrap()
                .to_string();
            if user.contains("FAILME") {
                return Response::new(500).body(r#"{"error": {"message": "boom"}}"#);
            }
            let text = user.split("Page text:\n").nth(1).unwrap_or("");
            let first: Vec<&str> = text.split_whitespace().take(6).collect();
            let summary = format!("Summary: {}", first.join(" "));
            Response::json(&json!({
                "choices": [{"finish_reason": "stop",
                             "message": {"role": "assistant",
                                         "content": json!({"summary": summary}).to_string()}}],
                "usage": {"prompt_tokens": 100, "completion_tokens": 10},
            }))
        }
        _ => Response::new(404),
    })
}

struct Db(PathBuf);

impl Db {
    fn new(server: &Server) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "shlv-sum-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            format!(
                "[ai]\nsummarize = \"local\"\n\n[provider.local]\nkind = \"openai-compatible\"\n\
                 base_url = \"{}\"\nmodel = \"m\"\n",
                server.url("/v1")
            ),
        )
        .unwrap();
        Db(dir)
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let o = Command::new(env!("CARGO_BIN_EXE_shlv"))
            .args(args)
            .env("SHLV_DB", self.0.join("shlv.db"))
            .env("SHLV_CONFIG", self.0.join("config.toml"))
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

impl Drop for Db {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn summarizes_and_search_finds_it() {
    let s = server();
    let db = Db::new(&s);
    db.ok(&["add", &s.url("/p/tokio"), "--title", "Tokio"]);
    let (code, out, err) = db.run(&["summarize", "1"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "1  Summary: Tokio is an asynchronous runtime for\n");
    assert!(
        err.contains("summarizing 1 bookmark with provider local (openai-compatible, m at"),
        "{err}"
    );
    assert!(
        err.contains("1 request: 100 input tokens, 10 output tokens"),
        "{err}"
    );
    assert_eq!(
        db.summary("1"),
        "Summary: Tokio is an asynchronous runtime for"
    );
    // Words only in the summary are searchable.
    assert!(db.ok(&["search", "asynchronous"]).starts_with("1  Tokio"));
    // Already summarized: skipped unless --force.
    let (code, out, err) = db.run(&["summarize", "1"]);
    assert_eq!((code, out.as_str()), (0, ""));
    assert!(
        err.contains("skipped 1 bookmark that already have a summary"),
        "{err}"
    );
    assert_eq!(db.ok(&["summarize", "1", "--force"]).lines().count(), 1);
}

#[test]
fn bulk_run_reports_failures_and_keeps_progress() {
    let s = server();
    let db = Db::new(&s);
    db.ok(&["add", &s.url("/p/tokio")]);
    db.ok(&["add", &s.url("/p/shell")]);
    db.ok(&["add", &s.url("/p/missing")]);
    db.ok(&["add", &s.url("/p/zebra"), "--note", "FAILME"]);
    db.ok(&["add", &s.url("/p/zebra?again=1")]);
    let (code, out, err) = db.run(&["summarize", "--all", "-j", "3"]);
    assert_eq!(code, 1, "failures make the run fail: {err}");
    assert_eq!(out.lines().count(), 2, "{out}");
    assert!(err.contains("2: skipped, no text on the page"), "{err}");
    assert!(
        err.contains("3: failed:") && err.contains("HTTP 404"),
        "{err}"
    );
    assert!(
        err.contains("4: failed:")
            && err.contains("provider local (summarize)")
            && err.contains("boom"),
        "{err}"
    );
    assert!(
        err.contains("summarized 2 of 5 bookmarks: 1 without page text, 2 failed"),
        "{err}"
    );
    // The successes were committed; a rerun only tries the rest.
    assert!(db.summary("1").is_string() && db.summary("5").is_string());
    let (_, _, err) = db.run(&["summarize", "--all"]);
    assert!(err.contains("summarizing 3 bookmarks"), "{err}");
}

#[test]
fn large_runs_need_yes() {
    let s = server();
    let db = Db::new(&s);
    for i in 0..21 {
        db.ok(&["add", &s.url(&format!("/p/tokio?n={i}"))]);
    }
    let (code, _, err) = db.run(&["summarize", "--all"]);
    assert_eq!(code, 1);
    assert!(
        err.contains("summarizing 21 bookmarks with provider local"),
        "{err}"
    );
    assert!(
        err.contains("that sends 21 bookmarks to the provider; pass --yes"),
        "{err}"
    );
    assert!(s.requests().is_empty(), "nothing is sent without --yes");
    let (code, out, _) = db.run(&["summarize", "--all", "--yes", "-j", "4"]);
    assert_eq!((code, out.lines().count()), (0, 21));
}

#[test]
fn needs_a_provider() {
    let s = server();
    let db = Db::new(&s);
    std::fs::write(db.0.join("config.toml"), "").unwrap();
    db.ok(&["add", "https://a.example/"]);
    let (code, _, err) = db.run(&["summarize", "1"]);
    assert_eq!(code, 1);
    assert!(
        err.contains("no provider for summarize: set [ai] summarize"),
        "{err}"
    );
}
