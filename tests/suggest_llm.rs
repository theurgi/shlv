//! The LLM layer of `shlv suggest` against recorded provider replies (feature `ai`).
#![cfg(feature = "ai")]

mod common;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use common::server::{Response, Server};
use serde_json::{Value, json};

/// Pages under /p/ and a chat endpoint answering with the queued replies in order.
struct Fixture {
    server: Server,
    replies: Arc<Mutex<VecDeque<String>>>,
    dir: PathBuf,
}

impl Fixture {
    fn new(config: &str) -> Self {
        let replies: Arc<Mutex<VecDeque<String>>> = Arc::default();
        let q = replies.clone();
        let server = Server::start(move |r| match r.path.split('?').next().unwrap() {
            "/p/tokio" => Response::html(format!(
                "<main><p>Tokio is an asynchronous runtime for Rust. {}</p></main>",
                "More words here. ".repeat(10)
            )),
            "/v1/chat/completions" => {
                let content = q.lock().unwrap().pop_front().expect("a queued reply");
                Response::json(&json!({"choices": [{"finish_reason": "stop",
                    "message": {"role": "assistant", "content": content}}]}))
            }
            _ => Response::new(404),
        });
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "shlv-llmtag-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = Fixture {
            server,
            replies,
            dir,
        };
        f.config(config);
        f
    }

    fn config(&self, extra: &str) {
        std::fs::write(
            self.dir.join("config.toml"),
            format!(
                "{extra}\n[provider.local]\nkind = \"openai-compatible\"\n\
                 base_url = \"{}\"\nmodel = \"m\"\n",
                self.server.url("/v1")
            ),
        )
        .unwrap();
    }

    fn reply(&self, content: Value) {
        self.replies.lock().unwrap().push_back(content.to_string());
    }

    fn url(&self, path: &str) -> String {
        self.server.url(path)
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let o = Command::new(env!("CARGO_BIN_EXE_shlv"))
            .args(args)
            .env("SHLV_DB", self.dir.join("shlv.db"))
            .env("SHLV_CONFIG", self.dir.join("config.toml"))
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

    fn pending(&self) -> Vec<Value> {
        self.ok(&["suggest", "ls", "--json"])
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// The tag enum the provider was offered in chat request `i`.
    fn offered(&self, i: usize) -> Value {
        let chats: Vec<_> = self
            .server
            .requests()
            .into_iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .collect();
        chats[i].json()["response_format"]["json_schema"]["schema"]["properties"]["tags"]
            ["items"]["enum"]
            .clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

const CANONICAL: &str = "[tags.canonical]\nrust = [\"rs\"]\nasync = []\n\
                         python = { aliases = [\"py\"], description = \"the Python language\" }\n";

#[test]
fn confirm_mode_offers_pending_suggestions() {
    let f = Fixture::new(&format!("[ai]\ntag = \"local\"\n{CANONICAL}"));
    f.ok(&["add", &f.url("/p/tokio"), "--title", "Tokio"]);
    f.reply(json!({"tags": ["rs", "async", "golang"]}));
    let out = f.ok(&["suggest", "1"]);
    assert!(
        out.contains("llm: 1 bookmark sent, 0 failed: 2 new suggestions"),
        "{out}"
    );
    assert!(out.contains("#async (llm)  #rust (llm)"), "{out}");
    let p = f.pending();
    assert_eq!(p.len(), 2);
    assert!(
        p.iter()
            .all(|s| s["source"] == "llm" && s["confidence"].is_null())
    );
    // The whole canonical list was offered, and the page text sent.
    assert_eq!(f.offered(0), json!(["async", "python", "rust"]));

    // Rejected tags aren't offered again, and are dropped if the model names them anyway.
    f.ok(&["suggest", "reject", "1", "async"]);
    f.ok(&["suggest", "accept", "1", "rust"]);
    f.reply(json!({"tags": ["async", "python"]}));
    let out = f.ok(&["suggest", "1"]);
    assert_eq!(f.offered(1), json!(["python"]));
    assert!(out.contains("1 new suggestion"), "{out}");
    let p = f.pending();
    assert_eq!((p.len(), p[0]["tag"].as_str()), (1, Some("python")));
}

#[test]
fn auto_mode_applies_tags() {
    let f = Fixture::new(&format!(
        "[ai]\ntag = \"local\"\n[tags]\nsuggestions = \"auto\"\n{CANONICAL}"
    ));
    f.ok(&["add", &f.url("/p/tokio")]);
    f.reply(json!({"tags": ["rust", "async"]}));
    let out = f.ok(&["suggest", "1"]);
    assert!(out.contains("2 tags applied"), "{out}");
    assert!(f.ok(&["ls"]).contains("#async  #rust"));
    assert!(f.ok(&["search", "#rust"]).starts_with("1 "));
    assert!(f.pending().is_empty());
}

#[test]
fn rules_and_llm_share_one_row() {
    let f = Fixture::new(&format!(
        "[ai]\ntag = \"local\"\n{CANONICAL}[tags.rules]\n\"127.0.0.1\" = [\"rust\"]\n"
    ));
    f.ok(&["add", &f.url("/p/tokio")]);
    f.reply(json!({"tags": ["rust", "async"]}));
    f.ok(&["suggest", "1"]);
    let p = f.pending();
    let rust: Vec<_> = p.iter().filter(|s| s["tag"] == "rust").collect();
    assert_eq!(rust.len(), 1, "{p:?}");
    // The rule's confidence (1.0) is higher than the LLM's (none), so the rule keeps the row.
    assert_eq!(
        (&rust[0]["source"], &rust[0]["confidence"]),
        (&json!("rule"), &json!(1.0))
    );
}

#[test]
fn bulk_runs_pick_untagged_bookmarks_once() {
    let f = Fixture::new(&format!("[ai]\ntag = \"local\"\n{CANONICAL}"));
    f.ok(&["add", &f.url("/p/tokio?1")]);
    f.ok(&["add", &f.url("/p/tokio?2"), "-t", "rust"]);
    f.ok(&["add", &f.url("/p/tokio?3")]);
    f.reply(json!({"tags": ["rust"]}));
    f.reply(json!({"tags": ["rust"]}));
    let (code, out, err) = f.run(&["suggest", "--all", "-j", "1"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        err.contains("suggesting tags for 2 bookmarks with provider local"),
        "{err}"
    );
    assert!(out.contains("llm: 2 bookmarks sent"), "{out}");
    // Already asked: a rerun sends nothing, unless --force; --max-tags widens the set.
    let (_, _, err) = f.run(&["suggest", "--all"]);
    assert!(err.contains("no bookmarks to send"), "{err}");
    for _ in 0..3 {
        f.reply(json!({"tags": []}));
    }
    let (_, _, err) = f.run(&["suggest", "--all", "--force", "--max-tags", "2"]);
    assert!(err.contains("suggesting tags for 3 bookmarks"), "{err}");
}

#[test]
fn same_provider_also_summarizes() {
    let f = Fixture::new(&format!(
        "[ai]\ntag = \"local\"\nsummarize = \"local\"\n{CANONICAL}"
    ));
    f.ok(&["add", &f.url("/p/tokio")]);
    f.reply(json!({"summary": "Tokio, an async runtime.", "tags": ["rust"]}));
    let out = f.ok(&["suggest", "1"]);
    assert!(
        out.contains("1 bookmark summarized in the same request"),
        "{out}"
    );
    let v: Value = serde_json::from_str(&f.ok(&["show", "1", "--json"])).unwrap();
    assert_eq!(v["summary"], "Tokio, an async runtime.");
}

#[test]
fn needs_a_canonical_list_and_reports_failures() {
    let f = Fixture::new("[ai]\ntag = \"local\"\n");
    f.ok(&["add", &f.url("/p/tokio")]);
    let (code, _, err) = f.run(&["suggest", "1"]);
    assert_eq!(code, 1);
    assert!(
        err.contains("the canonical tag list, which is empty"),
        "{err}"
    );

    f.config(&format!("[ai]\ntag = \"local\"\n{CANONICAL}"));
    f.reply(json!("not an object"));
    f.reply(json!("still not"));
    let (code, out, err) = f.run(&["suggest", "1"]);
    assert_eq!(code, 1, "{out}");
    assert!(
        err.contains("1: failed:") && err.contains("invalid reply twice"),
        "{err}"
    );

    // Without [ai] tag and rules, there's nothing to suggest with.
    f.config("");
    let (code, _, err) = f.run(&["suggest", "1"]);
    assert_eq!(code, 1);
    assert!(
        err.contains("no domain rules") && err.contains("no [ai] tag provider"),
        "{err}"
    );
}
