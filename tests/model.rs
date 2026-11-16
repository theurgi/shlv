//! The tag classifier on a small hand-made fixture (feature `model`). Quality is judged in task
//! 025 on a public dataset; this checks the plumbing and that obvious cases come out right.
#![cfg(feature = "model")]

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use shlv::choose::{ChooseTags, Item};
use shlv::model::canonical_list;
use shlv::{NewBookmark, Store};

const FIXTURE: &[(&str, &str, &str)] = &[
    (
        "rust",
        "doc.rust-lang.org",
        "The Rust book: ownership and borrowing",
    ),
    ("rust", "docs.rs", "serde: serialization for Rust structs"),
    (
        "rust",
        "blog.rust-lang.org",
        "Announcing Rust 1.83 with const improvements",
    ),
    ("rust", "tokio.rs", "Tokio tutorial: async Rust networking"),
    ("rust", "github.com", "ripgrep: fast grep written in Rust"),
    (
        "rust",
        "doc.rust-lang.org",
        "Rust by Example: traits and generics",
    ),
    ("cooking", "seriouseats.com", "The best pasta sauce recipe"),
    (
        "cooking",
        "bonappetit.com",
        "Weeknight chicken dinner recipes",
    ),
    (
        "cooking",
        "seriouseats.com",
        "How to bake sourdough bread at home",
    ),
    ("cooking", "food52.com", "Pasta with garlic and olive oil"),
    ("cooking", "bonappetit.com", "Easy soup recipes for winter"),
    ("cooking", "food52.com", "Baking bread: a beginner recipe"),
    (
        "travel",
        "lonelyplanet.com",
        "Lisbon travel guide: neighborhoods and food",
    ),
    ("travel", "wikivoyage.org", "Kyoto travel guide and temples"),
    (
        "travel",
        "lonelyplanet.com",
        "Best hiking trails in Patagonia",
    ),
    ("travel", "seat61.com", "Train travel from London to Rome"),
    (
        "travel",
        "wikivoyage.org",
        "Iceland ring road travel itinerary",
    ),
    ("travel", "seat61.com", "Night trains across Europe"),
];

fn store() -> Store {
    let mut s = Store::open_in_memory().unwrap();
    for (n, (tag, host, title)) in FIXTURE.iter().enumerate() {
        // Repeat each with a variant so every tag has enough examples for the trained model.
        for v in 0..9 {
            s.add(&NewBookmark {
                url: format!("https://{host}/{n}/{v}"),
                title: Some(format!("{title} part {v}")),
                tags: vec![tag.to_string()],
                ..Default::default()
            })
            .unwrap();
        }
    }
    s.add_canonical(&[
        "rust".into(),
        "cooking".into(),
        "travel".into(),
        "music".into(),
    ])
    .unwrap();
    s.describe_tag("music", "songs, albums, and musicians")
        .unwrap();
    s
}

fn item(title: &str, url: &str) -> Item {
    Item {
        url: url.into(),
        title: Some(title.into()),
        ..Default::default()
    }
}

fn probs(c: &shlv::model::Classifier, title: &str, url: &str) -> HashMap<String, f64> {
    c.probabilities(&item(title, url)).into_iter().collect()
}

#[test]
fn trains_stores_and_predicts() {
    let mut s = store();
    assert!(s.stored_model().unwrap().is_none());
    let t = s.train_model().unwrap();
    assert_eq!((t.examples, t.tags), (162, 4));
    let stored = s.stored_model().unwrap().unwrap();
    assert!(!stored.stale);
    assert_eq!(stored.bytes, t.bytes);

    let c = s.classifier(false).unwrap();
    let p = probs(
        &c,
        "Borrowing and lifetimes in Rust",
        "https://doc.rust-lang.org/x",
    );
    assert!(p["rust"] > 0.9, "{p:?}");
    assert!(p["cooking"] < 0.1 && p["travel"] < 0.1, "{p:?}");
    let p = probs(&c, "A simple bread recipe", "https://food52.com/y");
    assert!(p["cooking"] > 0.8, "{p:?}");
    // No examples for music: its name and description still match.
    let p = probs(&c, "New albums and songs", "https://pitchfork.com/z");
    assert_eq!(p["music"], 0.15, "{p:?}");
    let list = canonical_list(&s).unwrap();
    let chosen = c
        .choose_tags(&item("Rust traits", "https://docs.rs/a"), &list)
        .unwrap();
    assert_eq!(chosen[0].tag, "rust");
    assert!(
        chosen
            .windows(2)
            .all(|w| w[0].confidence >= w[1].confidence)
    );
}

#[test]
fn retrains_when_the_tags_change() {
    let mut s = store();
    s.train_model().unwrap();
    s.add(&NewBookmark {
        url: "https://example.com/new".into(),
        title: Some("Spotify playlists for focus".into()),
        tags: vec!["music".into()],
        ..Default::default()
    })
    .unwrap();
    assert!(s.stored_model().unwrap().unwrap().stale);
    s.classifier(true).unwrap();
    let st = s.stored_model().unwrap().unwrap();
    assert!(!st.stale);
    assert_eq!(st.model.examples, 163);
    // Without retraining allowed, a stale model is used as it is.
    s.add_canonical(&["jazz".into()]).unwrap();
    s.classifier(false).unwrap();
    assert!(s.stored_model().unwrap().unwrap().stale);
}

fn shlv(db: &PathBuf, args: &[&str]) -> (bool, String, String) {
    let o = Command::new(env!("CARGO_BIN_EXE_shlv"))
        .args(args)
        .env("SHLV_DB", db)
        .env("SHLV_CONFIG", "/nonexistent")
        .output()
        .unwrap();
    (
        o.status.success(),
        String::from_utf8(o.stdout).unwrap(),
        String::from_utf8(o.stderr).unwrap(),
    )
}

#[test]
fn cli_train_status_eval() {
    let dir = std::env::temp_dir().join(format!("shlv-model-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("shlv.db");
    drop(Store::open(&db).map(|mut s| {
        let mut f = store();
        // Copy the fixture into the file database through export/import-free API calls.
        for b in f
            .list(&shlv::ListQuery {
                tags: vec![],
                limit: usize::MAX,
            })
            .unwrap()
        {
            s.add(&NewBookmark {
                url: b.url,
                title: b.title,
                tags: b.tags,
                ..Default::default()
            })
            .unwrap();
        }
        s.add_canonical(&[
            "rust".into(),
            "cooking".into(),
            "travel".into(),
            "music".into(),
        ])
        .unwrap();
        let _ = &mut f;
    }));
    let (ok, out, _) = shlv(&db, &["model", "status"]);
    assert!(ok && out.starts_with("no model yet"), "{out}");
    let (ok, out, err) = shlv(&db, &["model", "train"]);
    assert!(ok, "{err}");
    assert!(
        out.starts_with("trained on 162 examples for 4 canonical tags in "),
        "{out}"
    );
    let (_, out, _) = shlv(&db, &["model", "status"]);
    assert!(out.contains("on 162 examples for 4 tags"), "{out}");
    assert!(out.contains("fewer than 50 examples"), "{out}");
    let (ok, out, err) = shlv(&db, &["model", "eval"]);
    assert!(ok, "{err}");
    assert!(out.contains("tested on the newest"), "{out}");
    assert!(
        out.contains("classifier") && out.contains("names and descriptions only"),
        "{out}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
