//! Fetching against a local test server. Nothing here touches the internet.

mod common;

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use common::server::{Response, Server};
use shlv::fetch::{FetchError, Fetcher, Limits, title};

/// Fixture pages, by path.
fn pages() -> Server {
    Server::start(|r| match r.path.as_str() {
        "/title" => Response::html("<html><head><title>A  Page\n</title></head></html>"),
        "/entities" => Response::html(
            "<title>Tom &amp; Jerry &mdash; &#8220;Caf&eacute;&#8221; &lt;3 &#x1F600;</title>",
        ),
        "/og" => Response::html(
            "<head><meta property=\"og:title\" content=\"From &quot;OG&quot;\"></head>",
        ),
        "/untitled" => Response::html("<p>nothing</p>"),
        "/xhtml" => Response::new(200)
            .header("content-type", "application/xhtml+xml")
            .body("<html><title>XHTML</title></html>"),
        "/r1" => Response::redirect(302, "/r2"),
        "/r2" => Response::redirect(301, "/title"),
        "/loop" => Response::redirect(302, "/loop"),
        "/slow" => Response::html("<title>late</title>").delay(Duration::from_secs(3)),
        "/trickle" => Response::html(format!("<title>t</title>{}", "x".repeat(2000)))
            .chunked(100, Duration::from_millis(200)),
        "/big" => {
            let mut body = b"<title>Big</title>".to_vec();
            body.resize(8 << 20, b'x');
            Response::html(body)
        }
        "/json" => Response::json(&serde_json::json!({"title": "no"})),
        "/pdf" => Response::new(200)
            .header("content-type", "application/pdf")
            .body("%PDF-1.4"),
        "/bare" => Response::new(200).body("<!DOCTYPE html><title>Sniffed</title>"),
        "/latin1" => Response::new(200)
            .header("content-type", "text/html; charset=ISO-8859-1")
            .body(b"<title>Caf\xe9 \x96 cr\xe8me</title>".to_vec()),
        "/latin1-meta" => Response::new(200)
            .header("content-type", "text/html")
            .body(b"<meta charset=windows-1252><title>\x93Quoted\x94</title>".to_vec()),
        "/undeclared" => Response::new(200)
            .header("content-type", "text/html")
            .body(b"<title>na\xefve</title>".to_vec()),
        _ => Response::new(404).body("not found"),
    })
}

fn fetch(server: &Server, path: &str) -> Result<shlv::fetch::Page, FetchError> {
    Fetcher::default().get(&server.url(path))
}

fn fetch_title(server: &Server, path: &str) -> Option<String> {
    title(&fetch(server, path).unwrap().html)
}

#[test]
fn titles_entities_and_og() {
    let s = pages();
    assert_eq!(fetch_title(&s, "/title").as_deref(), Some("A Page"));
    assert_eq!(
        fetch_title(&s, "/entities").as_deref(),
        Some("Tom & Jerry — “Café” <3 😀")
    );
    assert_eq!(fetch_title(&s, "/og").as_deref(), Some("From \"OG\""));
    assert_eq!(fetch_title(&s, "/untitled"), None);
    assert_eq!(fetch_title(&s, "/xhtml").as_deref(), Some("XHTML"));
    assert_eq!(fetch_title(&s, "/bare").as_deref(), Some("Sniffed"));
}

#[test]
fn sends_user_agent() {
    let s = pages();
    fetch(&s, "/title").unwrap();
    let ua = s.requests()[0].header("user-agent").unwrap().to_string();
    assert_eq!(ua, shlv::fetch::USER_AGENT);
    assert!(ua.starts_with(&format!("shlv/{}", env!("CARGO_PKG_VERSION"))));
}

#[test]
fn follows_redirects_up_to_a_limit() {
    let s = pages();
    let page = fetch(&s, "/r1").unwrap();
    assert_eq!(page.url, s.url("/title"));
    assert_eq!(title(&page.html).as_deref(), Some("A Page"));
    assert!(matches!(
        fetch(&s, "/loop"),
        Err(FetchError::TooManyRedirects)
    ));
    let one = Fetcher::new(Limits {
        redirects: 1,
        ..Limits::default()
    });
    assert!(matches!(
        one.get(&s.url("/r1")),
        Err(FetchError::TooManyRedirects)
    ));
}

#[test]
fn times_out() {
    let s = pages();
    let quick = Fetcher::new(Limits {
        total: Duration::from_millis(300),
        ..Limits::default()
    });
    for path in ["/slow", "/trickle"] {
        let t = Instant::now();
        let r = quick.get(&s.url(path));
        assert!(matches!(r, Err(FetchError::Timeout)), "{path}: {r:?}");
        assert!(t.elapsed() < Duration::from_secs(2), "{path}");
    }
}

#[test]
fn caps_the_body() {
    let s = pages();
    let page = fetch(&s, "/big").unwrap();
    assert!(page.truncated);
    assert_eq!(page.html.len() as u64, Limits::default().max_bytes);
    assert_eq!(title(&page.html).as_deref(), Some("Big"));
    assert!(!fetch(&s, "/title").unwrap().truncated);
}

#[test]
fn rejects_errors_and_non_html() {
    let s = pages();
    assert!(matches!(fetch(&s, "/nope"), Err(FetchError::Status(404))));
    match fetch(&s, "/json") {
        Err(FetchError::NotHtml(t)) => assert_eq!(t, "application/json"),
        r => panic!("{r:?}"),
    }
    assert!(matches!(fetch(&s, "/pdf"), Err(FetchError::NotHtml(_))));
    assert!(matches!(
        Fetcher::default().get("ftp://example.com/"),
        Err(FetchError::Scheme(_))
    ));
    // Nothing listens on port 1.
    assert!(matches!(
        Fetcher::default().get("http://127.0.0.1:1/"),
        Err(FetchError::Network(_))
    ));
}

#[test]
fn non_utf8_pages() {
    let s = pages();
    assert_eq!(fetch_title(&s, "/latin1").as_deref(), Some("Café – crème"));
    assert_eq!(fetch_title(&s, "/latin1-meta").as_deref(), Some("“Quoted”"));
    assert_eq!(fetch_title(&s, "/undeclared").as_deref(), Some("naïve"));
}

/// A temporary database for CLI runs, removed on drop.
struct Db(PathBuf);

impl Db {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "shlv-fetch-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Db(dir)
    }

    /// Runs shlv; returns (exit code, stdout, stderr).
    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let o = Command::new(env!("CARGO_BIN_EXE_shlv"))
            .args(args)
            .env("SHLV_DB", self.0.join("shlv.db"))
            .env("SHLV_CONFIG", self.0.join("config.toml"))
            .output()
            .unwrap();
        (
            o.status.code().unwrap_or(-1),
            String::from_utf8(o.stdout).unwrap(),
            String::from_utf8(o.stderr).unwrap(),
        )
    }

    fn ok(&self, args: &[&str]) -> String {
        let (code, out, err) = self.run(args);
        assert_eq!(code, 0, "shlv {args:?} failed: {err}");
        out
    }

    fn json(&self, id: &str) -> serde_json::Value {
        serde_json::from_str(&self.ok(&["show", id, "--json"])).unwrap()
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn add_fetch_sets_the_title() {
    let s = pages();
    let db = Db::new();
    let out = db.ok(&["add", &s.url("/entities"), "--fetch"]);
    assert!(out.contains("Tom & Jerry"), "{out}");
    // --title wins, and nothing is requested for it.
    let n = s.requests().len();
    db.ok(&["add", &s.url("/title"), "--fetch", "--title", "Mine"]);
    assert_eq!(db.json("2")["title"], "Mine");
    assert_eq!(s.requests().len(), n);
}

#[test]
fn add_fetch_failure_still_adds() {
    let s = pages();
    let db = Db::new();
    for (path, why) in [
        ("/nope", "HTTP 404"),
        ("/pdf", "not an HTML page (application/pdf)"),
    ] {
        let (code, out, err) = db.run(&["add", &s.url(path), "--fetch", "-t", "x"]);
        assert_eq!(code, 0, "{err}");
        assert!(out.starts_with("added "), "{out}");
        assert!(
            err.contains("warning: cannot fetch") && err.contains(why),
            "{err}"
        );
    }
    let (code, _, err) = db.run(&["add", "http://127.0.0.1:1/", "--fetch"]);
    assert_eq!(code, 0);
    assert!(err.contains("cannot fetch"), "{err}");
    let (code, _, err) = db.run(&["add", &s.url("/untitled"), "--fetch"]);
    assert_eq!(code, 0);
    assert!(err.contains("has no title"), "{err}");
    assert_eq!(db.ok(&["ls"]).lines().count(), 4);
}

#[test]
fn fetch_backfills_titles() {
    let s = pages();
    let db = Db::new();
    db.ok(&["add", &s.url("/title")]);
    db.ok(&["add", &s.url("/og"), "--title", "Kept"]);
    db.ok(&["add", &s.url("/latin1")]);
    db.ok(&["add", &s.url("/nope")]);
    db.ok(&["add", &s.url("/untitled")]);
    let (code, out, err) = db.run(&["fetch", "--all", "-j", "2"]);
    assert_eq!(code, 1, "a failure makes the run fail");
    assert_eq!(out.lines().count(), 2, "{out}");
    assert!(err.contains("5: ") && err.contains("has no title"), "{err}");
    assert!(
        err.contains("4: cannot fetch") && err.contains("HTTP 404"),
        "{err}"
    );
    assert!(
        err.contains("fetched 2 titles of 4 bookmarks: 1 without a title, 1 failed"),
        "{err}"
    );
    assert_eq!(db.json("1")["title"], "A Page");
    assert_eq!(db.json("2")["title"], "Kept");
    assert_eq!(db.json("3")["title"], "Café – crème");
    // An explicit id refetches even with a title; --force does for --all.
    db.ok(&["fetch", "2"]);
    assert_eq!(db.json("2")["title"], "From \"OG\"");
    let (_, _, err) = db.run(&["fetch", "--all", "--force"]);
    assert!(err.contains("of 5 bookmarks"), "{err}");
    let (code, _, err) = db.run(&["fetch", "9"]);
    assert_eq!(code, 1);
    assert!(err.contains("no bookmark with id 9"), "{err}");
}
