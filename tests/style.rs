//! Tag style (task 033) through the library: every entry point fixes what it can and refuses
//! the rest, existing names are left alone, and without a style nothing changes.

use shlv::choose::NewTag;
use shlv::{
    CanonicalEntry, Edit, ImportItem, ImportOptions, NewBookmark, ProposalSource, ProposalState,
    Separator, Store, StrictTags, TagSort, TagStyle,
};

fn styled(sep: Option<Separator>, max: Option<usize>) -> Store {
    let mut s = Store::open_in_memory().unwrap();
    s.set_style(TagStyle {
        separator: sep,
        max_length: max,
    });
    s
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn add(s: &mut Store, url: &str, tags: &[&str]) -> anyhow::Result<i64> {
    Ok(s.add(&NewBookmark {
        url: url.into(),
        tags: strings(tags),
        ..Default::default()
    })?
    .id)
}

fn tags_of(s: &Store, id: i64) -> Vec<String> {
    s.get(id).unwrap().unwrap().tags
}

fn all_tags(s: &Store) -> Vec<String> {
    let mut v: Vec<String> = s
        .tags(TagSort::Name)
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    v.sort();
    v
}

#[test]
fn typed_tags_are_fixed_or_refused() {
    let mut s = styled(Some(Separator::Underscore), Some(10));
    let id = add(&mut s, "a.com", &["Home Lab", "sci-fi", "#Rust"]).unwrap();
    assert_eq!(tags_of(&s, id), ["home_lab", "rust", "sci_fi"]);

    let e = add(&mut s, "b.com", &["rust", "self-hosting"]).unwrap_err();
    assert_eq!(
        e.to_string(),
        "\"self_hosting\" is longer than [tags.style] max_length = 10"
    );
    // Refused before anything is written.
    assert!(s.find_by_url("b.com").unwrap().is_none());
    assert!(add(&mut s, "b.com", &["- _"]).is_err());

    // Edit: added tags too; removed ones are looked up in either form.
    s.edit(
        id,
        &Edit {
            add_tags: strings(&["web dev"]),
            remove_tags: strings(&["home-lab", "Sci Fi"]),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(tags_of(&s, id), ["rust", "web_dev"]);
    let e = s
        .edit(
            id,
            &Edit {
                add_tags: strings(&["networking-basics"]),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(e.to_string().contains("max_length = 10"), "{e}");
}

#[test]
fn separators_join_or_vanish() {
    let mut s = styled(Some(Separator::None), None);
    let id = add(&mut s, "a.com", &["home-lab", "Home_Lab", "sci fi"]).unwrap();
    assert_eq!(tags_of(&s, id), ["homelab", "scifi"]);
    let mut s = styled(Some(Separator::Hyphen), None);
    let id = add(&mut s, "a.com", &["home_lab", " sci _ fi "]).unwrap();
    assert_eq!(tags_of(&s, id), ["home-lab", "sci-fi"]);
}

#[test]
fn existing_tags_are_left_alone() {
    let mut s = Store::open_in_memory().unwrap();
    let a = add(&mut s, "a.com", &["web-dev", "self-hosting-infrastructure"]).unwrap();
    s.add_alias("home lab", "web-dev").unwrap();
    s.set_style(TagStyle {
        separator: Some(Separator::Underscore),
        max_length: Some(12),
    });
    // Nothing was rewritten.
    assert_eq!(all_tags(&s), ["self-hosting-infrastructure", "web-dev"]);
    // Existing names still work, as typed or styled.
    let b = add(&mut s, "b.com", &["self-hosting-infrastructure", "web-dev"]).unwrap();
    assert_eq!(tags_of(&s, b), tags_of(&s, a));
    let c = add(&mut s, "c.com", &["home-lab"]).unwrap();
    assert_eq!(tags_of(&s, c), ["web-dev"]);
    // Searching and listing find them too.
    let q = shlv::ListQuery {
        tags: strings(&["web dev"]),
        limit: 10,
    };
    assert_eq!(s.list(&q).unwrap().len(), 3);
    // A new name is styled; `tag rename` fixes an old one, under the style.
    let e = s
        .rename_tag("self-hosting-infrastructure", "self hosting stack")
        .unwrap_err();
    assert!(e.to_string().contains("max_length = 12"), "{e}");
    let r = s
        .rename_tag("self-hosting-infrastructure", "self host")
        .unwrap();
    assert_eq!((r.tag.as_str(), r.bookmarks), ("self_host", 2));
    s.rename_tag("web-dev", "web dev").unwrap();
    assert_eq!(all_tags(&s), ["self_host", "web_dev"]);
}

#[test]
fn tag_commands_follow_the_style() {
    let mut s = styled(Some(Separator::Hyphen), Some(8));
    assert_eq!(s.add_canonical(&strings(&["Home_Lab", "rust"])).unwrap(), 2);
    let e = s.add_canonical(&strings(&["networking"])).unwrap_err();
    assert!(e.to_string().contains("max_length = 8"), "{e}");
    // Aliases are names to type: separators fixed, no length limit.
    s.add_alias("Home_Laboratory", "home lab").unwrap();
    let id = add(&mut s, "a.com", &["home laboratory"]).unwrap();
    assert_eq!(tags_of(&s, id), ["home-lab"]);
    s.describe_tag("home_lab", "servers at home").unwrap();
    s.remove_canonical(&strings(&["Home_Lab"])).unwrap();
    s.merge_tags("home_lab", "rust").unwrap();
    assert_eq!(all_tags(&s), ["rust"]);

    // A canonical list file: tags fixed or refused with the line, aliases fixed.
    let entries = shlv::tag::parse_canonical("web_dev web_development # the web\n");
    assert_eq!(
        entries,
        [CanonicalEntry {
            line: 1,
            tag: "web_dev".into(),
            aliases: strings(&["web_development"]),
            description: Some("the web".into()),
        }]
    );
    s.import_canonical(&entries).unwrap();
    let id = add(&mut s, "b.com", &["web development"]).unwrap();
    assert_eq!(tags_of(&s, id), ["web-dev"]);
    let e = s
        .import_canonical(&shlv::tag::parse_canonical("go\ndatabases\n"))
        .unwrap_err();
    assert_eq!(
        format!("{e:#}"),
        "line 2: \"databases\" is longer than [tags.style] max_length = 8"
    );
}

#[test]
fn import_fixes_and_drops() {
    let mut s = styled(Some(Separator::Hyphen), Some(10));
    add(&mut s, "x.com", &["rust"]).unwrap();
    let item = |url: &str, tags: &[&str]| ImportItem {
        url: url.into(),
        tags: strings(tags),
        ..Default::default()
    };
    let items = [
        item("a.com", &["Home_Lab", "home lab", "rust"]),
        item("b.com", &["self_hosting_stuff", "home-lab"]),
        item("c.com", &["self hosting stuff"]),
    ];
    let sum = s.import(&items, ImportOptions::default()).unwrap();
    assert_eq!(sum.added, 3);
    // Each refused name is reported once.
    assert_eq!(
        sum.refused_tags,
        ["\"self-hosting-stuff\" is longer than [tags.style] max_length = 10"]
    );
    let a = s.find_by_url("a.com").unwrap().unwrap();
    assert_eq!(a.tags, ["home-lab", "rust"]);
    let b = s.find_by_url("b.com").unwrap().unwrap();
    assert_eq!(b.tags, ["home-lab"]);
    assert!(s.find_by_url("c.com").unwrap().unwrap().tags.is_empty());
}

#[test]
fn proposals_follow_the_style() {
    let mut s = styled(Some(Separator::Underscore), Some(10));
    let id = add(&mut s, "a.com", &[]).unwrap();
    let new = |n: &str| NewTag {
        name: n.into(),
        description: None,
    };
    let st = s
        .propose_tags(
            id,
            ProposalSource::Llm,
            &[new("Home-Lab"), new("self-hosting"), new("")],
        )
        .unwrap();
    assert_eq!(st.proposed, ["home_lab"]);
    assert_eq!(
        st.refused,
        ["\"self_hosting\" is longer than [tags.style] max_length = 10"]
    );
    // Decisions find proposals in either form; rejecting ahead of time stores the styled name.
    s.reject_proposal("web dev").unwrap();
    let st = s
        .propose_tags(id, ProposalSource::Llm, &[new("web-dev")])
        .unwrap();
    assert_eq!(st.rejected, ["web_dev"]);
    let d = s.accept_proposal("home-lab").unwrap();
    assert_eq!(d.tag, "home_lab");

    // strict_tags = "propose": typed names are styled before they become proposals.
    s.set_strict_tags(StrictTags::Propose);
    let added = s
        .add(&NewBookmark {
            url: "b.com".into(),
            tags: strings(&["Sci-Fi"]),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(added.diverted.proposed, ["sci_fi"]);
    let names: Vec<String> = s
        .proposals(ProposalState::Pending)
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert_eq!(names, ["sci_fi"]);
}

#[test]
fn proposals_from_before_the_style_are_fixed_on_accept() {
    let mut s = Store::open_in_memory().unwrap();
    let id = add(&mut s, "a.com", &[]).unwrap();
    let new = |n: &str| NewTag {
        name: n.into(),
        description: None,
    };
    s.propose_tags(
        id,
        ProposalSource::Llm,
        &[new("home-lab"), new("self-hosting")],
    )
    .unwrap();
    s.set_style(TagStyle {
        separator: Some(Separator::Underscore),
        max_length: Some(10),
    });
    assert_eq!(s.accept_proposal("home-lab").unwrap().tag, "home_lab");
    let e = s.accept_proposal("self-hosting").unwrap_err();
    assert_eq!(
        e.to_string(),
        "cannot accept \"self-hosting\": \"self_hosting\" is longer than [tags.style] \
         max_length = 10; alias it to a tag or reject it"
    );
    assert_eq!(tags_of(&s, id), ["home_lab"]);
}

#[test]
fn no_style_changes_nothing() {
    let mut s = Store::open_in_memory().unwrap();
    let long = "a-very-long-tag-name-that-goes-on-and-on";
    let id = add(&mut s, "a.com", &["sci_fi", "Home Lab", long]).unwrap();
    assert_eq!(
        tags_of(&s, id),
        [
            "a-very-long-tag-name-that-goes-on-and-on",
            "home-lab",
            "sci_fi"
        ]
    );
}
