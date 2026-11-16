//! The Netscape bookmark file format (`<!DOCTYPE NETSCAPE-Bookmark-file-1>`), as browsers export
//! and import it. Hand-written: the format is loose HTML, and only a few tags matter.

use std::io::{self, Write};

use crate::html::decode;
use crate::html::{Tag, tag_end};
use crate::store::{Bookmark, normalize_tag};
use crate::transfer::ImportItem;

/// The doctype that marks a Netscape bookmark file.
pub const DOCTYPE: &str = "<!DOCTYPE NETSCAPE-Bookmark-file-1>";

/// Parses a Netscape bookmark file into import items, in document order.
///
/// Reads `<A HREF ADD_DATE TAGS>title</A>` and a following `<DD>` as the note. Tag names are
/// matched case-insensitively and closing tags are optional. With `folders_as_tags`, each
/// bookmark is tagged with the names of the `<H3>` folders that contain it, except the browser's
/// own root folders (toolbar, other bookmarks). URLs are not checked here; see
/// [`crate::Store::import`].
pub fn parse(text: &str, folders_as_tags: bool) -> Vec<ImportItem> {
    let mut items: Vec<ImportItem> = Vec::new();
    // One entry per open <DL>: the folder tag it stands for, if any.
    let mut folders: Vec<Option<String>> = Vec::new();
    // The <H3> just read, waiting for its <DL>.
    let mut pending_folder: Option<String> = None;
    // Whether a <DD> now describes the last bookmark (rather than a folder, or nothing).
    let mut dd_target = false;
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        rest = &rest[start..];
        if rest.starts_with("<!--") {
            rest = rest.find("-->").map_or("", |end| &rest[end + 3..]);
            continue;
        }
        let Some(end) = tag_end(rest) else { break };
        let tag = Tag::parse(&rest[1..end]);
        rest = &rest[end + 1..];
        let (body, after) = rest.split_at(rest.find('<').unwrap_or(rest.len()));
        match (tag.name, tag.closing) {
            (n, false) if n.eq_ignore_ascii_case("a") => {
                let Some(href) = tag.attr("href") else {
                    dd_target = false;
                    continue;
                };
                let tags = tag
                    .attr("tags")
                    .map(|t| decode(&t))
                    .into_iter()
                    .flat_map(|t| t.split(',').map(str::to_string).collect::<Vec<_>>())
                    .chain(folders.iter().flatten().cloned())
                    .filter(|t| !t.trim().is_empty())
                    .collect();
                items.push(ImportItem {
                    url: decode(&href).trim().to_string(),
                    title: non_empty(decode(body)),
                    created_at: tag.attr("add_date").and_then(|d| parse_date(&d)),
                    updated_at: tag.attr("last_modified").and_then(|d| parse_date(&d)),
                    tags,
                    ..Default::default()
                });
                dd_target = true;
                rest = after;
            }
            (n, false) if n.eq_ignore_ascii_case("dd") => {
                if dd_target && let Some(item) = items.last_mut() {
                    item.note = non_empty(decode(body));
                }
                dd_target = false;
                rest = after;
            }
            (n, false) if n.eq_ignore_ascii_case("h3") => {
                let root = tag.attr("personal_toolbar_folder").is_some()
                    || tag.attr("unfiled_bookmarks_folder").is_some();
                pending_folder = (folders_as_tags && !root)
                    .then(|| normalize_tag(&decode(body)))
                    .filter(|t| !t.is_empty());
                dd_target = false;
                rest = after;
            }
            (n, false) if n.eq_ignore_ascii_case("dl") => {
                folders.push(pending_folder.take());
                dd_target = false;
            }
            (n, true) if n.eq_ignore_ascii_case("dl") => {
                folders.pop();
                dd_target = false;
            }
            // <DT> starts the next entry; <HR> is a separator. Neither can have a <DD> after.
            (n, false) if n.eq_ignore_ascii_case("dt") || n.eq_ignore_ascii_case("hr") => {
                dd_target = false;
            }
            // </A>, </H3>, <p>, <META>, <TITLE>, ...: nothing to do.
            _ => {}
        }
    }
    items
}

/// `ADD_DATE` is in seconds, but some tools write milliseconds or microseconds. Values too large
/// for seconds (after the year 5000) are read as those. Returns Unix milliseconds; 0 and
/// unparsable dates are `None`.
fn parse_date(s: &str) -> Option<i64> {
    let n: i64 = s.trim().parse().ok().filter(|&n| n > 0)?;
    Some(match n {
        n if n >= 100_000_000_000_000 => n / 1000,
        n if n >= 100_000_000_000 => n,
        n => n * 1000,
    })
}

fn non_empty(s: String) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Escapes text for element content and double- or single-quoted attributes. Line breaks become
/// numeric references so a note stays on its `<DD>` line.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            c => out.push(c),
        }
    }
    out
}

/// Writes the file header and opens the bookmark list.
pub fn write_header(out: &mut impl Write) -> io::Result<()> {
    write!(
        out,
        "{DOCTYPE}\n\
         <!-- This is an automatically generated file.\n     \
         It will be read and overwritten.\n     \
         DO NOT EDIT! -->\n\
         <META HTTP-EQUIV=\"Content-Type\" CONTENT=\"text/html; charset=UTF-8\">\n\
         <TITLE>Bookmarks</TITLE>\n\
         <H1>Bookmarks</H1>\n\
         <DL><p>\n"
    )
}

/// Writes one bookmark: `<DT><A>` with times in seconds and comma-separated tags, then the note
/// as `<DD>`.
pub fn write_bookmark(out: &mut impl Write, b: &Bookmark) -> io::Result<()> {
    write!(
        out,
        "    <DT><A HREF=\"{}\" ADD_DATE=\"{}\" LAST_MODIFIED=\"{}\"",
        escape(&b.url),
        b.created_at.div_euclid(1000),
        b.updated_at.div_euclid(1000),
    )?;
    if !b.tags.is_empty() {
        write!(out, " TAGS=\"{}\"", escape(&b.tags.join(",")))?;
    }
    writeln!(out, ">{}</A>", escape(b.title.as_deref().unwrap_or("")))?;
    if let Some(note) = &b.note {
        writeln!(out, "    <DD>{}", escape(note))?;
    }
    Ok(())
}

/// Closes the bookmark list.
pub fn write_footer(out: &mut impl Write) -> io::Result<()> {
    writeln!(out, "</DL><p>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_entities() {
        assert_eq!(
            decode("a &amp; b &lt;c&gt; &quot;d&quot; &#39;e&#39; &#x263A; &#9731;"),
            "a & b <c> \"d\" 'e' ☺ ☃"
        );
        assert_eq!(decode("AT&T &bogus; & &#xZZ; &"), "AT&T &bogus; & &#xZZ; &");
        assert_eq!(decode(&escape("<a href='x'>&\n\"")), "<a href='x'>&\n\"");
    }

    #[test]
    fn dates_in_seconds_millis_or_micros() {
        assert_eq!(parse_date("1700000000"), Some(1_700_000_000_000));
        assert_eq!(parse_date("1700000000123"), Some(1_700_000_000_123));
        assert_eq!(parse_date("1700000000123456"), Some(1_700_000_000_123));
        assert_eq!(parse_date("0"), None);
        assert_eq!(parse_date("soon"), None);
    }

    #[test]
    fn loose_html() {
        // Lowercase tags, no closing tags, a `>` inside a quoted attribute, a comment with tags.
        let html = "<!doctype netscape-bookmark-file-1>\n<!-- <dt><a href=\"no\">x</a> -->\n\
            <dl><p>\n<dt><a href=\"https://a.example/?q=1>2\" add_date=\"10\">A &amp; B\n\
            <dd>note &lt;1&gt;\n  spanning lines\n<dt><h3>Folder</h3>\n<dd>folder note\n<dl><p>\
            <dt><a href='https://b.example/' tags=\"x, y,,\">B</a>\n</dl><p>\
            <dt><A HREF=\"https://c.example/\"></A>\n</dl>";
        let items = parse(html, true);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].url, "https://a.example/?q=1>2");
        assert_eq!(items[0].title.as_deref(), Some("A & B"));
        assert_eq!(items[0].created_at, Some(10_000));
        assert_eq!(items[0].note.as_deref(), Some("note <1>\n  spanning lines"));
        assert_eq!(items[1].tags, ["x", " y", "folder"]);
        assert_eq!(items[1].note, None);
        assert_eq!(items[2].title, None);
        assert!(items[2].tags.is_empty());
    }
}
