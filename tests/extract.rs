//! Main-text extraction on saved real pages (feature `fetch`). Sources and licenses are in
//! tests/fixtures/pages/README.md.
#![cfg(feature = "fetch")]

use shlv::extract::{MAX_WORDS, extract};

struct Case {
    page: &'static str,
    /// Sentences (or fragments) the extract must contain.
    must: &'static [&'static str],
    /// Boilerplate it must not contain.
    must_not: &'static [&'static str],
}

const CASES: &[Case] = &[
    Case {
        page: "rust-blog", // blog post
        must: &[
            "The Rust team is happy to announce a new version of Rust, 1.83.0.",
            "This release includes several large extensions to what code running in const \
             contexts can do.",
            "static S: i32 = 25;\nconst C: &i32 = &S;",
        ],
        must_not: &["Maintained by", "Get help", "Rust Blog"],
    },
    Case {
        page: "rust-book", // docs page with a chapter sidebar
        must: &[
            "Ownership is a set of rules that govern how a Rust program manages memory.",
            "Because ownership is a new concept for many programmers, it does take some time to \
             get used to.",
        ],
        must_not: &["Keyboard shortcuts", "Coal", "Navy", "Ayu"],
    },
    Case {
        page: "github-readme", // a README on GitHub
        must: &[
            "ripgrep is a line-oriented search tool that recursively searches the current \
             directory for a regex pattern.",
            "Dual-licensed under MIT or the UNLICENSE.",
        ],
        must_not: &[
            "Sign in",
            "Pricing",
            "Notifications",
            "You signed in with another tab",
        ],
    },
    Case {
        page: "wikinews", // news article
        must: &[
            "The Danish firm Monta published a performance ranking of electric vehicle (EV) \
             chargers on Wednesday.",
            "AC chargers performance was topped by a Mennekes EV charger",
        ],
        must_not: &[
            "Jump to content",
            "Privacy policy",
            "Main page",
            "Download as PDF",
        ],
    },
    Case {
        page: "wikipedia-talk", // threaded discussion
        must: &[
            "I don't really think Digg/reddit/Netscape/Pligg etc are \"social bookmarking\" and \
             are instead \"social news\".",
            "I highly agree with JoshuaSchachter's opinion.",
        ],
        must_not: &[
            "Jump to content",
            "Privacy policy",
            "Edit this page",
            "Main page",
        ],
    },
    Case {
        page: "wikipedia", // encyclopedia article
        must: &[
            "Social bookmarking is an online service which allows users to add, annotate, edit, \
             and share bookmarks of web documents.",
            "Unlike file sharing, social bookmarking does not save the resources themselves",
        ],
        must_not: &[
            "Jump to content",
            "Learn how and when to remove",
            "Why Links Are Important for SEO",
        ],
    },
    Case {
        page: "mdn", // reference docs
        must: &[
            "The <article> HTML element represents a self-contained composition in a document",
            "Permitted ARIA roles application, document, feed, main",
        ],
        must_not: &[
            "Skip to main content",
            "Your blueprint for a better internet",
            "Report a problem",
        ],
    },
    Case {
        page: "python-docs", // Sphinx docs
        must: &[
            "The sqlite3 module was written by Gerhard Häring.",
            "Tutorial teaches how to use the sqlite3 module.",
        ],
        must_not: &[
            "Table of Contents",
            "Previous topic",
            "Navigation",
            "Copyright",
        ],
    },
    Case {
        page: "sqlite-fts5", // single long docs page
        must: &[
            "FTS5 is an SQLite virtual table module that provides full-text search functionality \
             to database applications.",
            "CREATE VIRTUAL TABLE email USING fts5(sender, title, body);",
        ],
        must_not: &["Small. Fast. Reliable", "Download"],
    },
    Case {
        page: "serde-docsrs", // rustdoc on docs.rs
        must: &[
            "Serde is a framework for serializing and deserializing Rust data structures \
             efficiently and generically.",
            "A data structure that can be deserialized from any data format supported by Serde.",
        ],
        must_not: &["All Items", "Skip to main content", "Privacy policy"],
    },
];

fn page(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/pages/{name}.html",
        env!("CARGO_MANIFEST_DIR")
    );
    String::from_utf8_lossy(&std::fs::read(path).unwrap()).into_owned()
}

#[test]
fn real_pages() {
    for c in CASES {
        let html = page(c.page);
        let text = extract(&html);
        for s in c.must {
            assert!(text.contains(s), "{}: missing {s:?}", c.page);
        }
        for s in c.must_not {
            assert!(
                html.contains(s),
                "{}: {s:?} isn't in the page; pick another",
                c.page
            );
            assert!(!text.contains(s), "{}: kept boilerplate {s:?}", c.page);
        }
        assert!(text.split_whitespace().count() <= MAX_WORDS, "{}", c.page);
    }
}

#[test]
fn javascript_shell_has_no_text() {
    let html = page("excalidraw");
    assert!(html.contains("You need to enable JavaScript to run this app."));
    assert_eq!(extract(&html), "");
}

#[test]
fn citation_markers_are_dropped() {
    let text = extract(&page("wikipedia"));
    assert!(
        text.contains("share bookmarks of web documents. Many online"),
        "{text}"
    );
    assert!(!text.contains("[1]"));
}
