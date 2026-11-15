//! Tag management through the library: list, rename, merge, aliases, canonical list.

use shlv::tag::{format_canonical, parse_canonical};
use shlv::{CanonicalEntry, NewBookmark, Store, TagInfo, TagSort};

fn store() -> Store {
    Store::open_in_memory().unwrap()
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

fn tag(s: &Store, name: &str) -> TagInfo {
    s.tags(TagSort::Name)
        .unwrap()
        .into_iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("no live tag {name}"))
}

fn names(s: &Store) -> Vec<String> {
    s.tags(TagSort::Name)
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect()
}

fn tags_of(s: &Store, id: i64) -> Vec<String> {
    s.get(id).unwrap().unwrap().tags
}

/// Ids matching a search, sorted.
fn found(s: &Store, q: &str) -> Vec<i64> {
    let mut ids: Vec<i64> = s.search(q, 100).unwrap().iter().map(|b| b.id).collect();
    ids.sort();
    ids
}

/// Sets every bookmark's and tag's `updated_at` to 0, so a bump is visible without sleeping.
fn age(s: &Store) {
    s.conn()
        .execute_batch("UPDATE bookmark SET updated_at = 0; UPDATE tag SET updated_at = 0;")
        .unwrap();
}

fn updated(s: &Store, id: i64) -> i64 {
    s.get(id).unwrap().unwrap().updated_at
}

/// (deleted_at, uid) of a tag row by name, live or not.
fn row(s: &Store, name: &str) -> Option<(Option<i64>, String)> {
    s.conn()
        .query_row(
            "SELECT deleted_at, uid FROM tag WHERE name = ?1",
            [name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok()
}

fn err(r: anyhow::Result<impl std::fmt::Debug>) -> String {
    format!("{:#}", r.unwrap_err())
}

#[test]
fn ls_counts_marks_and_sorts() {
    let mut s = store();
    add(&mut s, "a.com", &["rust", "web"]);
    add(&mut s, "b.com", &["rust"]);
    let gone = add(&mut s, "c.com", &["rust", "zig"]);
    s.delete(gone).unwrap();
    s.add_canonical(&strings(&["web", "empty"])).unwrap();
    s.add_alias("rs", "rust").unwrap();
    s.add_alias("rust-lang", "rust").unwrap();

    let all = s.tags(TagSort::Name).unwrap();
    let summary: Vec<_> = all
        .iter()
        .map(|t| (t.name.as_str(), t.count, t.canonical, t.aliases.join(",")))
        .collect();
    assert_eq!(
        summary,
        [
            ("empty", 0, true, String::new()),
            ("rust", 2, false, "rs,rust-lang".into()),
            ("web", 1, true, String::new()),
            // Its only bookmark was deleted; the tag stays live.
            ("zig", 0, false, String::new()),
        ]
    );
    assert_eq!(all[1].uid.len(), 36);

    let by_count: Vec<_> = s
        .tags(TagSort::Count)
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(by_count, ["rust", "web", "empty", "zig"]);
}

#[test]
fn rename_keeps_row_and_touches_bookmarks() {
    let mut s = store();
    let a = add(&mut s, "a.com", &["rust", "web"]);
    let b = add(&mut s, "b.com", &["web"]);
    s.add_alias("rs", "rust").unwrap();
    let before = tag(&s, "rust");
    age(&s);

    let r = s.rename_tag("Rust", "Rust Lang").unwrap();
    assert_eq!((r.tag.as_str(), r.bookmarks), ("rust-lang", 1));
    let after = tag(&s, "rust-lang");
    assert_eq!(after.uid, before.uid);
    assert_eq!(after.aliases, ["rs"]);
    assert!(after.updated_at > 0);
    assert!(row(&s, "rust").is_none());

    assert_eq!(tags_of(&s, a), ["rust-lang", "web"]);
    assert!(updated(&s, a) > 0);
    assert_eq!(updated(&s, b), 0, "untouched bookmark");
    // The search index follows the new name.
    assert_eq!(found(&s, "rust"), [a], "prefix of rust-lang");
    assert_eq!(found(&s, "lang"), [a]);
    assert_eq!(found(&s, "#rust-lang"), [a]);
    assert_eq!(found(&s, "#rs"), [a]);
}

#[test]
fn rename_conflicts() {
    let mut s = store();
    add(&mut s, "a.com", &["rust", "web", "go"]);
    s.add_alias("golang", "go").unwrap();
    s.add_alias("rs", "rust").unwrap();

    let e = err(s.rename_tag("rust", "web"));
    assert!(
        e.contains("already exists") && e.contains("shlv tag merge rust web"),
        "{e}"
    );
    let e = err(s.rename_tag("rust", "golang"));
    assert!(e.contains("alias of \"go\""), "{e}");
    let e = err(s.rename_tag("rs", "x"));
    assert!(e.contains("\"rs\" is an alias of \"rust\""), "{e}");
    assert!(err(s.rename_tag("nope", "x")).contains("no tag"));
    assert!(err(s.rename_tag("rust", "rust")).contains("already named"));
    assert!(err(s.rename_tag("rust", " ")).contains("empty"));
    assert_eq!(names(&s), ["go", "rust", "web"]);

    // Renaming to the tag's own alias absorbs the alias.
    s.rename_tag("rust", "rs").unwrap();
    assert_eq!(tag(&s, "rs").aliases, Vec::<String>::new());
}

#[test]
fn merge_retags_tombstones_and_aliases() {
    let mut s = store();
    let a = add(&mut s, "a.com", &["js"]);
    let b = add(&mut s, "b.com", &["js", "javascript"]);
    let c = add(&mut s, "c.com", &["javascript"]);
    let d = add(&mut s, "d.com", &["web"]);
    s.add_alias("ecmascript", "js").unwrap();
    s.add_alias("node-js", "javascript").unwrap();
    s.add_canonical(&strings(&["js"])).unwrap();
    let js_uid = tag(&s, "js").uid;
    age(&s);

    let r = s.merge_tags("JS", "javascript").unwrap();
    assert_eq!((r.tag.as_str(), r.bookmarks), ("javascript", 2));

    for id in [a, b, c] {
        assert_eq!(tags_of(&s, id), ["javascript"], "bookmark {id}");
    }
    assert!(updated(&s, a) > 0 && updated(&s, b) > 0);
    assert_eq!(updated(&s, c), 0, "already had only the target tag");
    assert_eq!(updated(&s, d), 0);

    let js = tag(&s, "javascript");
    assert_eq!(js.count, 3);
    assert_eq!(js.aliases, ["ecmascript", "js", "node-js"]);
    assert!(js.canonical, "inherits canonical from the merged tag");
    assert!(js.updated_at > 0);
    assert_eq!(names(&s), ["javascript", "web"]);

    // FROM is a tombstone with its uid, bumped.
    let (deleted, uid) = row(&s, "js").unwrap();
    assert!(deleted.is_some());
    assert_eq!(uid, js_uid);

    // The index dropped the old name.
    assert_eq!(found(&s, "javascript"), [a, b, c]);
    assert_eq!(found(&s, "#js"), [a, b, c], "the alias resolves");
    assert!(
        s.search("js", 10).unwrap().is_empty(),
        "no row has the word js"
    );

    // Re-adding FROM resolves to the alias and leaves the tombstone alone.
    let e = add(&mut s, "e.com", &["js", "ecmascript"]);
    assert_eq!(tags_of(&s, e), ["javascript"]);
    assert!(row(&s, "js").unwrap().0.is_some(), "tombstone not revived");
    assert_eq!(names(&s), ["javascript", "web"]);
}

#[test]
fn merge_into_alias_and_errors() {
    let mut s = store();
    let a = add(&mut s, "a.com", &["py", "python", "web"]);
    s.add_alias("python3", "python").unwrap();
    s.merge_tags("py", "python3").unwrap();
    assert_eq!(tags_of(&s, a), ["python", "web"]);

    let e = err(s.merge_tags("web", "nope"));
    assert!(e.contains("no tag \"nope\"") && e.contains("rename"), "{e}");
    assert!(err(s.merge_tags("nope", "web")).contains("no tag \"nope\""));
    assert!(err(s.merge_tags("py", "web")).contains("alias of \"python\""));
    assert!(err(s.merge_tags("python", "python3")).contains("same tag"));
}

#[test]
fn rename_onto_a_merged_name_frees_the_tombstone() {
    let mut s = store();
    let a = add(&mut s, "a.com", &["js"]);
    s.merge_tags("js", "javascript").unwrap_err(); // No target yet.
    add(&mut s, "b.com", &["javascript"]);
    s.merge_tags("js", "javascript").unwrap();
    let (_, old_uid) = row(&s, "js").unwrap();
    let keep_uid = tag(&s, "javascript").uid;

    // Prefer the short name after all.
    s.rename_tag("javascript", "js").unwrap();
    let js = tag(&s, "js");
    assert_eq!(js.uid, keep_uid);
    assert!(js.aliases.is_empty());
    assert_eq!(js.count, 2);
    assert_eq!(tags_of(&s, a), ["js"]);
    // The old tombstone is still there, under its uid, for sync.
    let tomb: (Option<i64>, String) = s
        .conn()
        .query_row(
            "SELECT deleted_at, name FROM tag WHERE uid = ?1",
            [&old_uid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(tomb.0.is_some());
    assert_ne!(tomb.1, "js");
}

#[test]
fn aliases_add_and_remove() {
    let mut s = store();
    let a = add(&mut s, "a.com", &["rust", "go"]);
    age(&s);

    assert!(s.add_alias("RS", "rust").unwrap());
    assert!(tag(&s, "rust").updated_at > 0);
    assert!(!s.add_alias("rs", "rust").unwrap(), "already there");
    assert_eq!(updated(&s, a), 0, "aliases don't touch bookmarks");

    let e = err(s.add_alias("go", "rust"));
    assert!(
        e.contains("is a tag") && e.contains("shlv tag merge go rust"),
        "{e}"
    );
    let e = err(s.add_alias("rs", "go"));
    assert!(e.contains("already an alias of \"rust\""), "{e}");
    assert!(err(s.add_alias("rust", "rust")).contains("itself"));
    assert!(err(s.add_alias("x", "nope")).contains("no tag"));
    assert!(err(s.add_alias("x", "rs")).contains("alias of \"rust\""));

    // Aliases resolve on input.
    let b = add(&mut s, "b.com", &["rs"]);
    assert_eq!(tags_of(&s, b), ["rust"]);

    age(&s);
    assert_eq!(s.remove_alias("rs").unwrap().as_deref(), Some("rust"));
    assert!(tag(&s, "rust").updated_at > 0);
    assert_eq!(s.remove_alias("rs").unwrap(), None);
    // Without the alias, the name becomes a tag of its own.
    let c = add(&mut s, "c.com", &["rs"]);
    assert_eq!(tags_of(&s, c), ["rs"]);
}

#[test]
fn canonical_add_rm_ls() {
    let mut s = store();
    add(&mut s, "a.com", &["rust", "web"]);
    s.add_alias("rs", "rust").unwrap();
    age(&s);

    assert_eq!(s.add_canonical(&strings(&["rust", "Zig"])).unwrap(), 2);
    assert_eq!(s.add_canonical(&strings(&["rust"])).unwrap(), 0);
    let canon: Vec<_> = s
        .canonical_tags()
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(canon, ["rust", "zig"]);
    assert_eq!(tag(&s, "rust").aliases, ["rs"]);
    assert!(tag(&s, "rust").updated_at > 0);
    assert_eq!(tag(&s, "web").updated_at, 0);

    let e = err(s.add_canonical(&strings(&["web", "rs"])));
    assert!(e.contains("alias of \"rust\""), "{e}");
    assert!(!tag(&s, "web").canonical, "failed batch changes nothing");

    assert_eq!(s.remove_canonical(&strings(&["zig", "web"])).unwrap(), 1);
    assert!(err(s.remove_canonical(&strings(&["nope"]))).contains("no tag"));
    assert_eq!(s.canonical_tags().unwrap().len(), 1);
    assert_eq!(names(&s), ["rust", "web", "zig"], "unmarking keeps the tag");
}

#[test]
fn canonical_text_round_trip() {
    let text = "# languages\n\
                JavaScript js  ecmascript # the usual\n\
                \n   \n\
                rust\n\
                c# csharp\n\
                #go golang\n";
    let parsed = parse_canonical(text);
    let entry = |line, tag: &str, aliases: &[&str]| CanonicalEntry {
        line,
        tag: tag.into(),
        aliases: strings(aliases),
    };
    assert_eq!(
        parsed,
        [
            entry(2, "javascript", &["js", "ecmascript"]),
            entry(5, "rust", &[]),
            entry(6, "c#", &["csharp"]),
        ]
    );

    let mut s = store();
    let stats = s.import_canonical(&parsed).unwrap();
    assert_eq!((stats.tags, stats.marked, stats.aliases), (3, 3, 3));
    let out = format_canonical(&s.canonical_tags().unwrap());
    assert_eq!(out, "c# csharp\njavascript ecmascript js\nrust\n");
    assert_eq!(parse_canonical(&out).len(), 3);

    // Importing again changes nothing.
    let again = s.import_canonical(&parse_canonical(&out)).unwrap();
    assert_eq!((again.marked, again.aliases), (0, 0));
}

#[test]
fn import_adds_without_removing_and_is_atomic() {
    let mut s = store();
    add(&mut s, "a.com", &["rust", "js"]);
    s.add_canonical(&strings(&["rust"])).unwrap();
    s.add_alias("rs", "rust").unwrap();

    let stats = s
        .import_canonical(&parse_canonical("python py\nrust rust-lang\n"))
        .unwrap();
    assert_eq!((stats.marked, stats.aliases), (1, 2));
    assert_eq!(
        format_canonical(&s.canonical_tags().unwrap()),
        "python py\nrust rs rust-lang\n"
    );

    // An alias that's a live tag fails the whole import, naming the line.
    let e = err(s.import_canonical(&parse_canonical("go golang\njavascript js\n")));
    assert!(e.contains("line 2") && e.contains("\"js\" is a tag"), "{e}");
    assert!(!names(&s).contains(&"go".to_string()), "rolled back");

    // So does an alias that already belongs to another tag.
    let e = err(s.import_canonical(&parse_canonical("ruby rs\n")));
    assert!(
        e.contains("line 1") && e.contains("alias of \"rust\""),
        "{e}"
    );
}
