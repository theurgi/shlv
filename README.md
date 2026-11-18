# shlv

A fast, resource-efficient bookmark manager for the terminal. Bookmarks live in a single SQLite
database with full-text search. Everything works offline.

`shlv` on its own opens a keyboard-driven TUI: search as you type, open, add, edit, tag, and delete;
see [The TUI](docs/tui.md). The commands below are for scripts and integrations.

## Install

```sh
nix run github:theurgi/shlv -- --help
cargo install --git https://github.com/theurgi/shlv
```

The default build is offline. Page titles from the web, AI summaries and tags, and the tag
classifier are optional features; see [Install](docs/install.md) for them, the flake overlay, and
the Home Manager module.

## Usage

```console
$ shlv add https://www.sqlite.org/fts5.html -t sqlite -t search --title "SQLite FTS5"
added 1  SQLite FTS5  https://www.sqlite.org/fts5.html  #search  #sqlite
$ shlv add https://doc.rust-lang.org/book/ -t rust
added 2  https://doc.rust-lang.org/book/  #rust
$ shlv ls
2  https://doc.rust-lang.org/book/  #rust
1  SQLite FTS5  https://www.sqlite.org/fts5.html  #search  #sqlite
$ shlv search fts5
1  SQLite FTS5  https://www.sqlite.org/fts5.html  #search  #sqlite
```

| Command                        | What it does                                                          |
| ------------------------------ | --------------------------------------------------------------------- |
| `add URL [-t TAG]...`          | Add a bookmark, or update it if the URL is already saved              |
| `ls [-t TAG]... [-n N]`        | List bookmarks, newest first                                          |
| `search QUERY...`              | Words, `"phrases"`, `#tag`, `tag:x`, `site:host`, `AND/OR/NOT`        |
| `show ID`                      | Show one bookmark in full                                             |
| `edit ID...\|- [-t ±TAG]...`   | Change tags (or title, note) of bookmarks; `-` reads ids from stdin   |
| `rm ID\|URL...`                | Delete bookmarks                                                      |
| `open ID`                      | Open a bookmark with `$BROWSER` or the system opener                  |
| `tag ls [--sort count]`        | Tags with bookmark counts; `*` marks canonical, `=` lists aliases     |
| `tag rename OLD NEW`           | Rename a tag                                                          |
| `tag merge FROM INTO`          | Retag FROM's bookmarks as INTO; FROM becomes an alias                 |
| `tag alias ALIAS TAG`          | Make ALIAS resolve to TAG on input (`--rm ALIAS` removes it)          |
| `tag canonical add\|rm\|ls`    | Mark, unmark, or list canonical tags                                  |
| `tag canonical import FILE`    | Add a canonical list: one tag per line, aliases after it              |
| `tag canonical export`         | Print the canonical list in that format (`--toml`/`--nix`: config)    |
| `tag proposals [ls]`           | Names proposed for the list, most bookmarks first                     |
| `tag proposals accept NAME`    | Add a proposed name to the list and tag its bookmarks                 |
| `tag proposals alias NAME TAG` | Make a proposed name an alias of TAG and tag its bookmarks with it    |
| `tag proposals reject NAME`    | Remember a name: it isn't proposed again                              |
| `tag health`                   | Measure the list: redundant, broad, rare, misnamed tags, and gaps     |
| `review [-n N]`                | Review bookmarks in batches, one key per decision; see docs/config.md |
| `import FILE`                  | Import Netscape HTML, JSON lines, or a list of URLs; `-` is stdin     |
| `export [--format html]`       | Print every bookmark as JSON lines, or HTML that browsers import      |
| `suggest ID...\|--all`         | Suggest tags: domain rules, then the `[ai] tag` provider (`ai`)       |
| `suggest ls [ID]`              | List pending suggestions                                              |
| `suggest accept ID [TAG]...`   | Accept a bookmark's suggestions (`--all` accepts every one)           |
| `suggest reject ID [TAG]...`   | Reject suggestions; they aren't offered again for that bookmark       |
| `config path\|show`            | Print the config file path, or the effective config                   |
| `fetch ID...\|--all`           | Set titles from the pages (`--all`: bookmarks without one) (`fetch`)  |
| `summarize ID...\|--all`       | Summarize pages with the `[ai] summarize` provider (`ai`)             |
| `batch status\|collect`        | Check on or apply Message Batches from `--batch` (`ai`)               |
| `model train\|status\|eval`    | Train, inspect, or measure the tag classifier (`model`)               |

`ls`, `search`, `tag ls`, `tag proposals ls`, and `suggest ls` take `--json` for one JSON object per
line. `-n 0` shows every result.

To tag many bookmarks at once, pipe a search into `edit`, which reads ids from the start of each
line (or `--json`); `--pick` goes through them one key each to choose some:

```console
$ shlv search wifi -n 0 | shlv edit - -t networking --pick
```

The change is one transaction: every id must exist, and with `strict_tags` every added tag must be
canonical, or nothing changes.

The database is at `$SHLV_DB`, else `$XDG_DATA_HOME/shlv/shlv.db`, else
`~/.local/share/shlv/shlv.db`.

## Documentation

- [Install](docs/install.md): Nix, Home Manager, Cargo, and optional features
- [The TUI](docs/tui.md): the keys and views of bare `shlv`
- [Config and tags](docs/config.md): the config file, canonical tags, domain rules, suggestions,
  proposals, and tag health
- [Import and export](docs/import-export.md): browser bookmarks, URL lists, moving from another app
- [Page titles](docs/fetch.md): `add --fetch` and `shlv fetch` (`fetch` feature)
- [AI providers](docs/ai.md): summaries and tag suggestions from an LLM (`ai` feature)
- [Tag classifier](docs/classifier.md): offline tag suggestions learned from your bookmarks (`model`
  feature)

## Performance

Measured with [`bench/run.sh`](bench/run.sh) on 100k synthetic bookmarks (release build, warm cache,
binary and database on tmpfs, in a VM where starting a process takes about 4.4 ms):

| Command                    | Mean   |
| -------------------------- | ------ |
| `ls` (first page)          | 5.2 ms |
| `search kalomi` (28 hits)  | 5.9 ms |
| `search rust` (51k hits)   | 31 ms  |
| `add` (new URL)            | 5.4 ms |
| `export` (100k, JSON)      | 0.2 s  |
| `import` (100k, JSON)      | 1.9 s  |
| Release binary             | 5.1 MB |
| Database size per bookmark | 723 B  |

## Development

The toolchain comes from the flake's dev shell, through [direnv](https://direnv.net/) (`.envrc`) or
`nix develop`.

```sh
cargo test
direnv exec . bench/run.sh   # benchmarks; generates bench/data/ once
nix flake check              # what CI runs: build, tests, clippy, rustfmt, oxfmt, nixfmt
```

Format Rust with `cargo fmt`, Markdown and JSON with `oxfmt`, and Nix with `nixfmt`.

## License

[MIT](LICENSE)
