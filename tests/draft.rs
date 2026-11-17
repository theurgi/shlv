//! Drafting the list (task 035): the sample spreads across sites, and drafts merge.

use shlv::draft::{DraftTag, Drafted, Redundant, merge, site, spread};
use shlv::{NewBookmark, Store};

#[test]
fn sites() {
    assert_eq!(site("https://www.GitHub.com/a/b?c"), "github.com");
    assert_eq!(
        site("http://user@blog.example.org:8080/x"),
        "blog.example.org"
    );
    assert_eq!(site("https://www./"), "www.");
    assert_eq!(site("not a url"), "");
}

#[test]
fn the_sample_spreads_across_sites() {
    // 50 on one site, 5 on another, and 10 sites with one each.
    let mut all: Vec<(i64, String)> = Vec::new();
    for i in 0..50 {
        all.push((i, format!("https://big.example/{i}")));
    }
    for i in 50..55 {
        all.push((i, format!("https://mid.example/{i}")));
    }
    for i in 55..65 {
        all.push((i, format!("https://s{i}.example/")));
    }
    let site_of = |id: i64| match id {
        0..50 => "big",
        50..55 => "mid",
        _ => "small",
    };
    let (s, sites) = spread(all.clone(), 20);
    assert_eq!(sites, 12);
    assert_eq!(s.len(), 20);
    let on = |s: &[i64], which: &str| s.iter().filter(|&&id| site_of(id) == which).count();
    // One from each of the 12 sites, then a second from the two with more, and so on.
    assert_eq!(on(&s, "big"), 5);
    assert_eq!(on(&s, "mid"), 5);
    assert_eq!(on(&s, "small"), 10);
    // The two largest sites lead every round.
    assert_eq!(site_of(s[0]), "big");
    assert_eq!(site_of(s[1]), "mid");
    // Stable from run to run, and not simply the first bookmarks of a site.
    assert_eq!(spread(all.clone(), 20).0, s);
    let big: Vec<i64> = s
        .iter()
        .copied()
        .filter(|&id| site_of(id) == "big")
        .collect();
    assert_ne!(big, [0, 1, 2, 3, 4]);

    // A sample larger than the library takes all of it, still interleaved.
    let (s, _) = spread(all, 1000);
    assert_eq!(s.len(), 65);
    assert_ne!(site_of(s[0]), site_of(s[1]));
}

#[test]
fn the_store_samples_the_whole_library() {
    let mut s = Store::open_in_memory().unwrap();
    for (i, u) in [
        "https://a.example/1",
        "https://a.example/2",
        "https://www.b.example/",
    ]
    .iter()
    .enumerate()
    {
        s.add(&NewBookmark {
            url: (*u).into(),
            title: Some(format!("T{i}")),
            ..NewBookmark::default()
        })
        .unwrap();
    }
    let sample = s.draft_sample(2).unwrap();
    assert_eq!(sample.total, 3);
    assert_eq!(sample.total_sites, 2);
    assert_eq!(sample.bookmarks.len(), 2);
    assert_eq!(sample.sites(), 2);
}

fn t(name: &str, desc: Option<&str>, interest: Option<&str>) -> DraftTag {
    DraftTag {
        name: name.into(),
        description: desc.map(str::to_string),
        interest: interest.map(str::to_string),
    }
}

#[test]
fn drafts_merge_by_name_and_support() {
    let r = |tag: &str, into: &str| Redundant {
        tag: tag.into(),
        into: into.into(),
        reason: None,
    };
    let drafts = [
        Drafted {
            tags: vec![t("rust", None, None), t("nix", Some("Nix"), None)],
            redundant: vec![r("js", "javascript")],
            refused: Vec::new(),
        },
        Drafted {
            tags: vec![
                t("wifi", None, None),
                t("rust", Some("Rust"), Some("code")),
                t("nix", Some("other"), None),
            ],
            redundant: vec![r("js", "ecmascript"), r("style", "css")],
            refused: Vec::new(),
        },
        Drafted {
            tags: vec![t("nix", None, Some("ops"))],
            ..Drafted::default()
        },
    ];
    let m = merge(&drafts, 2);
    assert_eq!(
        m.tags,
        [
            (t("nix", Some("Nix"), Some("ops")), 3),
            (t("rust", Some("Rust"), Some("code")), 2),
        ]
    );
    assert_eq!(m.cut, 1);
    assert_eq!(m.redundant, [r("js", "javascript"), r("style", "css")]);
    assert_eq!(merge(&[], 5), Default::default());
}
