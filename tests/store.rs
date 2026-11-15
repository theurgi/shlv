use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use shlv::{Edit, ListQuery, NewBookmark, Store};

/// A temporary directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "shlv-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn open() -> (TempDir, Store) {
    let dir = TempDir::new();
    // A nested path checks that parent directories are created.
    let store = Store::open(&dir.0.join("a/b/shlv.db")).unwrap();
    (dir, store)
}

fn bm(url: &str, title: &str, tags: &[&str]) -> NewBookmark {
    NewBookmark {
        url: url.into(),
        title: Some(title.into()),
        note: None,
        tags: tags.iter().map(|t| t.to_string()).collect(),
    }
}

fn ids(v: &[shlv::Bookmark]) -> Vec<i64> {
    v.iter().map(|b| b.id).collect()
}

#[test]
fn opens_with_pragmas_and_schema() {
    let (_d, s) = open();
    let c = s.conn();
    let mode: String = c
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    let sync: i64 = c.query_row("PRAGMA synchronous", [], |r| r.get(0)).unwrap();
    assert_eq!(sync, 1); // NORMAL
    let fk: i64 = c
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fk, 1);
    let v: i64 = c
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 2);
}

#[test]
fn reopening_keeps_data_and_skips_migrations() {
    let dir = TempDir::new();
    let path = dir.0.join("shlv.db");
    let id = Store::open(&path)
        .unwrap()
        .add(&bm("https://a.com", "A", &[]))
        .unwrap()
        .id;
    let s = Store::open(&path).unwrap();
    assert_eq!(s.get(id).unwrap().unwrap().title.as_deref(), Some("A"));
}

#[test]
fn rejects_newer_schema() {
    let dir = TempDir::new();
    let path = dir.0.join("shlv.db");
    let s = Store::open(&path).unwrap();
    s.conn().pragma_update(None, "user_version", 99).unwrap();
    drop(s);
    let err = Store::open(&path).err().unwrap().to_string();
    assert!(err.contains("newer"), "{err}");
}

#[test]
fn add_and_get() {
    let (_d, mut s) = open();
    let a = s
        .add(&NewBookmark {
            url: "example.com/x".into(),
            title: Some("Example".into()),
            note: Some("my note".into()),
            tags: vec!["Rust".into(), "Web Dev".into()],
        })
        .unwrap();
    assert!(a.created);
    let b = s.get(a.id).unwrap().unwrap();
    assert_eq!(b.url, "https://example.com/x");
    assert_eq!(b.title.as_deref(), Some("Example"));
    assert_eq!(b.note.as_deref(), Some("my note"));
    assert_eq!(b.tags, vec!["rust", "web-dev"]);
    assert_eq!(b.uid.len(), 36);
    assert_eq!(&b.uid[14..15], "7"); // UUIDv7
    assert_eq!(b.created_at, b.updated_at);
    assert!(s.get(a.id + 1).unwrap().is_none());
}

#[test]
fn rejects_bad_input() {
    let (_d, mut s) = open();
    assert!(s.add(&bm("", "x", &[])).is_err());
    assert!(s.add(&bm("https://a.com/x y", "x", &[])).is_err());
    assert!(s.add(&bm("https://a.com", "x", &["  "])).is_err());
}

#[test]
fn duplicate_url_updates_existing() {
    let (_d, mut s) = open();
    let a = s
        .add(&bm("https://Example.com/?b=2&a=1#frag", "Old", &["one"]))
        .unwrap();
    let b = s
        .add(&NewBookmark {
            url: "https://example.com:443/?a=1&b=2&utm_source=x".into(),
            title: None,
            note: Some("note".into()),
            tags: vec!["two".into()],
        })
        .unwrap();
    assert_eq!(b.id, a.id);
    assert!(!b.created);
    let got = s.get(a.id).unwrap().unwrap();
    assert_eq!(got.title.as_deref(), Some("Old")); // not given, kept
    assert_eq!(got.note.as_deref(), Some("note"));
    assert_eq!(got.tags, vec!["one", "two"]);
    assert_eq!(s.list(&ListQuery::default()).unwrap().len(), 1);
    // The FTS row was replaced, not duplicated.
    assert_eq!(ids(&s.search("old", 10).unwrap()), vec![a.id]);
    assert_eq!(ids(&s.search("two", 10).unwrap()), vec![a.id]);
    assert_eq!(
        s.find_by_url("example.com/?a=1&b=2").unwrap().map(|b| b.id),
        Some(a.id)
    );
}

#[test]
fn edit_fields_and_tags() {
    let (_d, mut s) = open();
    let id = s
        .add(&bm("https://a.com", "Alpha", &["x", "y"]))
        .unwrap()
        .id;
    let ok = s
        .edit(
            id,
            &Edit {
                title: Some("Beta".into()),
                note: Some("hello".into()),
                add_tags: vec!["z".into()],
                remove_tags: vec!["x".into(), "missing".into()],
            },
        )
        .unwrap();
    assert!(ok);
    let b = s.get(id).unwrap().unwrap();
    assert_eq!(b.title.as_deref(), Some("Beta"));
    assert_eq!(b.note.as_deref(), Some("hello"));
    assert_eq!(b.tags, vec!["y", "z"]);
    assert!(b.updated_at >= b.created_at);
    assert!(s.search("alpha", 10).unwrap().is_empty());
    assert_eq!(ids(&s.search("beta", 10).unwrap()), vec![id]);
    assert!(s.search("#x", 10).unwrap().is_empty());
    assert_eq!(ids(&s.search("#z", 10).unwrap()), vec![id]);

    // Empty string clears.
    s.edit(
        id,
        &Edit {
            note: Some(String::new()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(s.get(id).unwrap().unwrap().note, None);

    assert!(!s.edit(id + 100, &Edit::default()).unwrap());
}

#[test]
fn delete_leaves_tombstone() {
    let (_d, mut s) = open();
    let id = s.add(&bm("https://a.com", "Alpha", &["x"])).unwrap().id;
    assert!(s.delete(id).unwrap());
    assert!(!s.delete(id).unwrap());
    assert!(s.get(id).unwrap().is_none());
    assert!(s.list(&ListQuery::default()).unwrap().is_empty());
    assert!(s.search("alpha", 10).unwrap().is_empty());

    let c = s.conn();
    let (title, deleted, uid): (Option<String>, Option<i64>, String) = c
        .query_row(
            "SELECT title, deleted_at, uid FROM bookmark WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(title, None);
    assert!(deleted.is_some());
    assert!(!uid.is_empty());
    let fts: i64 = c
        .query_row("SELECT count(*) FROM bookmark_fts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fts, 0);
    let tags: i64 = c
        .query_row("SELECT count(*) FROM bookmark_tag", [], |r| r.get(0))
        .unwrap();
    assert_eq!(tags, 0);
    assert!(!s.edit(id, &Edit::default()).unwrap());

    // The URL can be bookmarked again, as a new bookmark.
    let again = s.add(&bm("https://a.com", "Again", &[])).unwrap();
    assert!(again.created);
    assert_ne!(again.id, id);
}

#[test]
fn list_newest_first_with_tags_and_limit() {
    let (_d, mut s) = open();
    let a = s.add(&bm("https://a.com", "A", &["x", "y"])).unwrap().id;
    let b = s.add(&bm("https://b.com", "B", &["x"])).unwrap().id;
    let c = s.add(&bm("https://c.com", "C", &[])).unwrap().id;
    let all = |tags: &[&str], limit| {
        ids(&s
            .list(&ListQuery {
                tags: tags.iter().map(|t| t.to_string()).collect(),
                limit,
            })
            .unwrap())
    };
    assert_eq!(all(&[], 50), vec![c, b, a]);
    assert_eq!(all(&[], 2), vec![c, b]);
    assert_eq!(all(&["x"], 50), vec![b, a]);
    assert_eq!(all(&["X", "y"], 50), vec![a]);
    assert_eq!(all(&["nope"], 50), Vec::<i64>::new());
}

#[test]
fn search_ranks_title_highest() {
    let (_d, mut s) = open();
    let in_note = s
        .add(&NewBookmark {
            url: "https://n.com".into(),
            title: Some("Something else".into()),
            note: Some("about rust".into()),
            tags: vec![],
        })
        .unwrap()
        .id;
    let in_title = s.add(&bm("https://t.com", "Rust book", &[])).unwrap().id;
    assert_eq!(ids(&s.search("rust", 10).unwrap()), vec![in_title, in_note]);
}

#[test]
fn search_prefix_and_diacritics() {
    let (_d, mut s) = open();
    let id = s
        .add(&bm("https://a.com", "Café programming", &[]))
        .unwrap()
        .id;
    assert_eq!(ids(&s.search("prog", 10).unwrap()), vec![id]);
    assert_eq!(ids(&s.search("p", 10).unwrap()), vec![id]);
    assert_eq!(ids(&s.search("cafe", 10).unwrap()), vec![id]);
    assert!(s.search("xyz", 10).unwrap().is_empty());
}

#[test]
fn search_shorthands() {
    let (_d, mut s) = open();
    let a = s
        .add(&bm("https://docs.rs/x", "Async guide", &["rust"]))
        .unwrap()
        .id;
    let b = s
        .add(&bm("https://blog.example.com/y", "Async in JS", &["js"]))
        .unwrap()
        .id;
    let c = s
        .add(&bm("https://example.com/z", "Other", &["rust"]))
        .unwrap()
        .id;
    assert_eq!(ids(&s.search("async #rust", 10).unwrap()), vec![a]);
    assert_eq!(ids(&s.search("async tag:js", 10).unwrap()), vec![b]);
    assert_eq!(ids(&s.search("site:example.com", 10).unwrap()), vec![c, b]);
    assert_eq!(
        ids(&s.search("async site:example.com", 10).unwrap()),
        vec![b]
    );
    assert_eq!(ids(&s.search("#rust", 10).unwrap()), vec![c, a]);
    assert!(s.search("#nope", 10).unwrap().is_empty());
    // Tags are in the index, so a bare word finds them too.
    assert_eq!(ids(&s.search("js", 10).unwrap()), vec![b]);
}

#[test]
fn search_resolves_aliases() {
    let (_d, mut s) = open();
    let id = s
        .add(&bm("https://a.com", "A", &["javascript"]))
        .unwrap()
        .id;
    s.conn()
        .execute(
            "INSERT INTO tag_alias (alias, tag_id) SELECT 'js', id FROM tag WHERE name = 'javascript'",
            [],
        )
        .unwrap();
    assert_eq!(ids(&s.search("#js", 10).unwrap()), vec![id]);
    // Adding with an alias attaches the canonical tag.
    let b = s.add(&bm("https://b.com", "B", &["js"])).unwrap().id;
    assert_eq!(s.get(b).unwrap().unwrap().tags, vec!["javascript"]);
}

#[test]
fn search_fts_syntax_and_errors() {
    let (_d, mut s) = open();
    let a = s
        .add(&bm("https://a.com", "rust-lang book", &[]))
        .unwrap()
        .id;
    let b = s.add(&bm("https://b.com", "go book", &[])).unwrap().id;
    assert_eq!(ids(&s.search("rust-lang", 10).unwrap()), vec![a]);
    assert_eq!(ids(&s.search("book NOT go", 10).unwrap()), vec![a]);
    assert_eq!(ids(&s.search("rust OR go", 10).unwrap()).len(), 2);
    assert_eq!(ids(&s.search("\"go book\"", 10).unwrap()), vec![b]);
    assert_eq!(ids(&s.search("title:go", 10).unwrap()), vec![b]);
    let err = s.search("AND", 10).unwrap_err().to_string();
    assert!(err.contains("invalid search query"), "{err}");
}

#[test]
fn each_variants_stream_and_stop_on_error() {
    let (_d, mut s) = open();
    for i in 0..5 {
        s.add(&bm(&format!("https://{i}.com"), "Same title", &[]))
            .unwrap();
    }
    let mut seen = 0;
    let err = s
        .list_each(&ListQuery::default(), |_| {
            seen += 1;
            if seen == 2 {
                anyhow::bail!("stop");
            }
            Ok(())
        })
        .unwrap_err();
    assert_eq!(err.to_string(), "stop");
    assert_eq!(seen, 2);

    let mut seen = 0;
    s.search_each("same", 3, |_| {
        seen += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(seen, 3);
    assert_eq!(s.max_id().unwrap(), 5);
}

#[test]
fn upgrades_a_version_1_database() {
    let dir = TempDir::new();
    let path = dir.0.join("shlv.db");
    let s = Store::open(&path).unwrap();
    s.conn()
        .execute_batch("DROP TABLE meta; PRAGMA user_version = 1;")
        .unwrap();
    drop(s);
    let s = Store::open(&path).unwrap();
    let v: i64 = s
        .conn()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 2);
    let n: i64 = s
        .conn()
        .query_row("SELECT count(*) FROM meta", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn import_keeps_valid_unused_uids_and_times() {
    use shlv::{ImportItem, ImportOptions};
    let (_d, mut s) = open();
    let gone = s.add(&bm("https://gone.example", "", &[])).unwrap().id;
    let gone_uid = s.get(gone).unwrap().unwrap().uid;
    s.delete(gone).unwrap();
    let uid = "0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";
    let item = |url: &str, uid: &str| ImportItem {
        url: url.into(),
        uid: Some(uid.into()),
        created_at: Some(1_000),
        tags: vec!["A b".into(), " ".into()],
        ..Default::default()
    };
    let items = [
        item("https://a.example/", uid),
        item("https://b.example/", "not-a-uuid"),
        item("https://c.example/", &gone_uid),
        item("https://d.example/", uid),
    ];
    let sum = s.import(&items, ImportOptions::default()).unwrap();
    assert_eq!((sum.added, sum.updated, sum.skipped.len()), (4, 0, 0));
    let got = |url| s.find_by_url(url).unwrap().unwrap();
    let a = got("https://a.example/");
    assert_eq!(a.uid, uid);
    assert_eq!((a.created_at, a.updated_at), (1_000, 1_000));
    assert_eq!(a.tags, ["a-b"]);
    for url in [
        "https://b.example/",
        "https://c.example/",
        "https://d.example/",
    ] {
        let b = got(url);
        assert!(
            b.uid != uid && b.uid != gone_uid && b.uid != "not-a-uuid",
            "{url}"
        );
    }
}
