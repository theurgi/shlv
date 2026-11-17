//! `shlv tag draft` against recorded provider replies, answered on stdin (feature `ai`). Without
//! a terminal, each answer is a line whose first character is the key.
#![cfg(feature = "ai")]

mod common;

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use common::server::{Request, Response, Server};
use serde_json::{Value, json};

struct Fixture {
    server: Server,
    replies: Arc<Mutex<VecDeque<String>>>,
    dir: PathBuf,
}

impl Fixture {
    fn new(extra: &str) -> Self {
        let replies: Arc<Mutex<VecDeque<String>>> = Arc::default();
        let q = replies.clone();
        let server = Server::start(move |r| match r.path.as_str() {
            "/v1/chat/completions" => {
                let content = q.lock().unwrap().pop_front().expect("a queued reply");
                Response::json(&json!({"choices": [{"finish_reason": "stop",
                    "message": {"role": "assistant", "content": content}}],
                    "usage": {"prompt_tokens": 100, "completion_tokens": 10}}))
            }
            _ => Response::new(404),
        });
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "shlv-draft-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            format!(
                "[ai]\ntag = \"local\"\n{extra}\n[provider.local]\nkind = \"openai-compatible\"\n\
                 base_url = \"{}\"\nmodel = \"m\"\n",
                server.url("/v1")
            ),
        )
        .unwrap();
        Fixture {
            server,
            replies,
            dir,
        }
    }

    fn reply(&self, content: Value) {
        self.reply_text(&content.to_string());
    }

    fn reply_text(&self, content: &str) {
        self.replies.lock().unwrap().push_back(content.to_string());
    }

    /// Runs shlv with `input` on stdin.
    fn run(&self, args: &[&str], input: &str) -> (i32, String, String) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_shlv"))
            .args(args)
            .env("SHLV_DB", self.dir.join("shlv.db"))
            .env("SHLV_CONFIG", self.dir.join("config.toml"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let o = child.wait_with_output().unwrap();
        (
            o.status.code().unwrap_or(-1),
            String::from_utf8(o.stdout).unwrap(),
            String::from_utf8(o.stderr).unwrap(),
        )
    }

    fn ok(&self, args: &[&str], input: &str) -> String {
        let (code, out, err) = self.run(args, input);
        assert_eq!(code, 0, "shlv {args:?}: {err}");
        out
    }

    /// Imports bookmarks given as JSON objects.
    fn import(&self, items: Value) {
        let path = self.dir.join("import.json");
        std::fs::write(&path, items.to_string()).unwrap();
        self.ok(&["import", path.to_str().unwrap()], "");
    }

    fn chats(&self) -> Vec<Request> {
        self.server
            .requests()
            .into_iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn system(c: &Request) -> String {
    c.json()["messages"][0]["content"]
        .as_str()
        .unwrap()
        .to_string()
}

fn user(c: &Request) -> String {
    c.json()["messages"][1]["content"]
        .as_str()
        .unwrap()
        .to_string()
}

/// `n` bookmarks, each on its own site.
fn sites(n: usize) -> Value {
    (0..n)
        .map(|i| json!({"url": format!("https://site{i}.example/page"), "title": format!("Page {i}")}))
        .collect()
}

#[test]
fn drafts_a_list_from_scratch() {
    let f = Fixture::new("[tags.style]\nguidelines = \"Prefer tool names over categories.\"\n");
    f.import(json!([
        {"url": "https://github.com/NixOS/nixpkgs", "title": "nixpkgs"},
        {"url": "https://github.com/nix-community/home-manager", "title": "home-manager"},
        {"url": "https://www.example.org/secret-path?token=abc", "title": "Proxmox notes",
         "note": "PRIVATE NOTE"},
        {"url": "https://blog.example.net/flakes", "title": "Flakes",
         "summary": "How Nix flakes pin inputs."},
    ]));
    f.ok(&["tag", "interests", "--add", "nix"], "");
    f.reply(json!({"tags": [
        {"name": "nix", "description": "the Nix language and package manager", "interest": "nix"},
        {"name": "Home Lab", "description": "servers at home", "interest": ""},
        {"name": "flake", "description": "Nix flakes", "interest": "nix"},
        {"name": "rust", "description": "the Rust language", "interest": "bogus"},
    ]}));
    let answers = [
        "y", // 4 bookmarks: asked before sending
        "k", // nix
        "m", // flake: merge into nix
        "nix", "k", // home-lab (no interest: after the nix group)
        "d", // rust
        "",  // save
    ];
    let (code, out, err) = f.run(&["tag", "draft"], &(answers.join("\n") + "\n"));
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        out,
        "drafted: 2 new tags on the list (nix home-lab), 1 alias (flake → nix), 2 descriptions\n"
    );
    // What is sent, and where, comes first.
    assert!(
        err.starts_with("shlv: tag draft sends to provider local (openai-compatible, m at "),
        "{err}"
    );
    assert!(
        err.contains(
            "  4 of 4 bookmarks, from 3 of 3 sites: each one's site, title, and its \
                      summary (1 has one; --fetch sends page text for the rest)"
        ),
        "{err}"
    );
    assert!(err.contains("no tag list yet: it drafts one"), "{err}");
    assert!(err.contains("your interests: nix\n"), "{err}");
    assert!(
        err.contains("your [tags.style] guidelines (1 line)"),
        "{err}"
    );
    assert!(err.contains("draft: 4 tags"), "{err}");
    assert!(
        err.contains("1 request: 100 input tokens, 10 output tokens"),
        "{err}"
    );

    let chats = f.chats();
    assert_eq!(chats.len(), 1);
    let body = String::from_utf8_lossy(&chats[0].body);
    // Sites, titles, and summaries only: no paths, queries, or notes.
    for gone in ["secret-path", "token=abc", "PRIVATE NOTE", "NixOS/nixpkgs"] {
        assert!(!body.contains(gone), "{gone}: {body}");
    }
    let u = user(&chats[0]);
    assert!(u.starts_with("Bookmarks:\n- "), "{u}");
    assert!(u.contains("- github.com: nixpkgs\n"), "{u}");
    assert!(u.contains("- example.org: Proxmox notes"), "{u}");
    assert!(
        u.contains("- blog.example.net: Flakes\n  How Nix flakes pin inputs."),
        "{u}"
    );
    let s = system(&chats[0]);
    assert!(s.contains("Draft their tag list: up to 30 tags"), "{s}");
    assert!(s.contains("Their interests are: nix."), "{s}");
    assert!(
        s.ends_with("guidelines for tags:\nPrefer tool names over categories."),
        "{s}"
    );
    let req = chats[0].json();
    assert_eq!(
        req["response_format"]["json_schema"]["schema"]["properties"]["tags"]["items"]["properties"]
            ["interest"]["enum"],
        json!(["nix", ""])
    );
    assert!(req["response_format"]["json_schema"]["schema"]["properties"]["redundant"].is_null());
    assert_eq!(req["max_tokens"], 400 + 60 * 30);

    let list = f.ok(&["tag", "canonical", "export"], "");
    assert!(list.contains("nix flake # the Nix language"), "{list}");
    assert!(list.contains("home-lab # servers at home"), "{list}");
    assert!(!list.contains("rust"), "{list}");
    // Tags kept under a stored interest are recorded under it.
    let h: Value = serde_json::from_str(&f.ok(&["tag", "health", "--json"], "")).unwrap();
    assert_eq!(h["interests"][0]["tags"], json!(["nix"]));
}

#[test]
fn with_a_list_proposes_additions_and_redundant_tags() {
    let f = Fixture::new("");
    f.ok(&["add", "https://a.example/", "-t", "js", "-t", "css"], "");
    f.ok(
        &[
            "tag",
            "canonical",
            "add",
            "js",
            "javascript",
            "css",
            "style",
        ],
        "",
    );
    f.reply(json!({
        "tags": [
            {"name": "javascript", "description": "already listed"},
            {"name": "wasm", "description": "WebAssembly"},
        ],
        "redundant": [
            {"tag": "js", "into": "javascript", "reason": "a synonym"},
            {"tag": "style", "into": "css", "reason": "too close"},
            {"tag": "nope", "into": "javascript", "reason": "not on the list"},
            {"tag": "css", "into": "nowhere", "reason": "no such tag"},
        ],
    }));
    let answers = [
        "y", "k", // wasm
        "r", // js: a listed tag can't be renamed here
        "m", // js: merge into the suggested tag
        "", "k", // style: leave it
        "",  // save
    ];
    let (code, out, err) = f.run(&["tag", "draft"], &(answers.join("\n") + "\n"));
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        out,
        "drafted: 1 new tag on the list (wasm), 1 alias (js → javascript), 1 description\n"
    );
    assert!(
        err.contains("your tag list (4 tags, with descriptions and aliases)"),
        "{err}"
    );
    assert!(err.contains("draft: 1 tag, 2 redundant"), "{err}");
    assert!(err.contains("→ javascript: a synonym"), "{err}");
    assert!(
        err.contains("js is on the list: `shlv tag rename` renames it"),
        "{err}"
    );
    assert!(err.contains("left as is"), "{err}");

    let s = system(&f.chats()[0]);
    assert!(
        s.contains("Propose only additions: up to 30 new tags"),
        "{s}"
    );
    assert!(
        s.contains("\n\nTags:\n- css\n- javascript\n- js\n- style"),
        "{s}"
    );
    assert_eq!(
        f.chats()[0].json()["response_format"]["json_schema"]["schema"]["properties"]["redundant"]
            ["items"]["properties"]["tag"]["enum"],
        json!(["css", "javascript", "js", "style"])
    );

    let list = f.ok(&["tag", "canonical", "ls"], "");
    assert!(!list.contains("js "), "{list}");
    assert!(list.contains("style"), "{list}");
    // The bookmark's tag followed the merge.
    let ls = f.ok(&["ls"], "");
    assert!(ls.contains("javascript"), "{ls}");
    assert!(!ls.contains(" js"), "{ls}");
}

#[test]
fn a_reply_that_echoes_the_list_is_retried() {
    let f = Fixture::new("");
    f.import(sites(3));
    f.ok(&["tag", "canonical", "add", "rust"], "");
    f.reply(json!({"tags": [{"name": "rust", "description": "again"}], "redundant": []}));
    f.reply(json!({"tags": [{"name": "nix", "description": "Nix"}], "redundant": []}));
    let out = f.ok(&["tag", "draft", "--yes"], "k\n\n");
    assert_eq!(out, "drafted: 1 new tag on the list (nix), 1 description\n");
    let chats = f.chats();
    assert_eq!(chats.len(), 2);
    let retry = chats[1].json()["messages"][3]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(retry.contains("already on the list"), "{retry}");

    // Nothing new at all: nothing to go through.
    f.reply(json!({"tags": [], "redundant": []}));
    let (code, out, err) = f.run(&["tag", "draft", "--yes"], "");
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "");
    assert!(err.contains("the draft has nothing to add"), "{err}");
}

#[test]
fn chunks_are_merged_and_a_failed_one_is_skipped() {
    let f = Fixture::new("");
    f.import(sites(30));
    let tag = |n: &str| json!({"name": n, "description": format!("about {n}")});
    f.reply(json!({"tags": [tag("rust"), tag("nix")]}));
    f.reply_text("not JSON");
    f.reply_text("still not JSON");
    f.reply(json!({"tags": [tag("wifi"), tag("nix"), tag("audio")]}));
    f.reply(json!({"tags": [tag("nix"), tag("rust"), tag("audio")]}));
    let (code, out, err) = f.run(
        &[
            "tag", "draft", "-s", "30", "--chunk", "8", "-n", "3", "--yes",
        ],
        "k\nk\nk\n\n",
    );
    assert_eq!(code, 0, "{err}");
    // nix in three drafts, rust and audio in two; wifi in one is left out.
    assert_eq!(
        out,
        "drafted: 3 new tags on the list (nix rust audio), 3 descriptions\n"
    );
    assert!(err.contains("in 4 requests of up to 8"), "{err}");
    assert!(err.contains("request 1 of 4: 2 tags"), "{err}");
    assert!(
        err.contains("request 2 of 4: shlv: provider local (draft): invalid reply twice"),
        "{err}"
    );
    assert!(
        err.contains("1 tag proposed by fewer requests left out"),
        "{err}"
    );

    let chats = f.chats();
    assert_eq!(chats.len(), 5);
    // Each request has its own part of the sample, and the names drafted so far.
    let lines = |c: &Request| user(c).lines().filter(|l| l.starts_with("- ")).count();
    assert_eq!(chats.iter().map(lines).collect::<Vec<_>>(), [8, 8, 8, 8, 6]);
    assert!(!system(&chats[0]).contains("already drafted"));
    assert!(system(&chats[1]).ends_with("rather than a synonym: rust, nix"));
    assert!(system(&chats[4]).ends_with("rather than a synonym: rust, nix, wifi, audio"));
    // Every site once.
    let mut all: Vec<String> = chats
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 2)
        .flat_map(|(_, c)| user(c).lines().map(str::to_string).collect::<Vec<_>>())
        .filter(|l| l.starts_with("- "))
        .collect();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), 30);
}

#[test]
fn asks_before_sending() {
    let f = Fixture::new("");
    // Nothing to draft from.
    let (code, _, err) = f.run(&["tag", "draft", "--yes"], "");
    assert_eq!(code, 1);
    assert!(err.contains("no bookmarks to draft from"), "{err}");

    f.import(sites(25));
    // Above 20 bookmarks: --yes is needed, and nothing is sent without it.
    let (code, _, err) = f.run(&["tag", "draft"], "y\n");
    assert_eq!(code, 1);
    assert!(err.contains("25 of 25 bookmarks"), "{err}");
    assert!(
        err.contains("that sends 25 bookmarks to the provider; pass --yes"),
        "{err}"
    );
    // At or below it, a question; declined, nothing is sent.
    let (code, _, err) = f.run(&["tag", "draft", "-s", "20"], "n\n");
    assert_eq!(code, 0, "{err}");
    assert!(
        err.contains("20 of 25 bookmarks, from 20 of 25 sites"),
        "{err}"
    );
    assert!(err.contains("nothing sent"), "{err}");
    assert!(f.chats().is_empty());

    // Sent, then discarded at the end: nothing written.
    f.reply(json!({"tags": [{"name": "nix", "description": "Nix"}]}));
    let (code, out, err) = f.run(&["tag", "draft", "-s", "5"], "y\nk\nn\n");
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "");
    assert!(err.contains("nothing written"), "{err}");
    assert_eq!(f.ok(&["tag", "canonical", "ls"], ""), "");
}
