//! Review rounds (task 039): decisions, the queue and its filters, history, and the CLI session.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use shlv::review::{BatchStats, Decision, ReviewFilter};
use shlv::{DomainRules, NewBookmark, ProposalState, Source, Store};

fn add(s: &mut Store, url: &str, title: &str, tags: &[&str]) -> i64 {
    s.add(&NewBookmark {
        url: url.into(),
        title: Some(title.into()),
        tags: tags.iter().map(|t| t.to_string()).collect(),
        ..Default::default()
    })
    .unwrap()
    .id
}

fn rules(pairs: &[(&str, &[&str])]) -> DomainRules {
    let map: BTreeMap<String, Vec<String>> = pairs
        .iter()
        .map(|(h, t)| (h.to_string(), t.iter().map(|t| t.to_string()).collect()))
        .collect();
    DomainRules::new(map).unwrap()
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn store() -> Store {
    let mut s = Store::open_in_memory().unwrap();
    s.add_canonical(&strings(&["code", "rust", "docs", "cooking"]))
        .unwrap();
    s.set_rules(rules(&[("github.com", &["code", "docs"])]));
    add(&mut s, "https://github.com/rust-lang/rust", "Rust", &[]); // 1
    add(&mut s, "https://github.com/tokio-rs/tokio", "Tokio", &[]); // 2
    add(&mut s, "https://example.com/wifi", "Fix wifi", &["misc"]); // 3
    add(&mut s, "https://cooking.com/pasta", "Pasta", &["cooking"]); // 4
    s
}

fn tags(s: &Store, id: i64) -> Vec<String> {
    s.get(id).unwrap().unwrap().tags
}

fn ids(q: &[shlv::review::Queued]) -> Vec<i64> {
    let mut v: Vec<i64> = q.iter().map(|q| q.id).collect();
    v.sort();
    v
}

#[test]
fn commits_a_decision_per_bookmark() {
    let mut s = store();
    s.apply_rules(None).unwrap();
    let (batch, started) = s.start_review_batch().unwrap();
    let d = Decision {
        accept: strings(&["code"]),
        reject: strings(&["docs"]),
        add: strings(&["rust"]),
        remove: vec![],
        propose: strings(&["Compilers", "rust"]),
    };
    let c = s.commit_review(batch, 1, &d).unwrap();
    assert_eq!(c.tagged, 2);
    assert_eq!(c.rejected, 1);
    assert_eq!(c.diverted.proposed, ["compilers"]);
    assert_eq!(tags(&s, 1), ["code", "rust"]);
    // The rejected suggestion isn't offered again; the bookmark is reviewed and leaves the queue.
    assert!(s.pending(Some(1)).unwrap().is_empty());
    assert_eq!(s.apply_rules(Some(&[1])).unwrap().pending, 0);
    assert!(s.is_reviewed(1).unwrap());
    let q = s.review_queue(&ReviewFilter::default()).unwrap();
    assert_eq!(ids(&q), [2, 3, 4]);
    assert_eq!(s.proposals_since(started).unwrap(), 1);
    assert_eq!(s.proposals_for(1).unwrap(), ["compilers"]);

    // Removing a tag; a bookmark can be reviewed with no tags at all.
    let c = s
        .commit_review(
            batch,
            3,
            &Decision {
                remove: strings(&["misc"]),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(c.removed, 1);
    assert!(tags(&s, 3).is_empty());
    assert!(s.is_reviewed(3).unwrap());

    // Failures change nothing: a tag off the list, a tag that isn't pending, a missing bookmark.
    for d in [
        Decision {
            accept: strings(&["code"]),
            add: strings(&["misc"]),
            ..Default::default()
        },
        Decision {
            accept: strings(&["rust"]),
            ..Default::default()
        },
    ] {
        assert!(s.commit_review(batch, 2, &d).is_err());
        assert!(!s.is_reviewed(2).unwrap());
        assert!(tags(&s, 2).is_empty());
    }
    assert!(s.commit_review(batch, 99, &Decision::default()).is_err());
    // A typed tag that is also suggested takes the suggestion, with its source.
    s.commit_review(
        batch,
        2,
        &Decision {
            add: strings(&["code"]),
            reject: strings(&["docs"]),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(s.pending(Some(2)).unwrap().is_empty());
    assert_eq!(tags(&s, 2), ["code"]);

    // Deleting a bookmark forgets its review.
    s.delete(4).unwrap();
    assert!(s.review_queue(&ReviewFilter::default()).unwrap().is_empty());
}

#[test]
fn queue_and_filters() {
    let mut s = store();
    s.apply_rules(None).unwrap();
    s.offer_choices(
        3,
        Source::Model,
        &[shlv::choose::Choice {
            tag: "docs".into(),
            confidence: Some(0.6),
        }],
    )
    .unwrap();
    s.offer_choices(
        4,
        Source::Model,
        &[shlv::choose::Choice {
            tag: "rust".into(),
            confidence: Some(0.9),
        }],
    )
    .unwrap();
    let all = s.review_queue(&ReviewFilter::default()).unwrap();
    assert_eq!(ids(&all), [1, 2, 3, 4]);
    let tagged: Vec<i64> = all.iter().filter(|q| q.tagged).map(|q| q.id).collect();
    assert_eq!(tagged, [4]);
    assert_eq!(all.iter().find(|q| q.id == 1).unwrap().host, "github.com");

    fn f(s: &Store, tag: Option<&str>, source: Option<Source>, below: Option<f64>) -> Vec<i64> {
        let q = s
            .review_queue(&ReviewFilter {
                tag: tag.map(String::from),
                source,
                below,
            })
            .unwrap();
        ids(&q)
    }
    assert_eq!(f(&s, Some("docs"), None, None), [1, 2, 3]);
    assert_eq!(f(&s, None, Some(Source::Model), None), [3, 4]);
    assert_eq!(f(&s, Some("docs"), Some(Source::Model), None), [3]);
    assert_eq!(f(&s, None, Some(Source::Model), Some(0.7)), [3]);
    // --below alone keeps unreviewed bookmarks: rules' 1.0 is out, none counts as 0.
    assert_eq!(f(&s, None, None, Some(0.95)), [3, 4]);
    assert!(
        s.review_queue(&ReviewFilter {
            tag: Some("nope".into()),
            ..Default::default()
        })
        .is_err()
    );

    // With a suggestion filter, reviewed bookmarks with pending suggestions still match.
    let (batch, _) = s.start_review_batch().unwrap();
    s.commit_review(batch, 3, &Decision::default()).unwrap();
    s.offer_choices(
        3,
        Source::Model,
        &[shlv::choose::Choice {
            tag: "code".into(),
            confidence: Some(0.55),
        }],
    )
    .unwrap();
    assert_eq!(f(&s, None, Some(Source::Model), None), [3, 4]);
    assert_eq!(
        ids(&s.review_queue(&ReviewFilter::default()).unwrap()),
        [1, 2, 4]
    );
    assert_eq!(s.reviewed_ids().unwrap(), [4, 3]);
    assert_eq!(s.reviewed_hosts().unwrap()["example.com"], 1);
}

#[test]
fn history() {
    let mut s = store();
    let (a, _) = s.start_review_batch().unwrap();
    let st = BatchStats {
        reviewed: 30,
        skipped: 2,
        proposed: 7,
        model_shown: 20,
        model_accepted: 12,
        tags_added: 3,
    };
    s.finish_review_batch(a, &st).unwrap();
    // Unfinished batches and ones that reviewed nothing aren't history.
    s.start_review_batch().unwrap();
    let (c, _) = s.start_review_batch().unwrap();
    s.finish_review_batch(c, &BatchStats::default()).unwrap();
    let h = s.review_history().unwrap();
    assert_eq!(h.len(), 1);
    assert_eq!(h[0].stats, st);
    assert!(s.finish_review_batch(99, &st).is_err());
}

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("shlv-review-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Runs shlv with `answers` on stdin; (success, stdout, stderr).
fn shlv(dir: &Path, args: &[&str], answers: &str) -> (bool, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_shlv"))
        .args(args)
        .env("SHLV_DB", dir.join("db"))
        .env("SHLV_CONFIG", dir.join("config.toml"))
        .env("BROWSER", "true")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(answers.as_bytes())
        .unwrap();
    let o = child.wait_with_output().unwrap();
    (
        o.status.success(),
        String::from_utf8(o.stdout).unwrap(),
        String::from_utf8(o.stderr).unwrap(),
    )
}

#[test]
fn cli_session() {
    let dir = tmp("session");
    std::fs::write(
        dir.join("config.toml"),
        "[tags.rules]\n\"github.com\" = [\"code\"]\n",
    )
    .unwrap();
    {
        let mut s = Store::open(&dir.join("db")).unwrap();
        add(
            &mut s,
            "https://github.com/rust-lang/rust",
            "Rust compiler",
            &[],
        );
        add(
            &mut s,
            "https://github.com/tokio-rs/tokio",
            "Tokio runtime",
            &[],
        );
        add(&mut s, "https://example.com/wifi", "Fix home wifi", &[]);
        add(&mut s, "https://cooking.com/pasta", "Pasta recipe", &[]);
        add(&mut s, "https://cooking.com/soup", "Soup", &[]);
        s.add_canonical(&strings(&["code", "rust"])).unwrap();
    }
    let (ok, out, _) = shlv(&dir, &["review", "--history"], "");
    assert!(ok);
    assert_eq!(out, "no review batches yet\n");

    // Batch 1 (3 bookmarks, across domains: cooking.com and github.com have two each, so one of
    // each first, newest first, then example.com):
    // - 5 Soup: open it, then add a new name, confirmed, and save.
    // - 2 Tokio: #code from the rule; add #rust by prefix, then uncheck #code, then save.
    // - 3 wifi: Enter with nothing, then Enter again to save it without tags.
    // Then the proposals screen adds #cooking, and the next-batch prompt goes on to batch 2:
    // - 4 Pasta: skip.  - 1 Rust: q ends the batch.
    let answers = "o\nt\ncooking\ny\n\n\
                   t\nru\n1\n\n\
                   \n\n\
                   a\n\
                   \n\
                   s\nq\n";
    let (ok, out, err) = shlv(&dir, &["review", "-n", "3"], answers);
    assert!(ok, "{err}");
    assert_eq!(out, "reviewed 3 bookmarks in 2 batches, 1 skipped\n");
    for want in [
        "batch 1: 3 of 5 to review, spread across domains, untagged first",
        "[2/3] 2  Tokio runtime",
        "  1 [x] #code  rule",
        "  2 [x] #rust  added",
        "  1 [ ] #code  rule",
        "[2/3] 2  Tokio runtime  #rust  (not #code)",
        "  1 [x] #cooking  new: proposed for the list",
        "[1/3] 5  Soup  +#cooking proposed",
        "no tags: Enter again",
        "[3/3] 3  Fix home wifi  no tags",
        "1 name for the list, from this batch",
        "#cooking  on 1 bookmark  (user)  added, tagged 1 bookmark",
        "batch 2: 2 of 2 to review",
        "[1/2] 4  Pasta recipe  skipped",
        "[2/2] 1  Rust compiler  stopped",
    ] {
        assert!(err.contains(want), "missing {want:?} in:\n{err}");
    }
    let pos = |s: &str| err.find(s).unwrap();
    assert!(pos("[1/3] 5") < pos("[2/3] 2") && pos("[2/3] 2") < pos("[3/3] 3"));
    #[cfg(feature = "model")]
    {
        // Back-fill for the new tag on reviewed bookmarks, then a retrain.
        assert!(
            err.contains("back-fill #cooking on reviewed bookmarks"),
            "{err}"
        );
        assert!(err.contains("classifier retrained on"), "{err}");
    }

    let s = Store::open(&dir.join("db")).unwrap();
    assert_eq!(tags(&s, 2), ["rust"]);
    assert_eq!(tags(&s, 5), ["cooking"]);
    assert!(tags(&s, 3).is_empty() && s.is_reviewed(3).unwrap());
    assert!(!s.is_reviewed(4).unwrap() && !s.is_reviewed(1).unwrap());
    assert!(s.proposals(ProposalState::Pending).unwrap().is_empty());
    let h = s.review_history().unwrap();
    assert_eq!(h.len(), 1);
    assert_eq!(
        (
            h[0].stats.reviewed,
            h[0].stats.proposed,
            h[0].stats.tags_added
        ),
        (3, 1, 1)
    );
    drop(s);
    let (_, out, _) = shlv(&dir, &["review", "--history"], "");
    assert_eq!(
        out,
        "batch  reviewed  skipped  proposed  added  classifier accepted\n    \
         1         3        0         1      1  -\n"
    );
    // Filters: only the pending #code suggestion on 1 matches; Ctrl-C isn't available in line
    // mode, so end of input stops.
    let (ok, out, err) = shlv(&dir, &["review", "--tag", "code"], "\n");
    assert!(ok, "{err}");
    assert!(err.contains("least confident suggestions first"), "{err}");
    assert!(err.contains("Rust compiler  #code"), "{err}");
    assert_eq!(out, "reviewed 1 bookmark in 1 batch\n");
    let (ok, _, err) = shlv(&dir, &["review", "--tag", "nope"], "");
    assert!(!ok && err.contains("no tag \"nope\""), "{err}");
    let (ok, _, err) = shlv(&dir, &["review", "--below", "2"], "");
    assert!(!ok, "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cli_alias_and_reject_proposals() {
    let dir = tmp("proposals");
    {
        let mut s = Store::open(&dir.join("db")).unwrap();
        add(&mut s, "https://a.com/1", "One", &[]);
        add(&mut s, "https://b.com/2", "Two", &[]);
        s.add_canonical(&strings(&["programming"])).unwrap();
    }
    // 1: propose "coding"; 2: propose "junk". Then alias coding to programming (by prefix),
    // reject junk.
    let answers = "t\ncoding\ny\n\nt\njunk\ny\n\nl\nprog\nr\nq\n";
    let (ok, out, err) = shlv(&dir, &["review"], answers);
    assert!(ok, "{err}");
    assert_eq!(out, "reviewed 2 bookmarks in 1 batch\n");
    assert!(
        err.contains("alias of #programming, tagged 1 bookmark"),
        "{err}"
    );
    assert!(
        err.contains("#junk  on 1 bookmark  (user)  rejected"),
        "{err}"
    );
    let s = Store::open(&dir.join("db")).unwrap();
    assert_eq!(tags(&s, 1), ["programming"]);
    assert!(tags(&s, 2).is_empty());
    assert_eq!(
        s.proposals(ProposalState::Rejected).unwrap()[0].name,
        "junk"
    );
    drop(s);
    let (ok, _, err) = shlv(&dir, &["review"], "");
    assert!(ok && err.contains("every bookmark is reviewed"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A tag added after a batch is offered on the bookmarks reviewed before it existed.
#[cfg(feature = "model")]
#[test]
fn cli_backfills_new_tags() {
    let dir = tmp("backfill");
    {
        let mut s = Store::open(&dir.join("db")).unwrap();
        add(
            &mut s,
            "https://a.com/1",
            "Music theory for programmers",
            &[],
        );
        add(&mut s, "https://b.com/2", "Jazz albums", &[]);
        s.add_canonical(&strings(&["code"])).unwrap();
    }
    // 1: saved without tags (Enter twice); 2: propose #music. Add it; back-fill offers it on 1
    // (its name is in the title): yes. Then stop.
    let answers = "\n\nt\nmusic\ny\n\na\ny\nq\n";
    let (ok, out, err) = shlv(&dir, &["review"], answers);
    assert!(ok, "{err}");
    assert_eq!(out, "reviewed 2 bookmarks in 1 batch\n");
    assert!(
        err.contains("back-fill #music on reviewed bookmarks"),
        "{err}"
    );
    assert!(err.contains("#music (model 0.45, reviewed)?"), "{err}");
    assert!(
        err.contains("#music: 1 accepted, 0 rejected, 0 still pending"),
        "{err}"
    );
    let s = Store::open(&dir.join("db")).unwrap();
    assert_eq!(tags(&s, 1), ["music"]);
    assert_eq!(tags(&s, 2), ["music"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(feature = "model")]
#[test]
fn least_sure_first() {
    let mut s = store();
    let c = s.classifier(true).unwrap();
    // "Rust" names a tag (lexical 0.45, near 0.5); "Fix wifi" matches nothing.
    let scores = s.review_confidence(&[1, 3, 99], &c).unwrap();
    assert_eq!(scores.len(), 2);
    assert_eq!(shlv::review::least_confident(scores, 2), [1, 3]);
}
