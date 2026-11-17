//! Tag health on a small collection with meaningful tags: every section has a known answer.

use shlv::choose::Item;
use shlv::health::{HealthOptions, StyleKind};
use shlv::{NewBookmark, Separator, Store, TagStyle};

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

/// 20 bookmarks:
/// - `ml` and `machine-learning` together on 4 (redundant); `ml` alone on none.
/// - `programming` on 10 of 20 (too broad at 0.3).
/// - canonical `haskell` on none, `elm` on one (rare).
/// - `plugin` and `plugins`; `rust`, `wifi`, and `rust-wifi`; `home_lab` (style, with `-`).
/// - 6 without tags: 3 on arxiv.org, 3 more with "kubernetes" in the title on other hosts.
fn fixture() -> Store {
    let mut s = Store::open_in_memory().unwrap();
    for i in 0..4 {
        add(
            &mut s,
            &format!("https://ml{i}.example/"),
            &format!("Neural nets {i}"),
            &["ml", "machine-learning", "programming"],
        );
    }
    for i in 0..5 {
        add(
            &mut s,
            &format!("https://code{i}.example/"),
            &format!("Code {i}"),
            &["programming", "rust"],
        );
    }
    add(
        &mut s,
        "https://elm.example/",
        "Elm",
        &["elm", "programming"],
    );
    add(&mut s, "https://p1.example/", "Plugin A", &["plugin"]);
    add(&mut s, "https://p2.example/", "Plugin B", &["plugins"]);
    add(
        &mut s,
        "https://w.example/",
        "Rust on an ESP32",
        &["rust-wifi", "wifi"],
    );
    add(&mut s, "https://h.example/", "My rack", &["home_lab"]);
    for i in 0..3 {
        add(
            &mut s,
            &format!("https://arxiv.org/abs/{i}"),
            &format!("Paper number {i}"),
            &[],
        );
    }
    for host in ["a.example", "b.example", "c.example"] {
        add(
            &mut s,
            &format!("https://{host}/k8s"),
            "Kubernetes operators explained",
            &[],
        );
    }
    let all = [
        "ml",
        "machine-learning",
        "programming",
        "rust",
        "elm",
        "haskell",
        "plugin",
        "plugins",
        "wifi",
        "rust-wifi",
        "home_lab",
    ];
    s.add_canonical(&all.map(String::from)).unwrap();
    s
}

#[test]
fn reports_each_section() {
    let mut s = fixture();
    s.set_style(TagStyle {
        separator: Some(Separator::Hyphen),
        max_length: None,
    });
    let h = s.health(&HealthOptions::default(), None).unwrap();

    let c = &h.coverage;
    assert_eq!((c.bookmarks, c.tagged, c.canonical_tagged), (20, 14, 14));
    assert_eq!((c.tags, c.canonical), (11, 11));
    assert_eq!(c.histogram, [6, 3, 7, 4, 0]);
    assert!((c.tags_per_tagged - 29.0 / 14.0).abs() < 1e-9);

    assert_eq!(h.redundant.len(), 1, "{:?}", h.redundant);
    let r = &h.redundant[0];
    // Same counts: the names break the tie, so the merge is predictable.
    assert_eq!((r.a.as_str(), r.b.as_str()), ("ml", "machine-learning"));
    assert_eq!((r.together, r.jaccard), (4, 1.0));

    let broad: Vec<&str> = h.broad.iter().map(|b| b.tag.as_str()).collect();
    assert_eq!(broad, ["programming"]);

    let rare: Vec<(&str, usize)> = h.rare.iter().map(|r| (r.tag.as_str(), r.count)).collect();
    assert_eq!(rare[0], ("haskell", 0));
    assert!(rare.contains(&("elm", 1)));
    assert!(!rare.iter().any(|(t, _)| *t == "rust"));
    assert!(h.rare.iter().all(|r| r.created_at.is_some() && !r.pinned));

    assert_eq!(h.gaps.bookmarks, 6);
    assert_eq!(h.gaps.placeable, None);
    let hosts: Vec<(&str, usize)> = h
        .gaps
        .by_host
        .iter()
        .map(|g| (g.key.as_str(), g.ids.len()))
        .collect();
    assert_eq!(hosts, [("arxiv.org", 3)]);
    let words: Vec<&str> = h.gaps.by_word.iter().map(|g| g.key.as_str()).collect();
    for w in ["kubernetes", "operators", "explained", "paper", "number"] {
        assert!(words.contains(&w), "{words:?}");
    }

    let style: Vec<(&str, StyleKind)> = h.style.iter().map(|i| (i.tag.as_str(), i.kind)).collect();
    assert!(style.contains(&("home_lab", StyleKind::Style)), "{style:?}");
    assert!(style.contains(&("plugins", StyleKind::Plural)));
    assert!(style.contains(&("rust-wifi", StyleKind::Compound)));
    assert_eq!(style.len(), 3, "{style:?}");
}

#[test]
fn thresholds_change_the_answer() {
    let s = fixture();
    let opts = HealthOptions {
        broad: 0.6,
        rare: 0,
        min_group: 4,
        min_together: 5,
        ..HealthOptions::default()
    };
    let h = s.health(&opts, None).unwrap();
    assert!(h.redundant.is_empty());
    assert!(h.broad.is_empty());
    let rare: Vec<&str> = h.rare.iter().map(|r| r.tag.as_str()).collect();
    assert_eq!(rare, ["haskell"]);
    assert!(h.gaps.by_host.is_empty() && h.gaps.by_word.is_empty());
}

#[test]
fn without_a_canonical_list_gaps_are_untagged_bookmarks() {
    let mut s = Store::open_in_memory().unwrap();
    add(&mut s, "https://a.example/", "A", &["stray"]);
    add(&mut s, "https://b.example/", "B", &[]);
    let h = s.health(&HealthOptions::default(), None).unwrap();
    assert_eq!(h.gaps.bookmarks, 1);
    // With a list, a bookmark with only non-canonical tags is a gap too.
    s.add_canonical(&["other".to_string()]).unwrap();
    let h = s.health(&HealthOptions::default(), None).unwrap();
    assert_eq!(h.gaps.bookmarks, 2);
    assert_eq!(h.coverage.canonical_tagged, 0);
}

#[test]
fn placeable_bookmarks_are_counted_not_grouped() {
    let s = fixture();
    let place = |i: &Item| i.url.contains("arxiv");
    let h = s.health(&HealthOptions::default(), Some(&place)).unwrap();
    assert_eq!((h.gaps.bookmarks, h.gaps.placeable), (6, Some(3)));
    assert!(h.gaps.by_host.is_empty());
    assert_eq!(h.gaps.by_word[0].key, "explained");
}

#[test]
fn word_groups_name_existing_tags() {
    let mut s = fixture();
    s.add_canonical(&["kubernetes".to_string()]).unwrap();
    let h = s.health(&HealthOptions::default(), None).unwrap();
    let k = h
        .gaps
        .by_word
        .iter()
        .find(|g| g.key == "kubernetes")
        .unwrap();
    assert_eq!(k.existing.as_deref(), Some("kubernetes"));
}

#[test]
fn deleted_bookmarks_and_empty_stores() {
    let s = Store::open_in_memory().unwrap();
    let h = s.health(&HealthOptions::default(), None).unwrap();
    assert_eq!(h.coverage.bookmarks, 0);
    assert!(h.broad.is_empty() && h.style.is_empty());

    let mut s = fixture();
    for id in 1..=4 {
        s.delete(id).unwrap();
    }
    let h = s.health(&HealthOptions::default(), None).unwrap();
    assert_eq!(h.coverage.bookmarks, 16);
    // Without the ml bookmarks, rust (5) and programming (6) are together on 5.
    let pairs: Vec<(&str, &str)> = h
        .redundant
        .iter()
        .map(|r| (r.a.as_str(), r.b.as_str()))
        .collect();
    assert_eq!(pairs, [("rust", "programming")]);
    let rare: Vec<&str> = h.rare.iter().map(|r| r.tag.as_str()).collect();
    assert!(rare.contains(&"ml") && rare.contains(&"machine-learning"));
}
