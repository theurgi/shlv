//! `shlv edit` (task 036): one bookmark, several, or a list on stdin, optionally picked one key
//! each.

use std::io::{self, Write};
use std::process::ExitCode;

use anyhow::{Result, bail};
use shlv::{Bookmark, Edit, Edited, MissingBookmarks, Store};

use crate::screen::{Key, Screen};
use crate::{Style, id_width, plural, print_diverted, print_line, read_input};

#[derive(clap::Args)]
pub struct EditArgs {
    /// Bookmarks to change; "-" reads them from stdin: the output of `search` or `ls` (plain or
    /// --json), or one id per line
    #[arg(required = true, value_name = "ID|-")]
    ids: Vec<String>,
    /// New title ("" clears it; one bookmark only)
    #[arg(long)]
    title: Option<String>,
    /// New note ("" clears it)
    #[arg(long)]
    note: Option<String>,
    /// +TAG adds, -TAG removes (repeatable); a bare TAG adds
    #[arg(short, long = "tag", value_name = "±TAG", allow_hyphen_values = true)]
    tags: Vec<String>,
    /// Choose which of the bookmarks to change, one key each
    #[arg(long)]
    pick: bool,
}

/// `shlv edit`.
pub fn run_edit(
    store: &mut Store,
    args: EditArgs,
    out: &mut impl Write,
    style: &Style,
) -> Result<ExitCode> {
    let mut edit = Edit {
        title: args.title,
        note: args.note,
        ..Default::default()
    };
    for t in args.tags {
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
    let mut ids = Vec::new();
    let mut piped = false;
    for a in &args.ids {
        if a == "-" {
            if piped {
                bail!("\"-\" given twice; stdin can be read once");
            }
            piped = true;
            ids.extend(parse_ids(&read_input("-".as_ref())?)?);
        } else {
            match a.parse::<i64>() {
                Ok(id) => ids.push(id),
                Err(_) => bail!("{a:?} isn't a bookmark id"),
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    ids.retain(|&id| seen.insert(id));
    if edit.title.is_some() && ids.len() > 1 {
        bail!(
            "--title would give {} bookmarks one title; edit one at a time",
            ids.len()
        );
    }
    if ids.is_empty() {
        out.flush()?;
        eprintln!("shlv: no bookmark ids on stdin; nothing changed");
        return Ok(ExitCode::SUCCESS);
    }
    if args.pick {
        let mut screen = if piped {
            Screen::open_tty().map_err(|e| {
                anyhow::anyhow!("--pick reads keys from the terminal (stdin carries ids): {e}")
            })?
        } else {
            Screen::open()
        };
        let marks = describe_edit(&edit, &screen.style);
        let Some(chosen) = pick(&mut screen, store, &ids, &marks)? else {
            writeln!(screen, "nothing changed")?;
            return Ok(ExitCode::SUCCESS);
        };
        ids = chosen;
        if ids.is_empty() {
            writeln!(screen, "none picked; nothing changed")?;
            return Ok(ExitCode::SUCCESS);
        }
    }
    // Fails with MissingBookmarks (changing nothing) if an id is gone.
    let edited = store.edit_many(&ids, &edit)?;
    if let [id] = ids[..] {
        let b = store.get(id)?.ok_or(MissingBookmarks(vec![id]))?;
        write!(out, "updated ")?;
        print_line(out, &b, 0, style)?;
    } else {
        print_summary(out, &edited, style)?;
    }
    print_diverted(out, &edited.diverted, style)?;
    Ok(ExitCode::SUCCESS)
}

/// Bookmark ids from text: per non-empty line, a JSON object's `id` (`--json` output), or a
/// leading id (`search` and `ls` output, `jq .id`). Anything else is an error, so a stray line
/// never edits the wrong bookmark.
pub fn parse_ids(text: &str) -> Result<Vec<i64>> {
    #[derive(serde::Deserialize)]
    struct Line {
        id: i64,
    }
    let mut ids = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let id = if line.starts_with('{') {
            serde_json::from_str::<Line>(line).ok().map(|l| l.id)
        } else {
            line.split_whitespace().next().and_then(|t| t.parse().ok())
        };
        match id {
            Some(id) => ids.push(id),
            None => bail!(
                "stdin line {}: no bookmark id at its start: {}",
                n + 1,
                cut(line, 60)
            ),
        }
    }
    Ok(ids)
}

/// `+#a -#b`, for the pick screen's header.
fn describe_edit(edit: &Edit, s: &Style) -> String {
    let mut parts = Vec::new();
    for t in &edit.add_tags {
        parts.push(format!("+{}#{t}{}", s.tag, s.reset));
    }
    for t in &edit.remove_tags {
        parts.push(format!("-{}#{t}{}", s.tag, s.reset));
    }
    if edit.note.is_some() {
        parts.push("the note".into());
    }
    if edit.title.is_some() {
        parts.push("the title".into());
    }
    parts.join(" ")
}

const HELP: &str = "\
  y, Enter  pick this bookmark
  n         skip it
  a         pick this one and the rest
  b         back to the previous one
  q         done: change the ones picked so far
  Ctrl-C    quit without changing anything";

/// Asks about each bookmark; the picked ids in order, or `None` if the user quit.
fn pick(screen: &mut Screen, store: &Store, ids: &[i64], marks: &str) -> Result<Option<Vec<i64>>> {
    let mut missing = Vec::new();
    let mut books = Vec::new();
    for &id in ids {
        match store.get(id)? {
            Some(b) => books.push(b),
            None => missing.push(id),
        }
    }
    if !missing.is_empty() {
        return Err(MissingBookmarks(missing).into());
    }
    let width = id_width(store)?;
    let keys = screen.dim("y pick  n skip  a pick the rest  b back  q done  ? help");
    writeln!(
        screen,
        "{marks} on which of {}?\n{keys}",
        plural(books.len(), "bookmark")
    )?;
    let mut picked: Vec<Option<bool>> = vec![None; books.len()];
    let mut i = 0;
    while i < books.len() {
        let line = row(screen, &books[i], width);
        let q = screen.dim("?");
        write!(screen, "{line}  {q} ")?;
        let decided = match screen.key()? {
            Key::Enter | Key::Char('y') => Some(true),
            Key::Char('n') => Some(false),
            Key::Char('a') => {
                for p in &mut picked[i..] {
                    p.get_or_insert(true);
                }
                screen.redraw()?;
                let rest = books.len() - i;
                writeln!(screen, "{line}  y, and the {} after it", rest - 1)?;
                break;
            }
            Key::Char('b') => {
                screen.redraw()?;
                i = i.saturating_sub(1);
                continue;
            }
            Key::Char('q') | Key::Eof => {
                screen.redraw()?;
                break;
            }
            Key::Interrupt => {
                screen.redraw()?;
                return Ok(None);
            }
            Key::Char('?') => {
                screen.redraw()?;
                writeln!(screen, "{HELP}")?;
                None
            }
            Key::Char(c) => {
                screen.redraw()?;
                writeln!(screen, "{c:?} isn't a key here; ? lists them")?;
                None
            }
        };
        if let Some(p) = decided {
            picked[i] = Some(p);
            screen.redraw()?;
            writeln!(screen, "{line}  {}", if p { "y" } else { "n" })?;
            i += 1;
        }
    }
    Ok(Some(
        books
            .iter()
            .zip(&picked)
            .filter(|(_, p)| **p == Some(true))
            .map(|(b, _)| b.id)
            .collect(),
    ))
}

/// One bookmark on one line, cut to fit the terminal so it can be redrawn in place.
fn row(screen: &Screen, b: &Bookmark, width: usize) -> String {
    let s = &screen.style;
    let mut text = String::new();
    if let Some(t) = &b.title {
        text.push_str(&t.split_whitespace().collect::<Vec<_>>().join(" "));
        text.push_str("  ");
    }
    text.push_str(&b.url);
    for t in &b.tags {
        text.push_str("  #");
        text.push_str(t);
    }
    let room = screen.width().saturating_sub(width + 6).max(20);
    format!("{}{:>width$}{}  {}", s.id, b.id, s.reset, cut(&text, room))
}

/// `text` cut to `n` characters, with an ellipsis if it was longer.
fn cut(text: &str, n: usize) -> String {
    match text.char_indices().nth(n.saturating_sub(1)) {
        Some((at, _)) if text.chars().count() > n => format!("{}…", &text[..at]),
        _ => text.to_string(),
    }
}

/// `updated 12 bookmarks  +#wifi 10  -#old 3`, and how many were already as asked.
fn print_summary(out: &mut impl Write, e: &Edited, s: &Style) -> io::Result<()> {
    write!(out, "updated {}", plural(e.changed, "bookmark"))?;
    for (t, n) in &e.added {
        write!(out, "  +{}#{t}{} {n}", s.tag, s.reset)?;
    }
    for (t, n) in &e.removed {
        write!(out, "  -{}#{t}{} {n}", s.tag, s.reset)?;
    }
    if e.unchanged > 0 {
        write!(out, "  {}({} already so){}", s.dim, e.unchanged, s.reset)?;
    }
    writeln!(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ids() {
        let text = "  12  Title  https://a.com  #x\n\n{\"id\":7,\"url\":\"b\"}\n3\n";
        assert_eq!(parse_ids(text).unwrap(), [12, 7, 3]);
        let err = parse_ids("1\nhttps://a.com\n").unwrap_err().to_string();
        assert!(err.contains("stdin line 2"), "{err}");
        assert!(parse_ids("{\"url\":\"a\"}").is_err());
        assert_eq!(cut("abcdef", 4), "abc…");
        assert_eq!(cut("abcd", 4), "abcd");
    }
}
