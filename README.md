# shlv

A fast, resource-efficient bookmark manager for the command line. Bookmarks live in a single SQLite
database with full-text search. Everything works offline.

## Install

With Nix:

```sh
nix run github:theurgi/shlv -- --help
nix profile add github:theurgi/shlv
```

The flake also provides `overlays.default`, which adds `pkgs.shlv`.

With Cargo:

```sh
cargo install --git https://github.com/theurgi/shlv
```

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

| Command                     | What it does                                                   |
| --------------------------- | -------------------------------------------------------------- |
| `add URL [-t TAG]...`       | Add a bookmark, or update it if the URL is already saved       |
| `ls [-t TAG]... [-n N]`     | List bookmarks, newest first                                   |
| `search QUERY...`           | Words, `"phrases"`, `#tag`, `tag:x`, `site:host`, `AND/OR/NOT` |
| `show ID`                   | Show one bookmark in full                                      |
| `edit ID [-t +TAG/-TAG]...` | Change a bookmark's title, note, or tags                       |
| `rm ID\|URL...`             | Delete bookmarks                                               |
| `open ID`                   | Open a bookmark with `$BROWSER` or the system opener           |

`ls` and `search` take `--json` for one JSON object per line. `-n 0` shows every result.

The database is at `$SHLV_DB`, else `$XDG_DATA_HOME/shlv/shlv.db`, else
`~/.local/share/shlv/shlv.db`.

## Performance

Measured with [`bench/run.sh`](bench/run.sh) on 100k synthetic bookmarks (release build, warm cache,
in a VM where starting a process takes about 5 ms):

| Command                    | Mean    |
| -------------------------- | ------- |
| `ls` (first page)          | 11.6 ms |
| `search kalomi` (28 hits)  | 19 ms   |
| `search rust` (51k hits)   | 47 ms   |
| `add` (new URL)            | 16 ms   |
| Release binary             | 3.2 MB  |
| Database size per bookmark | 723 B   |

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
