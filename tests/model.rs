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
        for v in 0..18 {
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
    assert_eq!((t.examples, t.tags), (324, 4));
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
    assert_eq!(st.model.examples, 325);
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
        out.starts_with("trained on 324 examples for 4 canonical tags in "),
        "{out}"
    );
    let (_, out, _) = shlv(&db, &["model", "status"]);
    assert!(out.contains("on 324 examples for 4 tags"), "{out}");
    assert!(out.contains("fewer than 100 examples"), "{out}");
    let (ok, out, err) = shlv(&db, &["model", "eval"]);
    assert!(ok, "{err}");
    assert!(out.contains("tested on the newest"), "{out}");
    assert!(
        out.contains("classifier") && out.contains("names and descriptions only"),
        "{out}"
    );
    // Calibration of what's offered, and the sampling error, next to the all-pairs ECE.
    assert!(
        out.contains("ECE≥0.5") && out.contains("95% sampling error"),
        "{out}"
    );
    assert!(
        out.contains("Classifier choices offered (p ≥ 0.5)")
            || out.contains("offered nothing at p ≥ 0.5"),
        "{out}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A small library with canonical tags, then `music` added to the list: two reviewed bookmarks
/// and two unreviewed ones mention it.
fn backfill_store() -> Store {
    let mut s = Store::open_in_memory().unwrap();
    for (url, title, tags) in [
        ("https://a.com/1", "Rust async book", vec!["rust"]),
        (
            "https://b.com/2",
            "Music theory for programmers",
            vec!["rust"],
        ),
        ("https://c.com/3", "Lisbon guide", vec!["travel"]),
        (
            "https://d.com/4",
            "Best albums and songs of 2024",
            vec!["travel"],
        ),
        ("https://e.com/5", "Music festivals in Europe", vec![]),
        ("https://f.com/6", "New songs and albums this week", vec![]),
        ("https://g.com/7", "Pasta recipe", vec![]),
    ] {
        s.add(&NewBookmark {
            url: url.into(),
            title: Some(title.into()),
            tags: tags.into_iter().map(String::from).collect(),
            ..Default::default()
        })
        .unwrap();
    }
    s.add_canonical(&["rust".into(), "travel".into(), "music".into()])
        .unwrap();
    s.describe_tag("music", "songs, albums, and musicians")
        .unwrap();
    s
}

fn ranked(s: &Store, tag: &str) -> Vec<(i64, bool, Option<f64>)> {
    s.backfill_candidates(tag, None)
        .unwrap()
        .into_iter()
        .map(|c| (c.suggestion.id, c.reviewed, c.suggestion.confidence))
        .collect()
}

#[test]
fn backfills_a_new_tag() {
    let mut s = backfill_store();
    let config = shlv::config::ModelConfig::default();
    let c = s.classifier(true).unwrap();
    // music has no examples: only its lexical score, below the usual threshold of 0.5.
    assert!(c.is_cold("music"));
    let st = s.backfill_model("music", None, &c, &config).unwrap();
    assert_eq!((st.checked, st.pending, st.applied), (7, 4, 0));
    // Only music is offered, reviewed bookmarks first, then by score (a name match, 0.45, over
    // two description words, 0.15).
    assert!(s.pending(None).unwrap().iter().all(|p| p.tag == "music"));
    assert_eq!(
        ranked(&s, "music"),
        [
            (2, true, Some(0.45)),
            (4, true, Some(0.15)),
            (5, false, Some(0.45)),
            (6, false, Some(0.15)),
        ]
    );
    // Narrowed to ids.
    let some = s.backfill_candidates("music", Some(&[4, 6, 7])).unwrap();
    assert_eq!(
        some.iter().map(|c| c.suggestion.id).collect::<Vec<_>>(),
        [4, 6]
    );

    // Accepted and rejected ones aren't offered again; the LLM would be asked about the
    // reviewed bookmark the classifier passed over.
    s.accept(2, &["music".into()]).unwrap();
    s.reject(4, &["music".into()]).unwrap();
    assert_eq!(s.backfill_unoffered("music").unwrap(), [3, 1]);
    let c = s.classifier(true).unwrap();
    let st = s.backfill_model("music", None, &c, &config).unwrap();
    assert_eq!(st.pending, 0);
    let ids: Vec<i64> = ranked(&s, "music").iter().map(|r| r.0).collect();
    assert_eq!(ids, [5, 6]);

    // Only canonical tags are back-filled; aliases resolve to theirs.
    s.add_alias("tunes", "music").unwrap();
    assert_eq!(s.backfill_tag("tunes").unwrap(), "music");
    s.add(&NewBookmark {
        url: "https://h.com/8".into(),
        tags: vec!["misc".into()],
        ..Default::default()
    })
    .unwrap();
    let e = s.backfill_tag("misc").unwrap_err().to_string();
    assert!(e.contains("isn't on the canonical list"), "{e}");
    assert!(s.backfill_tag("nope").is_err());
}

#[test]
fn cli_backfill() {
    let dir = std::env::temp_dir().join(format!("shlv-backfill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("shlv.db");
    {
        let mut s = Store::open(&db).unwrap();
        let mem = backfill_store();
        for b in mem
            .list(&shlv::ListQuery {
                tags: vec![],
                limit: usize::MAX,
            })
            .unwrap()
            .into_iter()
            .rev()
        {
            s.add(&NewBookmark {
                url: b.url,
                title: b.title,
                tags: b.tags,
                ..Default::default()
            })
            .unwrap();
        }
        s.add_canonical(&["rust".into(), "travel".into(), "music".into()])
            .unwrap();
        s.describe_tag("music", "songs, albums, and musicians")
            .unwrap();
    }
    let (ok, out, err) = shlv(&db, &["suggest", "--tag", "music", "--all"]);
    assert!(ok, "{err}");
    assert_eq!(
        out,
        "classifier checked 7 bookmarks for #music: 4 new suggestions\n\
         4 candidates for #music, 2 of them reviewed (listed first):\n\
         2  Music theory for programmers  https://b.com/2  #music (model 0.45)\n\
         4  Best albums and songs of 2024  https://d.com/4  #music (model 0.15)\n\
         5  Music festivals in Europe  https://e.com/5  #music (model 0.45)\n\
         6  New songs and albums this week  https://f.com/6  #music (model 0.15)\n\
         (shlv suggest --tag music --pick ... to go through them, or suggest accept/reject ID \
         music)\n"
    );
    // Go through them: accept, reject, skip, then stop.
    let mut child = Command::new(env!("CARGO_BIN_EXE_shlv"))
        .args(["suggest", "--tag", "music", "--all", "--pick"])
        .env("SHLV_DB", &db)
        .env("SHLV_CONFIG", "/nonexistent")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"y\nn\ns\nq\n")
        .unwrap();
    let o = child.wait_with_output().unwrap();
    let (out, err) = (
        String::from_utf8(o.stdout).unwrap(),
        String::from_utf8(o.stderr).unwrap(),
    );
    assert!(o.status.success(), "{err}");
    assert_eq!(
        out,
        "classifier checked 7 bookmarks for #music: 0 new suggestions\n\
         #music: 1 accepted, 1 rejected, 2 still pending\n"
    );
    assert!(err.contains("#music (model 0.45, reviewed)?"), "{err}");
    assert!(err.contains("stopped"), "{err}");
    let (_, out, _) = shlv(&db, &["show", "2"]);
    assert!(out.contains("tags:     music rust"), "{out}");
    // The rejected one isn't offered again; the skipped one still is.
    let (_, out, _) = shlv(&db, &["suggest", "--tag", "music", "4", "5"]);
    assert_eq!(
        out,
        "classifier checked 2 bookmarks for #music: 0 new suggestions\n\
         1 candidate for #music, 0 of them reviewed (listed first):\n\
         5  Music festivals in Europe  https://e.com/5  #music (model 0.45)\n\
         (shlv suggest --tag music --pick ... to go through them, or suggest accept/reject ID \
         music)\n"
    );
    let (ok, _, err) = shlv(&db, &["suggest", "--tag", "rock", "--all"]);
    assert!(!ok && err.contains("no tag \"rock\""), "{err}");
    let (ok, _, err) = shlv(&db, &["suggest", "--pick", "--all"]);
    assert!(!ok && err.contains("--tag"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}
