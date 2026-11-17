# Import and export

`import` reads Netscape HTML (what browsers export), JSON lines (what `export` writes), and plain
lists of URLs, detecting the format from the content (`--format` overrides it). It treats URLs
already saved the way `add` does: it updates the bookmark and merges tags. It keeps creation times,
titles, tags, and notes, and skips `javascript:` and `place:` URLs. Browser folders become tags only
with `--folders-as-tags`. With `strict_tags`, an import with non-canonical tags writes nothing
unless you pass `--drop-unknown-tags`, or `--propose-unknown-tags` to keep them as
[proposals](config.md#proposals) for the list (always the case with `strict_tags = "propose"`). Tags
are fixed to the [tag style](config.md#tag-style); tags it refuses (too long) are dropped and
reported. `export` then `import` (JSON) reproduces a database, uids included. Domain rules run on
imported bookmarks as they do on `add`; `--no-rules` skips them. A title that is only the bookmark's
URL is dropped, so `shlv fetch --all` can find a real one.

A URL list has one URL per line, in any form `add` takes (`lwn.net` works). Blank lines and lines
starting with `#` are skipped. A line with spaces in it stops the import with its line number, and
nothing is written. Bookmarks from a list are added now, with no title until `shlv fetch --all`.

## Moving from another app

Export its bookmarks as HTML, or extract the URLs into a list. To leave the other app's tags, notes,
and summaries behind, `import --urls-only` keeps only each bookmark's URL, add date, and title
(`--no-titles` drops titles too). Titles stay by default: they're page titles rather than another
app's opinions, search finds bookmarks by them before any `fetch`, and pages that have gone since
were saved still have one. URLs already saved merge as usual and keep their own tags and notes.

From Karakeep (Settings → Import / export), either:

- **The HTML export**, with `import --urls-only`. It keeps titles and dates, but Karakeep escapes
  `%` in URLs again, so `https://example.org/caf%C3%A9` comes out as `…/caf%25C3%25A9`, a different
  page. Its dates are when a bookmark was last saved, not first.
- **The URLs from its JSON export**, which keeps them as they are, without titles or dates:
  `jq -r '.bookmarks[].content.url // empty' karakeep.json > urls.txt`. Text notes and uploaded
  files have no URL and are left out.

```sh
shlv import urls.txt
shlv fetch --all     # titles for bookmarks without one (`fetch` feature)
shlv suggest --all   # the classifier or [ai] tag provider; the rules ran on import
```
