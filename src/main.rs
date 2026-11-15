use std::io::{self, BufWriter, IsTerminal, Write};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use shlv::{Bookmark, Edit, ListQuery, NewBookmark, Store};

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
    let mut store = Store::open_default()?;
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
    }
    out.flush()?;
    Ok(ExitCode::SUCCESS)
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
