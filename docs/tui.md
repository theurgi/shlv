# The TUI

`shlv` with no arguments on a terminal opens the TUI: browse, search, open, add, edit, tag, and
delete bookmarks from the keyboard. With a subcommand, or when stdin or stdout isn't a terminal,
`shlv` is the CLI as before (bare `shlv` without a terminal prints the usage and exits with 2).

The TUI is built by default. It reads the same config and database as the CLI and goes through the
same code, so strict tags, pinned tags, proposals, and domain rules apply the same way. The keys are
a first proposal and may change.

## Screen

The top line lists the views; `1`-`6` switch between them. Only Bookmarks is built so far; Review,
Tags, Rules, Bootstrap, and Status are placeholders. The bar at the bottom shows the view, the
search, the position in the list, marked bookmarks, and running background jobs; the line below it
is for messages and prompts.

## Everywhere

| Key         | Action                                                     |
| ----------- | ---------------------------------------------------------- |
| `1`-`6`     | Switch view                                                |
| `:`         | Command line: `:q`, `:help`, a view name (`:review`, `:t`) |
| `?`         | The current view's keys (any key closes them)              |
| `q`, Ctrl-C | Quit (Ctrl-C works from anywhere, a form included)         |

## Bookmarks

The whole library, newest first, with the current bookmark in a detail pane: URL, dates, tags with
the layer that applied each (`rule`, `model`, `llm`, and its confidence; user tags are plain),
pending suggestions, names proposed for the list, note, and summary.

| Key               | Action                                                                 |
| ----------------- | ---------------------------------------------------------------------- |
| `j` `k`, `↓` `↑`  | Down, up                                                               |
| `g` `G`           | Top, bottom                                                            |
| `Ctrl-d` `Ctrl-u` | Half a page down, up (`Ctrl-f` `Ctrl-b`, PageDown, PageUp: a page)     |
| `/`               | Search as you type; `Enter` keeps the search, `Esc` goes back          |
| `Enter`, `o`      | Open in the browser                                                    |
| `Tab`             | Toggle the detail pane                                                 |
| `a`               | Add a bookmark: URL, title, tags                                       |
| `e`               | Edit the title and note                                                |
| `t`               | Edit the tags; `Tab` completes                                         |
| `dd`              | Delete                                                                 |
| `u`               | Undo the last delete (as many as were made this session)               |
| `v`               | Mark the bookmark and move down; `t`, `dd`, `y`, `n` act on all marked |
| `y` `n`           | Accept, reject every pending suggestion                                |
| `s`               | Go through the suggestions one by one: `y`, `n`, `j` `k`, `Esc`        |
| `F`               | Fetch the title (`fetch` feature)                                      |
| `S`               | Summarize with the `[ai] summarize` provider (`ai` feature)            |
| `Ctrl-l`          | Reload, after changes made elsewhere (the CLI, another TUI)            |
| `Esc`             | Clear the marks, then the search                                       |

**Search** uses the CLI's query language (`#tag`, `tag:x`, `site:host`, `"phrases"`, `AND`, `OR`,
`NOT`; see `shlv search --help`) and ranks the same way. Results update with each key; a query that
doesn't parse yet (a trailing `OR`) keeps the last results and says why on the bar.

**Adding** runs the domain rules and the classifier as `shlv add` does, and says what they
suggested; `y` accepts it. Pasting while the list has focus starts adding the pasted URL. With the
`fetch` feature, a bookmark added without a title gets one fetched in the background.

**Editing tags** of one bookmark shows its tags as text: edit the list, and tags left out are
removed (`-tag` removes one too). With bookmarks marked, the text is a change instead: `tag` or
`+tag` adds, `-tag` removes. `Tab` completes a tag name or alias, most used first; more `Tab`s cycle
through the candidates. A tag that strict mode refuses leaves the form open with the error.

**Forms** (`a`, `e`, `t`): `Enter` saves, `Esc` cancels, `↑` `↓` move between fields (`Tab` too,
except in a tag field), `Alt-Enter` starts a new line in the note. Text fields take the usual
`Ctrl-a`, `Ctrl-e`, `Ctrl-u`, `Ctrl-k`, `Ctrl-w`.

**Undo** brings a deleted bookmark back with its id, fields, tags (and their sources), and
suggestions. Proposals it carried and its review history aren't kept. Undo lasts for the session.

**Opening** runs `$BROWSER` or the system opener as `shlv open` does, with the TUI suspended until
it returns, so a terminal browser gets the terminal. A browser command that stays running until the
browser closes keeps the TUI waiting too; point `$BROWSER` at one that returns (`xdg-open`,
`firefox` with a window already open).

## Background jobs

Fetching titles and summarizing run on a worker thread with its own database connection, one job at
a time; the bar shows the current step (`⟳ 2/5 fetching https://…`) and the TUI stays usable. When a
job ends, its bookmark is redrawn and the message line says how it went. The worker starts with the
first job, so a session without one doesn't pay for it.

## Performance

On the 100k benchmark library (release build, binary and database on tmpfs): the first screen is up
about 11 ms after start, over the cost of the terminal itself (`script` running `shlv` and `q`: 37.5
ms, running `true`: 26.1 ms). Listing keeps only ids (100k in 6 ms) and reads the rows on screen. A
search key costs what `shlv search` does over all results: 6 ms for `rust async`, 50 ms for `rust`
(51k hits), 80 ms for `r`; keys typed faster than that are handled together, with one search.
