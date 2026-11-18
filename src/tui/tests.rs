//! The TUI driven by key sequences against a temporary database, checking the screen (rendered
//! to a `TestBackend`) and the data.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use shlv::config::Config;
use shlv::{DomainRules, NewBookmark, Store};

use super::App;
use super::app::{Tone, View};

/// A database file in a fresh temporary directory, removed on drop.
struct Db {
    dir: PathBuf,
}

impl Db {
    fn new() -> Db {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "shlv-tui-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Db { dir }
    }

    fn path(&self) -> PathBuf {
        self.dir.join("shlv.db")
    }

    fn store(&self) -> Store {
        Store::open(&self.path()).unwrap()
    }

    /// Adds bookmarks, oldest first: (url, title, tags).
    fn seed(&self, items: &[(&str, &str, &[&str])]) {
        let mut s = self.store();
        for (url, title, tags) in items {
            s.add(&NewBookmark {
                url: url.to_string(),
                title: (!title.is_empty()).then(|| title.to_string()),
                note: None,
                tags: tags.iter().map(|t| t.to_string()).collect(),
            })
            .unwrap();
            // Distinct creation times keep "newest first" well defined.
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    fn app(&self) -> App {
        self.app_with(Config::default(), |_| {})
    }

    /// An app whose store (and worker's store) `setup` configures.
    fn app_with(&self, config: Config, setup: fn(&mut Store)) -> App {
        let path = self.path();
        let mut store = self.store();
        setup(&mut store);
        let worker_config = toml_config(&config);
        let worker = Box::new(move || {
            let mut s = Store::open(&path)?;
            setup(&mut s);
            Ok((s, worker_config))
        });
        App::new(store, config, worker).unwrap()
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A copy of a config, through its TOML (it isn't `Clone`).
fn toml_config(c: &Config) -> Config {
    let text = toml::to_string(c).unwrap();
    Config::parse(&text, "test.toml".as_ref()).unwrap()
}

/// Presses keys: characters as typed, and `<Enter>`, `<Esc>`, `<Tab>`, `<BackTab>`, `<Up>`,
/// `<Down>`, `<BS>`, `<C-x>` (Ctrl), `<A-Enter>` (Alt).
fn press(app: &mut App, keys: &str) {
    let mut rest = keys;
    while let Some(c) = rest.chars().next() {
        if c == '<'
            && let Some(end) = rest.find('>')
            && end > 1
        {
            let name = &rest[1..end];
            rest = &rest[end + 1..];
            let (mods, name) = match name.split_once('-') {
                Some(("C", n)) => (KeyModifiers::CONTROL, n),
                Some(("A", n)) => (KeyModifiers::ALT, n),
                _ => (KeyModifiers::NONE, name),
            };
            let code = match name {
                "Enter" => KeyCode::Enter,
                "Esc" => KeyCode::Esc,
                "Tab" => KeyCode::Tab,
                "BackTab" => KeyCode::BackTab,
                "Up" => KeyCode::Up,
                "Down" => KeyCode::Down,
                "BS" => KeyCode::Backspace,
                n if n.chars().count() == 1 => KeyCode::Char(n.chars().next().unwrap()),
                n => panic!("unknown key <{n}>"),
            };
            app.key(KeyEvent::new(code, mods));
            app.settle();
            continue;
        }
        rest = &rest[c.len_utf8()..];
        app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        app.settle();
    }
}

/// The screen as text, one string per row (trailing spaces trimmed).
fn screen(app: &mut App, width: u16, height: u16) -> Vec<String> {
    let mut t = Terminal::new(TestBackend::new(width, height)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let buf = t.backend().buffer();
    (0..height)
        .map(|y| {
            let row: String = (0..width).map(|x| buf[(x, y)].symbol()).collect();
            row.trim_end().to_string()
        })
        .collect()
}

fn text(app: &mut App) -> String {
    screen(app, 120, 30).join("\n")
}

fn message(app: &App) -> String {
    app.message().map(|(m, _)| m.clone()).unwrap_or_default()
}

fn tags(db: &Db, url: &str) -> Vec<String> {
    db.store().find_by_url(url).unwrap().unwrap().tags
}

fn three(db: &Db) {
    db.seed(&[
        ("https://a.example/one", "Rust async guide", &["rust"]),
        ("https://b.example/two", "Python tips", &["python"]),
        (
            "https://c.example/three",
            "Rust web frameworks",
            &["rust", "web"],
        ),
    ]);
}

#[test]
fn lists_newest_first_and_moves() {
    let db = Db::new();
    three(&db);
    let mut app = db.app();
    let s = screen(&mut app, 120, 20);
    assert!(s[0].contains("1 Bookmarks"), "{s:#?}");
    let first = s
        .iter()
        .position(|l| l.contains("Rust web frameworks"))
        .unwrap();
    let last = s
        .iter()
        .position(|l| l.contains("Rust async guide"))
        .unwrap();
    assert!(first < last, "newest first: {s:#?}");
    assert!(s[18].contains("1/3"), "{s:#?}");
    // The detail pane shows the current bookmark in full.
    assert!(text(&mut app).contains("https://c.example/three"));
    press(&mut app, "j");
    let t = text(&mut app);
    assert!(
        t.contains("2/3") && t.contains("https://b.example/two"),
        "{t}"
    );
    press(&mut app, "G");
    assert_eq!(app.bookmarks.cursor, 2);
    press(&mut app, "jjg");
    assert_eq!(app.bookmarks.cursor, 0);
    // Tab hides the detail pane.
    press(&mut app, "<Tab>");
    assert!(!text(&mut app).contains("https://c.example/three"));
}

#[test]
fn scrolls_over_a_large_library() {
    let db = Db::new();
    {
        let mut s = db.store();
        for i in 0..300 {
            s.add(&NewBookmark {
                url: format!("https://x.example/{i}"),
                title: Some(format!("Page {i}")),
                ..Default::default()
            })
            .unwrap();
        }
    }
    let mut app = db.app();
    screen(&mut app, 100, 20);
    press(&mut app, "G");
    let s = screen(&mut app, 100, 20).join("\n");
    assert!(s.contains("300/300"), "{s}");
    press(&mut app, "<C-u>");
    let s = screen(&mut app, 100, 20).join("\n");
    assert!(s.contains("292/300"), "half a page of 16 rows up: {s}");
}

#[test]
fn searches_as_you_type() {
    let db = Db::new();
    three(&db);
    let mut app = db.app();
    press(&mut app, "/rust");
    assert_eq!(app.bookmarks.ids.len(), 2);
    let t = text(&mut app);
    assert!(!t.contains("Python tips"), "{t}");
    assert!(t.contains("/rust") && t.contains("2 of 3"), "{t}");
    // The query language: tags, sites, NOT.
    press(&mut app, "<C-u>#python");
    assert_eq!(app.bookmarks.ids.len(), 1);
    press(&mut app, "<C-u>rust site:c.example");
    assert_eq!(app.bookmarks.ids.len(), 1);
    press(&mut app, "<C-u>rust NOT web");
    assert_eq!(app.bookmarks.ids.len(), 1);
    // An unfinished query keeps the last results and says why.
    press(&mut app, "<C-u>rust O");
    let before = app.bookmarks.ids.clone();
    press(&mut app, "R");
    assert_eq!(app.bookmarks.ids, before);
    assert!(text(&mut app).contains("invalid search query"));
    press(&mut app, "<C-w><Enter>");
    assert_eq!(app.bookmarks.query, "rust ");
    assert!(!text(&mut app).contains("invalid search query"));
    // Esc in the list drops the search.
    press(&mut app, "<Esc>");
    assert_eq!(app.bookmarks.ids.len(), 3);
    // Esc while typing goes back to the search before it.
    press(&mut app, "/web<Enter>/xyz<Esc>");
    assert_eq!(app.bookmarks.query, "web");
    assert_eq!(app.bookmarks.ids.len(), 1);
    press(&mut app, "<Esc>/zzzz");
    assert!(text(&mut app).contains("Nothing matches"));
}

#[test]
fn adds_with_rules_and_accepts_suggestions() {
    let db = Db::new();
    three(&db);
    let mut app = db.app_with(Config::default(), |s| {
        let rules = [("news.example".to_string(), vec!["news".to_string()])];
        s.set_rules(DomainRules::new(rules.into_iter().collect()).unwrap());
    });
    press(&mut app, "a");
    assert!(text(&mut app).contains("Add a bookmark"));
    press(
        &mut app,
        "news.example/today<Tab>Today's news<Tab>daily<Enter>",
    );
    let b = db
        .store()
        .find_by_url("https://news.example/today")
        .unwrap()
        .expect("added");
    assert_eq!(b.title.as_deref(), Some("Today's news"));
    assert_eq!(b.tags, ["daily"]);
    assert!(
        message(&app).contains("suggested #news"),
        "{}",
        message(&app)
    );
    // The new bookmark is current, its suggestion in the detail pane.
    assert_eq!(app.bookmarks.current(), Some(b.id));
    let t = text(&mut app);
    assert!(t.contains("suggested #news (rule"), "{t}");
    press(&mut app, "y");
    assert_eq!(tags(&db, "https://news.example/today"), ["daily", "news"]);
    let t = text(&mut app);
    assert!(
        t.contains("#daily #news (rule") && !t.contains("y/n/s"),
        "{t}"
    );
}

#[test]
fn add_errors_keep_the_form() {
    let db = Db::new();
    let mut app = db.app_with(Config::default(), |s| {
        s.set_strict_tags(true);
    });
    let t = text(&mut app);
    assert!(t.contains("No bookmarks yet"), "{t}");
    press(&mut app, "a<Enter>");
    assert_eq!(message(&app), "a URL, please");
    press(&mut app, "x.example<Tab><Tab>nope<Enter>");
    let (m, tone) = app.message().unwrap().clone();
    assert_eq!(tone, Tone::Error);
    assert!(m.contains("canonical"), "{m}");
    assert!(
        text(&mut app).contains("Add a bookmark"),
        "the form stays open"
    );
    press(&mut app, "<Esc>");
    assert!(!text(&mut app).contains("Add a bookmark"));
    assert!(db.store().list_ids().unwrap().is_empty());
}

#[test]
fn edits_tags_with_completion() {
    let db = Db::new();
    three(&db);
    let mut app = db.app();
    // The current bookmark (c) has rust and web; the text is its tag list.
    press(&mut app, "t");
    assert!(text(&mut app).contains("Tags of 3"));
    press(&mut app, "<C-w><C-w>py<Tab>");
    assert!(text(&mut app).contains("python"));
    press(&mut app, " fresh<Enter>");
    assert_eq!(tags(&db, "https://c.example/three"), ["fresh", "python"]);
    assert!(message(&app).contains("+#python"), "{}", message(&app));
    // `-tag` removes, too.
    press(&mut app, "t -fresh<Enter>");
    assert_eq!(tags(&db, "https://c.example/three"), ["python"]);
    // Completion learns new tags, and says when nothing matches.
    press(&mut app, "t<C-u>fr<Tab>");
    assert!(text(&mut app).contains("Tags: fresh"));
    press(&mut app, " zq<Tab>");
    assert_eq!(message(&app), "no tag starts that way");
}

#[test]
fn edits_title_and_note() {
    let db = Db::new();
    three(&db);
    let mut app = db.app();
    press(
        &mut app,
        "e<C-u>Better title<Down>first<A-Enter>second<Enter>",
    );
    let b = db
        .store()
        .find_by_url("https://c.example/three")
        .unwrap()
        .unwrap();
    assert_eq!(b.title.as_deref(), Some("Better title"));
    assert_eq!(b.note.as_deref(), Some("first\nsecond"));
    let t = text(&mut app);
    assert!(t.contains("Better title") && t.contains("second"), "{t}");
    press(&mut app, "e<Enter>");
    assert_eq!(message(&app), "nothing changed");
}

#[test]
fn deletes_and_undoes() {
    let db = Db::new();
    three(&db);
    let mut app = db.app_with(Config::default(), |s| {
        let rules = [("c.example".to_string(), vec!["sites".to_string()])];
        s.set_rules(DomainRules::new(rules.into_iter().collect()).unwrap());
        s.apply_rules(None).unwrap();
    });
    press(&mut app, "dx");
    assert_eq!(db.store().list_ids().unwrap().len(), 3, "d then x cancels");
    press(&mut app, "dd");
    assert_eq!(db.store().list_ids().unwrap().len(), 2);
    assert!(message(&app).contains("deleted"), "{}", message(&app));
    assert!(!text(&mut app).contains("Rust web frameworks"));
    press(&mut app, "u");
    let s = db.store();
    let b = s.find_by_url("https://c.example/three").unwrap().unwrap();
    assert_eq!(b.tags, ["rust", "web"]);
    assert_eq!(b.title.as_deref(), Some("Rust web frameworks"));
    assert_eq!(
        s.pending(Some(b.id)).unwrap().len(),
        1,
        "suggestions come back"
    );
    assert_eq!(
        s.search_ids("frameworks", 10).unwrap(),
        [b.id],
        "and the index"
    );
    assert_eq!(app.bookmarks.current(), Some(b.id));
    press(&mut app, "u");
    assert_eq!(message(&app), "nothing to undo");
}

#[test]
fn marks_several() {
    let db = Db::new();
    three(&db);
    let mut app = db.app();
    // Mark c and b, tag both, then delete both and undo.
    press(&mut app, "vv");
    assert_eq!(app.bookmarks.marked.len(), 2);
    assert!(text(&mut app).contains("2 marked"));
    press(&mut app, "t+shared -rust<Enter>");
    assert_eq!(tags(&db, "https://c.example/three"), ["shared", "web"]);
    assert_eq!(tags(&db, "https://b.example/two"), ["python", "shared"]);
    assert_eq!(tags(&db, "https://a.example/one"), ["rust"]);
    press(&mut app, "dd");
    assert_eq!(db.store().list_ids().unwrap().len(), 1);
    assert!(app.bookmarks.marked.is_empty());
    press(&mut app, "u");
    assert_eq!(db.store().list_ids().unwrap().len(), 3);
    // Esc clears marks before the search.
    press(&mut app, "/rust<Enter>v<Esc>");
    assert!(app.bookmarks.marked.is_empty());
    assert_eq!(app.bookmarks.query, "rust");
}

#[test]
fn decides_suggestions_one_by_one() {
    let db = Db::new();
    three(&db);
    let mut app = db.app_with(Config::default(), |s| {
        let rules = [(
            "c.example".to_string(),
            vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()],
        )];
        s.set_rules(DomainRules::new(rules.into_iter().collect()).unwrap());
        s.apply_rules(None).unwrap();
    });
    // On c: reject alpha, skip beta, accept gamma, then leave.
    press(&mut app, "s");
    assert!(text(&mut app).contains("y accept · n reject"));
    press(&mut app, "njy<Esc>");
    let s = db.store();
    let id = s
        .find_by_url("https://c.example/three")
        .unwrap()
        .unwrap()
        .id;
    let pending: Vec<String> = s
        .pending(Some(id))
        .unwrap()
        .into_iter()
        .map(|p| p.tag)
        .collect();
    assert_eq!(pending, ["beta"]);
    assert_eq!(
        tags(&db, "https://c.example/three"),
        ["gamma", "rust", "web"]
    );
    press(&mut app, "n");
    assert!(db.store().pending(Some(id)).unwrap().is_empty());
    press(&mut app, "j s");
    assert_eq!(message(&app), "no pending suggestions");
}

#[test]
fn opens_the_current_bookmark() {
    let db = Db::new();
    three(&db);
    let mut app = db.app();
    press(&mut app, "j<Enter>");
    assert_eq!(app.take_open().as_deref(), Some("https://b.example/two"));
    press(&mut app, "o");
    assert_eq!(app.take_open().as_deref(), Some("https://b.example/two"));
}

#[test]
fn switches_views_and_runs_commands() {
    let db = Db::new();
    three(&db);
    let mut app = db.app();
    press(&mut app, "3");
    assert_eq!(app.view, View::Tags);
    assert!(text(&mut app).contains("Names proposed"));
    press(&mut app, ":bookmarks<Enter>");
    assert_eq!(app.view, View::Bookmarks);
    press(&mut app, ":rev<Enter>");
    assert_eq!(app.view, View::Review);
    press(&mut app, ":nope<Enter>");
    assert!(message(&app).contains("unknown command :nope"));
    press(&mut app, "1?");
    let t = text(&mut app);
    assert!(
        t.contains("Keys") && t.contains("undo the last delete"),
        "{t}"
    );
    // Any key closes the help, and does nothing else.
    press(&mut app, "q");
    assert!(!app.quit);
    assert!(!text(&mut app).contains("undo the last delete"));
    press(&mut app, ":q<Enter>");
    assert!(app.quit);
}

#[test]
fn quits_with_q_and_ctrl_c() {
    let db = Db::new();
    let mut app = db.app();
    press(&mut app, "q");
    assert!(app.quit);
    let mut app = db.app();
    press(&mut app, "a<C-c>");
    assert!(app.quit, "Ctrl-C quits from a form too");
    // `q` typed in a search is text.
    let mut app = db.app();
    press(&mut app, "/q");
    assert!(!app.quit);
}

#[test]
fn pasting_a_url_starts_adding_it() {
    let db = Db::new();
    let mut app = db.app();
    app.event(Event::Paste("https://pasted.example/x\n".into()));
    press(&mut app, "<Enter>");
    assert!(
        db.store()
            .find_by_url("https://pasted.example/x")
            .unwrap()
            .is_some()
    );
}

#[cfg(feature = "fetch")]
#[path = "../../tests/common/server.rs"]
mod server;

#[cfg(feature = "fetch")]
mod fetch {
    use super::*;

    use super::server::{Response, Server};

    fn pages() -> Server {
        Server::start(|r| match r.path.as_str() {
            "/titled" => Response::html("<title>A fetched title</title><p>hi</p>"),
            "/other" => Response::html("<title>Another</title>"),
            _ => Response::new(404),
        })
    }

    #[test]
    fn add_fetches_the_title_in_the_background() {
        let server = pages();
        let db = Db::new();
        let mut app = db.app();
        let url = server.url("/titled");
        press(&mut app, &format!("a{url}<Enter>"));
        assert!(app.busy());
        app.finish_jobs();
        assert_eq!(message(&app), "1: title: A fetched title");
        let b = db.store().find_by_url(&url).unwrap().unwrap();
        assert_eq!(b.title.as_deref(), Some("A fetched title"));
        assert!(text(&mut app).contains("A fetched title"));
    }

    #[test]
    fn fetches_marked_titles_and_reports_failures() {
        let server = pages();
        let db = Db::new();
        let (a, b) = (server.url("/other"), server.url("/missing"));
        db.seed(&[(a.as_str(), "", &[]), (b.as_str(), "", &[])]);
        let mut app = db.app();
        press(&mut app, "vvF");
        assert!(text(&mut app).contains("⟳"));
        app.finish_jobs();
        let (m, tone) = app.message().unwrap().clone();
        let s = db.store();
        assert_eq!(
            s.find_by_url(&a).unwrap().unwrap().title.as_deref(),
            Some("Another")
        );
        // The failed one comes last (the list is newest first, so b went first; a's success
        // replaced its message). Either way nothing is stuck.
        assert!(!app.busy(), "{m} {tone:?}");
    }
}

#[cfg(feature = "ai")]
mod ai {
    use super::*;

    use super::server::{Response, Server};

    #[test]
    fn summarizes_in_the_background() {
        let server = Server::start(|r| match r.path.as_str() {
            "/page" => Response::html(format!(
                "<title>Page</title><main><p>Zebras are striped equids from Africa. {}</p></main>",
                "More about zebras. ".repeat(20)
            )),
            "/v1/chat/completions" => Response::json(&serde_json::json!({
                "choices": [{"finish_reason": "stop", "message": {"role": "assistant",
                    "content": serde_json::json!({"summary": "About zebras."}).to_string()}}],
            })),
            _ => Response::new(404),
        });
        let db = Db::new();
        let url = server.url("/page");
        db.seed(&[(url.as_str(), "Page", &[])]);
        let config = Config::parse(
            &format!(
                "[ai]\nsummarize = \"local\"\n\n[provider.local]\nkind = \"openai-compatible\"\n\
                 base_url = \"{}\"\nmodel = \"m\"\n",
                server.url("/v1")
            ),
            "test.toml".as_ref(),
        )
        .unwrap();
        let mut app = db.app_with(config, |_| {});
        press(&mut app, "S");
        app.finish_jobs();
        assert_eq!(message(&app), "1: summarized");
        let b = db.store().find_by_url(&url).unwrap().unwrap();
        assert_eq!(b.summary.as_deref(), Some("About zebras."));
        assert!(text(&mut app).contains("About zebras."));
        // Without a provider, S says how to set one up.
        let mut app = db.app();
        press(&mut app, "S");
        assert!(message(&app).contains("no provider for summarize"));
    }

    fn seed_server(replies: Vec<serde_json::Value>) -> Server {
        let q = std::sync::Mutex::new(std::collections::VecDeque::from(replies));
        Server::start(move |r| match r.path.as_str() {
            "/v1/chat/completions" => {
                let content = q.lock().unwrap().pop_front().expect("a queued reply");
                Response::json(&serde_json::json!({"choices": [{"finish_reason": "stop",
                    "message": {"role": "assistant", "content": content.to_string()}}]}))
            }
            _ => Response::new(404),
        })
    }

    #[test]
    fn seeds_from_interests() {
        use serde_json::json;
        let server = seed_server(vec![
            json!({"interests": [
                {"interest": "nix", "tags": [
                    {"name": "nix", "description": "the Nix language"},
                    {"name": "flake", "description": "Nix flakes"},
                    {"name": "nixpkgs", "description": "the package collection"},
                ]},
                {"interest": "network", "tags": [
                    {"name": "wifi", "description": "wireless networking"},
                    {"name": "linux", "description": "already on the list"},
                    {"name": "proxmox", "description": "Proxmox VE"},
                ]},
            ]}),
            json!({"interests": [{"interest": "audio", "tags": [
                {"name": "speakers", "description": "loudspeakers"},
            ]}]}),
        ]);
        let db = Db::new();
        {
            let mut s = db.store();
            s.add_canonical(&["linux".to_string()]).unwrap();
            s.save_interests(&["nix".to_string(), "network".to_string()])
                .unwrap();
        }
        let config = Config::parse(
            &format!(
                "[ai]\ntag = \"local\"\n\n[provider.local]\nkind = \"openai-compatible\"\n\
                 base_url = \"{}\"\nmodel = \"m\"\n",
                server.url("/v1")
            ),
            "test.toml".as_ref(),
        )
        .unwrap();
        let mut app = db.app_with(config, |_| {});
        // s shows what is sent; n sends nothing.
        press(&mut app, "5s");
        let t = text(&mut app);
        assert!(t.contains("tag seed sends to"), "{t}");
        assert!(t.contains("interests: network, nix"), "{t}");
        press(&mut app, "n");
        assert_eq!(message(&app), "nothing sent");
        assert!(server.requests().is_empty());
        press(&mut app, "sy");
        app.finish_jobs();
        let t = text(&mut app);
        assert!(t.contains("Candidates from tag seed"), "{t}");
        assert!(t.contains("on the list"), "linux is listed: {t}");
        // By interest: network's wifi: rename (keeps); linux is skipped; proxmox: drop. nix's
        // nix: keep; flake: describe, keep; nixpkgs: merge into nix (Tab completes it).
        press(&mut app, "r<C-u>Wireless<Enter>");
        press(&mut app, "d");
        press(&mut app, "y");
        press(&mut app, "e<C-u>Nix flakes and lock files<Enter>y");
        press(&mut app, "mni<Tab><Enter>");
        // More interests: their candidates join the list.
        press(&mut app, "aaudio<Enter>");
        app.finish_jobs();
        assert!(
            message(&app).starts_with("added 1 candidate"),
            "{}",
            message(&app)
        );
        press(&mut app, "d");
        let t = text(&mut app);
        assert!(
            t.contains("3 to keep, 1 to merge, 2 dropped, 0 undecided"),
            "{t}"
        );
        press(&mut app, "w");
        assert_eq!(
            message(&app),
            "seeded: 3 new tags on the list (wireless nix flake), 1 alias (nixpkgs → nix), \
             3 descriptions"
        );
        let s = db.store();
        let names: Vec<String> = s
            .canonical_tags()
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, ["flake", "linux", "nix", "wireless"]);
        assert_eq!(
            s.tag_info("flake").unwrap().unwrap().description.as_deref(),
            Some("Nix flakes and lock files")
        );
        assert_eq!(s.tag_info("nixpkgs").unwrap().unwrap().name, "nix");
    }
}

#[test]
fn queued_keys_run_one_search() {
    let db = Db::new();
    three(&db);
    let mut app = db.app();
    // Keys handled without settling in between, as the event loop does with queued input.
    for c in "/python".chars() {
        app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    assert_eq!(app.bookmarks.ids.len(), 3, "nothing searched yet");
    app.settle();
    assert_eq!(app.bookmarks.ids.len(), 1);
    // Enter applies typing that hasn't been searched.
    for c in " NOT tips".chars() {
        app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.bookmarks.query, "python NOT tips");
    assert!(app.bookmarks.ids.is_empty());
}

#[test]
fn rules_view_adds_removes_and_offers() {
    let db = Db::new();
    db.seed(&[
        ("https://a.bandcamp.com/", "A", &["music"]),
        ("https://b.bandcamp.com/", "B", &["music"]),
        ("https://c.bandcamp.com/", "C", &["music"]),
        ("https://d.bandcamp.com/", "D", &[]),
        ("https://github.com/x", "X", &[]),
    ]);
    let mut app = db.app_with(Config::default(), |s| {
        let rules = [("docs.rs".to_string(), vec!["rust".to_string()])];
        s.set_rules(DomainRules::new(rules.into_iter().collect()).unwrap());
    });
    press(&mut app, "4");
    let t = text(&mut app);
    assert!(t.contains("docs.rs"), "{t}");
    assert!(t.contains("#rust"), "{t}");
    assert!(t.contains("pinned"), "{t}");
    assert!(t.contains("Offered"), "{t}");
    assert!(t.contains("on 3 of 3 tagged bookmarks"), "{t}");
    // Pinned rules stay.
    press(&mut app, "d");
    assert!(
        message(&app).contains("pinned by [tags.rules]"),
        "{}",
        message(&app)
    );
    // y on the offer creates the rule and runs it on the site.
    press(&mut app, "jy");
    assert!(
        message(&app).contains("added rule bandcamp.com → #music; 1 new suggestion"),
        "{}",
        message(&app)
    );
    let s = db.store();
    assert_eq!(s.saved_rules().tags_of("bandcamp.com"), ["music"]);
    let d = s.find_by_url("https://d.bandcamp.com/").unwrap().unwrap();
    assert_eq!(s.pending(Some(d.id)).unwrap()[0].tag, "music");
    assert!(!text(&mut app).contains("Offered"));

    // a adds one with a form; tags complete.
    press(&mut app, "agithub.com/x<Tab>code git<Enter>");
    assert!(
        message(&app).contains("added rule github.com → #code #git; 2 new suggestions"),
        "{}",
        message(&app)
    );
    let t = text(&mut app);
    assert!(t.contains("github.com"), "{t}");
    // Bad input keeps the form open with the error.
    press(&mut app, "anot a host<Tab>x<Enter>");
    assert!(
        message(&app).contains("not a host name"),
        "{}",
        message(&app)
    );
    assert!(text(&mut app).contains("Add a rule"));
    press(&mut app, "<Esc>");

    // dd removes the rule under the cursor and its pending suggestions.
    press(&mut app, "g");
    let first = db.store().rule_list().unwrap()[0].clone();
    assert_eq!(
        (first.host.as_str(), first.tag.as_str()),
        ("bandcamp.com", "music")
    );
    press(&mut app, "dd");
    assert_eq!(
        message(&app),
        "removed rule bandcamp.com → #music; dropped 1 pending suggestion"
    );
    assert!(db.store().saved_rules().tags_of("bandcamp.com").is_empty());

    // Enter shows the rule's bookmarks.
    press(&mut app, "<Enter>");
    assert_eq!(app.view, View::Bookmarks);
    assert_eq!(app.bookmarks.query, "site:docs.rs #rust");
}

#[test]
fn rules_view_declines_offers() {
    let db = Db::new();
    db.seed(&[
        ("https://x.example/1", "", &["blog"]),
        ("https://x.example/2", "", &["blog"]),
        ("https://x.example/3", "", &["blog"]),
    ]);
    let mut app = db.app();
    press(&mut app, ":rules<Enter>");
    assert!(text(&mut app).contains("No domain rules yet"));
    press(&mut app, "n");
    assert_eq!(message(&app), "won't offer x.example → #blog again");
    assert!(!text(&mut app).contains("Offered"));
    assert!(db.store().rule_candidates(3, 0.8).unwrap().is_empty());
}

/// Waits for the worker, and steps through back-fill (which only the `model` feature reaches)
/// with `q`, until the batch is done.
fn finish_batch(app: &mut App) {
    for _ in 0..20 {
        app.finish_jobs();
        match app.review_phase() {
            "idle" => return,
            "backfill" => press(app, "q"),
            "offering" | "training" | "picking" => {}
            p => panic!("batch stuck in {p}"),
        }
    }
    panic!("batch didn't finish");
}

fn reviewed(db: &Db, id: i64) -> bool {
    db.store().is_reviewed(id).unwrap()
}

fn review_db(db: &Db) {
    db.seed(&[
        ("https://github.com/rust-lang/rust", "Rust compiler", &[]),
        ("https://github.com/tokio-rs/tokio", "Tokio runtime", &[]),
        ("https://example.com/wifi", "Fix home wifi", &[]),
        ("https://cooking.com/pasta", "Pasta recipe", &[]),
        ("https://cooking.com/soup", "Soup", &[]),
    ]);
    db.store()
        .add_canonical(&["code".to_string(), "rust".to_string()])
        .unwrap();
}

fn github_code(s: &mut Store) {
    let rules = [("github.com".to_string(), vec!["code".to_string()])];
    s.set_rules(DomainRules::new(rules.into_iter().collect()).unwrap());
}

#[test]
fn reviews_a_batch() {
    let db = Db::new();
    review_db(&db);
    let mut app = db.app_with(Config::default(), github_code);
    press(&mut app, "2");
    assert!(text(&mut app).contains("No batches yet"));
    // Batches of 3.
    press(&mut app, "f<Down><Down><Down><C-u>3<Enter>");
    press(&mut app, "<Enter>");
    app.finish_jobs();
    assert_eq!(app.review_phase(), "deciding");
    let t = text(&mut app);
    assert!(t.contains("spread across domains, untagged first"), "{t}");
    // 5 Soup first (one bookmark per site, newest first): a new name, proposed.
    assert!(t.contains("[1/3] 5  Soup"), "{t}");
    press(&mut app, "tcooking<Enter>");
    assert!(text(&mut app).contains("#cooking  new: proposed for the list"));
    press(&mut app, "<Enter>");
    // 2 Tokio: #code from the rule; add #rust by completion, uncheck #code.
    let t = text(&mut app);
    assert!(t.contains("[2/3] 2  Tokio runtime"), "{t}");
    assert!(t.contains("1 [x] #code  rule"), "{t}");
    press(&mut app, "tru<Tab><Enter>1");
    let t = text(&mut app);
    assert!(t.contains("1 [ ] #code  rule"), "{t}");
    assert!(t.contains("2 [x] #rust  added"), "{t}");
    press(&mut app, "y");
    // 3 wifi: nothing, so Enter asks first.
    press(&mut app, "<Enter>");
    assert!(
        message(&app).contains("no tags: Enter again"),
        "{}",
        message(&app)
    );
    press(&mut app, "<Enter>");
    // The name proposed: add it.
    assert_eq!(app.review_phase(), "proposals");
    let t = text(&mut app);
    assert!(t.contains("1 name for the list, from this batch"), "{t}");
    assert!(t.contains("#cooking  on 1 bookmark  (user)"), "{t}");
    press(&mut app, "a");
    finish_batch(&mut app);
    let t = text(&mut app);
    assert!(
        t.contains("batch  reviewed  skipped  proposed  added"),
        "{t}"
    );
    assert!(
        t.contains("    1         3        0         1      1  -"),
        "{t}"
    );

    assert_eq!(tags(&db, "https://github.com/tokio-rs/tokio"), ["rust"]);
    assert_eq!(tags(&db, "https://cooking.com/soup"), ["cooking"]);
    assert!(tags(&db, "https://example.com/wifi").is_empty() && reviewed(&db, 3));
    let s = db.store();
    assert!(
        s.proposals(shlv::ProposalState::Pending)
            .unwrap()
            .is_empty()
    );
    assert!(s.rules().is_empty(), "the worker's rules aren't saved");
    drop(s);

    // Batch 2: skip one, end the batch at the next.
    press(&mut app, "<Enter>");
    app.finish_jobs();
    let t = text(&mut app);
    assert!(t.contains("[1/2] 4  Pasta recipe"), "{t}");
    press(&mut app, "s");
    press(&mut app, "q");
    finish_batch(&mut app);
    assert!(!reviewed(&db, 4) && !reviewed(&db, 1));
    let h = db.store().review_history().unwrap();
    assert_eq!(h.len(), 1, "a batch that reviewed nothing isn't history");

    // Filters: only the pending #code suggestion on 1 matches.
    press(&mut app, "f#code<Enter>");
    assert!(message(&app).contains("filtered"), "{}", message(&app));
    press(&mut app, "<Enter>");
    app.finish_jobs();
    let t = text(&mut app);
    assert!(t.contains("least confident suggestions first"), "{t}");
    assert!(t.contains("1  Rust compiler"), "{t}");
    press(&mut app, "<Enter>");
    finish_batch(&mut app);
    assert_eq!(tags(&db, "https://github.com/rust-lang/rust"), ["code"]);
    press(&mut app, "f<C-u>nope<Enter>");
    assert!(
        message(&app).contains("no tag \"nope\""),
        "{}",
        message(&app)
    );
    press(&mut app, "<Esc>");
    // Every bookmark reviewed but 4.
    press(&mut app, "f<C-u><Enter><Enter>");
    app.finish_jobs();
    press(&mut app, "<Enter><Enter>");
    finish_batch(&mut app);
    assert!(
        message(&app).starts_with("batch done: reviewed 1 bookmark;"),
        "{}",
        message(&app)
    );
    press(&mut app, "<Enter>");
    app.finish_jobs();
    assert_eq!(message(&app), "every bookmark is reviewed");
}

#[test]
fn review_aliases_and_rejects_names() {
    let db = Db::new();
    db.seed(&[
        ("https://a.com/1", "One", &[]),
        ("https://b.com/2", "Two", &[]),
    ]);
    db.store()
        .add_canonical(&["programming".to_string()])
        .unwrap();
    let mut app = db.app();
    press(&mut app, "2<Enter>");
    app.finish_jobs();
    press(&mut app, "tcoding<Enter><Enter>tjunk<Enter><Enter>");
    assert_eq!(app.review_phase(), "proposals");
    let t = text(&mut app);
    assert!(t.contains("2 names for the list"), "{t}");
    // The newest bookmark came first, so junk is listed... by name: coding, junk.
    press(&mut app, "lprog<Tab><Enter>");
    assert!(
        text(&mut app).contains("alias of #programming, tagged 1 bookmark"),
        "{}",
        text(&mut app)
    );
    press(&mut app, "r");
    finish_batch(&mut app);
    assert_eq!(tags(&db, "https://a.com/1"), ["programming"]);
    assert!(tags(&db, "https://b.com/2").is_empty());
    let s = db.store();
    assert_eq!(
        s.proposals(shlv::ProposalState::Rejected).unwrap()[0].name,
        "junk"
    );
}

#[test]
fn review_offers_a_rule_and_deletes() {
    let db = Db::new();
    db.seed(&[
        ("https://a.bandcamp.com/", "A", &[]),
        ("https://b.bandcamp.com/", "B", &[]),
        ("https://c.bandcamp.com/", "C", &[]),
        ("https://d.bandcamp.com/", "D", &[]),
        ("https://spam.example/", "Spam", &[]),
    ]);
    db.store().add_canonical(&["music".to_string()]).unwrap();
    let mut app = db.app();
    press(&mut app, "2<Enter>");
    app.finish_jobs();
    // Tag the bandcamp bookmarks #music until a rule is offered; delete the spam.
    let (mut offered, mut deleted) = (false, false);
    while app.review_phase() == "deciding" {
        if text(&mut app).contains("Spam") {
            press(&mut app, "dd");
            assert!(db.store().get(5).unwrap().is_none());
            deleted = true;
        } else if offered {
            press(&mut app, "<Enter>");
        } else {
            press(&mut app, "tmusic<Enter><Enter>");
            if message(&app).contains("R makes it a rule") {
                offered = true;
                assert!(text(&mut app).contains("Rule? bandcamp.com → #music"));
                press(&mut app, "R");
                assert!(
                    message(&app).starts_with("added rule bandcamp.com → #music"),
                    "{}",
                    message(&app)
                );
            }
        }
    }
    assert!(offered && deleted);
    finish_batch(&mut app);
    let s = db.store();
    assert_eq!(s.saved_rules().tags_of("bandcamp.com"), ["music"]);
    // The rule's suggestion reached the bookmark under review when R was pressed, and Enter
    // accepted it.
    for u in ["a", "b", "c", "d"] {
        assert_eq!(
            tags(&db, &format!("https://{u}.bandcamp.com/")),
            ["music"],
            "{u}"
        );
    }
    assert!(s.get(5).unwrap().is_none());
    drop(s);
    // The deleted bookmark comes back from Bookmarks.
    press(&mut app, "1u");
    assert!(db.store().get(5).unwrap().is_some());
}

#[test]
fn review_queue_filters_and_decides() {
    let db = Db::new();
    db.seed(&[
        ("https://github.com/a", "A", &[]),
        ("https://github.com/b", "B", &[]),
        ("https://docs.rs/c", "C", &[]),
    ]);
    let mut app = db.app_with(Config::default(), |s| {
        let rules = [
            ("github.com".to_string(), vec!["code".to_string()]),
            (
                "docs.rs".to_string(),
                vec!["rust".to_string(), "docs".to_string()],
            ),
        ];
        s.set_rules(DomainRules::new(rules.into_iter().collect()).unwrap());
    });
    {
        let mut s = db.store();
        github_code(&mut s);
        let rules = [
            ("github.com".to_string(), vec!["code".to_string()]),
            (
                "docs.rs".to_string(),
                vec!["rust".to_string(), "docs".to_string()],
            ),
        ];
        s.set_rules(DomainRules::new(rules.into_iter().collect()).unwrap());
        s.apply_rules(None).unwrap();
    }
    press(&mut app, "2<Tab>");
    let t = text(&mut app);
    assert!(t.contains("queue: 4 pending"), "{t}");
    assert!(t.contains("#docs  rule"), "{t}");
    // The first is docs.rs's #docs (newest bookmark first, then by tag): accept it, then reject
    // the bookmark's other.
    press(&mut app, "y");
    assert_eq!(message(&app), "3: accepted #docs");
    press(&mut app, "N");
    assert_eq!(message(&app), "3: rejected #rust");
    assert_eq!(tags(&db, "https://docs.rs/c"), ["docs"]);
    assert!(text(&mut app).contains("queue: 2 pending"));
    // Filters: no model suggestions.
    press(&mut app, "ff");
    let t = text(&mut app);
    assert!(t.contains("queue: 0 pending, model"), "{t}");
    assert!(t.contains("No pending suggestions match"), "{t}");
    press(&mut app, "ff<");
    assert!(text(&mut app).contains("below 0.9"));
    // Rules are sure (1.0), so none is below.
    assert!(text(&mut app).contains("queue: 0 pending"));
    press(&mut app, ">");
    assert!(text(&mut app).contains("queue: 2 pending"));
    press(&mut app, "<Tab>");
    assert!(text(&mut app).contains("Enter starts one"));
}

/// A tag added after a batch is offered on the bookmarks reviewed before it existed; the
/// offering runs on the worker.
#[cfg(feature = "model")]
#[test]
fn review_backfills_new_tags() {
    let db = Db::new();
    db.seed(&[
        ("https://a.com/1", "Music theory for programmers", &[]),
        ("https://b.com/2", "Jazz albums", &[]),
    ]);
    db.store().add_canonical(&["code".to_string()]).unwrap();
    let mut app = db.app();
    press(&mut app, "2<Enter>");
    app.finish_jobs();
    // Theory: no tags; jazz: propose #music, whichever comes first.
    for _ in 0..2 {
        if text(&mut app).contains("Jazz albums") {
            press(&mut app, "tmusic<Enter><Enter>");
        } else {
            press(&mut app, "<Enter><Enter>");
        }
    }
    press(&mut app, "a");
    app.finish_jobs();
    assert_eq!(app.review_phase(), "backfill");
    let t = text(&mut app);
    assert!(
        t.contains("Back-fill: #music on which of 1 reviewed bookmarks?"),
        "{t}"
    );
    assert!(
        t.contains("Music theory for programmers  (model 0.45, reviewed)"),
        "{t}"
    );
    press(&mut app, "y");
    app.finish_jobs();
    assert_eq!(app.review_phase(), "idle");
    let m = message(&app);
    assert!(
        m.contains("#music: 1 accepted, 0 rejected, 0 still pending"),
        "{m}"
    );
    assert!(m.contains("classifier retrained on"), "{m}");
    assert_eq!(tags(&db, "https://a.com/1"), ["music"]);
    assert_eq!(tags(&db, "https://b.com/2"), ["music"]);
}

fn pin_rust(s: &mut Store) {
    let map: std::collections::BTreeMap<String, shlv::DeclaredEntry> = [(
        "rust".to_string(),
        shlv::DeclaredEntry {
            aliases: vec!["rs".to_string()],
            description: None,
        },
    )]
    .into_iter()
    .collect();
    s.pin(shlv::DeclaredTags::new(map).unwrap()).unwrap();
}

#[test]
fn tags_list_edits() {
    let db = Db::new();
    three(&db);
    db.store().describe_tag("python", "the language").unwrap();
    let mut app = db.app_with(Config::default(), pin_rust);
    press(&mut app, "3");
    let t = text(&mut app);
    assert!(t.contains(" * #rust    2  pinned  = rs"), "{t}");
    assert!(t.contains("   #python  1  # the language"), "{t}");
    // Filter as you type, by name or alias.
    press(&mut app, "/w");
    assert!(text(&mut app).contains("1 of 3 tags  /w"));
    press(&mut app, "<Esc>/rs<Enter>");
    let t = text(&mut app);
    assert!(t.contains("#rust") && !t.contains("#web"), "{t}");
    // Pinned tags refuse to be renamed; the form stays open with the reason.
    press(&mut app, "r<C-u>rustlang<Enter>");
    assert!(
        message(&app).contains("cannot rename pinned tag rust"),
        "{}",
        message(&app)
    );
    press(&mut app, "<Esc><Esc>");
    // web: rename, alias, describe, canonical, then merge into rust.
    press(&mut app, "/web<Enter>r<C-u>www<Enter>");
    assert_eq!(message(&app), "renamed #web to #www (1 bookmarks)");
    press(&mut app, "<Esc>/www<Enter>");
    press(&mut app, "aw3<Enter>");
    assert_eq!(message(&app), "w3 is an alias of #www");
    press(&mut app, "eweb pages<Enter>");
    assert_eq!(message(&app), "described #www");
    press(&mut app, "c");
    assert_eq!(message(&app), "#www is canonical");
    assert!(text(&mut app).contains(" * #www  1  = w3  # web pages"));
    press(&mut app, "A<Enter>");
    assert_eq!(message(&app), "removed alias w3 (of #www)");
    press(&mut app, "mru<Tab><Enter>");
    assert!(
        message(&app).starts_with("merged #www into #rust (1 bookmarks)"),
        "{}",
        message(&app)
    );
    let s = db.store();
    assert!(s.tag_info("www").unwrap().unwrap().name == "rust");
    drop(s);
    // Enter shows the tag's bookmarks.
    press(&mut app, "<Esc>s");
    assert_eq!(message(&app), "sorted by count");
    press(&mut app, "g<Enter>");
    assert_eq!(app.view, View::Bookmarks);
    assert_eq!(app.bookmarks.query, "#rust");
    assert_eq!(app.bookmarks.ids.len(), 2);
}

#[test]
fn tags_proposals() {
    let db = Db::new();
    three(&db);
    {
        let mut s = db.store();
        s.add_canonical(&["rust".to_string(), "python".to_string()])
            .unwrap();
        s.set_strict_tags(shlv::StrictTags::Propose);
        for (url, tag) in [
            ("https://d.example/", "golang"),
            ("https://e.example/", "py"),
            ("https://f.example/", "junk"),
        ] {
            s.add(&NewBookmark {
                url: url.into(),
                tags: vec![tag.into()],
                ..Default::default()
            })
            .unwrap();
        }
    }
    let mut app = db.app();
    press(&mut app, "3<Tab>");
    let t = text(&mut app);
    assert!(t.contains("3 names proposed"), "{t}");
    assert!(t.contains("#golang  on 1 bookmark  (user)"), "{t}");
    // By name: golang, junk, py.
    press(&mut app, "a");
    assert_eq!(
        message(&app),
        "added #golang to the list; tagged 1 bookmark"
    );
    assert!(text(&mut app).contains("2 names proposed"));
    press(&mut app, "r");
    assert_eq!(message(&app), "rejected junk: it won't be proposed again");
    press(&mut app, "lpyt<Tab><Enter>");
    assert_eq!(
        message(&app),
        "py is an alias of #python; tagged 1 bookmark"
    );
    assert!(text(&mut app).contains("No names proposed"));
    assert_eq!(tags(&db, "https://e.example/"), ["python"]);
}

#[test]
fn tags_health_fixes() {
    let db = Db::new();
    let mut items: Vec<(String, &[&str])> = Vec::new();
    for i in 0..3 {
        items.push((
            format!("https://m{i}.example/"),
            &["ml", "machine-learning"],
        ));
    }
    for i in 0..3 {
        items.push((format!("https://blog.site/{i}"), &[]));
    }
    let items: Vec<(&str, &str, &[&str])> =
        items.iter().map(|(u, t)| (u.as_str(), "", *t)).collect();
    db.seed(&items);
    let mut app = db.app();
    press(&mut app, "3<Tab><Tab>");
    app.finish_jobs();
    let t = text(&mut app);
    assert!(t.contains("Redundant"), "{t}");
    assert!(
        t.contains("#ml + #machine-learning  together on 3 of 3 and 3"),
        "{t}"
    );
    assert!(t.contains("m merge the first into the second"), "{t}");
    assert!(t.contains("Uncovered bookmarks, by site"), "{t}");
    // Merge the pair; health measures again.
    press(&mut app, "m");
    assert!(
        message(&app).starts_with("merged #ml into #machine-learning"),
        "{}",
        message(&app)
    );
    app.finish_jobs();
    let t = text(&mut app);
    assert!(!t.contains("Redundant"), "{t}");
    // Tag the uncovered blog.site bookmarks.
    let mut found = false;
    for _ in 0..10 {
        if text(&mut app).contains("t tag them") {
            found = true;
            break;
        }
        press(&mut app, "j");
    }
    assert!(found, "{}", text(&mut app));
    press(&mut app, "t<C-u>blog<Enter>");
    assert!(
        message(&app).starts_with("tagged 3 bookmarks: #blog on 3"),
        "{}",
        message(&app)
    );
    assert_eq!(tags(&db, "https://blog.site/1"), ["blog"]);
}

#[cfg(feature = "model")]
#[test]
fn tags_backfill() {
    let db = Db::new();
    db.seed(&[
        ("https://a.com/1", "Jazz albums", &["music"]),
        ("https://b.com/2", "Music theory for programmers", &[]),
        ("https://c.com/3", "Gardening", &[]),
    ]);
    db.store().add_canonical(&["music".to_string()]).unwrap();
    let mut app = db.app();
    press(&mut app, "3/music<Enter>b");
    app.finish_jobs();
    let t = text(&mut app);
    assert!(t.contains("Back-fill: #music on which of"), "{t}");
    assert!(t.contains("Music theory for programmers"), "{t}");
    press(&mut app, "y");
    // Whatever else is offered: stop.
    if !message(&app).starts_with("#music:") {
        press(&mut app, "q");
    }
    assert!(
        message(&app).starts_with("#music: 1 accepted"),
        "{}",
        message(&app)
    );
    assert_eq!(tags(&db, "https://b.com/2"), ["music"]);
}

#[test]
fn status_view_and_commands() {
    let db = Db::new();
    three(&db);
    let mut app = db.app();
    press(&mut app, "6");
    let t = text(&mut app);
    assert!(t.contains("config      (none)"), "{t}");
    assert!(t.contains(&db.path().display().to_string()), "{t}");
    assert!(
        t.contains("Classifier") && t.contains("none running"),
        "{t}"
    );
    press(&mut app, "x");
    assert_eq!(message(&app), "no jobs");

    // Tab completes command names, subcommands, then tags.
    press(&mut app, ":ta<Tab>");
    assert!(text(&mut app).contains(":tag"), "{}", text(&mut app));
    press(&mut app, " ren<Tab> we<Tab> www<Enter>");
    assert_eq!(message(&app), "renamed #web to #www (1 bookmarks)");
    press(&mut app, ":tag merge www rust<Enter>");
    assert!(
        message(&app).starts_with("merged #www into #rust"),
        "{}",
        message(&app)
    );
    press(&mut app, ":tag canonical rust<Enter>");
    assert_eq!(message(&app), "marked 1 tag canonical");
    press(&mut app, ":tag describe rust the language<Enter>");
    assert_eq!(message(&app), "described #rust");
    assert_eq!(
        db.store()
            .tag_info("rust")
            .unwrap()
            .unwrap()
            .description
            .as_deref(),
        Some("the language")
    );
    press(&mut app, ":tag frob<Enter>");
    assert!(
        message(&app).starts_with("tag rename A B"),
        "{}",
        message(&app)
    );
    press(&mut app, ":rule add a.example rust<Enter>");
    assert!(
        message(&app).starts_with("added rule a.example → #rust"),
        "{}",
        message(&app)
    );
    press(&mut app, ":rule rm a.example<Enter>");
    assert_eq!(message(&app), "removed rule a.example → #rust");
    press(&mut app, ":interest add nix home-lab<Enter>");
    assert_eq!(message(&app), "saved 2 new interests");
    press(&mut app, ":interest rm h<Tab><Enter>");
    assert_eq!(message(&app), "forgot 1 interest");
    press(&mut app, ":search #python<Enter>");
    assert_eq!(app.view, View::Bookmarks);
    assert_eq!(app.bookmarks.ids.len(), 1);
    press(&mut app, ":frob<Enter>");
    assert!(message(&app).contains("unknown command"));
    press(&mut app, ":jobs<Enter>");
    assert_eq!(app.view, View::Status);
    press(&mut app, "?");
    assert!(text(&mut app).contains("Commands (:name)"));
}

#[test]
fn cancels_a_job_before_it_starts() {
    let db = Db::new();
    three(&db);
    let path = db.path();
    // The worker takes a moment to open its store, so the job is still queued when cancelled.
    let worker = Box::new(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        Ok((Store::open(&path)?, Config::default()))
    });
    let mut app = App::new(db.store(), Config::default(), worker).unwrap();
    press(&mut app, "6");
    app.send_job(super::worker::Job::Health);
    assert!(text(&mut app).contains("measuring the tag list"));
    press(&mut app, "x");
    assert!(text(&mut app).contains("cancelling: measuring the tag list"));
    app.finish_jobs();
    assert!(message(&app).contains("cancelled"), "{}", message(&app));
    assert!(text(&mut app).contains("none running"));
}

#[test]
fn bootstrap_interests() {
    let db = Db::new();
    let mut app = db.app_with(Config::default(), |s| {
        s.set_pinned_interests(&["audio".to_string()]);
    });
    press(&mut app, "5");
    let t = text(&mut app);
    assert!(t.contains("audio  pinned in config"), "{t}");
    press(&mut app, "anix, Home-Lab<Enter>");
    assert_eq!(
        message(&app),
        "saved 2 new interests (s expands them into tags)"
    );
    let t = text(&mut app);
    assert!(t.contains("home-lab") && t.contains("nix"), "{t}");
    // By name: audio, home-lab, nix. The pinned one stays.
    press(&mut app, "dd");
    assert!(
        message(&app).contains("pinned by [tags] interests"),
        "{}",
        message(&app)
    );
    press(&mut app, "jdd");
    assert_eq!(message(&app), "forgot interest home-lab");
    let names: Vec<String> = db
        .store()
        .interests()
        .unwrap()
        .into_iter()
        .map(|i| i.name)
        .collect();
    assert_eq!(
        names,
        ["nix"],
        "pinned interests come from config, not the database"
    );
}
