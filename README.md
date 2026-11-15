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

With [Home Manager](https://github.com/nix-community/home-manager), add `shlv` as a flake input,
then import its module. `settings` is written to `~/.config/shlv/config.toml` (see
[Config](#config)):

```nix
{ shlv, ... }:
{
  imports = [ shlv.homeManagerModules.default ];

  programs.shlv = {
    enable = true;
    settings = {
      strict_tags = true;
      tags.canonical = {
        javascript = [ "js" "ecmascript" ];
        rust = [ ];
      };
      tags.rules."docs.rs" = [ "rust" ];
    };
  };
}
```

Don't put secrets in `settings`, since it ends up in the Nix store.

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

| Command                      | What it does                                                        |
| ---------------------------- | ------------------------------------------------------------------- |
| `add URL [-t TAG]...`        | Add a bookmark, or update it if the URL is already saved            |
| `ls [-t TAG]... [-n N]`      | List bookmarks, newest first                                        |
| `search QUERY...`            | Words, `"phrases"`, `#tag`, `tag:x`, `site:host`, `AND/OR/NOT`      |
| `show ID`                    | Show one bookmark in full                                           |
| `edit ID [-t +TAG/-TAG]...`  | Change a bookmark's title, note, or tags                            |
| `rm ID\|URL...`              | Delete bookmarks                                                    |
| `open ID`                    | Open a bookmark with `$BROWSER` or the system opener                |
| `tag ls [--sort count]`      | Tags with bookmark counts; `*` marks canonical, `=` lists aliases   |
| `tag rename OLD NEW`         | Rename a tag                                                        |
| `tag merge FROM INTO`        | Retag FROM's bookmarks as INTO; FROM becomes an alias               |
| `tag alias ALIAS TAG`        | Make ALIAS resolve to TAG on input (`--rm ALIAS` removes it)        |
| `tag canonical add\|rm\|ls`  | Mark, unmark, or list canonical tags                                |
| `tag canonical import FILE`  | Add a canonical list: one tag per line, aliases after it            |
| `tag canonical export`       | Print the canonical list in that format                             |
| `import FILE`                | Import a browser export (Netscape HTML) or JSON lines; `-` is stdin |
| `export [--format html]`     | Print every bookmark as JSON lines, or HTML that browsers import    |
| `suggest ID...\|--all`       | Suggest tags from domain rules (see [Config](#config))              |
| `suggest ls [ID]`            | List pending suggestions                                            |
| `suggest accept ID [TAG]...` | Accept a bookmark's suggestions (`--all` accepts every one)         |
| `suggest reject ID [TAG]...` | Reject suggestions; they aren't offered again for that bookmark     |
| `config path\|show`          | Print the config file path, or the effective config                 |

`ls`, `search`, `tag ls`, and `suggest ls` take `--json` for one JSON object per line. `-n 0` shows
every result.

`import` detects the format and treats URLs already saved the way `add` does: it updates the
bookmark and merges tags. It keeps creation times, titles, tags, and notes, and skips `javascript:`
and `place:` URLs. Browser folders become tags only with `--folders-as-tags`. With `strict_tags`, an
import with non-canonical tags writes nothing unless you pass `--drop-unknown-tags`. `export` then
`import` (JSON) reproduces a database, uids included.

The database is at `$SHLV_DB`, else `$XDG_DATA_HOME/shlv/shlv.db`, else
`~/.local/share/shlv/shlv.db`.

### Config

An optional config file lives at `$SHLV_CONFIG`, else `$XDG_CONFIG_HOME/shlv/config.toml`, else
`~/.config/shlv/config.toml`. shlv only reads it, so it can be a read-only symlink. Unknown keys are
errors.

```toml
# Reject tags in add and edit that aren't canonical or an alias of a canonical tag.
strict_tags = true

# Declare the canonical list: each tag with its aliases. When this table is present, even empty,
# the config is the source of truth: the database mirrors it on startup, and `tag canonical
# add|rm|import`, `tag alias`, and renaming or merging declared tags are refused.
[tags.canonical]
javascript = ["js", "ecmascript"]
rust = []

# Domain rules: bookmarks on a host, or any of its subdomains, get these tags suggested. With
# strict_tags, rule tags must be canonical (or aliases of canonical tags).
[tags.rules]
"developer.mozilla.org" = ["javascript"]
"docs.rs" = ["rust"]
```

Declared names must already be normalized (`web-dev`, not `Web Dev`). Mirroring marks declared tags
canonical, points their aliases at them, and unmarks canonical tags that aren't listed; bookmarks
keep their tags. A declared alias that is a tag in the database is merged into its declared tag.
Mirroring is skipped while the list is unchanged.

`add` records the tags of matching rules as pending suggestions, and `suggest ID...|--all` runs the
rules on bookmarks you already have. Review them with `suggest ls`, then `suggest accept` or
`suggest reject`. Accepted tags remember that a rule added them. A rejected suggestion isn't offered
again for that bookmark, and neither is a rule's tag that you removed with `edit`. To apply
suggestions without review, set `suggestions = "auto"` under `[tags]`. Rule hosts and tags must be
normalized, like declared names (`github.com`, not `GitHub.com` or a URL). Later tagging layers (a
local model and an LLM, both opt-in) will add their suggestions to the same list.

## Performance

Measured with [`bench/run.sh`](bench/run.sh) on 100k synthetic bookmarks (release build, warm cache,
in a VM where starting a process takes about 5 ms):

| Command                    | Mean    |
| -------------------------- | ------- |
| `ls` (first page)          | 11.6 ms |
| `search kalomi` (28 hits)  | 19 ms   |
| `search rust` (51k hits)   | 47 ms   |
| `add` (new URL)            | 16 ms   |
| `export` (100k, JSON)      | 0.7 s   |
| `import` (100k, JSON)      | 5.0 s   |
| Release binary             | 3.4 MB  |
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
