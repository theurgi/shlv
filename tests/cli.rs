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

    /// Runs shlv with `input` on stdin; returns (success, stdout, stderr).
    fn run_stdin(&self, args: &[&str], input: &str) -> (bool, String, String) {
        use std::io::Write;
        use std::process::Stdio;
        let mut c = self.cmd(args);
        c.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = c.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let o = child.wait_with_output().unwrap();
        (
            o.status.success(),
            String::from_utf8(o.stdout).unwrap(),
            String::from_utf8(o.stderr).unwrap(),
        )
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
fn edit_many_and_from_stdin() {
    let db = Db::new();
    db.ok(&[
        "add",
        "https://a.com/wifi",
        "--title",
        "Wifi A",
        "-t",
        "old",
    ]);
    db.ok(&["add", "https://b.com/wifi", "--title", "Wifi B"]);
    db.ok(&["add", "https://c.com", "--title", "Other", "-t", "old"]);

    let out = db.ok(&["edit", "1", "2", "1", "-t", "net", "-t", "-old"]);
    assert_eq!(out, "updated 2 bookmarks  +#net 2  -#old 1\n");
    let out = db.ok(&["edit", "1", "2", "-t", "net"]);
    assert_eq!(out, "updated 0 bookmarks  +#net 0  (2 already so)\n");

    // Plain search output, --json, and bare ids all work on stdin.
    let found = db.ok(&["search", "wifi"]);
    let (ok, out, err) = db.run_stdin(&["edit", "-", "-t", "wifi"], &found);
    assert!(ok, "{err}");
    assert_eq!(out, "updated 2 bookmarks  +#wifi 2\n");
    let json = db.ok(&["ls", "--json", "-t", "wifi"]);
    let (ok, out, _) = db.run_stdin(&["edit", "-", "3", "-t", "x"], &json);
    assert!(ok);
    assert_eq!(out, "updated 3 bookmarks  +#x 3\n");
    let (ok, out, _) = db.run_stdin(&["edit", "-", "-t", "-x"], "3\n\n1\n");
    assert!(ok);
    assert_eq!(out, "updated 2 bookmarks  -#x 2\n");
    let (ok, _, err) = db.run_stdin(&["edit", "-", "-t", "y"], "");
    assert!(ok);
    assert!(err.contains("no bookmark ids on stdin"), "{err}");

    // A stray line or a missing id fails, and nothing changes.
    let (ok, _, err) = db.run_stdin(&["edit", "-", "-t", "y"], "1\nhttps://c.com\n");
    assert!(!ok);
    assert!(err.contains("stdin line 2"), "{err}");
    let err = db.fail(&["edit", "1", "9", "8", "-t", "y"]);
    assert!(err.contains("no bookmark with ids 9 8"), "{err}");
    assert!(!db.ok(&["tag", "ls"]).contains("y\n"));
    assert!(
        db.fail(&["edit", "1", "2", "--title", "T"])
            .contains("one title")
    );
    assert!(
        db.fail(&["edit", "x", "-t", "y"])
            .contains("isn't a bookmark id")
    );

    // Strict mode refuses the whole edit; propose mode proposes once for all of them.
    std::fs::write(db.config(), "strict_tags = true\n").unwrap();
    db.ok(&["tag", "canonical", "add", "net", "wifi"]);
    let err = db.fail(&["edit", "1", "2", "-t", "new"]);
    assert!(err.contains("not canonical"), "{err}");
    std::fs::write(db.config(), "strict_tags = \"propose\"\n").unwrap();
    let out = db.ok(&["edit", "1", "2", "-t", "new"]);
    assert!(
        out.starts_with("updated 0 bookmarks  (2 already so)\nproposed  #new  "),
        "{out}"
    );
    assert!(db.ok(&["tag", "proposals"]).contains("new"));
}

#[test]
fn edit_pick() {
    let db = Db::new();
    for u in ["a.com", "b.com", "c.com", "d.com"] {
        db.ok(&["add", u]);
    }
    // Without a terminal, answers are lines: y, n, b(ack), n, then a(ll the rest).
    let (ok, out, err) = db.run_stdin(
        &["edit", "1", "2", "3", "4", "--pick", "-t", "p"],
        "y\nn\nb\nn\na\n",
    );
    assert!(ok, "{err}");
    assert!(err.contains("+#p on which of 4 bookmarks?"), "{err}");
    assert_eq!(out, "updated 3 bookmarks  +#p 3\n");
    assert_eq!(db.ok(&["ls", "-t", "p"]).lines().count(), 3);
    assert!(!db.ok(&["show", "2"]).contains("tags"));

    // q stops with what was picked so far (here nothing); the end of input works like q.
    let (ok, out, _) = db.run_stdin(&["edit", "1", "2", "3", "--pick", "-t", "-p"], "n\nq\n");
    assert!(ok);
    assert_eq!(out, "");
    let (ok, out, _) = db.run_stdin(&["edit", "1", "2", "--pick", "-t", "-p"], "y\n");
    assert!(ok);
    assert_eq!(out, "updated 1  https://a.com\n");
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
    #[cfg(not(feature = "fetch"))]
    assert!(
        db.fail(&["add", "https://a.com", "--fetch"])
            .contains("no fetch support")
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
    assert_eq!(v[2]["description"], serde_json::Value::Null);
    assert_eq!(
        db.ok(&["tag", "describe", "zig", "a systems language"]),
        "described zig\n"
    );
    assert_eq!(
        db.ok(&["tag", "describe", "zig", "a systems language"]),
        "zig: unchanged\n"
    );
    let v = json_lines(&db.ok(&["tag", "ls", "--json"]));
    assert_eq!(v[2]["description"], "a systems language");
    assert!(
        db.ok(&["tag", "ls"])
            .ends_with(" 0 * zig  # a systems language\n")
    );
    assert_eq!(
        db.ok(&["tag", "canonical", "export"]),
        "web\nzig # a systems language\n"
    );
    assert!(
        db.fail(&["tag", "describe", "rs", "x"])
            .contains("alias of \"rust\"")
    );
    assert_eq!(
        db.ok(&["tag", "describe", "zig", ""]),
        "cleared the description of zig\n"
    );
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
        "imported 2 tags: 1 newly canonical, 2 aliases added, 1 description set\n"
    );
    assert_eq!(
        db.ok(&["tag", "canonical", "export"]),
        "javascript ecmascript # comment\nrust rs\n"
    );

    // From stdin; import only adds.
    let (ok, _, err) = db.run_stdin(&["tag", "canonical", "import", "-"], "go golang\n");
    assert!(ok, "{err}");
    assert_eq!(
        db.ok(&["tag", "canonical", "export"]),
        "go golang\njavascript ecmascript # comment\nrust rs\n"
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
    if cfg!(feature = "ai") {
        return;
    }
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
        "strict_tags = true\n\n[tags.canonical]\njavascript = [\"js\", \"ecmascript\"]\n\
         rust = []\ninfra = { aliases = [\"ops\"], description = \"servers\" }\n",
    )
    .unwrap();
    let path = db.config().display().to_string();

    // First run syncs the list; the live tag `js` is folded into `javascript`, and says so.
    // `misc`, canonical in the database only, stays.
    let (ok, out, err) = db.run(&["ls"]);
    assert!(ok, "{err}");
    assert!(out.contains("#javascript  #misc  #old"), "{out}");
    assert_eq!(
        err,
        format!("shlv: merged tag js into javascript (1 bookmark): {path} declares it an alias\n")
    );
    assert_eq!(
        db.ok(&["tag", "canonical", "export"]),
        "infra ops # servers\njavascript ecmascript js\nmisc\nrust\n"
    );
    // Unchanged list: nothing to report.
    assert_eq!(db.run(&["tag", "canonical", "ls"]).2, "");

    // Edits that would undo the declared list are refused, naming the config file.
    let refused = |args: &[&str], what: &str| {
        let err = db.fail(args);
        assert_eq!(
            err,
            format!(
                "shlv: {what}: it is pinned by [tags.canonical] in the config\n\
                 hint: change it in {path}\n"
            ),
            "{args:?}"
        );
    };
    refused(
        &["tag", "canonical", "rm", "misc", "rust"],
        "cannot unmark pinned tag rust",
    );
    refused(
        &["tag", "alias", "--rm", "js"],
        "cannot remove alias js of pinned tag javascript",
    );
    refused(
        &["tag", "describe", "infra", "servers and CI"],
        "cannot change the description of pinned tag infra",
    );
    refused(
        &["tag", "rename", "Rust", "rust-lang"],
        "cannot rename pinned tag rust",
    );
    refused(
        &["tag", "merge", "rust", "misc"],
        "cannot merge away pinned tag rust",
    );
    let (ok, _, err) = db.run_stdin(&["tag", "canonical", "import", "-"], "infra # other\n");
    assert!(!ok);
    assert!(
        err.contains("line 1: cannot change the description of pinned tag infra: it is pinned")
            && err.contains(&format!("hint: change it in {path}")),
        "{err}"
    );

    // The rest works, pinned tags included: add, alias, describe, merge into, rename others.
    db.ok(&["tag", "canonical", "add", "go"]);
    db.ok(&["tag", "alias", "golang", "go"]);
    db.ok(&["tag", "alias", "rs", "rust"]);
    db.ok(&["tag", "describe", "rust", "the language"]);
    db.ok(&["tag", "rename", "old", "older"]);
    db.ok(&["tag", "merge", "older", "js"]);
    db.ok(&["tag", "canonical", "rm", "misc"]);
    let (ok, _, err) = db.run_stdin(
        &["tag", "canonical", "import", "-"],
        "zig # a language\ninfra devops # servers\n",
    );
    assert!(ok, "{err}");
    // Strict mode's hint suggests `tag canonical add`, which works now.
    let err = db.fail(&["add", "b.com", "-t", "nim"]);
    assert!(err.contains("shlv tag canonical add nim"), "{err}");
    assert!(
        db.ok(&["add", "b.com", "-t", "golang", "-t", "ecmascript"])
            .contains("#go  #javascript")
    );
    let list = "go golang\ninfra devops ops # servers\njavascript ecmascript js older\n\
                rust rs # the language\nzig # a language\n";
    assert_eq!(db.ok(&["tag", "canonical", "export"]), list);
    // A changed list resyncs and keeps all of that: pinning `zig` keeps its description.
    std::fs::write(
        db.config(),
        std::fs::read_to_string(db.config()).unwrap() + "zig = []\n",
    )
    .unwrap();
    assert_eq!(db.ok(&["tag", "canonical", "export"]), list);

    // In the config's shape, marking what the config pins.
    assert_eq!(
        db.ok(&["tag", "canonical", "export", "--toml"]),
        "[tags.canonical]\ngo = [\"golang\"]\n\
         infra = { aliases = [\"devops\", \"ops\"], description = \"servers\" } # pinned\n\
         javascript = [\"ecmascript\", \"js\", \"older\"] # pinned\n\
         rust = { aliases = [\"rs\"], description = \"the language\" } # pinned\n\
         zig = { aliases = [], description = \"a language\" } # pinned\n"
    );
    assert_eq!(
        db.ok(&["tag", "canonical", "export", "--nix"]),
        "programs.shlv.settings.tags.canonical = {\n  go = [ \"golang\" ];\n  \
         infra = { aliases = [ \"devops\" \"ops\" ]; description = \"servers\"; }; # pinned\n  \
         javascript = [ \"ecmascript\" \"js\" \"older\" ]; # pinned\n  \
         rust = { aliases = [ \"rs\" ]; description = \"the language\"; }; # pinned\n  \
         zig = { aliases = [ ]; description = \"a language\"; }; # pinned\n};\n"
    );
    assert!(
        db.fail(&["tag", "canonical", "export", "--toml", "--nix"])
            .contains("cannot be used with")
    );

    // Dropping a tag from the list unpins it; it stays canonical and can be unmarked now.
    std::fs::write(db.config(), "[tags.canonical]\njavascript = [\"js\"]\n").unwrap();
    assert_eq!(db.ok(&["tag", "canonical", "export"]), list);
    db.ok(&["tag", "canonical", "rm", "rust"]);
    db.ok(&["tag", "alias", "--rm", "ops"]);
    db.ok(&["tag", "describe", "infra", ""]);
    // A conflict is an error at load time.
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

    // Without the section, nothing is pinned.
    std::fs::write(db.config(), "").unwrap();
    db.ok(&["tag", "alias", "--rm", "js"]);
    db.ok(&["tag", "canonical", "rm", "javascript"]);
    assert!(
        !db.ok(&["tag", "canonical", "export", "--toml"])
            .contains("pinned")
    );
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
            "shlv: domain rules use tags that aren't canonical (strict_tags is on): rust (docs.rs)\n\
             hint: make the tags canonical, or change the rules ([tags.rules] in {path}, or \
             `shlv rule rm`)\n"
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

    // With a declared list, rule tags may be canonical in the database only.
    std::fs::write(
        db.config(),
        "strict_tags = true\n[tags.canonical]\ncode = []\n\
         [tags.rules]\n\"github.com\" = [\"code\"]\n\"docs.rs\" = [\"rust\"]\n\
         \"go.dev\" = [\"go\"]\n",
    )
    .unwrap();
    assert!(
        db.fail(&["add", "go.dev/x"])
            .contains("aren't canonical (strict_tags is on): go (go.dev)")
    );
    db.ok(&["tag", "canonical", "add", "go"]);
    db.ok(&["add", "go.dev/x"]);
}

#[test]
fn import_runs_domain_rules() {
    let db = Db::new();
    let path = db.config().display().to_string();
    std::fs::create_dir_all(&db.0).unwrap();
    std::fs::write(db.config(), "[tags.rules]\n\"github.com\" = [\"code\"]\n").unwrap();
    let file = db.0.join("in.jsonl");
    std::fs::write(
        &file,
        "{\"url\":\"https://github.com/a\"}\n{\"url\":\"https://example.com/\"}\n",
    )
    .unwrap();
    let file = file.to_str().unwrap();
    assert_eq!(
        db.ok(&["import", file]),
        format!(
            "imported {file}: 2 added, 0 updated, 0 skipped\n\
             rules matched 1 of 2 bookmarks: 1 new suggestion\n"
        )
    );
    assert_eq!(
        db.ok(&["import", "--no-rules", file]),
        format!("imported {file}: 0 added, 2 updated, 0 skipped\n")
    );

    std::fs::write(
        db.config(),
        "strict_tags = true\n[tags.rules]\n\"github.com\" = [\"code\"]\n",
    )
    .unwrap();
    assert_eq!(
        db.fail(&["import", file]),
        format!(
            "shlv: nothing imported: domain rules use tags that aren't canonical \
             (strict_tags is on): code (github.com)\n\
             hint: make the tags canonical, change the rules ([tags.rules] in {path}, or \
             `shlv rule rm`), or pass --no-rules\n"
        )
    );
}

#[test]
fn import_urls_only_karakeep_html() {
    let db = Db::new();
    std::fs::create_dir_all(&db.0).unwrap();
    std::fs::write(
        db.config(),
        "[tags.rules]\n\"rust-lang.org\" = [\"rust\"]\n",
    )
    .unwrap();
    // Already saved, with a tag and a note of its own: merged as today, keeping both.
    db.ok(&["add", "https://lwn.net", "-t", "linux", "--note", "mine"]);
    let file = fixture("karakeep.html");
    assert_eq!(
        db.ok(&["import", "--urls-only", &file]),
        format!(
            "imported {file}: 2 added, 2 updated, 0 skipped\n\
             rules matched 1 of 3 bookmarks: 1 new suggestion\n"
        )
    );
    let v = json_lines(&db.ok(&["export"]));
    assert_eq!(v.len(), 3);
    // No Karakeep tags; a later duplicate's title wins and the earlier date stays.
    let rust = find(&v, "https://www.rust-lang.org/");
    assert_eq!(rust["tags"], serde_json::json!([]));
    assert_eq!(rust["title"], "Rust (older copy)");
    assert_eq!(rust["created_at"], 1_690_000_000_000i64);
    let tom = find(&v, "https://example.com/a?x=1&y=2");
    assert_eq!(tom["title"], "Tom & Jerry: \"A History\"");
    assert_eq!(tom["tags"], serde_json::json!([]));
    assert_eq!(tom["created_at"], 1_700_000_200_000i64);
    // Karakeep writes the URL as the title of an untitled bookmark; that isn't a title.
    let lwn = find(&v, "https://lwn.net");
    assert!(lwn["title"].is_null(), "{lwn}");
    assert_eq!(lwn["tags"], serde_json::json!(["linux"]));
    assert_eq!(lwn["note"], "mine");
    assert_eq!(lwn["created_at"], 1_700_000_100_000i64);
    assert_eq!(db.ok(&["suggest", "ls"]).lines().count(), 1);
}

#[test]
fn import_urls_only_without_titles() {
    let db = Db::new();
    let file = fixture("karakeep.html");
    db.ok(&["import", "--urls-only", "--no-titles", "--no-rules", &file]);
    for b in json_lines(&db.ok(&["export"])) {
        assert!(
            b["title"].is_null() && b["tags"] == serde_json::json!([]),
            "{b}"
        );
    }
}

#[test]
fn import_url_list() {
    let db = Db::new();
    std::fs::create_dir_all(&db.0).unwrap();
    std::fs::write(
        db.config(),
        "[tags.rules]\n\"rust-lang.org\" = [\"rust\"]\n",
    )
    .unwrap();
    db.ok(&["add", "https://lwn.net", "-t", "linux"]);
    let file = db.0.join("urls.txt");
    std::fs::write(
        &file,
        "\u{feff}# from somewhere\r\n\r\nhttps://www.rust-lang.org/\r\n  lwn.net/  \r\n\
         https://example.org/caf%C3%A9\r\nhttps://www.rust-lang.org\r\njavascript:void(0)\r\n",
    )
    .unwrap();
    let file = file.to_str().unwrap();
    assert_eq!(
        db.ok(&["import", file]),
        format!(
            "imported {file}: 2 added, 2 updated, 1 skipped\n\
             rules matched 1 of 3 bookmarks: 1 new suggestion\n"
        )
    );
    let v = json_lines(&db.ok(&["export"]));
    assert_eq!(v.len(), 3);
    let cafe = find(&v, "https://example.org/caf%C3%A9");
    assert!(cafe["title"].is_null() && cafe["tags"] == serde_json::json!([]));
    assert_eq!(
        find(&v, "https://lwn.net")["tags"],
        serde_json::json!(["linux"])
    );

    // Stdin and --format; a line that isn't a URL writes nothing.
    let db = Db::new();
    let (ok, out, _) = db.run_stdin(&["import", "--format", "urls", "-"], "a.example\n");
    assert!(
        ok && out.ends_with(": 1 added, 0 updated, 0 skipped\n"),
        "{out}"
    );
    let (ok, _, err) = db.run_stdin(&["import", "-"], "a.example\nb.example Title\n");
    assert!(!ok);
    assert_eq!(
        err,
        "shlv: cannot read -: line 2: not a URL: \"b.example Title\"\n"
    );
    assert_eq!(json_lines(&db.ok(&["export"])).len(), 1);
    // Another app's export in one JSON object.
    let (ok, _, err) = db.run_stdin(&["import", "-"], "{\"bookmarks\": []}\n");
    assert!(!ok);
    assert_eq!(err, "shlv: cannot read -: line 1: no \"url\"\n");
}

#[test]
fn import_urls_only_json_lines() {
    let db = Db::new();
    std::fs::create_dir_all(&db.0).unwrap();
    let file = db.0.join("in.jsonl");
    std::fs::write(
        &file,
        "{\"url\":\"https://a.example/\",\"title\":\"A\",\"note\":\"n\",\"summary\":\"s\",\
         \"tags\":[\"x\"],\"created_at\":1000,\"updated_at\":5000,\
         \"uid\":\"0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b\"}\n",
    )
    .unwrap();
    db.ok(&["import", "--urls-only", file.to_str().unwrap()]);
    let v = json_lines(&db.ok(&["export"]));
    let a = find(&v, "https://a.example/");
    assert_eq!(a["title"], "A");
    assert_eq!(
        (a["created_at"].as_i64(), a["updated_at"].as_i64()),
        (Some(1000), Some(1000))
    );
    assert!(a["note"].is_null() && a["summary"].is_null(), "{a}");
    assert_eq!(a["tags"], serde_json::json!([]));
    assert_ne!(a["uid"], "0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b");
}

#[test]
fn import_urls_only_refuses_tag_options() {
    let db = Db::new();
    let file = fixture("chromium.html");
    for flag in ["--folders-as-tags", "--drop-unknown-tags"] {
        let err = db.fail(&["import", "--urls-only", flag, &file]);
        assert!(
            err.contains(&format!("'--urls-only' cannot be used with '{flag}'")),
            "{err}"
        );
    }
    let err = db.fail(&["import", "--no-titles", &file]);
    assert!(err.contains("--urls-only"), "{err}");
}

#[test]
fn strict_propose_and_tag_proposals() {
    let db = Db::new();
    db.ok(&["tag", "canonical", "add", "rust"]);
    std::fs::write(db.config(), "strict_tags = \"propose\"\n").unwrap();
    let out = db.ok(&["add", "a.example", "-t", "rust", "-t", "Web Dev"]);
    assert_eq!(
        out,
        "added 1  https://a.example  #rust\n\
         proposed  #web-dev  (not canonical; `shlv tag proposals accept web-dev` adds them)\n"
    );
    let out = db.ok(&["edit", "1", "-t", "css"]);
    assert!(
        out.ends_with(
            "proposed  #css  (not canonical; `shlv tag proposals accept css` adds them)\n"
        ),
        "{out}"
    );
    assert_eq!(
        db.ok(&["tag", "proposals"]),
        "1  css  (user)\n1  web-dev  (user)\n"
    );

    // Rejected names are dropped, with a note.
    assert_eq!(
        db.ok(&["tag", "proposals", "reject", "css"]),
        "rejected css (it was proposed for 1 bookmark); it won't be proposed again\n"
    );
    let (ok, out, err) = db.run(&["add", "b.example", "-t", "css"]);
    assert!(ok, "{err}");
    assert_eq!(out, "added 2  https://b.example\n");
    assert!(err.contains("dropped css: rejected as proposals"), "{err}");

    // Import proposes too, in this mode or with --propose-unknown-tags.
    let (ok, out, err) = db.run(&["import", &fixture("firefox.html")]);
    assert!(ok, "{err}");
    assert!(out.contains("7 added"), "{out}");
    assert!(
        err.contains(
            "kept 4 tags as proposals (not canonical): programming to-read long-form lang\n"
        ),
        "{err}"
    );
    std::fs::write(db.config(), "strict_tags = true\n").unwrap();
    let err = db.fail(&["import", &fixture("firefox.html")]);
    assert!(err.contains("--propose-unknown-tags"), "{err}");
    let (ok, _, err) = db.run(&["import", "--propose-unknown-tags", &fixture("firefox.html")]);
    assert!(ok, "{err}");
    let err = db.fail(&[
        "import",
        "--propose-unknown-tags",
        "--drop-unknown-tags",
        "x",
    ]);
    assert!(err.contains("cannot be used with"), "{err}");

    let out = db.ok(&["tag", "proposals", "accept", "web-dev"]);
    assert_eq!(out, "accepted web-dev: canonical, tagged 1 bookmark\n");
    assert!(db.ok(&["show", "1"]).contains("tags:     rust web-dev\n"));
    let err = db.fail(&["tag", "proposals", "accept", "nope"]);
    assert!(err.contains("no proposal \"nope\""), "{err}");
    let err = db.fail(&["tag", "proposals", "alias", "lang", "nope"]);
    assert!(err.contains("no tag \"nope\""), "{err}");
    let out = db.ok(&["tag", "proposals", "alias", "lang", "rust"]);
    assert!(out.starts_with("aliased lang to rust: tagged "), "{out}");
    assert!(db.ok(&["tag", "ls"]).contains("rust  = lang"));
    let err = db.fail(&["tag", "proposals", "reject", "rust"]);
    assert!(err.contains("is on the list"), "{err}");
    let v = json_lines(&db.ok(&["tag", "proposals", "ls", "--rejected", "--json"]));
    assert_eq!(v.len(), 1);
    assert_eq!(
        (&v[0]["name"], &v[0]["state"]),
        (&serde_json::json!("css"), &serde_json::json!("rejected"))
    );
}

#[test]
fn tag_style() {
    let db = Db::new();
    db.ok(&[
        "add",
        "a.com",
        "-t",
        "web-dev",
        "-t",
        "self-hosting-infrastructure",
    ]);
    std::fs::write(
        db.config(),
        "[tags.style]\nseparator = \"_\"\nmax_length = 12\nguidelines = \"Singular nouns.\"\n",
    )
    .unwrap();
    let shown = db.ok(&["config", "show"]);
    assert!(
        shown.contains(
            "[tags.style]\nseparator = \"_\"\nmax_length = 12\nguidelines = \"Singular nouns.\"\n"
        ),
        "{shown}"
    );
    // New names are fixed or refused; existing ones are left alone.
    db.ok(&["add", "b.com", "-t", "Home Lab", "-t", "web-dev"]);
    let tags = db.ok(&["tag", "ls"]);
    for t in ["home_lab", "web-dev", "self-hosting-infrastructure"] {
        assert!(tags.contains(t), "{tags}");
    }
    let err = db.fail(&["add", "c.com", "-t", "self hosting stack"]);
    assert!(
        err.contains("\"self_hosting_stack\" is longer than [tags.style] max_length = 12"),
        "{err}"
    );
    let (ok, out, err) = db.run_stdin(
        &["import", "-"],
        "{\"url\": \"d.com\", \"tags\": [\"sci fi\", \"science-fiction\"]}\n",
    );
    assert!(ok, "{err}");
    assert!(out.contains("1 added"), "{out}");
    assert!(
        err.contains(
            "dropped tag: \"science_fiction\" is longer than [tags.style] max_length = 12"
        ),
        "{err}"
    );
    assert!(db.ok(&["tag", "ls"]).contains("sci_fi"));

    // Config names must follow the style, with a position.
    std::fs::write(
        db.config(),
        "[tags.style]\nseparator = \"_\"\n[tags.canonical]\nweb-dev = []\n",
    )
    .unwrap();
    let path = db.config().display().to_string();
    let err = db.fail(&["ls"]);
    assert!(
        err.contains(&format!(
            "{path}:4:1: [tags.canonical]: \"web-dev\" breaks [tags.style]; write \"web_dev\""
        )),
        "{err}"
    );
}

#[test]
fn tag_health_reports_and_validates_flags() {
    let db = Db::new();
    for i in 0..3 {
        let url = format!("https://m{i}.example/");
        let (ok, _, err) = db.run(&["add", &url, "-t", "ml", "-t", "machine-learning"]);
        assert!(ok, "{err}");
    }
    db.run(&["add", "https://u.example/", "--title", "Untagged"]);
    let (ok, out, err) = db.run(&["tag", "health"]);
    assert!(ok, "{err}");
    assert!(
        out.starts_with("coverage: 4 bookmarks, 3 tagged (75%)"),
        "{out}"
    );
    // Equal counts: the name that sorts first stays.
    assert!(
        out.contains("ml + machine-learning  together on 3 of 3 and 3 (1.00 Jaccard)  (shlv tag merge ml machine-learning)"),
        "{out}"
    );
    assert!(out.contains("gaps: 1 bookmark without tags"), "{out}");
    let (ok, out, _) = db.run(&["tag", "health", "--json", "--min-together", "4"]);
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["redundant"], serde_json::json!([]));
    assert_eq!(
        v["coverage"]["histogram"],
        serde_json::json!([1, 0, 3, 0, 0])
    );
    let (ok, _, err) = db.run(&["tag", "health", "--redundant", "1.5"]);
    assert!(!ok && err.contains("not a share between 0 and 1"), "{err}");
}

#[test]
fn rule_add_ls_rm() {
    let db = Db::new();
    db.ok(&["add", "https://a.bandcamp.com/x"]);
    std::fs::write(db.config(), "[tags.rules]\n\"github.com\" = [\"code\"]\n").unwrap();
    db.ok(&["add", "https://bandcamp.com/y", "-t", "music"]);
    assert_eq!(
        db.ok(&["rule", "add", "https://Bandcamp.com/", "music"]),
        "added rule bandcamp.com → #music\nrules matched 2 of 2 bookmarks: 1 new suggestion\n"
    );
    assert_eq!(
        db.ok(&["rule", "add", "bandcamp.com", "music"]),
        "bandcamp.com already gets #music\n"
    );
    db.ok(&["add", "https://github.com/x"]);
    assert_eq!(
        db.ok(&["rule", "ls"]),
        "bandcamp.com  #music  1 bookmark (1 pending)\n\
         github.com    #code  1 bookmark (1 pending)  # pinned\n"
    );
    let v = json_lines(&db.ok(&["rule", "ls", "--json"]));
    assert_eq!(v[1]["host"], "github.com");
    assert_eq!(v[1]["pinned"], true);
    let e = db.fail(&["rule", "rm", "github.com"]);
    assert!(
        e.contains("(#code): it is pinned by [tags.rules] in the config\nhint: change it in"),
        "{e}"
    );
    assert_eq!(
        db.ok(&["rule", "rm", "bandcamp.com"]),
        "removed rule bandcamp.com → #music; dropped 1 pending suggestion\n"
    );
    assert_eq!(db.ok(&["suggest", "ls"]).lines().count(), 1);
}
