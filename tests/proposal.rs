//! Tag proposals through the library (task 031): every source, every decision, aliases and
//! rejections, and the paths without proposals left as they were.

use std::collections::BTreeMap;

use shlv::choose::NewTag;
use shlv::{
    DeclaredEntry, DeclaredTags, Edit, ImportItem, ImportOptions, NewBookmark, NonCanonicalTags,
    Pinned, Proposal, ProposalSource, ProposalState, Store, StrictTags, SuggestMode, TagSort,
};

fn store() -> Store {
    Store::open_in_memory().unwrap()
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

fn tags_of(s: &Store, id: i64) -> Vec<String> {
    s.get(id).unwrap().unwrap().tags
}

fn new(name: &str, description: Option<&str>) -> NewTag {
    NewTag {
        name: name.into(),
        description: description.map(str::to_string),
    }
}

fn pending(s: &Store) -> Vec<Proposal> {
    s.proposals(ProposalState::Pending).unwrap()
}

/// (name, bookmark ids) of pending proposals, most bookmarks first.
fn summary(s: &Store) -> Vec<(String, Vec<i64>)> {
    pending(s)
        .into_iter()
        .map(|p| (p.name, p.bookmarks))
        .collect()
}

fn rejected(s: &Store) -> Vec<String> {
    s.proposals(ProposalState::Rejected)
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect()
}

/// `bookmark_tag.source` of a bookmark's tag.
fn source(s: &Store, id: i64, tag: &str) -> String {
    s.conn()
        .query_row(
            "SELECT bt.source FROM bookmark_tag bt JOIN tag t ON t.id = bt.tag_id \
             WHERE bt.bookmark_id = ?1 AND t.name = ?2",
            rusqlite::params![id, tag],
            |r| r.get(0),
        )
        .unwrap()
}

fn canonical(s: &Store) -> Vec<String> {
    s.canonical_tags()
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect()
}

#[test]
fn ai_names_are_normalized_and_collect_bookmarks() {
    let mut s = store();
    s.add_canonical(&strings(&["rust"])).unwrap();
    s.add_alias("rs", "rust").unwrap();
    let a = add(&mut s, "https://a.example/", &["go"]);
    let b = add(&mut s, "https://b.example/", &[]);
    s.add_alias("golang", "go").unwrap();

    let st = s
        .propose_tags(
            a,
            ProposalSource::Llm,
            &[
                new("Wi Fi", Some("wireless networks")),
                new("rs", None),     // an alias of a list tag: a suggestion of it
                new("golang", None), // an alias of a tag off the list: proposed as that tag
                new(" ", None),
                new("#wi-fi", Some("ignored: the first description stays")),
            ],
        )
        .unwrap();
    assert_eq!(st.proposed, ["wi-fi", "go"]);
    assert_eq!((st.suggested.pending, st.suggested.applied), (1, 0));
    assert_eq!(
        s.pending(Some(a)).unwrap()[0].tag,
        "rust",
        "the alias became a suggestion"
    );
    // Nothing new landed on the bookmark.
    assert_eq!(tags_of(&s, a), ["go"]);

    let st = s
        .propose_tags(b, ProposalSource::Llm, &[new("wi-fi", Some("Wi-Fi"))])
        .unwrap();
    assert_eq!(st.proposed, ["wi-fi"]);
    // Proposed again for the same bookmark: not new.
    let st = s
        .propose_tags(b, ProposalSource::Llm, &[new("wi-fi", None)])
        .unwrap();
    assert!(st.proposed.is_empty());

    assert_eq!(
        summary(&s),
        [("wi-fi".into(), vec![b, a]), ("go".into(), vec![a])]
    );
    let p = &pending(&s)[0];
    assert_eq!(p.description.as_deref(), Some("wireless networks"));
    assert_eq!(p.source, ProposalSource::Llm);
    assert_eq!(s.proposals_for(a).unwrap(), ["go", "wi-fi"]);

    // In auto mode an alias of a list tag is applied, like any suggestion.
    s.set_suggest_mode(SuggestMode::Auto);
    let st = s
        .propose_tags(b, ProposalSource::Llm, &[new("RS", None)])
        .unwrap();
    assert_eq!(st.suggested.applied, 1);
    assert_eq!(tags_of(&s, b), ["rust"]);
    assert_eq!(source(&s, b, "rust"), "llm");
}

#[test]
fn accepting_puts_the_name_on_the_list_and_tags_its_bookmarks() {
    let mut s = store();
    let a = add(&mut s, "https://a.example/", &[]);
    let b = add(&mut s, "https://b.example/", &[]);
    let gone = add(&mut s, "https://c.example/", &[]);
    for id in [a, b, gone] {
        s.propose_tags(id, ProposalSource::Llm, &[new("wifi", Some("wireless"))])
            .unwrap();
    }
    s.delete(gone).unwrap();
    assert_eq!(summary(&s), [("wifi".into(), vec![b, a])]);

    let d = s.accept_proposal("WiFi").unwrap();
    assert_eq!((d.tag.as_str(), d.tagged, d.described), ("wifi", 2, true));
    assert_eq!(canonical(&s), ["wifi"]);
    assert_eq!(
        s.tags(TagSort::Name).unwrap()[0].description.as_deref(),
        Some("wireless")
    );
    assert_eq!(tags_of(&s, a), ["wifi"]);
    // The AI's tag stays marked as its own, so removing it later counts as a rejection.
    assert_eq!(source(&s, a, "wifi"), "llm");
    assert!(s.search("#wifi", 10).unwrap().len() == 2, "reindexed");
    assert!(pending(&s).is_empty());

    // Now on the list, the name is a suggestion, not a proposal.
    let st = s
        .propose_tags(a, ProposalSource::Llm, &[new("wifi", None)])
        .unwrap();
    assert!(st.proposed.is_empty());
    let e = s.accept_proposal("wifi").unwrap_err().to_string();
    assert_eq!(e, "no proposal \"wifi\" (see `shlv tag proposals`)");

    // A description doesn't replace the tag's own.
    s.add_canonical(&strings(&["net"])).unwrap();
    s.describe_tag("net", "networking").unwrap();
    s.propose_tags(a, ProposalSource::User, &[new("network", Some("other"))])
        .unwrap();
    s.add_alias("network", "net").unwrap();
    let d = s.accept_proposal("network").unwrap();
    assert_eq!((d.tag.as_str(), d.described), ("net", false));
    assert_eq!(tags_of(&s, a), ["net", "wifi"]);
    assert_eq!(source(&s, a, "net"), "user");
}

#[test]
fn accepting_with_a_declared_list_adds_to_the_database() {
    let mut s = store();
    let declared: BTreeMap<String, DeclaredEntry> =
        [("rust".to_string(), DeclaredEntry::default())].into();
    s.pin(DeclaredTags::new(declared).unwrap()).unwrap();
    let a = add(&mut s, "https://a.example/", &[]);
    s.propose_tags(
        a,
        ProposalSource::Llm,
        &[new("wifi", None), new("rust", None)],
    )
    .unwrap();
    s.accept_proposal("wifi").unwrap();
    assert_eq!(canonical(&s), ["rust", "wifi"]);
    assert!(!s.pinned().unwrap().is_tag("wifi"));
    // Pinned tags still refuse to be undone; the accepted one can be.
    assert!(
        s.remove_canonical(&strings(&["rust"]))
            .unwrap_err()
            .is::<Pinned>()
    );
    assert_eq!(s.remove_canonical(&strings(&["wifi"])).unwrap(), 1);
}

#[test]
fn aliasing_makes_the_name_resolve_to_a_tag() {
    let mut s = store();
    s.add_canonical(&strings(&["wifi", "audio"])).unwrap();
    let a = add(&mut s, "https://a.example/", &[]);
    let b = add(&mut s, "https://b.example/", &["wireless"]); // a tag off the list
    s.propose_tags(
        a,
        ProposalSource::Llm,
        &[new("wireless", None), new("sound", None)],
    )
    .unwrap();
    s.propose_tags(b, ProposalSource::Llm, &[new("wireless", None)])
        .unwrap();

    let e = s.alias_proposal("sound", "nope").unwrap_err().to_string();
    assert_eq!(
        e,
        "no tag \"nope\"; to add \"sound\" to the list, accept it"
    );
    let d = s.alias_proposal("sound", "audio").unwrap();
    assert_eq!((d.tag.as_str(), d.tagged), ("audio", 1));
    assert_eq!(add(&mut s, "https://c.example/", &["sound"]), 3);
    assert_eq!(tags_of(&s, 3), ["audio"]);

    // The name was also a tag off the list: it is merged in, with its bookmarks.
    let d = s.alias_proposal("wireless", "wifi").unwrap();
    assert_eq!((d.tag.as_str(), d.tagged), ("wifi", 1));
    assert_eq!(tags_of(&s, a), ["audio", "wifi"]);
    assert_eq!(tags_of(&s, b), ["wifi"]);
    let t = s.tags(TagSort::Name).unwrap();
    let wifi = t.iter().find(|t| t.name == "wifi").unwrap();
    assert_eq!(wifi.aliases, ["wireless"]);
    assert!(pending(&s).is_empty());

    // In strict mode the target must be canonical.
    s.set_strict_tags(true);
    add(&mut s, "https://d.example/", &["wifi"]);
    s.set_strict_tags(false);
    add(&mut s, "https://e.example/", &["misc"]);
    s.propose_tags(4, ProposalSource::Llm, &[new("stuff", None)])
        .unwrap();
    s.set_strict_tags(true);
    let e = s.alias_proposal("stuff", "misc").unwrap_err().to_string();
    assert!(e.contains("isn't canonical (strict_tags is on)"), "{e}");
}

#[test]
fn rejected_names_are_never_proposed_again() {
    let mut s = store();
    let a = add(&mut s, "https://a.example/", &[]);
    s.propose_tags(
        a,
        ProposalSource::Llm,
        &[new("misc", None), new("cool", None)],
    )
    .unwrap();
    assert_eq!(s.reject_proposal("Misc").unwrap(), 1);
    // Rejecting ahead of time works too.
    assert_eq!(s.reject_proposal("useful").unwrap(), 0);
    assert_eq!(rejected(&s), ["misc", "useful"]);
    assert_eq!(summary(&s), [("cool".into(), vec![a])]);

    // From the AI.
    let st = s
        .propose_tags(
            a,
            ProposalSource::Llm,
            &[new("misc", None), new("useful", None)],
        )
        .unwrap();
    assert_eq!((st.proposed.len(), st.rejected.len()), (0, 2));
    // From typing, in propose mode.
    s.set_strict_tags(StrictTags::Propose);
    let added = s
        .add(&NewBookmark {
            url: "https://b.example/".into(),
            tags: strings(&["misc", "new"]),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(added.diverted.proposed, ["new"]);
    assert_eq!(added.diverted.rejected, ["misc"]);
    // From import.
    let sum = s
        .import(
            &[ImportItem {
                url: "https://c.example/".into(),
                tags: strings(&["useful", "cool"]),
                ..Default::default()
            }],
            ImportOptions::default(),
        )
        .unwrap();
    assert_eq!(
        (sum.proposed, sum.dropped_tags),
        (strings(&["cool"]), strings(&["useful"]))
    );
    assert_eq!(rejected(&s), ["misc", "useful"]);

    // Accepting a rejected name undoes the rejection.
    let d = s.accept_proposal("misc").unwrap();
    assert_eq!(d.tagged, 0);
    assert_eq!(rejected(&s), ["useful"]);
    // A name on the list can't be rejected.
    let e = s.reject_proposal("misc").unwrap_err().to_string();
    assert!(e.contains("is on the list"), "{e}");
}

#[test]
fn propose_mode_turns_typed_tags_into_proposals() {
    let mut s = store();
    s.add_canonical(&strings(&["rust"])).unwrap();
    s.add_alias("rs", "rust").unwrap();
    s.set_strict_tags(StrictTags::Propose);
    let added = s
        .add(&NewBookmark {
            url: "https://a.example/".into(),
            tags: strings(&["rs", "Web Dev", "misc"]),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(added.diverted.proposed, ["web-dev", "misc"]);
    assert!(added.diverted.rejected.is_empty());
    let a = added.id;
    assert_eq!(tags_of(&s, a), ["rust"]);
    assert_eq!(pending(&s)[0].source, ProposalSource::User);

    let d = s
        .edit_diverting(
            a,
            &Edit {
                add_tags: strings(&["css"]),
                remove_tags: strings(&["misc"]), // withdraws the proposal for this bookmark
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(d.proposed, ["css"]);
    assert_eq!(
        summary(&s),
        [("css".into(), vec![a]), ("web-dev".into(), vec![a]),]
    );
    assert_eq!(tags_of(&s, a), ["rust"]);
    assert!(s.edit_diverting(99, &Edit::default()).unwrap().is_none());

    // Accepted, a user's proposal tags with source `user`.
    s.accept_proposal("web-dev").unwrap();
    assert_eq!(tags_of(&s, a), ["rust", "web-dev"]);
    assert_eq!(source(&s, a, "web-dev"), "user");
    // And the next time it's typed, it's simply a tag.
    let b = add(&mut s, "https://b.example/", &["web-dev"]);
    assert_eq!(tags_of(&s, b), ["web-dev"]);
}

#[test]
fn import_can_keep_unknown_tags_as_proposals() {
    let mut s = store();
    s.add_canonical(&strings(&["rust"])).unwrap();
    let items = [
        ImportItem {
            url: "https://a.example/".into(),
            tags: strings(&["rust", "Toolbar", "dev"]),
            ..Default::default()
        },
        ImportItem {
            url: "https://b.example/".into(),
            tags: strings(&["dev"]),
            ..Default::default()
        },
    ];
    // --drop-unknown-tags alone drops them, as before.
    let opts = ImportOptions {
        drop_non_canonical: true,
        ..Default::default()
    };
    let sum = s.import(&items, opts).unwrap();
    assert_eq!(sum.dropped_tags, ["toolbar", "dev"]);
    assert!(sum.proposed.is_empty() && pending(&s).is_empty());

    let opts = ImportOptions {
        propose_unknown: true,
        ..Default::default()
    };
    let sum = s.import(&items, opts).unwrap();
    assert_eq!(sum.proposed, ["toolbar", "dev"]);
    assert!(sum.dropped_tags.is_empty());
    assert_eq!(
        summary(&s),
        [("dev".into(), vec![2, 1]), ("toolbar".into(), vec![1])]
    );
    assert_eq!(pending(&s)[0].source, ProposalSource::Import);
    assert_eq!(tags_of(&s, 1), ["rust"]);
    let d = s.accept_proposal("dev").unwrap();
    assert_eq!(d.tagged, 2);
    assert_eq!(source(&s, 2, "dev"), "user");
}

#[test]
fn without_proposals_nothing_changes() {
    let mut s = store();
    // Strict off: any tag goes, and nothing is proposed.
    let a = add(&mut s, "https://a.example/", &["anything"]);
    assert_eq!(tags_of(&s, a), ["anything"]);
    // Strict on: an error, as before, and nothing is written.
    s.set_strict_tags(StrictTags::On);
    let e = s
        .add(&NewBookmark {
            url: "https://b.example/".into(),
            tags: strings(&["other"]),
            ..Default::default()
        })
        .unwrap_err();
    assert!(e.is::<NonCanonicalTags>());
    assert!(s.find_by_url("https://b.example/").unwrap().is_none());
    let sum = s
        .import(
            &[ImportItem {
                url: "https://c.example/".into(),
                tags: strings(&["other"]),
                ..Default::default()
            }],
            ImportOptions::default(),
        )
        .unwrap_err();
    assert!(sum.is::<NonCanonicalTags>());
    let n: i64 = s
        .conn()
        .query_row("SELECT count(*) FROM tag_proposal", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
}
