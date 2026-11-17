//! `shlv tag seed` against recorded provider replies, answered on stdin (feature `ai`). Without a
//! terminal, each answer is a line whose first character is the key.
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
                    "message": {"role": "assistant", "content": content}}]}))
            }
            _ => Response::new(404),
        });
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "shlv-seed-{}-{}",
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

const CONFIG: &str = "[tags.canonical]\nlinux = []\n\
                      [tags.style]\nguidelines = \"Prefer tool names over categories.\"\n";

fn first_reply() -> Value {
    json!({"interests": [
        {"interest": "nix", "tags": [
            {"name": "nix", "description": "the Nix language and package manager"},
            {"name": "flake", "description": "Nix flakes"},
            {"name": "nixpkgs", "description": "the package collection"},
        ]},
        {"interest": "network", "tags": [
            {"name": "wifi", "description": "wireless networking, 802.11"},
            {"name": "linux", "description": "already on the list"},
            {"name": "Reverse Proxy", "description": "Caddy, nginx, Traefik"},
            {"name": "proxmox", "description": "Proxmox VE"},
        ]},
    ]})
}

#[test]
fn seeds_the_list() {
    let f = Fixture::new(CONFIG);
    f.ok(
        &["add", "https://private.example/secret", "--title", "Secret"],
        "",
    );
    f.reply(first_reply());
    f.reply(json!({"interests": [{"interest": "audio", "tags": [
        {"name": "audio", "description": "sound and music gear"},
        {"name": "speakers", "description": "loudspeakers"},
    ]}]}));
    let answers = [
        "y", // send
        "k", // nix
        "e", // flake: edit the description, then keep
        "Nix flakes and lock files",
        "k",
        "m", // nixpkgs: merge into nix
        "nix",
        "r", // wifi: rename, which keeps it
        "Wireless",
        // linux: already on the list, not asked
        "",  // reverse-proxy: Enter keeps
        "d", // proxmox
        "a", // at the end: add an interest
        "audio",
        "k", // audio
        "d", // speakers
        "",  // save
    ];
    let (code, out, err) = f.run(
        &["tag", "seed", "nix", "network", "--save"],
        &(answers.join("\n") + "\n"),
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        out,
        "seeded: 5 new tags on the list (nix flake wireless reverse-proxy audio), \
         1 alias (nixpkgs → nix), 5 descriptions, 3 new interests saved\n"
    );
    // What is sent, and where, comes first.
    assert!(
        err.starts_with("shlv: tag seed sends to provider local (openai-compatible, m at "),
        "{err}"
    );
    assert!(err.contains("  interests: nix, network\n"), "{err}");
    assert!(
        err.contains("your [tags.style] guidelines (1 line)"),
        "{err}"
    );
    assert!(err.contains("nothing else: no bookmarks"), "{err}");
    assert!(err.contains("linux"), "{err}");
    assert!(err.contains("on the list"), "{err}");

    // Only the interests and the style went out: no bookmarks, no list.
    let chats = f.chats();
    assert_eq!(chats.len(), 2);
    for c in &chats {
        let body = String::from_utf8_lossy(&c.body);
        assert!(!body.contains("private.example"), "{body}");
        assert!(!body.contains("Secret"), "{body}");
        assert!(
            body.contains("Prefer tool names over categories."),
            "{body}"
        );
    }
    assert_eq!(
        chats[0].json()["messages"][1]["content"],
        "Interests:\n- nix\n- network"
    );
    assert_eq!(
        chats[1].json()["messages"][1]["content"],
        "Interests:\n- audio"
    );

    let list = f.ok(&["tag", "canonical", "export"], "");
    assert!(list.contains("flake # Nix flakes and lock files"), "{list}");
    assert!(list.contains("wireless # wireless networking"), "{list}");
    assert!(list.contains("nix nixpkgs # the Nix language"), "{list}");
    assert!(
        !list.contains("proxmox") && !list.contains("speakers"),
        "{list}"
    );
    let interests = f.ok(&["tag", "interests"], "");
    assert_eq!(
        interests,
        "audio  (saved)\nnetwork  (saved)\nnix  (saved)\n"
    );

    // Health shows the spread over the stored interests.
    f.ok(
        &["add", "https://a.example/", "-t", "flake", "-t", "wireless"],
        "",
    );
    let h: Value = serde_json::from_str(&f.ok(&["tag", "health", "--json"], "")).unwrap();
    assert_eq!(
        h["interests"],
        json!([
            {"interest": "audio", "saved": true, "pinned": false, "tags": ["audio"],
             "bookmarks": 0},
            {"interest": "network", "saved": true, "pinned": false,
             "tags": ["linux", "reverse-proxy", "wireless"], "bookmarks": 1},
            {"interest": "nix", "saved": true, "pinned": false, "tags": ["flake", "nix"],
             "bookmarks": 1},
        ])
    );
    let text = f.ok(&["tag", "health"], "");
    assert!(
        text.contains("  nix      2 tags, 1 bookmark: flake nix"),
        "{text}"
    );

    // With no topics, the stored interests are expanded; candidates already on the list aren't
    // asked about, and a candidate seen twice is shown once.
    f.reply(json!({"interests": [{"interest": "nix", "tags": [
        {"name": "nix", "description": "again"},
        {"name": "flake", "description": "again"},
    ]}, {"interest": "audio", "tags": []}, {"interest": "network", "tags": []}]}));
    let (code, out, err) = f.run(&["tag", "seed", "--yes"], "\n");
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        f.chats()[2].json()["messages"][1]["content"],
        "Interests:\n- audio\n- network\n- nix"
    );
    assert_eq!(out, "seeded: 0 new tags on the list\n");
    assert!(err.contains("no candidates for audio"), "{err}");
}

#[test]
fn asks_before_sending_and_can_discard() {
    let f = Fixture::new("");
    // Declined: nothing is sent.
    let (code, _, err) = f.run(&["tag", "seed", "nix"], "n\n");
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("nothing sent"), "{err}");
    assert!(f.chats().is_empty());

    // Sent, decided, then discarded at the end: nothing written.
    f.reply(first_reply());
    let (code, out, err) = f.run(&["tag", "seed", "nix", "network"], "y\nk\nk\nq\nn\n");
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "");
    assert!(err.contains("nothing written"), "{err}");
    assert_eq!(f.ok(&["tag", "canonical", "ls"], ""), "");

    // Stopping early with q saves what's decided; the rest is left out.
    f.reply(first_reply());
    let out = f.ok(&["tag", "seed", "nix", "network", "--yes"], "k\nd\nq\n\n");
    assert_eq!(out, "seeded: 1 new tag on the list (nix), 1 description\n");
    assert_eq!(f.ok(&["tag", "interests"], ""), "");

    // Nothing to expand.
    let (code, _, err) = f.run(&["tag", "seed"], "");
    assert_eq!(code, 1);
    assert!(err.contains("name some interests"), "{err}");
}

#[test]
fn pinned_interests_are_expanded_and_reported() {
    let f = Fixture::new("[tags]\ninterests = [\"Home Lab\"]\n");
    f.reply(json!({"interests": [{"interest": "home-lab", "tags": [
        {"name": "proxmox", "description": "Proxmox VE"},
    ]}]}));
    let out = f.ok(&["tag", "seed", "--yes"], "k\n\n");
    assert_eq!(
        out,
        "seeded: 1 new tag on the list (proxmox), 1 description\n"
    );
    assert_eq!(
        f.ok(&["tag", "interests"], ""),
        "home-lab  (pinned in config)\n"
    );
    let e = f.run(&["tag", "interests", "--rm", "home-lab"], "").2;
    assert!(e.contains("pinned in config"), "{e}");
}

#[test]
fn merges_follow_a_renamed_candidate() {
    let f = Fixture::new("");
    f.reply(json!({"interests": [{"interest": "nix", "tags": [
        {"name": "nixpkgs", "description": "packages"},
        {"name": "nix", "description": "the language"},
    ]}]}));
    // nixpkgs into nix, then nix renamed.
    let out = f.ok(&["tag", "seed", "nix", "--yes"], "m\nnix\nr\nnix-lang\n\n");
    assert_eq!(
        out,
        "seeded: 1 new tag on the list (nix-lang), 1 alias (nixpkgs → nix-lang), 1 description\n"
    );
}
