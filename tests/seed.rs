//! Seeding the list from interests (task 034), store side: stored interests, applying decisions,
//! and how tag health spreads the list over interests.

use shlv::health::HealthOptions;
use shlv::{
    Candidate, Decision, DeclaredTags, NewBookmark, SeedName, Separator, Store, TagInfo, TagStyle,
};

fn add(s: &mut Store, url: &str, tags: &[&str]) -> i64 {
    s.add(&NewBookmark {
        url: url.into(),
        tags: tags.iter().map(|t| t.to_string()).collect(),
        ..Default::default()
    })
    .unwrap()
    .id
}

fn cand(interest: &str, name: &str, desc: Option<&str>, decision: Decision) -> Candidate {
    Candidate {
        interest: interest.into(),
        name: name.into(),
        description: desc.map(str::to_string),
        edited: false,
        decision,
    }
}

fn canonical(s: &Store) -> Vec<TagInfo> {
    s.canonical_tags().unwrap()
}

fn names(s: &Store) -> Vec<String> {
    canonical(s).into_iter().map(|t| t.name).collect()
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn interests_saved_pinned_and_forgotten() {
    let mut s = Store::open_in_memory().unwrap();
    s.set_pinned_interests(&strings(&["Home Lab", "audio", "audio", " "]));
    assert_eq!(s.save_interests(&strings(&["nix", "Audio"])).unwrap(), 2);
    assert_eq!(s.save_interests(&strings(&["nix"])).unwrap(), 0);
    let i = s.interests().unwrap();
    let got: Vec<(&str, bool, bool)> = i
        .iter()
        .map(|i| (i.name.as_str(), i.saved, i.pinned))
        .collect();
    assert_eq!(
        got,
        [
            ("audio", true, true),
            ("home-lab", false, true),
            ("nix", true, false)
        ]
    );
    assert_eq!(s.forget_interests(&strings(&["nix", "none"])).unwrap(), 1);
    let e = s.forget_interests(&strings(&["audio"])).unwrap_err();
    assert!(e.to_string().contains("pinned in config"), "{e}");
}

#[test]
fn seed_names_are_styled_and_resolved() {
    let mut s = Store::open_in_memory().unwrap();
    s.set_style(TagStyle {
        separator: Some(Separator::Underscore),
        max_length: Some(10),
    });
    add(&mut s, "https://a.example/", &["wifi"]);
    s.add_canonical(&strings(&["nix"])).unwrap();
    s.add_alias("nixos", "nix").unwrap();
    assert_eq!(
        s.seed_name("Home Lab").unwrap(),
        SeedName::New("home_lab".into())
    );
    assert_eq!(
        s.seed_name("NixOS").unwrap(),
        SeedName::Tag {
            name: "nix".into(),
            canonical: true
        }
    );
    assert!(!s.seed_name("wifi").unwrap().listed());
    assert!(s.seed_name("reverse-proxies").is_err());
}

#[test]
fn applies_decisions() {
    let mut s = Store::open_in_memory().unwrap();
    // In use, not on the list: keeping it marks it canonical; another is merged into a candidate.
    let b = add(&mut s, "https://a.example/", &["wifi", "nixpkgs"]);
    s.add_canonical(&strings(&["network"])).unwrap();
    s.describe_tag("network", "networks").unwrap();
    let mut edited = cand("nix", "flake", Some("Nix flakes, edited"), Decision::Keep);
    edited.edited = true;
    let cands = [
        cand("nix", "nix", Some("the Nix language"), Decision::Keep),
        edited,
        cand(
            "nix",
            "nixpkgs",
            Some("the packages"),
            Decision::Merge("nix".into()),
        ),
        cand("nix", "nixos-wiki", None, Decision::Merge("nix".into())),
        cand("nix", "home-manager", None, Decision::Drop),
        cand("nix", "devenv", None, Decision::Undecided),
        cand("network", "wifi", Some("wireless"), Decision::Keep),
        // Already listed: its description isn't touched, but it is recorded under the interest.
        cand("network", "network", Some("other"), Decision::Listed),
        cand(
            "network",
            "mesh",
            None,
            Decision::Merge("home-manager".into()),
        ),
    ];
    let r = s.apply_seed(&cands, &strings(&["nix", "network"])).unwrap();
    assert_eq!(r.kept, ["nix", "flake", "wifi"]);
    assert_eq!(
        r.merged,
        [
            ("nixpkgs".to_string(), "nix".to_string()),
            ("nixos-wiki".to_string(), "nix".to_string())
        ]
    );
    assert_eq!(r.described, 3);
    assert_eq!(r.saved, 2);
    assert_eq!(r.skipped.len(), 1);
    assert!(r.skipped[0].contains("home-manager isn't a tag"), "{r:?}");

    assert_eq!(names(&s), ["flake", "network", "nix", "wifi"]);
    let nix = canonical(&s).into_iter().find(|t| t.name == "nix").unwrap();
    assert_eq!(nix.aliases, ["nixos-wiki", "nixpkgs"]);
    assert_eq!(nix.description.as_deref(), Some("the Nix language"));
    let network = canonical(&s).into_iter().find(|t| t.name == "network");
    assert_eq!(network.unwrap().description.as_deref(), Some("networks"));
    // The bookmark tagged nixpkgs now has nix.
    let tags: Vec<String> = s.get(b).unwrap().unwrap().tags;
    assert_eq!(tags, ["nix", "wifi"]);

    // An edited description replaces the current one; an unedited one doesn't.
    let mut again = cand("nix", "flake", Some("Nix flakes"), Decision::Keep);
    s.apply_seed(std::slice::from_ref(&again), &[]).unwrap();
    let desc = |s: &Store| {
        canonical(s)
            .into_iter()
            .find(|t| t.name == "flake")
            .unwrap()
            .description
    };
    assert_eq!(desc(&s).as_deref(), Some("Nix flakes, edited"));
    again.edited = true;
    s.apply_seed(&[again], &[]).unwrap();
    assert_eq!(desc(&s).as_deref(), Some("Nix flakes"));

    // Health: tags and bookmarks per interest.
    s.save_interests(&strings(&["audio"])).unwrap();
    let h = s.health(&HealthOptions::default(), None).unwrap();
    let spread: Vec<(&str, Vec<&str>, usize)> = h
        .interests
        .iter()
        .map(|i| {
            (
                i.interest.as_str(),
                i.tags.iter().map(String::as_str).collect(),
                i.bookmarks,
            )
        })
        .collect();
    assert_eq!(
        spread,
        [
            ("audio", vec![], 0),
            ("network", vec!["network", "wifi"], 1),
            ("nix", vec!["flake", "nix"], 1),
        ]
    );
}

#[test]
fn links_follow_merges_and_pinned_tags_stay() {
    let mut s = Store::open_in_memory().unwrap();
    let mut decl = std::collections::BTreeMap::new();
    decl.insert("audio".to_string(), shlv::DeclaredEntry::default());
    s.pin(DeclaredTags::new(decl).unwrap()).unwrap();
    s.save_interests(&strings(&["sound"])).unwrap();
    s.apply_seed(
        &[
            cand("sound", "speakers", None, Decision::Keep),
            cand("sound", "audio", None, Decision::Merge("speakers".into())),
        ],
        &[],
    )
    .map(|r| assert!(r.skipped[0].contains("pinned"), "{r:?}"))
    .unwrap();
    add(&mut s, "https://a.example/", &["speakers"]);
    s.merge_tags("speakers", "audio").unwrap();
    let h = s.health(&HealthOptions::default(), None).unwrap();
    assert_eq!(h.interests[0].interest, "sound");
    assert_eq!(h.interests[0].tags, ["audio"]);
    assert_eq!(h.interests[0].bookmarks, 1);
}
