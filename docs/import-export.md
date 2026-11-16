# Import and export

`import` detects the format and treats URLs already saved the way `add` does: it updates the
bookmark and merges tags. It keeps creation times, titles, tags, and notes, and skips `javascript:`
and `place:` URLs. Browser folders become tags only with `--folders-as-tags`. With `strict_tags`, an
import with non-canonical tags writes nothing unless you pass `--drop-unknown-tags`, or
`--propose-unknown-tags` to keep them as [proposals](config.md#proposals) for the list (always the
case with `strict_tags = "propose"`). `export` then `import` (JSON) reproduces a database, uids
included. Domain rules run on imported bookmarks as they do on `add`; `--no-rules` skips them. A
title that is only the bookmark's URL is dropped, so `shlv fetch --all` can find a real one.

To move from another app and tag afresh, `import --urls-only` keeps each bookmark's URL, add date,
and title, and drops its tags, notes, and summaries (`--no-titles` drops titles too). Titles stay by
default: they're page titles rather than another app's opinions, search finds bookmarks by them
before any `fetch`, and pages that have gone since were saved still have one. URLs already saved
merge as usual and keep their own tags and notes.

From Karakeep, use its JSON export (Settings → Import / export, format "JSON"), which `import` reads
as is: link bookmarks come in, text notes and uploaded files don't. Avoid its HTML export: it
escapes `%` in URLs again, so `https://example.org/caf%C3%A9` comes out as `…/caf%25C3%25A9`, a
different page. Karakeep's dates are when a bookmark was last saved, not first.

```sh
shlv import --urls-only karakeep-export.json
shlv fetch --all     # titles for bookmarks without one (`fetch` feature)
shlv suggest --all   # the classifier or [ai] tag provider; the rules ran on import
```
