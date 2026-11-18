//! Domain rules saved in the database (task 043): add and remove, pinned config rules, renames
//! and merges, what each rule tagged, and spotting rules worth offering.

use std::collections::BTreeMap;

use shlv::{DomainRules, NewBookmark, NonCanonicalTags, Pinned, RuleCandidate, Store};

fn rules(pairs: &[(&str, &[&str])]) -> DomainRules {
    DomainRules::new(
        pairs
            .iter()
            .map(|(h, t)| (h.to_string(), t.iter().map(|t| t.to_string()).collect()))
            .collect::<BTreeMap<_, _>>(),
    )
    .unwrap()
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn add(s: &mut Store, url: &str, tags: &[&str]) -> i64 {
    s.add(&NewBookmark {
        url: url.into(),
        tags: strings(tags),
        ..Default::default()
    })
    .unwrap()
    .id
}

fn pending(s: &Store) -> Vec<(i64, String)> {
    let mut v: Vec<(i64, String)> = s
        .pending(None)
        .unwrap()
        .into_iter()
        .map(|p| (p.id, p.tag))
        .collect();
    v.sort();
    v
}

/// (host, tag, pinned, saved, tagged, pending) for each listed rule.
fn listed(s: &Store) -> Vec<(String, String, bool, bool, usize, usize)> {
    s.rule_list()
        .unwrap()
        .into_iter()
        .map(|r| (r.host, r.tag, r.pinned, r.saved, r.tagged, r.pending))
        .collect()
}

fn row(
    host: &str,
    tag: &str,
    pinned: bool,
    saved: bool,
    tagged: usize,
    pending: usize,
) -> (String, String, bool, bool, usize, usize) {
    (host.into(), tag.into(), pinned, saved, tagged, pending)
}

#[test]
fn saved_rules_join_pinned_ones_and_persist() {
    let dir = std::env::temp_dir().join(format!("shlv-rules-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("shlv.db");
    {
        let mut s = Store::open(&path).unwrap();
        s.set_rules(rules(&[("github.com", &["code"])]));
        s.add_alias("tunes", "music").unwrap_err(); // no tag yet
        add(&mut s, "https://example.com/", &["music"]);
        s.add_alias("tunes", "music").unwrap();
        // A URL names its host; aliases resolve; names are normalized.
        let a = s
            .add_rule("https://Bandcamp.com/x", &strings(&["Tunes", "Indie"]))
            .unwrap();
        assert_eq!(a.host, "bandcamp.com");
        assert_eq!(a.added, ["music", "indie"]);
        let a = s
            .add_rule("github.com", &strings(&["code", "git"]))
            .unwrap();
        assert_eq!(
            (a.added, a.existing),
            (strings(&["git"]), strings(&["code"]))
        );
        assert!(s.add_rule("not a host", &strings(&["x"])).is_err());
        assert!(s.add_rule("github.com", &[]).is_err());

        let b = add(&mut s, "https://artist.bandcamp.com/album", &[]);
        let g = add(&mut s, "https://github.com/x", &[]);
        assert_eq!(
            pending(&s),
            [
                (b, "indie".into()),
                (b, "music".into()),
                (g, "code".into()),
                (g, "git".into())
            ]
        );
    }
    // Saved rules come back with the database; pinned ones come from config each time.
    let s = Store::open(&path).unwrap();
    assert_eq!(
        listed(&s),
        [
            row("bandcamp.com", "indie", false, true, 1, 1),
            row("bandcamp.com", "music", false, true, 1, 1),
            row("github.com", "git", false, true, 1, 1),
        ]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn strict_mode_refuses_rules_for_tags_off_the_list() {
    let mut s = Store::open_in_memory().unwrap();
    s.add_canonical(&strings(&["music"])).unwrap();
    s.set_strict_tags(true);
    let e = s
        .add_rule("bandcamp.com", &strings(&["tunes"]))
        .unwrap_err();
    assert_eq!(e.downcast_ref::<NonCanonicalTags>().unwrap().0, ["tunes"]);
    s.add_rule("bandcamp.com", &strings(&["music"])).unwrap();
    assert_eq!(s.rules().tags_of("bandcamp.com"), ["music"]);
}

#[test]
fn removing_drops_the_rules_pending_suggestions() {
    let mut s = Store::open_in_memory().unwrap();
    s.set_rules(rules(&[("github.com", &["code"])]));
    s.add_rule("github.com", &strings(&["git"])).unwrap();
    s.add_rule("gist.github.com", &strings(&["git", "snippet"]))
        .unwrap();
    let a = add(&mut s, "https://github.com/a", &[]);
    let b = add(&mut s, "https://gist.github.com/b", &[]);
    let c = add(&mut s, "https://github.com/c", &[]);
    s.accept(c, &strings(&["git"])).unwrap();

    // Pinned rules stay, and nothing changes.
    let e = s
        .remove_rule("github.com", &strings(&["code"]))
        .unwrap_err();
    assert!(e.downcast_ref::<Pinned>().is_some(), "{e}");
    let e = s
        .remove_rule("github.com", &strings(&["git", "code"]))
        .unwrap_err();
    assert!(e.downcast_ref::<Pinned>().is_some());
    assert!(s.remove_rule("github.com", &strings(&["nope"])).is_err());
    assert!(s.remove_rule("gitlab.com", &[]).is_err());
    assert_eq!(listed(&s).len(), 4);

    // github.com's git goes: a's pending git goes with it, but b's stays (gist.github.com's rule
    // still makes it), and c keeps the tag it accepted.
    let r = s.remove_rule("github.com", &strings(&["git"])).unwrap();
    assert_eq!((r.removed, r.dropped), (strings(&["git"]), 1));
    assert_eq!(
        pending(&s),
        [
            (a, "code".into()),
            (b, "code".into()),
            (b, "git".into()),
            (b, "snippet".into()),
            (c, "code".into())
        ]
    );
    assert_eq!(s.get(c).unwrap().unwrap().tags, ["git"]);
    // With no tags, all of a host's saved rules go.
    let r = s.remove_rule("gist.github.com", &[]).unwrap();
    assert_eq!((r.removed, r.dropped), (strings(&["git", "snippet"]), 2));
    // Only the pinned rule is left, and it can't be removed.
    let e = s.remove_rule("github.com", &[]).unwrap_err();
    assert!(e.downcast_ref::<Pinned>().is_some());
    assert_eq!(listed(&s), [row("github.com", "code", true, false, 3, 3)]);
}

#[test]
fn rules_follow_renames_and_merges() {
    let mut s = Store::open_in_memory().unwrap();
    add(&mut s, "https://example.com/", &["tunes", "audio"]);
    s.add_rule("bandcamp.com", &strings(&["tunes"])).unwrap();
    s.add_rule("soundcloud.com", &strings(&["tunes", "audio"]))
        .unwrap();
    s.rename_tag("tunes", "music").unwrap();
    assert_eq!(s.rules().tags_of("bandcamp.com"), ["music"]);
    // Merging into a tag the host's rule already gives leaves one rule.
    s.merge_tags("audio", "music").unwrap();
    assert_eq!(s.rules().tags_of("soundcloud.com"), ["music"]);
    let b = add(&mut s, "https://bandcamp.com/x", &[]);
    assert_eq!(pending(&s), [(b, "music".into())]);
    assert_eq!(
        listed(&s),
        [
            row("bandcamp.com", "music", false, true, 1, 1),
            row("soundcloud.com", "music", false, true, 0, 0),
        ]
    );
}

#[test]
fn applying_a_new_rule_to_a_site() {
    let mut s = Store::open_in_memory().unwrap();
    let a = add(&mut s, "https://a.bandcamp.com/", &[]);
    let b = add(&mut s, "https://bandcamp.com/b", &["music"]);
    add(&mut s, "https://example.com/", &[]);
    s.add_rule("bandcamp.com", &strings(&["music"])).unwrap();
    let stats = s.apply_rules_on("bandcamp.com").unwrap();
    assert_eq!((stats.checked, stats.matched, stats.pending), (2, 2, 1));
    assert_eq!(pending(&s), [(a, "music".into())]);
    let _ = b;
}

fn offered(s: &Store) -> Vec<(String, String, usize, usize)> {
    s.rule_candidates(3, 0.8)
        .unwrap()
        .into_iter()
        .map(
            |RuleCandidate {
                 host,
                 tag,
                 count,
                 of,
             }| (host, tag, count, of),
        )
        .collect()
}

#[test]
fn spots_sites_tagged_the_same_way() {
    let mut s = Store::open_in_memory().unwrap();
    // Three bandcamp artists, each on its own subdomain: the site is bandcamp.com.
    for a in ["a", "b", "c"] {
        add(&mut s, &format!("https://{a}.bandcamp.com/"), &["music"]);
    }
    // github.com: rust on 3 of 5 tagged bookmarks is too few to be a rule; untagged ones don't
    // count.
    for (i, t) in [
        &["rust"][..],
        &["rust"],
        &["rust", "cli"],
        &["go"],
        &["python"],
        &[],
    ]
    .iter()
    .enumerate()
    {
        add(&mut s, &format!("https://github.com/{i}"), t);
    }
    // One host makes a host rule, not one for its parent.
    for i in 0..4 {
        add(&mut s, &format!("https://blog.example.org/{i}"), &["blog"]);
    }
    add(&mut s, "https://example.org/", &["home"]);
    // Two of a tag is too few.
    add(&mut s, "https://news.site/1", &["news"]);
    add(&mut s, "https://news.site/2", &["news"]);
    assert_eq!(
        offered(&s),
        [
            ("blog.example.org".into(), "blog".into(), 4, 4),
            ("bandcamp.com".into(), "music".into(), 3, 3),
        ]
    );
    // One site's offers, as review asks after each bookmark.
    let at = |u: &str| -> Vec<String> {
        s.rule_candidates_at(u)
            .unwrap()
            .into_iter()
            .map(|c| format!("{} {}", c.host, c.tag))
            .collect()
    };
    assert_eq!(at("https://new.bandcamp.com/x"), ["bandcamp.com music"]);
    assert_eq!(at("https://blog.example.org/x"), ["blog.example.org blog"]);
    assert!(at("https://example.org/").is_empty());
    assert!(at("https://github.com/1").is_empty());
    // Declined offers and rules that exist aren't offered.
    s.decline_rule("blog.example.org", "blog").unwrap();
    s.add_rule("bandcamp.com", &strings(&["music"])).unwrap();
    assert_eq!(offered(&s), []);
    // Tags a rule applied are no evidence for another rule.
    s.remove_rule("bandcamp.com", &[]).unwrap();
    s.set_rules(rules(&[("news.site", &["news"])]));
    let n = add(&mut s, "https://news.site/3", &[]);
    s.accept(n, &[]).unwrap();
    assert_eq!(offered(&s), [("bandcamp.com".into(), "music".into(), 3, 3)]);
}
