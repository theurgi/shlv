# The TUI

`shlv` with no arguments on a terminal opens the TUI: browse, search, open, add, edit, tag, and
delete bookmarks from the keyboard. With a subcommand, or when stdin or stdout isn't a terminal,
`shlv` is the CLI as before (bare `shlv` without a terminal prints the usage and exits with 2).

The TUI is built by default. It reads the same config and database as the CLI and goes through the
same code, so strict tags, pinned tags, proposals, and domain rules apply the same way. The keys are
a first proposal and may change.

## Screen

The top line lists the views: Bookmarks, Review, Tags, Rules, Bootstrap, Status; `1`-`6` switch
between them. The bar at the bottom shows the view, the search, the position in the list, marked
bookmarks, and running background jobs; the line below it is for messages (until the next key),
prompts, and the keys of what you are in the middle of.

## Everywhere

| Key         | Action                                                          |
| ----------- | --------------------------------------------------------------- |
| `1`-`6`     | Switch view                                                     |
| `:`         | Command line: a view name (`:review`, `:t`) or an action, below |
| `?`         | The current view's keys (any key closes them)                   |
| `q`, Ctrl-C | Quit (Ctrl-C works from anywhere, a form included)              |

The **command line** takes a view's name (or the start of one) and the views' actions; `Tab`
completes command names, then subcommands, then tag names (or interests, or rule hosts):

| Command                                        | Action                                       |
| ---------------------------------------------- | -------------------------------------------- |
| `:search QUERY`                                | Show a search in Bookmarks                   |
| `:review`                                      | Start a review batch                         |
| `:tag rename A B`, `merge A B`                 | As `shlv tag`                                |
| `:tag alias ALIAS TAG`, `unalias ALIAS`        | Add or remove an alias                       |
| `:tag canonical TAG...`, `uncanonical TAG...`  | Mark or unmark canonical                     |
| `:tag describe TAG TEXT`                       | Describe a tag                               |
| `:rule add HOST TAG...`, `:rule rm HOST [TAG]` | As `shlv rule`                               |
| `:interest add NAME...`, `rm NAME...`          | Save or forget interests                     |
| `:seed [INTEREST...]`, `:draft [N]`            | `tag seed`, `tag draft` (feature `ai`)       |
| `:fetch`                                       | Fetch titles for bookmarks without one       |
| `:summarize`                                   | Summarize bookmarks without a summary (asks) |
| `:train`, `:eval`                              | Retrain or measure the classifier (`model`)  |
| `:jobs`, `:cancel [N]`                         | The Status view; cancel job N, or every job  |
| `:q`, `:help`                                  | Quit, the keys                               |

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

## Review

`shlv review` in the TUI (see [config](config.md#review)): batches of bookmarks, one key per
decision, and the queue of pending suggestions (`Tab` switches between them).

**Batches** are picked and their suggestions made on the worker (domain rules, the classifier, the
`[ai] tag` provider), as `shlv review` does: bookmarks not reviewed yet, spread across sites until
the classifier has 100 reviewed bookmarks to learn from, then the ones it is least sure about. `f`
sets filters (a suggested tag, its source, a confidence it must be below) and the batch size; with a
filter, batches hold bookmarks with matching suggestions, reviewed or not.

| Key          | In a batch                                                         |
| ------------ | ------------------------------------------------------------------ |
| `Enter`, `y` | Save: the checked tags; unchecked suggestions are rejected         |
| `1`-`9`      | Check or uncheck a tag (`j` `k` and `Space` too)                   |
| `t`, `+`     | Add a tag; `Tab` completes; a name off the list is proposed for it |
| `s`, `n`     | Skip the bookmark for now                                          |
| `o`          | Open it                                                            |
| `dd`         | Delete it (`u` in Bookmarks brings it back)                        |
| `R`          | Create the rule offered for its site                               |
| `q`, `Esc`   | End the batch here; its names, back-fill, and retrain still follow |

Digits toggle tags in a batch, so switch views with `:` there (`:bookmarks`). A bookmark saved
without tags asks for `Enter` again. After a save, if at least 3 of the bookmarks you tagged on its
site (80% of them) share a tag that no rule gives, the screen offers that rule; `R` creates it and
runs it on the site's bookmarks, including the one on screen.

After the bookmarks come the **names proposed** in the batch (`a` add to the canonical list, `l`
alias of a canonical tag, `r` reject, `s` skip), then **back-fill** for each tag just added: the
worker offers it on reviewed bookmarks (classifier or provider), and `y` `n` `s` decide each
candidate. Then the classifier retrains on the worker, and the batch joins the history shown before
the next one.

The **queue** lists pending suggestions, newest bookmark first: `y` `n` accept or reject one, `Y`
`N` all of a bookmark's, `f` cycles the source (rule, model, llm), `<` `>` lower or raise the
confidence they must be below, `o` opens. It shows at most 5000.

## Tags

The tag list and what `shlv tag` does to it, in three tabs (`Tab` cycles them).

**List:** every tag with its bookmark count, `*` for canonical tags, `pinned` for tags the config
declares, `=` its aliases, `#` its description. `/` filters as you type (names and aliases); `s`
sorts by name or count.

| Key     | Action                                                                  |
| ------- | ----------------------------------------------------------------------- |
| `Enter` | The tag's bookmarks, in Bookmarks                                       |
| `r`     | Rename                                                                  |
| `m`     | Merge into another tag (`Tab` completes); the old name becomes an alias |
| `a` `A` | Add an alias, remove one                                                |
| `e`     | Describe: what belongs under the tag, for the tagging layers            |
| `c`     | Mark or unmark canonical                                                |
| `b`     | Back-fill: offer the tag on every bookmark, then `y` `n` `s` each one   |

Edits the config pins (renaming a declared tag, say) are refused with the reason on the message
line. Back-fill runs the classifier and the `[ai] tag` provider on the worker, as
`shlv suggest --tag TAG --all --pick` does.

**Names proposed:** `a` adds a name to the canonical list and tags its bookmarks, `l` makes it an
alias of a tag, `r` rejects it for good.

**Health** measures the list on the worker (as `shlv tag health`) and puts each finding's fix one
key away: `m` (or `M`, the other way) merges a redundant pair, and `m` a plural into its singular;
`c` unmarks a too broad or rare tag; `b` back-fills a rare one; `t` tags a group of uncovered
bookmarks (by site or title word); `r` renames a badly named tag. `Enter` shows the bookmarks behind
a finding. After a fix, health measures again.

## Rules

Domain rules (see [config](config.md)): each host and tag, how many bookmarks the rule tagged or
suggested the tag for (pending ones in parentheses), and `pinned` for rules from `[tags.rules]` in
the config. Below them, **offered** rules: sites where you have tagged at least 3 bookmarks alike,
and that tag is on 80% of the site's tagged bookmarks. Subdomains count toward their site when more
than one of them does (`a.bandcamp.com` and `b.bandcamp.com` make `bandcamp.com`).

| Key      | Action                                                             |
| -------- | ------------------------------------------------------------------ |
| `j` `k`  | Down, up (`g` `G`: top, bottom)                                    |
| `Enter`  | The site's bookmarks with the tag, in Bookmarks (`site:host #tag`) |
| `a`      | Add a rule: a host (or a URL on it) and tags; `Tab` completes tags |
| `dd`     | Remove a rule and the pending suggestions only it made             |
| `y`      | Create the offered rule                                            |
| `n`      | Decline the offered rule; it isn't offered again                   |
| `Ctrl-l` | Reload                                                             |

A new rule runs on the bookmarks already on its site, so they get its tags as suggestions (or as
tags, with `suggestions = "auto"`). Rules pinned in config can't be removed here.

## Bootstrap

**Interests** are broad topics you bookmark (`nix`, `home-lab`): `a` saves some, `dd` forgets one
(those in `[tags] interests` in the config are pinned and stay). With the `ai` feature, `s` runs
`tag seed` on them (the `[ai] tag` provider expands them into candidate tags) and `D` runs
`tag draft` (it drafts tags from a sample of bookmarks, 200 by default). Both first show exactly
what is sent and where, and send only on `y`.

The **candidates** come back grouped by interest, with those already on the list shown as such:

| Key          | Action                                                     |
| ------------ | ---------------------------------------------------------- |
| `y`, `Enter` | Keep: put it on the list                                   |
| `d`          | Drop                                                       |
| `r`          | Rename, then keep                                          |
| `m`          | Merge into a tag or another candidate; it becomes an alias |
| `e`          | Edit the description                                       |
| `a`          | Add interests and expand them too (seed)                   |
| `w`          | Write the decisions (undecided ones are left out)          |
| `Esc` `Esc`  | Discard them                                               |

A tag `tag draft` finds redundant shows what it could merge into; `m` then `Enter` merges it there.

## Status

Where the config and database are, the classifier (`model status`: when it was trained, on how many
examples), its measurements once `e` runs `model eval`, and the background jobs. `t` retrains the
classifier, `F` fetches missing titles, `S` summarizes bookmarks without a summary (after saying
what is sent), `x` cancels the selected job and `X` every one.

## Background jobs

Slow work runs on a worker thread with its own database connection, one job at a time: fetching
titles and summarizing (one bookmark or all that lack one), picking review batches and running the
suggestion layers on them, back-fill, tag health, training and measuring the classifier, and seed
and draft requests. The bar shows the current step (`⟳ 2/5 fetching https://…`) and the TUI stays
usable; when a job ends, the view that started it gets the result and the message line says how it
went. A cancelled job stops before it starts, or (fetching or summarizing many) after the bookmark
it is on; what it did so far stays. The worker starts with the first job, so a session without one
doesn't pay for it.

## Performance

On the 100k benchmark library (release build, binary and database on tmpfs): the first screen is up
about 11 ms after start, over the cost of the terminal itself (`script` running `shlv` and `q`: 37.5
ms, running `true`: 26.1 ms). Listing keeps only ids (100k in 6 ms) and reads the rows on screen. A
search key costs what `shlv search` does over all results: 6 ms for `rust async`, 50 ms for `rust`
(51k hits), 80 ms for `r`; keys typed faster than that are handled together, with one search. The
Rules view lists rules in about 6 ms; spotting rules to offer reads every tag of the library, about
95 ms, when the view is first shown and after changes.
