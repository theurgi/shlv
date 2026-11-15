use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

/// A temporary database, removed on drop.
struct Db(PathBuf);

impl Db {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "shlv-cli-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Db(dir)
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_shlv"));
        c.args(args)
            .env("SHLV_DB", self.0.join("shlv.db"))
            .env_remove("BROWSER")
            .env_remove("NO_COLOR");
        c
    }

    /// Runs shlv; returns (success, stdout, stderr).
    fn run(&self, args: &[&str]) -> (bool, String, String) {
        output(self.cmd(args))
    }

    /// Runs shlv, asserting success; returns stdout.
    fn ok(&self, args: &[&str]) -> String {
        let (ok, out, err) = self.run(args);
        assert!(ok, "shlv {args:?} failed: {err}");
        out
    }

    /// Runs shlv, asserting failure; returns stderr.
    fn fail(&self, args: &[&str]) -> String {
        let (ok, out, err) = self.run(args);
        assert!(!ok, "shlv {args:?} should fail; stdout: {out}");
        err
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn output(mut c: Command) -> (bool, String, String) {
    let o = c.output().unwrap();
    (
        o.status.success(),
        String::from_utf8(o.stdout).unwrap(),
        String::from_utf8(o.stderr).unwrap(),
    )
}

fn json_lines(s: &str) -> Vec<serde_json::Value> {
    s.lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn add_ls_show() {
    let db = Db::new();
    let out = db.ok(&[
        "add",
        "example.com",
        "-t",
        "Rust",
        "--tag",
        "web",
        "--title",
        "Example",
        "--note",
        "a note",
    ]);
    assert_eq!(out, "added 1  Example  https://example.com  #rust  #web\n");
    db.ok(&["add", "https://b.org/x"]);
    assert_eq!(
        db.ok(&["ls"]),
        "2  https://b.org/x\n1  Example  https://example.com  #rust  #web\n"
    );
    assert_eq!(db.ok(&["ls", "-n", "1"]).lines().count(), 1);
    assert_eq!(db.ok(&["ls", "-t", "rust"]).lines().count(), 1);

    let show = db.ok(&["show", "1"]);
    assert!(show.contains("title:    Example"), "{show}");
    assert!(show.contains("tags:     rust web"), "{show}");
    assert!(show.contains("\na note\n"), "{show}");
    let j = json_lines(&db.ok(&["show", "1", "--json"]));
    assert_eq!(j[0]["note"], "a note");
}

#[test]
fn add_duplicate_updates() {
    let db = Db::new();
    db.ok(&["add", "https://a.com/?utm_source=x", "-t", "one"]);
    let out = db.ok(&["add", "HTTPS://A.COM/#top", "-t", "two"]);
    assert!(out.starts_with("updated 1 "), "{out}");
    assert!(out.contains("#one  #two"), "{out}");
}

#[test]
fn json_output_is_one_object_per_line() {
    let db = Db::new();
    db.ok(&["add", "https://a.com", "--title", "Alpha", "-t", "x"]);
    db.ok(&["add", "https://b.com", "--title", "Beta"]);
    let ls = json_lines(&db.ok(&["ls", "--json"]));
    assert_eq!(ls.len(), 2);
    assert_eq!(ls[1]["title"], "Alpha");
    assert_eq!(ls[1]["tags"], serde_json::json!(["x"]));
    assert!(ls[1]["created_at"].is_i64());
    let s = json_lines(&db.ok(&["search", "alp", "--json"]));
    assert_eq!(s.len(), 1);
    assert_eq!(s[0]["url"], "https://a.com");
}

#[test]
fn search() {
    let db = Db::new();
    db.ok(&[
        "add",
        "https://docs.rs/a",
        "--title",
        "Async Rust",
        "-t",
        "rust",
    ]);
    db.ok(&["add", "https://b.com", "--title", "Async JS"]);
    // Words may come as separate arguments.
    let out = db.ok(&["search", "async", "#rust"]);
    assert_eq!(out.lines().count(), 1);
    assert!(out.contains("Async Rust"));
    assert_eq!(db.ok(&["search", "site:b.com"]).lines().count(), 1);
    assert_eq!(db.ok(&["search", "nothing"]), "");
    let err = db.fail(&["search", "AND"]);
    assert!(err.contains("invalid search query"), "{err}");
}

#[test]
fn edit_tags() {
    let db = Db::new();
    db.ok(&["add", "https://a.com", "-t", "x", "-t", "y"]);
    let out = db.ok(&[
        "edit", "1", "-t", "+z", "-t", "-x", "-t", "w", "--title", "New",
    ]);
    assert_eq!(out, "updated 1  New  https://a.com  #w  #y  #z\n");
    db.ok(&["edit", "1", "--title", ""]);
    assert_eq!(db.ok(&["ls"]), "1  https://a.com  #w  #y  #z\n");
    assert!(db.fail(&["edit", "1"]).contains("nothing to change"));
    assert!(
        db.fail(&["edit", "9", "-t", "a"])
            .contains("no bookmark with id 9")
    );
}

#[test]
fn rm_by_id_and_url() {
    let db = Db::new();
    db.ok(&["add", "https://a.com"]);
    db.ok(&["add", "https://b.com"]);
    db.ok(&["add", "https://c.com"]);
    assert_eq!(db.ok(&["rm", "1", "B.com/"]), "deleted 1\ndeleted 2\n");
    assert_eq!(db.ok(&["ls"]), "3  https://c.com\n");
    // Partial failure: the rest still happen, exit code is non-zero.
    let (ok, out, err) = db.run(&["rm", "1", "3"]);
    assert!(!ok);
    assert_eq!(out, "deleted 3\n");
    assert!(err.contains("no bookmark 1"), "{err}");
}

#[test]
fn open_uses_browser() {
    let db = Db::new();
    db.ok(&["add", "https://a.com/x"]);
    let mut c = db.cmd(&["open", "1"]);
    c.env("BROWSER", "echo");
    let (ok, out, _) = output(c);
    assert!(ok);
    assert_eq!(out, "https://a.com/x\n");

    let mut c = db.cmd(&["open", "1"]);
    c.env("BROWSER", "echo [%s]:other");
    assert_eq!(output(c).1, "[https://a.com/x]\n");

    let mut c = db.cmd(&["open", "1"]);
    c.env("BROWSER", "false");
    let (ok, _, err) = output(c);
    assert!(!ok);
    assert!(err.contains("false failed"), "{err}");

    let mut c = db.cmd(&["open", "1"]);
    c.env("BROWSER", "/nonexistent/browser");
    assert!(!output(c).0);
    assert!(db.fail(&["open", "2"]).contains("no bookmark with id 2"));
}

#[test]
fn errors_and_usage() {
    let db = Db::new();
    assert!(
        db.fail(&["add", "https://a.com", "--fetch"])
            .contains("--fetch")
    );
    assert!(
        db.fail(&["add", "https://a.com", "-t", " "])
            .contains("empty tag")
    );
    assert!(db.fail(&["show", "abc"]).contains("invalid value"));
    let (ok, _, _) = db.run(&["bogus"]);
    assert!(!ok);
    // The database path is validated.
    let mut c = Command::new(env!("CARGO_BIN_EXE_shlv"));
    c.args(["ls"]).env("SHLV_DB", "/dev/null/x/shlv.db");
    let (ok, _, err) = output(c);
    assert!(!ok);
    assert!(err.starts_with("shlv: "), "{err}");
}

#[test]
fn no_color_when_piped() {
    let db = Db::new();
    db.ok(&["add", "https://a.com", "-t", "x"]);
    assert!(!db.ok(&["ls"]).contains('\x1b'));
}

#[test]
fn ids_align_to_widest_id_in_database() {
    let db = Db::new();
    for i in 1..=10 {
        let tag = if i == 9 { "nine" } else { "other" };
        db.ok(&["add", &format!("https://{i}.com"), "-t", tag]);
    }
    assert_eq!(db.ok(&["ls", "-t", "nine"]), " 9  https://9.com  #nine\n");
    assert_eq!(db.ok(&["search", "#nine"]), " 9  https://9.com  #nine\n");
    assert_eq!(
        db.ok(&["ls", "-n", "2"]).lines().nth(1),
        Some(" 9  https://9.com  #nine")
    );
}

#[test]
fn tag_ls() {
    let db = Db::new();
    for i in 0..10 {
        db.ok(&["add", &format!("https://{i}.com"), "-t", "rust"]);
    }
    db.ok(&["add", "https://x.com", "-t", "web"]);
    db.ok(&["tag", "canonical", "add", "web"]);
    db.ok(&["tag", "alias", "rs", "rust"]);
    db.ok(&["tag", "alias", "rust-lang", "rust"]);
    assert_eq!(
        db.ok(&["tag", "ls"]),
        "10   rust  = rs rust-lang\n 1 * web\n"
    );
    db.ok(&["tag", "canonical", "add", "zig"]);
    let by_count = db.ok(&["tag", "ls", "--sort", "count"]);
    let names: Vec<_> = by_count
        .lines()
        .map(|l| l.split_whitespace().last().unwrap())
        .collect();
    assert_eq!(names, ["rust-lang", "web", "zig"], "{by_count}");
    let v = json_lines(&db.ok(&["tag", "ls", "--json"]));
    assert_eq!(v.len(), 3);
    assert_eq!(v[0]["name"], "rust");
    assert_eq!(v[0]["count"], 10);
    assert_eq!(v[0]["canonical"], false);
    assert_eq!(v[0]["aliases"], serde_json::json!(["rs", "rust-lang"]));
    assert_eq!(v[2]["name"], "zig");
    assert_eq!(v[2]["canonical"], true);
    assert_eq!(v[2]["count"], 0);
    assert!(
        db.fail(&["tag", "ls", "--sort", "bogus"])
            .contains("invalid value")
    );
}

#[test]
fn tag_rename() {
    let db = Db::new();
    db.ok(&["add", "https://a.com", "-t", "rust", "-t", "web"]);
    let uid = json_lines(&db.ok(&["tag", "ls", "--json"]))[0]["uid"].clone();
    assert_eq!(
        db.ok(&["tag", "rename", "Rust", "rust-lang"]),
        "renamed rust to rust-lang (1 bookmark)\n"
    );
    assert_eq!(json_lines(&db.ok(&["tag", "ls", "--json"]))[0]["uid"], uid);
    assert_eq!(db.ok(&["ls"]), "1  https://a.com  #rust-lang  #web\n");
    assert_eq!(db.ok(&["search", "#rust-lang"]).lines().count(), 1);
    let err = db.fail(&["tag", "rename", "rust-lang", "web"]);
    assert!(
        err.contains("already exists") && err.contains("shlv tag merge"),
        "{err}"
    );
    assert!(db.fail(&["tag", "rename", "nope", "x"]).contains("no tag"));
}

#[test]
fn tag_merge() {
    let db = Db::new();
    db.ok(&["add", "https://a.com", "-t", "js"]);
    db.ok(&["add", "https://b.com", "-t", "js", "-t", "javascript"]);
    db.ok(&["tag", "alias", "ecmascript", "js"]);
    assert_eq!(
        db.ok(&["tag", "merge", "js", "javascript"]),
        "merged js into javascript (2 bookmarks); js is now an alias\n"
    );
    assert_eq!(db.ok(&["tag", "ls"]), "2   javascript  = ecmascript js\n");
    assert_eq!(
        db.ok(&["ls"]),
        "2  https://b.com  #javascript\n1  https://a.com  #javascript\n"
    );
    // Re-adding the old name lands on the target.
    assert_eq!(
        db.ok(&["add", "https://c.com", "-t", "js"]),
        "added 3  https://c.com  #javascript\n"
    );
    assert_eq!(db.ok(&["ls", "-t", "ecmascript"]).lines().count(), 3);
    let err = db.fail(&["tag", "merge", "javascript", "nope"]);
    assert!(err.contains("no tag \"nope\""), "{err}");
}

#[test]
fn tag_alias() {
    let db = Db::new();
    db.ok(&["add", "https://a.com", "-t", "rust", "-t", "go"]);
    assert_eq!(
        db.ok(&["tag", "alias", "rs", "rust"]),
        "added alias rs for rust\n"
    );
    assert_eq!(
        db.ok(&["tag", "alias", "rs", "rust"]),
        "rs is already an alias of rust\n"
    );
    assert_eq!(db.ok(&["ls", "-t", "rs"]).lines().count(), 1);
    let err = db.fail(&["tag", "alias", "go", "rust"]);
    assert!(
        err.contains("is a tag") && err.contains("shlv tag merge go rust"),
        "{err}"
    );
    assert_eq!(
        db.ok(&["tag", "alias", "--rm", "rs"]),
        "removed alias rs (of rust)\n"
    );
    assert!(
        db.fail(&["tag", "alias", "--rm", "rs"])
            .contains("no alias")
    );
    // TAG is required without --rm and refused with it.
    db.fail(&["tag", "alias", "rs"]);
    db.fail(&["tag", "alias", "--rm", "rs", "rust"]);
}

#[test]
fn tag_canonical() {
    let db = Db::new();
    db.ok(&["add", "https://a.com", "-t", "rust", "-t", "js"]);
    assert_eq!(
        db.ok(&["tag", "canonical", "add", "rust", "zig"]),
        "marked 2 tags canonical\n"
    );
    assert_eq!(db.ok(&["tag", "canonical", "ls"]), "rust\nzig\n");
    assert_eq!(
        db.ok(&["tag", "canonical", "rm", "zig"]),
        "unmarked 1 tag\n"
    );
    assert_eq!(db.ok(&["tag", "canonical", "ls"]), "rust\n");
    db.fail(&["tag", "canonical", "add"]);

    let file = db.0.join("tags.txt");
    std::fs::write(
        &file,
        "# my list\nJavaScript ecmascript  # comment\n\nrust rs\n",
    )
    .unwrap();
    assert_eq!(
        db.ok(&["tag", "canonical", "import", file.to_str().unwrap()]),
        "imported 2 tags: 1 newly canonical, 2 aliases added\n"
    );
    assert_eq!(
        db.ok(&["tag", "canonical", "export"]),
        "javascript ecmascript\nrust rs\n"
    );

    // From stdin; import only adds.
    let mut c = db.cmd(&["tag", "canonical", "import", "-"]);
    c.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = c.spawn().unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"go golang\n")
        .unwrap();
    assert!(child.wait_with_output().unwrap().status.success());
    assert_eq!(
        db.ok(&["tag", "canonical", "export"]),
        "go golang\njavascript ecmascript\nrust rs\n"
    );

    // A conflicting line fails the whole import.
    std::fs::write(&file, "python py\nweb js\n").unwrap();
    let err = db.fail(&["tag", "canonical", "import", file.to_str().unwrap()]);
    assert!(
        err.contains("line 2") && err.contains("\"js\" is a tag"),
        "{err}"
    );
    assert!(!db.ok(&["tag", "canonical", "ls"]).contains("python"));
    assert!(
        db.fail(&["tag", "canonical", "import", "/nonexistent"])
            .contains("cannot read")
    );
}
