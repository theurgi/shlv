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
            .env("SHLV_CONFIG", self.config())
            .env_remove("BROWSER")
            .env_remove("NO_COLOR");
        c
    }

    /// Where this test's config file goes (absent unless written).
    fn config(&self) -> PathBuf {
        self.0.join("config.toml")
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

#[test]
fn config_path_and_show() {
    let db = Db::new();
    let path = db.config().display().to_string();
    assert_eq!(db.ok(&["config", "path"]), format!("{path}\n"));
    assert_eq!(
        db.ok(&["config", "show"]),
        format!("# {path} (not found; defaults)\nstrict_tags = false\n")
    );

    let mut c = db.cmd(&["config", "path"]);
    c.env_remove("SHLV_CONFIG")
        .env("XDG_CONFIG_HOME", "/xdg")
        .env("HOME", "/home/u");
    assert_eq!(output(c).1, "/xdg/shlv/config.toml\n");
    let mut c = db.cmd(&["config", "path"]);
    c.env_remove("SHLV_CONFIG")
        .env("XDG_CONFIG_HOME", "relative")
        .env("HOME", "/home/u");
    assert_eq!(output(c).1, "/home/u/.config/shlv/config.toml\n");

    // Sections for features this build lacks are accepted and reported.
    std::fs::create_dir_all(&db.0).unwrap();
    std::fs::write(
        db.config(),
        "strict_tags = true\n[ai]\nmodel = \"m\"\n[provider.x]\nurl = \"u\"\n",
    )
    .unwrap();
    assert_eq!(
        db.ok(&["config", "show"]),
        format!(
            "# {path}\n# [ai] ignored: not in this build\n\
             # [provider] ignored: not in this build\nstrict_tags = true\n"
        )
    );
}

#[test]
fn config_errors_name_file_and_line() {
    let db = Db::new();
    db.ok(&["add", "a.com"]);
    std::fs::write(db.config(), "# settings\nstrict_tag = true\n").unwrap();
    let path = db.config().display().to_string();
    let err = db.fail(&["add", "b.com"]);
    assert!(
        err.contains(&format!("{path}:2:1: unknown field `strict_tag`")),
        "{err}"
    );
    assert!(
        db.fail(&["config", "show"])
            .contains(&format!("{path}:2:1"))
    );
    // Every command that opens the database reads the config (a declared tag list may need
    // mirroring); `config path` reads neither.
    assert!(db.fail(&["ls"]).contains(&format!("{path}:2:1")));
    assert!(db.ok(&["config", "path"]).contains(&path));

    std::fs::write(db.config(), "strict_tags = yes\n").unwrap();
    assert!(
        db.fail(&["edit", "1", "-t", "x"])
            .contains(&format!("{path}:1:15: "))
    );

    // A dangling symlink is an error, not silently "no config".
    std::fs::remove_file(db.config()).unwrap();
    std::os::unix::fs::symlink(db.0.join("gone.toml"), db.config()).unwrap();
    assert!(db.fail(&["add", "c.com"]).contains("broken symlink"));
}

#[test]
fn strict_tags_from_a_read_only_symlinked_config() {
    let db = Db::new();
    db.ok(&["tag", "canonical", "add", "rust"]);
    db.ok(&["tag", "alias", "rs", "rust"]);
    db.ok(&["add", "old.com", "-t", "misc"]);

    // Like a home-manager link into the Nix store: a symlink to a read-only file.
    let real = db.0.join("store-config.toml");
    std::fs::write(&real, "strict_tags = true\n").unwrap();
    let mut perms = std::fs::metadata(&real).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&real, perms).unwrap();
    std::os::unix::fs::symlink(&real, db.config()).unwrap();

    assert!(db.ok(&["add", "a.com", "-t", "rs"]).contains("#rust"));
    let err = db.fail(&["add", "b.com", "-t", "rust", "-t", "Web Dev", "-t", "misc"]);
    assert!(
        err.contains("not canonical (strict_tags is on): web-dev misc"),
        "{err}"
    );
    assert!(err.contains("shlv tag canonical add web-dev misc"), "{err}");
    assert_eq!(db.ok(&["ls"]).lines().count(), 2);

    let err = db.fail(&["edit", "1", "-t", "+other"]);
    assert!(
        err.contains("not canonical (strict_tags is on): other"),
        "{err}"
    );
    db.ok(&["edit", "1", "-t", "-misc", "-t", "rust"]);

    assert_eq!(
        std::fs::read_to_string(&real).unwrap(),
        "strict_tags = true\n"
    );
}

#[test]
fn declared_canonical_tags() {
    let db = Db::new();
    db.ok(&["add", "a.com", "-t", "js", "-t", "misc", "-t", "old"]);
    db.ok(&["tag", "canonical", "add", "misc"]);
    std::fs::write(
        db.config(),
        "strict_tags = true\n\n[tags.canonical]\njavascript = [\"js\", \"ecmascript\"]\nrust = []\n",
    )
    .unwrap();
    let path = db.config().display().to_string();

    // First run mirrors the list; the live tag `js` is folded into `javascript`, and says so.
    let (ok, out, err) = db.run(&["ls"]);
    assert!(ok, "{err}");
    assert!(out.contains("#javascript  #misc  #old"), "{out}");
    assert_eq!(
        err,
        format!("shlv: merged tag js into javascript (1 bookmark): {path} declares it an alias\n")
    );
    assert_eq!(
        db.ok(&["tag", "canonical", "export"]),
        "javascript ecmascript js\nrust\n"
    );
    // Unchanged list: nothing to report.
    assert_eq!(db.run(&["tag", "canonical", "ls"]).2, "");

    // Edits to the declared list are refused, naming the config file.
    let refused = |args: &[&str], what: &str| {
        let err = db.fail(args);
        assert!(
            err.contains(what) && err.contains(&format!("declared in {path} ([tags.canonical])")),
            "{args:?}: {err}"
        );
    };
    refused(
        &["tag", "canonical", "add", "go"],
        "cannot change canonical tags",
    );
    refused(
        &["tag", "canonical", "rm", "rust"],
        "cannot change canonical tags",
    );
    refused(
        &["tag", "canonical", "import", "-"],
        "cannot change canonical tags",
    );
    refused(&["tag", "alias", "rs", "rust"], "cannot change aliases");
    refused(&["tag", "alias", "--rm", "js"], "cannot change aliases");
    refused(
        &["tag", "rename", "Rust", "rust-lang"],
        "cannot rename declared tag rust",
    );
    refused(
        &["tag", "merge", "rust", "misc"],
        "cannot merge declared tag rust",
    );
    refused(
        &["tag", "merge", "old", "js"],
        "cannot merge into js, an alias of declared tag javascript",
    );

    // Undeclared tags can still be renamed and merged; merge leaves an alias that sync keeps.
    db.ok(&["tag", "rename", "old", "older"]);
    db.ok(&["tag", "merge", "older", "misc"]);
    assert!(db.ok(&["tag", "ls"]).contains("misc  = older"));

    // Strict mode points at the config file, not at `tag canonical add`.
    let err = db.fail(&["add", "b.com", "-t", "go"]);
    assert!(
        err.contains("not canonical (strict_tags is on): go"),
        "{err}"
    );
    assert!(err.contains(&format!("declare these in {path}")), "{err}");
    assert!(!err.contains("tag canonical add"), "{err}");
    assert!(
        db.ok(&["add", "b.com", "-t", "ecmascript"])
            .contains("#javascript")
    );

    // Dropping a tag from the list unmarks it; a conflict is an error at load time.
    std::fs::write(db.config(), "[tags.canonical]\njavascript = [\"js\"]\n").unwrap();
    assert_eq!(db.ok(&["tag", "canonical", "export"]), "javascript js\n");
    assert!(db.ok(&["tag", "ls"]).contains("misc  = older"));
    std::fs::write(
        db.config(),
        "[tags.canonical]\njs = []\njavascript = [\"js\"]\n",
    )
    .unwrap();
    let err = db.fail(&["ls"]);
    assert!(
        err.contains(&format!("{path}:1:1: "))
            && err.contains("\"js\" is declared both as a tag and as an alias of \"javascript\""),
        "{err}"
    );

    // Without the section, the database is editable again.
    std::fs::write(db.config(), "").unwrap();
    db.ok(&["tag", "canonical", "add", "go"]);
    db.ok(&["tag", "alias", "golang", "go"]);
}

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// Exported JSON lines without `id`, which a fresh database assigns anew.
fn without_ids(export: &str) -> Vec<serde_json::Value> {
    let mut v = json_lines(export);
    for b in &mut v {
        b.as_object_mut().unwrap().remove("id");
    }
    v
}

fn find<'a>(v: &'a [serde_json::Value], url: &str) -> &'a serde_json::Value {
    v.iter()
        .find(|b| b["url"] == url)
        .unwrap_or_else(|| panic!("no {url} in {v:?}"))
}

#[test]
fn import_firefox_html() {
    let db = Db::new();
    db.ok(&["add", "https://lwn.net", "-t", "news", "--note", "mine"]);
    let (ok, out, err) = db.run(&["import", &fixture("firefox.html")]);
    assert!(ok, "{err}");
    assert!(out.ends_with(": 6 added, 2 updated, 2 skipped\n"), "{out}");
    assert!(
        err.contains("skipped javascript: URL: javascript:(function"),
        "{err}"
    );
    assert!(
        err.contains("skipped place: URL: place:sort=8&maxResults=10"),
        "{err}"
    );

    let v = json_lines(&db.ok(&["export"]));
    assert_eq!(v.len(), 7);
    let rust = find(&v, "https://www.rust-lang.org/");
    // The duplicate later in the file updates it: its title wins, tags merge, the earlier
    // creation time wins.
    assert_eq!(rust["title"], "Rust (older duplicate)");
    assert_eq!(
        rust["tags"],
        serde_json::json!(["lang", "programming", "rust"])
    );
    assert_eq!(rust["created_at"], 1_690_000_000_000i64);
    let article = find(&v, "https://example.com/article?a=1&b=2");
    assert_eq!(article["title"], "Tom & Jerry: \"A History\" <2023>");
    assert_eq!(
        article["note"],
        "Recommended by Sam's newsletter.\nSecond line of the description."
    );
    assert_eq!(article["tags"], serde_json::json!(["long-form", "to-read"]));
    assert_eq!(article["created_at"], 1_700_000_500_000i64);
    assert_eq!(article["updated_at"], 1_700_000_600_000i64);
    assert_eq!(
        find(&v, "https://example.org/caf%C3%A9")["title"],
        "Café ☕ guide"
    );
    // The folder's <DD> doesn't attach to anything.
    assert!(find(&v, "https://support.mozilla.org/products/firefox")["note"].is_null());
    // An untitled bookmark already saved keeps its title, note, and tags.
    let lwn = find(&v, "https://lwn.net");
    assert_eq!(lwn["created_at"], 1_700_003_000_000i64);
    assert_eq!(lwn["note"], "mine");
    assert_eq!(lwn["tags"], serde_json::json!(["news"]));
    // No folder tags by default.
    assert!(
        find(&v, "https://news.ycombinator.com/")["tags"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn import_chromium_html_with_folders_as_tags() {
    let db = Db::new();
    let out = db.ok(&["import", "--folders-as-tags", &fixture("chromium.html")]);
    assert!(out.ends_with(": 6 added, 0 updated, 1 skipped\n"), "{out}");
    let v = json_lines(&db.ok(&["export"]));
    let tags = |url| find(&v, url)["tags"].clone();
    // The bookmarks bar is the browser's root, not a folder of the user's.
    assert_eq!(tags("https://github.com/"), serde_json::json!([]));
    assert_eq!(
        tags("https://doc.rust-lang.org/std/"),
        serde_json::json!(["dev-docs"])
    );
    assert_eq!(
        tags("https://developer.mozilla.org/en-US/docs/Web/HTML"),
        serde_json::json!(["dev-docs", "web"])
    );
    assert_eq!(
        tags("https://www.seriouseats.com/"),
        serde_json::json!(["recipes"])
    );
    let video = find(
        &v,
        "https://www.youtube.com/watch?v=dQw4w9WgXcQ&utm_source=share",
    );
    assert_eq!(video["title"], "Video & <stuff> — \"quoted\"");
    assert_eq!(video["created_at"], 1_699_001_000_000i64);
    find(&v, "chrome://settings/");
}

#[test]
fn json_round_trip() {
    let src = Db::new();
    src.ok(&["import", &fixture("firefox.html")]);
    src.ok(&[
        "add",
        "https://quotes.example/",
        "--title",
        "\"Quotes\" & <tags>, 'too'",
        "--note",
        "line one\nline two\ttabbed",
        "-t",
        "c++",
        "-t",
        "été",
    ]);
    let export = src.ok(&["export"]);
    assert_eq!(export, src.ok(&["export", "--format", "json"]));
    assert_eq!(export, src.ok(&["ls", "-n", "0", "--json"]));

    let dst = Db::new();
    let file = dst.0.join("export.jsonl");
    std::fs::create_dir_all(&dst.0).unwrap();
    std::fs::write(&file, &export).unwrap();
    let out = dst.ok(&["import", file.to_str().unwrap()]);
    assert!(out.ends_with(": 8 added, 0 updated, 0 skipped\n"), "{out}");
    // Everything but ids survives, uids and times included.
    assert_eq!(without_ids(&dst.ok(&["export"])), without_ids(&export));

    // Importing again updates in place.
    let out = dst.ok(&["import", file.to_str().unwrap()]);
    assert!(out.ends_with(": 0 added, 8 updated, 0 skipped\n"), "{out}");
    assert_eq!(json_lines(&dst.ok(&["export"])).len(), 8);
}

#[test]
fn html_round_trip() {
    let src = Db::new();
    src.ok(&["import", &fixture("firefox.html")]);
    src.ok(&[
        "add",
        "https://quotes.example/",
        "--title",
        "\"Quotes\" & <tags>, 'too'",
        "--note",
        "line one\nline two",
        "-t",
        "été",
    ]);
    let html = src.ok(&["export", "--format", "html"]);
    assert!(
        html.starts_with("<!DOCTYPE NETSCAPE-Bookmark-file-1>\n"),
        "{html}"
    );
    assert!(
        html.contains("TAGS=\"été\">&quot;Quotes&quot; &amp; &lt;tags&gt;, &#39;too&#39;</A>\n    <DD>line one&#10;line two\n"),
        "{html}"
    );

    let dst = Db::new();
    let file = dst.0.join("export.html");
    std::fs::create_dir_all(&dst.0).unwrap();
    std::fs::write(&file, &html).unwrap();
    dst.ok(&["import", file.to_str().unwrap()]);
    let (a, b) = (
        json_lines(&src.ok(&["export"])),
        json_lines(&dst.ok(&["export"])),
    );
    assert_eq!(a.len(), b.len());
    for (a, b) in a.iter().zip(&b) {
        for key in ["url", "title", "note", "tags"] {
            assert_eq!(a[key], b[key], "{key}");
        }
        // HTML keeps seconds.
        let secs = |v: &serde_json::Value| v["created_at"].as_i64().unwrap() / 1000;
        assert_eq!(secs(a), secs(b));
    }
}

#[test]
fn import_format_detection_and_errors() {
    let db = Db::new();
    // JSON lines from stdin; blank lines are fine, extra fields are ignored, missing ones default.
    let mut c = db.cmd(&["import", "-"]);
    c.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = c.spawn().unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            b"{\"url\":\"a.example\",\"id\":99,\"tags\":[\"X\"],\"created_at\":5000}\n\n\
              {\"url\":\"has space.example\"}\n",
        )
        .unwrap();
    let o = child.wait_with_output().unwrap();
    assert!(o.status.success());
    assert_eq!(
        String::from_utf8(o.stdout).unwrap(),
        "imported -: 1 added, 0 updated, 1 skipped\n"
    );
    let v = json_lines(&db.ok(&["export"]));
    assert_eq!(v[0]["url"], "https://a.example");
    assert_eq!(v[0]["tags"], serde_json::json!(["x"]));
    assert_eq!(v[0]["created_at"], 5000);

    // --format overrides detection.
    let err = db.fail(&["import", "--format", "json", &fixture("chromium.html")]);
    assert!(
        err.contains("line 1") && err.contains("--format html"),
        "{err}"
    );
    let file = db.0.join("plain.html");
    std::fs::write(&file, "<dl><dt><a href=\"https://b.example/\">B</a></dl>").unwrap();
    assert!(
        db.fail(&["import", file.to_str().unwrap()])
            .contains("line 1")
    );
    db.ok(&["import", "--format", "html", file.to_str().unwrap()]);

    std::fs::write(&file, "{\"url\":\"https://c.example/\"}\n{oops}\n").unwrap();
    let err = db.fail(&["import", file.to_str().unwrap()]);
    assert!(err.contains("line 2"), "{err}");
    assert!(db.fail(&["import", "/nonexistent"]).contains("cannot read"));
    assert_eq!(json_lines(&db.ok(&["export"])).len(), 2);
}

#[test]
fn import_strict_tags() {
    let db = Db::new();
    db.ok(&["tag", "canonical", "add", "rust"]);
    db.ok(&["tag", "alias", "programming", "rust"]);
    std::fs::write(db.config(), "strict_tags = true\n").unwrap();
    let err = db.fail(&["import", &fixture("firefox.html")]);
    assert!(
        err.contains("nothing imported")
            && err.contains(": to-read long-form lang\n")
            && err.contains("--drop-unknown-tags"),
        "{err}"
    );
    assert_eq!(db.ok(&["export"]), "");

    let (ok, out, err) = db.run(&["import", "--drop-unknown-tags", &fixture("firefox.html")]);
    assert!(ok, "{err}");
    assert!(out.ends_with(": 7 added, 1 updated, 2 skipped\n"), "{out}");
    assert!(
        err.contains("dropped tags: to-read long-form lang\n"),
        "{err}"
    );
    let v = json_lines(&db.ok(&["export"]));
    assert_eq!(
        find(&v, "https://www.rust-lang.org/")["tags"],
        serde_json::json!(["rust"])
    );
}

#[test]
fn closed_pipe_is_not_an_error() {
    let db = Db::new();
    std::fs::create_dir_all(&db.0).unwrap();
    // Well past a pipe buffer, so the writer sees the reader go away.
    let lines: String = (0..3000)
        .map(|i| {
            format!(
                "{{\"url\":\"https://example.com/{i}\",\"title\":\"{}\"}}\n",
                "x".repeat(100)
            )
        })
        .collect();
    let file = db.0.join("many.jsonl");
    std::fs::write(&file, lines).unwrap();
    db.ok(&["import", file.to_str().unwrap()]);
    for args in [
        &["export"][..],
        &["export", "--format", "html"],
        &["ls", "-n", "0", "--json"],
        &["search", "-n", "0", "--json", "x"],
    ] {
        let mut c = db.cmd(args);
        c.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = c.spawn().unwrap();
        let mut first = [0u8; 1];
        use std::io::Read;
        child.stdout.take().unwrap().read_exact(&mut first).unwrap();
        let o = child.wait_with_output().unwrap();
        let err = String::from_utf8(o.stderr).unwrap();
        assert!(o.status.success() && err.is_empty(), "{args:?}: {err}");
    }
}

#[test]
fn suggest_from_domain_rules() {
    let db = Db::new();
    let path = db.config().display().to_string();
    // Without rules there's nothing to suggest with, and add prints nothing extra.
    assert_eq!(
        db.ok(&["add", "github.com/a"]),
        "added 1  https://github.com/a\n"
    );
    let e = db.fail(&["suggest", "--all"]);
    assert!(
        e.contains(&format!("no domain rules in {path} ([tags.rules])")),
        "{e}"
    );

    std::fs::write(
        db.config(),
        "[tags.rules]\n\"github.com\" = [\"code\"]\n\"docs.rs\" = [\"rust\", \"docs\"]\n",
    )
    .unwrap();
    assert_eq!(
        db.ok(&["add", "docs.rs/serde", "--title", "Serde", "-t", "rust"]),
        "added 2  Serde  https://docs.rs/serde  #rust\n\
         suggested  #docs (rule)  (shlv suggest accept 2)\n"
    );
    assert_eq!(
        db.ok(&["add", "example.com"]),
        "added 3  https://example.com\n"
    );
    // Bookmark 1 predates the rules.
    assert_eq!(
        db.ok(&["suggest", "ls"]),
        "2  Serde  https://docs.rs/serde  #docs (rule)\n"
    );
    assert_eq!(
        db.ok(&["suggest", "1", "3"]),
        "rules matched 1 of 2 bookmarks: 1 new suggestion\n\
         1  https://github.com/a  #code (rule)\n"
    );
    assert_eq!(
        db.ok(&["suggest", "--all"]),
        "rules matched 2 of 3 bookmarks: 0 new suggestions\n"
    );
    let json = json_lines(&db.ok(&["suggest", "ls", "--json"]));
    assert_eq!(json.len(), 2);
    assert_eq!(
        json[0],
        serde_json::json!({"id": 2, "url": "https://docs.rs/serde", "title": "Serde",
                           "tag": "docs", "source": "rule", "confidence": 1.0})
    );
    assert_eq!(
        db.ok(&["suggest", "ls", "1"]),
        "1  https://github.com/a  #code (rule)\n"
    );

    assert_eq!(db.ok(&["suggest", "reject", "2"]), "rejected for 2: docs\n");
    assert_eq!(
        db.ok(&["suggest", "accept", "1", "code"]),
        "accepted code; now 1  https://github.com/a  #code\n"
    );
    assert_eq!(db.ok(&["suggest", "ls"]), "");
    assert_eq!(
        db.ok(&["suggest", "--all"]),
        "rules matched 2 of 3 bookmarks: 0 new suggestions\n"
    );
    assert_eq!(
        db.ok(&["suggest", "accept", "--all"]),
        "accepted 0 tags on 0 bookmarks\n"
    );
    let e = db.fail(&["suggest", "accept", "2"]);
    assert_eq!(e, "shlv: no pending suggestions for bookmark 2\n");
    let e = db.fail(&["suggest", "accept", "9"]);
    assert_eq!(e, "shlv: no bookmark with id 9\n");

    // Usage: ids or --all, not both or neither; subcommand names aren't ids.
    assert!(db.fail(&["suggest"]).contains("required arguments"));
    assert!(
        db.fail(&["suggest", "1", "--all"])
            .contains("cannot be used with")
    );
    assert!(
        db.fail(&["suggest", "accept", "--all", "1"])
            .contains("cannot be used with")
    );
    assert!(db.fail(&["suggest", "x"]).contains("invalid value 'x'"));
}

#[test]
fn suggest_auto_mode() {
    let db = Db::new();
    std::fs::create_dir_all(&db.0).unwrap();
    std::fs::write(
        db.config(),
        "[tags]\nsuggestions = \"auto\"\n\n[tags.rules]\n\"github.com\" = [\"code\", \"git\"]\n",
    )
    .unwrap();
    assert_eq!(
        db.ok(&["add", "gist.github.com/x", "-t", "snippet"]),
        "added 1  https://gist.github.com/x  #code  #git  #snippet\n"
    );
    // Removing a tag a rule applied rejects it: it doesn't come back.
    db.ok(&["edit", "1", "-t", "-git"]);
    assert_eq!(
        db.ok(&["suggest", "--all"]),
        "rules matched 1 of 1 bookmark: 0 tags applied to 0 bookmarks\n"
    );
    assert!(db.ok(&["show", "1"]).contains("tags:     code snippet\n"));
    assert!(
        db.ok(&["config", "show"])
            .contains("[tags]\nsuggestions = \"auto\"\n")
    );
}

#[test]
fn suggest_strict_rules() {
    let db = Db::new();
    let path = db.config().display().to_string();
    db.ok(&["tag", "canonical", "add", "code"]);
    std::fs::write(
        db.config(),
        "strict_tags = true\n[tags.rules]\n\"github.com\" = [\"code\"]\n\"docs.rs\" = [\"rust\"]\n",
    )
    .unwrap();
    // The canonical list is in the database: commands that apply rules check it.
    let e = db.fail(&["add", "example.com", "-t", "code"]);
    assert_eq!(
        e,
        format!(
            "shlv: {path}: [tags.rules] uses tags that aren't canonical (strict_tags is on): \
             rust (docs.rs)\nhint: make the tags canonical, or change the rules\n"
        )
    );
    assert!(db.fail(&["suggest", "--all"]).contains("rust (docs.rs)"));
    // Others still work, so the list can be fixed.
    db.ok(&["tag", "canonical", "add", "rust"]);
    db.ok(&["add", "docs.rs/x"]);
    assert_eq!(
        db.ok(&["suggest", "accept", "1"]),
        "accepted rust; now 1  https://docs.rs/x  #rust\n"
    );

    // A declared list is checked when the config loads, with a position.
    std::fs::write(
        db.config(),
        "strict_tags = true\n[tags.canonical]\ncode = []\n\
         [tags.rules]\n\"github.com\" = [\"code\"]\n\"docs.rs\" = [\"rust\"]\n",
    )
    .unwrap();
    assert_eq!(
        db.fail(&["ls"]),
        format!(
            "shlv: {path}:6:13: rule \"docs.rs\": not in [tags.canonical] (strict_tags is on): \
             rust\n"
        )
    );
}
