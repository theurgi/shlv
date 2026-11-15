use std::io::{self, BufWriter, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand, ValueEnum};
use shlv::config::{self, Config};
use shlv::store::normalize_tag;
use shlv::tag::{format_canonical, parse_canonical};
use shlv::{Bookmark, Edit, ListQuery, NewBookmark, Store, TagInfo, TagSort};

/// A fast bookmark manager.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Add a bookmark (or update it, if the URL is already saved)
    Add {
        url: String,
        /// Tag (repeatable)
        #[arg(short, long = "tag", value_name = "TAG")]
        tags: Vec<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        note: Option<String>,
        /// Fetch the page title (not built yet)
        #[arg(long)]
        fetch: bool,
    },
    /// List bookmarks, newest first
    Ls {
        /// Only bookmarks with this tag (repeatable; all must match)
        #[arg(short, long = "tag", value_name = "TAG")]
        tags: Vec<String>,
        /// Maximum results; 0 for all
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: usize,
        /// One JSON object per line
        #[arg(long)]
        json: bool,
    },
    /// Search bookmarks: words, "phrases", #tag, tag:x, site:host, AND/OR/NOT
    Search {
        #[arg(required = true)]
        query: Vec<String>,
        /// Maximum results; 0 for all
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: usize,
        /// One JSON object per line
        #[arg(long)]
        json: bool,
    },
    /// Show one bookmark in full
    Show {
        id: i64,
        /// Print as JSON
        #[arg(long)]
        json: bool,
    },
    /// Change a bookmark's title, note, or tags
    Edit {
        id: i64,
        /// New title ("" clears it)
        #[arg(long)]
        title: Option<String>,
        /// New note ("" clears it)
        #[arg(long)]
        note: Option<String>,
        /// +TAG adds, -TAG removes (repeatable); a bare TAG adds
        #[arg(short, long = "tag", value_name = "±TAG", allow_hyphen_values = true)]
        tags: Vec<String>,
    },
    /// Delete bookmarks by id or URL
    Rm {
        #[arg(required = true, value_name = "ID|URL")]
        targets: Vec<String>,
    },
    /// Open a bookmark in the browser ($BROWSER, else the system opener)
    Open { id: i64 },
    /// Manage tags: list, rename, merge, aliases, canonical list
    Tag {
        #[command(subcommand)]
        cmd: TagCmd,
    },
    /// Show where the config file is and what it sets
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print the config file path ($SHLV_CONFIG, else $XDG_CONFIG_HOME/shlv/config.toml)
    Path,
    /// Print the effective config, defaults included
    Show,
}

#[derive(Subcommand)]
enum TagCmd {
    /// List tags with their bookmark counts; * marks canonical tags, = lists aliases
    Ls {
        /// Sort order
        #[arg(long, value_enum, default_value_t = SortArg::Name)]
        sort: SortArg,
        /// One JSON object per line
        #[arg(long)]
        json: bool,
    },
    /// Rename a tag (use merge if NEW already exists)
    Rename { old: String, new: String },
    /// Move every bookmark from FROM to INTO and leave FROM as an alias of INTO
    Merge { from: String, into: String },
    /// Make ALIAS resolve to TAG on input, or remove an alias with --rm
    Alias {
        alias: String,
        #[arg(required_unless_present = "rm", conflicts_with = "rm")]
        tag: Option<String>,
        /// Remove ALIAS
        #[arg(long)]
        rm: bool,
    },
    /// Mark, unmark, list, import, or export canonical tags
    Canonical {
        #[command(subcommand)]
        cmd: CanonicalCmd,
    },
}

#[derive(Subcommand)]
enum CanonicalCmd {
    /// Mark tags canonical (creating them if needed)
    Add {
        #[arg(required = true)]
        tags: Vec<String>,
    },
    /// Unmark canonical tags (the tags stay)
    Rm {
        #[arg(required = true)]
        tags: Vec<String>,
    },
    /// List canonical tags
    Ls,
    /// Add tags and aliases from a file ("-" for stdin): one tag per line, aliases after it
    Import { file: PathBuf },
    /// Print the canonical list in the format import reads
    Export,
}

#[derive(Clone, Copy, ValueEnum)]
enum SortArg {
    Name,
    Count,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            if e.downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
            {
                // `shlv ls | head` closed the pipe early: not an error.
                return ExitCode::SUCCESS;
            }
            eprintln!("shlv: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    let stdout = io::stdout();
    let style = Style::detect(stdout.is_terminal());
    let mut out = BufWriter::new(stdout.lock());
    if let Cmd::Config { cmd } = cli.cmd {
        run_config(cmd, &mut out)?;
        out.flush()?;
        return Ok(ExitCode::SUCCESS);
    }
    let mut store = Store::open_default()?;
    // Only commands that write tags need settings; the rest don't touch the config file.
    if matches!(cli.cmd, Cmd::Add { .. } | Cmd::Edit { .. }) {
        let config = Config::load(&config::default_path()?)?;
        store.set_strict_tags(config.strict_tags);
    }
    let limit = |n: usize| if n == 0 { usize::MAX } else { n };

    match cli.cmd {
        Cmd::Add {
            url,
            tags,
            title,
            note,
            fetch,
        } => {
            if fetch {
                bail!("--fetch isn't available yet; this build has no fetch support");
            }
            let added = store.add(&NewBookmark {
                url,
                title,
                note,
                tags,
            })?;
            let b = store.get(added.id)?.context("bookmark vanished")?;
            let verb = if added.created { "added" } else { "updated" };
            write!(out, "{verb} ")?;
            print_line(&mut out, &b, 0, &style)?;
        }
        Cmd::Ls {
            tags,
            limit: n,
            json,
        } => {
            let q = ListQuery {
                tags,
                limit: limit(n),
            };
            let width = id_width(&store)?;
            store.list_each(&q, |b| print_item(&mut out, &b, json, width, &style))?;
        }
        Cmd::Search {
            query,
            limit: n,
            json,
        } => {
            let width = id_width(&store)?;
            store.search_each(&query.join(" "), limit(n), |b| {
                print_item(&mut out, &b, json, width, &style)
            })?;
        }
        Cmd::Show { id, json } => {
            let b = get(&store, id)?;
            if json {
                serde_json::to_writer(&mut out, &b)?;
                writeln!(out)?;
            } else {
                print_full(&mut out, &b)?;
            }
        }
        Cmd::Edit {
            id,
            title,
            note,
            tags,
        } => {
            let mut edit = Edit {
                title,
                note,
                ..Default::default()
            };
            for t in tags {
                if let Some(t) = t.strip_prefix('-') {
                    edit.remove_tags.push(t.to_string());
                } else {
                    edit.add_tags
                        .push(t.strip_prefix('+').unwrap_or(&t).to_string());
                }
            }
            if edit.title.is_none()
                && edit.note.is_none()
                && edit.add_tags.is_empty()
                && edit.remove_tags.is_empty()
            {
                bail!("nothing to change; pass --title, --note, or -t");
            }
            if !store.edit(id, &edit)? {
                bail!("no bookmark with id {id}");
            }
            let b = get(&store, id)?;
            write!(out, "updated ")?;
            print_line(&mut out, &b, 0, &style)?;
        }
        Cmd::Rm { targets } => {
            let mut failed = false;
            for t in &targets {
                let id = match t.parse::<i64>() {
                    Ok(id) => Some(id),
                    Err(_) => store.find_by_url(t)?.map(|b| b.id),
                };
                match id {
                    Some(id) if store.delete(id)? => writeln!(out, "deleted {id}")?,
                    _ => {
                        out.flush()?;
                        eprintln!("shlv: no bookmark {t}");
                        failed = true;
                    }
                }
            }
            if failed {
                out.flush()?;
                return Ok(ExitCode::FAILURE);
            }
        }
        Cmd::Open { id } => open(&get(&store, id)?.url)?,
        Cmd::Tag { cmd } => run_tag(&mut store, cmd, &mut out, &style)?,
        Cmd::Config { .. } => unreachable!("handled before opening the database"),
    }
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}

fn run_config(cmd: ConfigCmd, out: &mut impl Write) -> Result<()> {
    let path = config::default_path()?;
    match cmd {
        ConfigCmd::Path => writeln!(out, "{}", path.display())?,
        ConfigCmd::Show => {
            let config = Config::load(&path)?;
            let status = if path.exists() {
                ""
            } else {
                " (not found; defaults)"
            };
            writeln!(out, "# {}{status}", path.display())?;
            for s in config.ignored_sections() {
                writeln!(out, "# [{s}] ignored: not in this build")?;
            }
            out.write_all(config.to_toml().as_bytes())?;
        }
    }
    Ok(())
}

fn run_tag(store: &mut Store, cmd: TagCmd, out: &mut impl Write, style: &Style) -> Result<()> {
    match cmd {
        TagCmd::Ls { sort, json } => {
            let sort = match sort {
                SortArg::Name => TagSort::Name,
                SortArg::Count => TagSort::Count,
            };
            let tags = store.tags(sort)?;
            let width = tags.iter().map(|t| t.count).max().unwrap_or(0);
            let width = width.to_string().len();
            for t in &tags {
                if json {
                    serde_json::to_writer(&mut *out, t)?;
                    writeln!(out)?;
                } else {
                    print_tag(out, t, width, style)?;
                }
            }
        }
        TagCmd::Rename { old, new } => {
            let r = store.rename_tag(&old, &new)?;
            let n = plural(r.bookmarks, "bookmark");
            writeln!(out, "renamed {} to {} ({n})", normalize_tag(&old), r.tag)?;
        }
        TagCmd::Merge { from, into } => {
            let r = store.merge_tags(&from, &into)?;
            let (from, n) = (normalize_tag(&from), plural(r.bookmarks, "bookmark"));
            writeln!(
                out,
                "merged {from} into {} ({n}); {from} is now an alias",
                r.tag
            )?;
        }
        TagCmd::Alias {
            alias,
            tag: None,
            rm: true,
        } => {
            let alias = normalize_tag(&alias);
            match store.remove_alias(&alias)? {
                Some(tag) => writeln!(out, "removed alias {alias} (of {tag})")?,
                None => bail!("no alias {alias:?}"),
            }
        }
        TagCmd::Alias {
            alias,
            tag: Some(tag),
            rm: false,
        } => {
            let added = store.add_alias(&alias, &tag)?;
            let (alias, tag) = (normalize_tag(&alias), normalize_tag(&tag));
            if added {
                writeln!(out, "added alias {alias} for {tag}")?;
            } else {
                writeln!(out, "{alias} is already an alias of {tag}")?;
            }
        }
        TagCmd::Alias { .. } => unreachable!("clap requires TAG unless --rm, and not both"),
        TagCmd::Canonical { cmd } => match cmd {
            CanonicalCmd::Add { tags } => {
                let n = store.add_canonical(&tags)?;
                writeln!(out, "marked {} canonical", plural(n, "tag"))?;
            }
            CanonicalCmd::Rm { tags } => {
                let n = store.remove_canonical(&tags)?;
                writeln!(out, "unmarked {}", plural(n, "tag"))?;
            }
            CanonicalCmd::Ls => {
                for t in store.canonical_tags()? {
                    writeln!(out, "{}", t.name)?;
                }
            }
            CanonicalCmd::Import { file } => {
                let mut text = String::new();
                if file.as_os_str() == "-" {
                    io::stdin().read_to_string(&mut text)?;
                } else {
                    text = std::fs::read_to_string(&file)
                        .with_context(|| format!("cannot read {}", file.display()))?;
                }
                let s = store
                    .import_canonical(&parse_canonical(&text))
                    .with_context(|| format!("cannot import {}", file.display()))?;
                writeln!(
                    out,
                    "imported {}: {} newly canonical, {} added",
                    plural(s.tags, "tag"),
                    s.marked,
                    plural(s.aliases, "alias"),
                )?;
            }
            CanonicalCmd::Export => {
                out.write_all(format_canonical(&store.canonical_tags()?).as_bytes())?
            }
        },
    }
    Ok(())
}

/// One tag: count, canonical marker, name, aliases.
fn print_tag(out: &mut impl Write, t: &TagInfo, width: usize, s: &Style) -> io::Result<()> {
    let mark = if t.canonical { '*' } else { ' ' };
    write!(
        out,
        "{}{:>width$}{} {mark} {}{}{}",
        s.dim, t.count, s.reset, s.tag, t.name, s.reset
    )?;
    if !t.aliases.is_empty() {
        write!(out, "  {}= {}{}", s.dim, t.aliases.join(" "), s.reset)?;
    }
    writeln!(out)
}

fn plural(n: usize, word: &str) -> String {
    match (n, word.ends_with('s')) {
        (1, _) => format!("1 {word}"),
        (_, true) => format!("{n} {word}es"),
        _ => format!("{n} {word}s"),
    }
}

/// Width of the id column: the widest id the database has handed out, so rows align without
/// collecting them first.
fn id_width(store: &Store) -> Result<usize> {
    Ok(store.max_id()?.to_string().len())
}

fn get(store: &Store, id: i64) -> Result<Bookmark> {
    store
        .get(id)?
        .ok_or_else(|| anyhow!("no bookmark with id {id}"))
}

/// ANSI styles; all empty when color is off.
struct Style {
    dim: &'static str,
    id: &'static str,
    tag: &'static str,
    reset: &'static str,
}

impl Style {
    fn detect(tty: bool) -> Self {
        let color = tty && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
        if color {
            Style {
                dim: "\x1b[2m",
                id: "\x1b[33m",
                tag: "\x1b[36m",
                reset: "\x1b[0m",
            }
        } else {
            Style {
                dim: "",
                id: "",
                tag: "",
                reset: "",
            }
        }
    }
}

/// Prints one listed bookmark: a JSON line, or a human line with the id padded to `width`.
fn print_item(
    out: &mut impl Write,
    b: &Bookmark,
    json: bool,
    width: usize,
    style: &Style,
) -> Result<()> {
    if json {
        serde_json::to_writer(&mut *out, b)?;
        writeln!(out)?;
    } else {
        print_line(out, b, width, style)?;
    }
    Ok(())
}

/// One line: id, title, URL, tags. Title and note are flattened onto the line.
fn print_line(out: &mut impl Write, b: &Bookmark, width: usize, s: &Style) -> io::Result<()> {
    write!(out, "{}{:>width$}{}  ", s.id, b.id, s.reset)?;
    if let Some(title) = &b.title {
        write!(out, "{}  ", one_line(title))?;
    }
    write!(out, "{}{}{}", s.dim, b.url, s.reset)?;
    for t in &b.tags {
        write!(out, "  {}#{t}{}", s.tag, s.reset)?;
    }
    writeln!(out)
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn print_full(out: &mut impl Write, b: &Bookmark) -> io::Result<()> {
    writeln!(out, "id:       {}", b.id)?;
    writeln!(out, "url:      {}", b.url)?;
    if let Some(t) = &b.title {
        writeln!(out, "title:    {t}")?;
    }
    if !b.tags.is_empty() {
        writeln!(out, "tags:     {}", b.tags.join(" "))?;
    }
    writeln!(out, "created:  {}", format_time(b.created_at))?;
    writeln!(out, "updated:  {}", format_time(b.updated_at))?;
    writeln!(out, "uid:      {}", b.uid)?;
    if let Some(n) = &b.note {
        writeln!(out, "\n{n}")?;
    }
    if let Some(s) = &b.summary {
        writeln!(out, "\nSummary:\n{s}")?;
    }
    Ok(())
}

/// Formats Unix milliseconds as `YYYY-MM-DD HH:MM UTC`.
fn format_time(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days, from Howard Hinnant's date algorithms.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60
    )
}

/// Opens `url` with `$BROWSER` (first entry of a `:`-separated list; `%s` is replaced by the
/// URL), else `xdg-open`, or `open` on macOS.
fn open(url: &str) -> Result<()> {
    let browser = std::env::var("BROWSER").unwrap_or_default();
    let browser = browser.split(':').next().unwrap_or("").trim();
    let (prog, args): (String, Vec<String>) = if browser.is_empty() {
        let prog = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        (prog.into(), vec![url.into()])
    } else {
        let mut words = browser.split_whitespace().map(str::to_string);
        let prog = words.next().unwrap_or_default();
        let mut args: Vec<String> = words.collect();
        if args.iter().any(|a| a.contains("%s")) {
            args = args.iter().map(|a| a.replace("%s", url)).collect();
        } else {
            args.push(url.into());
        }
        (prog, args)
    };
    // Wait, so terminal browsers keep the terminal and failures are reported.
    let status = Command::new(&prog)
        .args(&args)
        .status()
        .with_context(|| format!("cannot run {prog}"))?;
    if !status.success() {
        bail!("{prog} failed ({status})");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_times() {
        assert_eq!(format_time(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_time(951_782_400_000), "2000-02-29 00:00 UTC");
        assert_eq!(format_time(1_794_717_000_000), "2026-11-15 04:30 UTC");
        assert_eq!(format_time(-60_000), "1969-12-31 23:59 UTC");
    }
}
