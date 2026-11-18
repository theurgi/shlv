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
    assert!(text(&mut app).contains("task 045"));
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
