//! Tag suggestions through the library: domain rules (on add, import, and in bulk), confirm and
//! auto modes, reject memory.

use std::collections::BTreeMap;

use shlv::{
    DomainRules, Edit, ImportItem, ImportOptions, NewBookmark, NonCanonicalRuleTags,
    NonCanonicalTags, RuleStats, Source, Store, SuggestMode,
};

fn rules(pairs: &[(&str, &[&str])]) -> DomainRules {
    DomainRules::new(
        pairs
            .iter()
            .map(|(h, t)| (h.to_string(), t.iter().map(|t| t.to_string()).collect()))
            .collect::<BTreeMap<_, _>>(),
    )
    .unwrap()
}

fn store(mode: SuggestMode) -> Store {
    let mut s = Store::open_in_memory().unwrap();
    s.set_rules(rules(&[
        ("github.com", &["code"]),
        ("docs.rs", &["rust", "docs"]),
    ]));
    s.set_suggest_mode(mode);
    s
}

fn add(s: &mut Store, url: &str, tags: &[&str]) -> i64 {
    s.add(&NewBookmark {
        url: url.into(),
        tags: tags.iter().map(|t| t.to_string()).collect(),
        ..Default::default()
    })
    .unwrap()
    .id
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// Pending suggestions as (bookmark id, tag).
fn pending(s: &Store) -> Vec<(i64, String)> {
    s.pending(None)
        .unwrap()
        .into_iter()
        .map(|p| (p.id, p.tag))
        .collect()
}

fn tags(s: &Store, id: i64) -> Vec<String> {
    s.get(id).unwrap().unwrap().tags
}

/// A bookmark's tags with their source and confidence, by name.
fn sources(s: &Store, id: i64) -> Vec<(String, String, Option<f64>)> {
    let mut stmt = s
        .conn()
        .prepare(
            "SELECT t.name, bt.source, bt.confidence FROM bookmark_tag bt \
             JOIN tag t ON t.id = bt.tag_id WHERE bt.bookmark_id = ?1 ORDER BY t.name",
        )
        .unwrap();
    stmt.query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn changes(s: &Store) -> u64 {
    s.conn().total_changes()
}

#[test]
fn confirm_mode_records_suggestions_on_add() {
    let mut s = store(SuggestMode::Confirm);
    let a = add(&mut s, "https://gist.github.com/x", &["mine"]);
    let b = add(&mut s, "docs.rs/serde", &["rust"]); // a user tag is never also suggested
    let c = add(&mut s, "https://example.com/github.com", &[]);

    assert_eq!(tags(&s, a), ["mine"]);
    assert_eq!(tags(&s, b), ["rust"]);
    assert_eq!(pending(&s), [(b, "docs".into()), (a, "code".into())]);
    assert!(s.pending(Some(c)).unwrap().is_empty());
    let p = &s.pending(Some(a)).unwrap()[0];
    assert_eq!(
        (p.source, p.confidence, p.url.as_str()),
        (Source::Rule, Some(1.0), "https://gist.github.com/x")
    );

    // Adding the same URL again doesn't duplicate anything.
    add(&mut s, "https://gist.github.com/x", &[]);
    assert_eq!(pending(&s).len(), 2);
}

#[test]
fn accepting_keeps_the_source_and_updates_the_bookmark() {
    let mut s = store(SuggestMode::Confirm);
    let a = add(&mut s, "https://docs.rs/tokio", &["async"]);
    let before = s.get(a).unwrap().unwrap().updated_at;
    std::thread::sleep(std::time::Duration::from_millis(2));

    assert_eq!(s.accept(a, &strings(&["docs"])).unwrap(), ["docs"]);
    assert_eq!(
        sources(&s, a),
        [
            ("async".into(), "user".into(), None),
            ("docs".into(), "rule".into(), Some(1.0)),
        ]
    );
    assert_eq!(pending(&s), [(a, "rust".into())]);
    assert!(s.get(a).unwrap().unwrap().updated_at > before);
    assert_eq!(s.search("#docs", 10).unwrap().len(), 1);

    // Only pending tags can be accepted; no pending tags at all is an error too.
    let e = s.accept(a, &strings(&["docs"])).unwrap_err().to_string();
    assert_eq!(
        e,
        "cannot accept \"docs\": not suggested for bookmark 1 (pending: rust)"
    );
    assert_eq!(s.accept(a, &[]).unwrap(), ["rust"]);
    let e = s.accept(a, &[]).unwrap_err().to_string();
    assert_eq!(e, "no pending suggestions for bookmark 1");
}

#[test]
fn accept_all_in_one_go() {
    let mut s = store(SuggestMode::Confirm);
    let a = add(&mut s, "https://github.com/a", &[]);
    let b = add(&mut s, "https://docs.rs/b", &[]);
    // A tag added by hand while pending isn't counted again.
    s.edit(
        b,
        &Edit {
            add_tags: strings(&["rust"]),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(pending(&s), [(b, "docs".into()), (a, "code".into())]);
    assert_eq!(s.accept_all().unwrap(), (2, 2));
    assert_eq!(tags(&s, a), ["code"]);
    assert_eq!(tags(&s, b), ["docs", "rust"]);
    assert_eq!(sources(&s, b)[1], ("rust".into(), "user".into(), None));
    assert!(pending(&s).is_empty());
    assert_eq!(s.accept_all().unwrap(), (0, 0));
}

#[test]
fn rejected_suggestions_are_not_offered_again() {
    let mut s = store(SuggestMode::Confirm);
    let a = add(&mut s, "https://docs.rs/a", &[]);
    assert_eq!(s.reject(a, &strings(&["docs"])).unwrap(), ["docs"]);
    assert_eq!(pending(&s), [(a, "rust".into())]);

    // Not by running the rules again, re-adding the URL, or switching to auto mode.
    let stats = s.apply_rules(None).unwrap();
    assert_eq!((stats.checked, stats.matched, stats.pending), (1, 1, 0));
    add(&mut s, "https://docs.rs/a", &[]);
    s.set_suggest_mode(SuggestMode::Auto);
    let stats = s.apply_rules(Some(&[a])).unwrap();
    assert_eq!((stats.applied, stats.tagged), (1, 1));
    assert_eq!(tags(&s, a), ["rust"]); // the pending one was applied, the rejected one wasn't
    assert!(pending(&s).is_empty());

    // Rejection is per bookmark.
    let b = add(&mut s, "https://docs.rs/b", &[]);
    assert_eq!(tags(&s, b), ["docs", "rust"]);

    // Rejecting with no tags rejects them all; then there's nothing left to act on.
    s.set_suggest_mode(SuggestMode::Confirm);
    let c = add(&mut s, "https://docs.rs/c", &[]);
    assert_eq!(s.reject(c, &[]).unwrap(), ["docs", "rust"]);
    assert!(s.reject(c, &[]).is_err());
    s.apply_rules(None).unwrap();
    assert!(pending(&s).is_empty());
}

#[test]
fn removing_an_applied_tag_rejects_it() {
    let mut s = store(SuggestMode::Auto);
    let a = add(&mut s, "https://github.com/a", &["mine"]);
    assert_eq!(
        sources(&s, a),
        [
            ("code".into(), "rule".into(), Some(1.0)),
            ("mine".into(), "user".into(), None),
        ]
    );
    let remove = |t: &str| Edit {
        remove_tags: strings(&[t]),
        ..Default::default()
    };
    s.edit(a, &remove("code")).unwrap();
    s.apply_rules(None).unwrap();
    add(&mut s, "https://github.com/a", &[]);
    assert_eq!(tags(&s, a), ["mine"]);

    // A user tag removed by hand isn't remembered: the rule may offer it later.
    let b = add(&mut s, "https://example.com/", &["code"]);
    s.edit(b, &remove("code")).unwrap();
    let n: i64 = s
        .conn()
        .query_row(
            "SELECT count(*) FROM tag_suggestion WHERE bookmark_id = ?1",
            [b],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn auto_mode_applies_on_add_and_for_all() {
    let mut s = Store::open_in_memory().unwrap();
    let a = add(&mut s, "https://github.com/a", &[]);
    let b = add(&mut s, "https://docs.rs/b", &["docs"]);
    add(&mut s, "https://example.com/", &[]);
    // No rules: nothing happens.
    assert_eq!(s.apply_rules(None).unwrap().matched, 0);
    s.set_rules(rules(&[
        ("github.com", &["code"]),
        ("docs.rs", &["rust", "docs"]),
    ]));
    s.set_suggest_mode(SuggestMode::Auto);

    let before = changes(&s);
    let stats = s.apply_rules(None).unwrap();
    assert_eq!(
        (
            stats.checked,
            stats.matched,
            stats.applied,
            stats.tagged,
            stats.pending
        ),
        (3, 2, 2, 2, 0)
    );
    assert!(changes(&s) > before);
    assert_eq!(tags(&s, a), ["code"]);
    assert_eq!(tags(&s, b), ["docs", "rust"]);
    assert_eq!(s.search("#code", 10).unwrap()[0].id, a);
    assert!(pending(&s).is_empty());

    // Again: nothing new, nothing written to bookmarks.
    let stats = s.apply_rules(None).unwrap();
    assert_eq!((stats.applied, stats.tagged), (0, 0));

    let e = s.apply_rules(Some(&[a, 99])).unwrap_err().to_string();
    assert_eq!(e, "no bookmark with id 99");
}

#[test]
fn strict_mode_checks_every_rule_tag() {
    let mut s = Store::open_in_memory().unwrap();
    s.add_canonical(&strings(&["javascript", "code"])).unwrap();
    s.add_alias("js", "javascript").unwrap();
    s.set_strict_tags(true);
    s.set_rules(rules(&[
        ("github.com", &["code"]),
        ("nodejs.org", &["js", "node"]),
        ("docs.rs", &["rust"]),
    ]));

    // Even a URL no rule matches fails, so a bad rule shows up right away; nothing is written.
    let before = changes(&s);
    let e = s
        .add(&NewBookmark {
            url: "https://example.com/".into(),
            ..Default::default()
        })
        .unwrap_err();
    let bad = e.downcast_ref::<NonCanonicalRuleTags>().unwrap();
    assert_eq!(
        bad.0,
        [
            ("docs.rs".to_string(), "rust".to_string()),
            ("nodejs.org".into(), "node".into())
        ]
    );
    assert_eq!(
        e.to_string(),
        "[tags.rules] uses tags that aren't canonical (strict_tags is on): rust (docs.rs), \
         node (nodejs.org)"
    );
    assert!(s.apply_rules(None).is_err());
    assert_eq!(changes(&s), before);

    // Aliases are fine, and resolve to their tag.
    s.set_rules(rules(&[("github.com", &["code"]), ("nodejs.org", &["js"])]));
    let a = add(&mut s, "https://nodejs.org/en", &[]);
    assert_eq!(pending(&s), [(a, "javascript".into())]);

    // A tag that stops being canonical while pending can't be accepted in strict mode.
    s.remove_canonical(&strings(&["javascript"])).unwrap();
    let e = s.accept(a, &[]).unwrap_err();
    assert_eq!(
        e.downcast_ref::<NonCanonicalTags>().unwrap().0,
        ["javascript"]
    );
    let e = s.accept_all().unwrap_err();
    assert!(e.downcast_ref::<NonCanonicalTags>().is_some());
    assert_eq!(pending(&s).len(), 1);
}

#[test]
fn accept_and_reject_take_aliases() {
    let mut s = store(SuggestMode::Confirm);
    let a = add(&mut s, "https://docs.rs/a", &[]);
    s.add_alias("rs", "rust").unwrap();
    assert_eq!(s.reject(a, &strings(&["rs"])).unwrap(), ["rust"]);
    assert_eq!(s.accept(a, &strings(&["#Docs"])).unwrap(), ["docs"]);
}

#[test]
fn merges_and_deletes_carry_suggestions() {
    let mut s = Store::open_in_memory().unwrap();
    s.set_rules(rules(&[
        ("github.com", &["coding"]),
        ("gitlab.com", &["code"]),
    ]));
    let a = add(&mut s, "https://github.com/a", &[]);
    let b = add(&mut s, "https://github.com/b", &[]);
    let c = add(&mut s, "https://gitlab.com/c", &[]);
    s.reject(b, &[]).unwrap();
    s.merge_tags("coding", "code").unwrap();
    assert_eq!(pending(&s), [(c, "code".into()), (a, "code".into())]);
    // b's rejection moved with the tag, and the rule's alias resolves to it.
    s.apply_rules(None).unwrap();
    assert_eq!(pending(&s).len(), 2);

    s.delete(a).unwrap();
    assert_eq!(pending(&s), [(c, "code".into())]);
    let n: i64 = s
        .conn()
        .query_row(
            "SELECT count(*) FROM tag_suggestion WHERE bookmark_id = ?1",
            [a],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn schema_takes_model_and_llm_suggestions() {
    let mut s = store(SuggestMode::Confirm);
    let a = add(&mut s, "https://example.com/", &["web"]);
    let c = s.conn();
    c.execute_batch(
        "INSERT INTO tag (uid, name, updated_at) VALUES ('u1', 'ml', 0), ('u2', 'ai', 0);
         INSERT INTO tag_suggestion (bookmark_id, tag_id, source, confidence, updated_at)
           SELECT 1, id, 'model', 0.82, 0 FROM tag WHERE name = 'ml';
         INSERT INTO tag_suggestion (bookmark_id, tag_id, source, confidence, updated_at)
           SELECT 1, id, 'llm', NULL, 0 FROM tag WHERE name = 'ai';",
    )
    .unwrap();
    let p = s.pending(Some(a)).unwrap();
    let got: Vec<_> = p
        .iter()
        .map(|p| (p.tag.as_str(), p.source, p.confidence))
        .collect();
    assert_eq!(
        got,
        [("ai", Source::Llm, None), ("ml", Source::Model, Some(0.82))]
    );
    assert!(
        c.execute(
            "INSERT INTO tag_suggestion (bookmark_id, tag_id, source, updated_at) \
             SELECT 1, id, 'user', 0 FROM tag WHERE name = 'web'",
            [],
        )
        .is_err()
    );
    s.accept(a, &[]).unwrap();
    assert_eq!(
        sources(&s, a),
        [
            ("ai".into(), "llm".into(), None),
            ("ml".into(), "model".into(), Some(0.82)),
            ("web".into(), "user".into(), None),
        ]
    );
}

fn item(url: &str, tags: &[&str]) -> ImportItem {
    ImportItem {
        url: url.into(),
        tags: strings(tags),
        created_at: Some(1_000),
        ..Default::default()
    }
}

#[test]
fn import_runs_the_rules() {
    let mut s = store(SuggestMode::Confirm);
    let old = add(&mut s, "https://github.com/old", &["code"]);
    let items = [
        item("https://github.com/old", &[]), // already has the rule's tag
        item("https://docs.rs/a", &["docs"]), // an imported tag is never also suggested
        item("https://docs.rs/a#frag", &[]), // a duplicate counts once
        item("https://example.com/", &[]),
    ];
    let sum = s.import(&items, ImportOptions::default()).unwrap();
    assert_eq!((sum.added, sum.updated), (2, 2));
    let r = sum.rules;
    assert_eq!((r.checked, r.matched, r.pending), (3, 2, 1));
    let a = s.find_by_url("https://docs.rs/a").unwrap().unwrap().id;
    assert_eq!(pending(&s), [(a, "rust".into())]);
    assert_eq!(tags(&s, old), ["code"]);

    // Rejections hold on re-import.
    s.reject(a, &[]).unwrap();
    let r = s.import(&items, ImportOptions::default()).unwrap().rules;
    assert_eq!((r.matched, r.pending), (2, 0));
    assert!(pending(&s).is_empty());

    // Skipped on request.
    let skip = ImportOptions {
        skip_rules: true,
        ..Default::default()
    };
    let sum = s.import(&[item("https://docs.rs/b", &[])], skip).unwrap();
    assert_eq!(sum.rules, RuleStats::default());
    assert!(pending(&s).is_empty());
}

#[test]
fn import_applies_rules_in_auto_mode() {
    let mut s = store(SuggestMode::Auto);
    let sum = s
        .import(
            &[
                item("https://docs.rs/a", &[]),
                item("https://github.com/b", &["mine"]),
            ],
            ImportOptions::default(),
        )
        .unwrap();
    let r = sum.rules;
    assert_eq!((r.matched, r.applied, r.tagged, r.pending), (2, 3, 2, 0));
    let a = s.find_by_url("https://docs.rs/a").unwrap().unwrap();
    assert_eq!(a.tags, ["docs", "rust"]);
    // Part of the import's write: the imported time stays.
    assert_eq!(a.updated_at, 1_000);
    assert_eq!(s.search("#rust", 10).unwrap()[0].id, a.id);
    let b = s.find_by_url("https://github.com/b").unwrap().unwrap().id;
    assert_eq!(
        sources(&s, b),
        [
            ("code".into(), "rule".into(), Some(1.0)),
            ("mine".into(), "user".into(), None),
        ]
    );
}

#[test]
fn import_checks_rule_tags_in_strict_mode() {
    let mut s = Store::open_in_memory().unwrap();
    s.add_canonical(&strings(&["code"])).unwrap();
    s.set_strict_tags(true);
    s.set_rules(rules(&[("github.com", &["code"]), ("docs.rs", &["rust"])]));
    let items = [item("https://example.com/", &["code"])];

    let before = changes(&s);
    let e = s.import(&items, ImportOptions::default()).unwrap_err();
    assert!(e.downcast_ref::<NonCanonicalRuleTags>().is_some());
    assert_eq!(changes(&s), before);

    let skip = ImportOptions {
        skip_rules: true,
        ..Default::default()
    };
    assert_eq!(s.import(&items, skip).unwrap().added, 1);
}
