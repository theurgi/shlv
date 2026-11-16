//! Tagging layers in order: rules, the classifier (feature `model`), then the provider when the
//! classifier is unsure (feature `ai`).
#![cfg(feature = "model")]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use std::sync::atomic::{AtomicU32, Ordering};

use shlv::config::ModelConfig;
use shlv::{NewBookmark, Store, SuggestMode};

const TOPICS: &[(&str, &str, &[&str])] = &[
    (
        "rust",
        "doc.rust-lang.org",
        &[
            "ownership and borrowing",
            "traits and generics",
            "async await",
            "error handling with Result",
            "lifetimes explained",
            "cargo workspaces",
        ],
    ),
    (
        "cooking",
        "seriouseats.com",
        &[
            "pasta sauce recipe",
            "sourdough bread baking",
            "chicken dinner recipes",
            "soup for winter",
            "garlic and olive oil",
            "baking cookies",
        ],
    ),
];

/// A database with 60 examples per tag (so the classifier trusts them), plus a declared-free
/// canonical list that also has an untrained tag, `music`.
fn fixture(path: &Path) {
    let mut s = Store::open(path).unwrap();
    for (tag, host, titles) in TOPICS {
        for (n, t) in titles.iter().enumerate() {
            for v in 0..10 {
                s.add(&NewBookmark {
                    // Half on a shared host, so titles carry the signal, not just hosts.
                    url: if v % 2 == 0 {
                        format!("https://{host}/{n}/{v}")
                    } else {
                        format!("https://medium.com/{tag}/{n}/{v}")
                    },
                    title: Some(format!("{t} {v}")),
                    tags: vec![tag.to_string()],
                    ..Default::default()
                })
                .unwrap();
            }
        }
    }
    s.add_canonical(&["rust".into(), "cooking".into(), "music".into()])
        .unwrap();
    s.describe_tag("music", "songs, albums, and musicians")
        .unwrap();
    s.train_model().unwrap();
}

fn tmp() -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "shlv-layers-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn shlv(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let o = Command::new(env!("CARGO_BIN_EXE_shlv"))
        .args(args)
        .env("SHLV_DB", dir.join("shlv.db"))
        .env("SHLV_CONFIG", dir.join("config.toml"))
        .output()
        .unwrap();
    (
        o.status.code().unwrap_or(-1),
        String::from_utf8(o.stdout).unwrap(),
        String::from_utf8(o.stderr).unwrap(),
    )
}

fn add(s: &mut Store, url: &str, title: &str) -> i64 {
    s.add(&NewBookmark {
        url: url.into(),
        title: Some(title.into()),
        ..Default::default()
    })
    .unwrap()
    .id
}

#[test]
fn thresholds_and_modes() {
    let dir = tmp();
    let db = dir.join("shlv.db");
    fixture(&db);
    let mut s = Store::open(&db).unwrap();
    let id = add(
        &mut s,
        "https://doc.rust-lang.org/new",
        "borrowing and lifetimes",
    );
    let c = s.classifier(false).unwrap();

    // Above the threshold: pending, with the probability.
    let cfg = ModelConfig::default();
    let st = s.apply_model(Some(&[id]), &c, &cfg).unwrap();
    assert_eq!((st.checked, st.pending, st.applied), (1, 1, 0));
    assert!(st.sure.contains(&id));
    let p = s.pending(Some(id)).unwrap();
    assert_eq!(p[0].tag, "rust");
    assert_eq!(p[0].source.as_str(), "model");
    assert!(p[0].confidence.unwrap() > 0.9);

    // A threshold above the probability offers nothing, and isn't sure.
    let id2 = add(
        &mut s,
        "https://doc.rust-lang.org/new2",
        "borrowing and lifetimes too",
    );
    let high = ModelConfig {
        threshold: 0.999,
        ..cfg
    };
    let st = s.apply_model(Some(&[id2]), &c, &high).unwrap();
    assert_eq!(st.pending, 0);
    assert!(!st.sure.contains(&id2));

    // Auto mode applies only at or above auto_threshold; below it, suggestions wait.
    s.set_suggest_mode(SuggestMode::Auto);
    let id3 = add(
        &mut s,
        "https://doc.rust-lang.org/new3",
        "traits and generics again",
    );
    let st = s.apply_model(Some(&[id3]), &c, &cfg).unwrap();
    assert_eq!((st.applied, st.pending), (1, 0));
    assert_eq!(s.get(id3).unwrap().unwrap().tags, ["rust"]);
    let strict = ModelConfig {
        auto_threshold: 1.0,
        ..cfg
    };
    let id4 = add(
        &mut s,
        "https://doc.rust-lang.org/new4",
        "cargo workspaces again",
    );
    let st = s.apply_model(Some(&[id4]), &c, &strict).unwrap();
    assert_eq!((st.applied, st.pending), (0, 1));

    // An untrained tag never makes the classifier sure.
    let id5 = add(
        &mut s,
        "https://example.com/m",
        "songs albums musicians music",
    );
    let low = ModelConfig {
        threshold: 0.4,
        ..cfg
    };
    let st = s.apply_model(Some(&[id5]), &c, &low).unwrap();
    assert!(!st.sure.contains(&id5));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rules_then_model_share_rows_and_add_runs_the_model() {
    let dir = tmp();
    fixture(&dir.join("shlv.db"));
    std::fs::write(
        dir.join("config.toml"),
        "[tags.rules]\n\"doc.rust-lang.org\" = [\"rust\"]\n",
    )
    .unwrap();
    // `add` runs rules and the classifier; both suggest rust: one row, the rule's 1.0 wins.
    let (code, out, err) = shlv(
        &dir,
        &[
            "add",
            "https://doc.rust-lang.org/x",
            "--title",
            "async await",
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("suggested  #rust (rule)"), "{out}");
    let (_, out, _) = shlv(&dir, &["suggest", "ls", "--json"]);
    assert_eq!(out.lines().count(), 1, "{out}");
    assert!(out.contains("\"source\":\"rule\""), "{out}");

    // A bookmark only the classifier knows about.
    let (_, out, _) = shlv(
        &dir,
        &[
            "add",
            "https://example.org/r",
            "--title",
            "sourdough bread baking",
        ],
    );
    assert!(out.contains("#cooking (model 0."), "{out}");

    // `suggest` runs rules first, then the classifier.
    let (code, out, err) = shlv(&dir, &["suggest", "--all"]);
    assert_eq!(code, 0, "{err}");
    let rules = out.find("rules matched").unwrap();
    let model = out.find("model checked").unwrap();
    assert!(rules < model, "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(feature = "ai")]
#[test]
fn provider_fallback() {
    use common::server::{Response, Server};
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let server = Server::start(move |r| {
        if r.path.starts_with("/p/") {
            return Response::html("<p>short</p>");
        }
        c.fetch_add(1, Ordering::Relaxed);
        Response::json(&json!({"choices": [{"finish_reason": "stop",
            "message": {"role": "assistant", "content": "{\"tags\": [\"music\"]}"}}]}))
    });
    let dir = tmp();
    fixture(&dir.join("shlv.db"));
    let mut s = Store::open(&dir.join("shlv.db")).unwrap();
    // One the classifier is sure about, one it isn't (nothing it knows).
    let sure = add(&mut s, &server.url("/p/rust"), "ownership and borrowing 3");
    let unsure = add(&mut s, &server.url("/p/jazz"), "a jazz record review");
    drop(s);
    let config = |use_: &str| {
        std::fs::write(
            dir.join("config.toml"),
            format!(
                "[ai]\ntag = \"local\"\n[model]\nthreshold = 0.8\nprovider = \"{use_}\"\n\
                 [provider.local]\nkind = \"openai-compatible\"\nbase_url = \"{}\"\nmodel = \"m\"\n",
                server.url("/v1")
            ),
        )
        .unwrap()
    };
    let ids = [sure.to_string(), unsure.to_string()];
    let args = ["suggest", ids[0].as_str(), ids[1].as_str()];

    config("fallback");
    let (code, out, err) = shlv(&dir, &args);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "only the unsure one: {out}"
    );
    assert!(out.contains("llm: 1 bookmark sent"), "{out}");

    config("always");
    let (_, out, _) = shlv(&dir, &args);
    assert_eq!(calls.load(Ordering::Relaxed), 3, "{out}");

    config("never");
    let (_, out, _) = shlv(&dir, &args);
    assert_eq!(calls.load(Ordering::Relaxed), 3, "{out}");
    assert!(!out.contains("llm:"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn model_config_errors() {
    let e = shlv::Config::parse("[model]\nthreshold = 1.5\n", Path::new("/c.toml"))
        .unwrap_err()
        .to_string();
    assert_eq!(
        e,
        "/c.toml: [model] threshold must be between 0 and 1, not 1.5"
    );
    let e = shlv::Config::parse("[model]\nprovider = \"sometimes\"\n", Path::new("/c.toml"))
        .unwrap_err()
        .to_string();
    assert!(
        e.starts_with("/c.toml:2:12: unknown variant `sometimes`"),
        "{e}"
    );
}
