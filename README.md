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

### Optional features

The default build is offline. Network features are Cargo features, off by default:

| Feature | Adds                                                                                |
| ------- | ----------------------------------------------------------------------------------- |
| `fetch` | `add --fetch` and `shlv fetch`: page titles from the web                            |
| `ai`    | AI summaries and tag suggestions through a provider you configure (implies `fetch`) |
| `model` | An in-binary tag classifier that learns from your tags; offline, no dependencies    |

The flake's `shlv-full` package has every feature (`nix run github:theurgi/shlv#shlv-full`). With
Cargo, pass `--features fetch,ai,model` (or `--all-features`).

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

| Command                      | What it does                                                         |
| ---------------------------- | -------------------------------------------------------------------- |
| `add URL [-t TAG]...`        | Add a bookmark, or update it if the URL is already saved             |
| `ls [-t TAG]... [-n N]`      | List bookmarks, newest first                                         |
| `search QUERY...`            | Words, `"phrases"`, `#tag`, `tag:x`, `site:host`, `AND/OR/NOT`       |
| `show ID`                    | Show one bookmark in full                                            |
| `edit ID [-t +TAG/-TAG]...`  | Change a bookmark's title, note, or tags                             |
| `rm ID\|URL...`              | Delete bookmarks                                                     |
| `open ID`                    | Open a bookmark with `$BROWSER` or the system opener                 |
| `tag ls [--sort count]`      | Tags with bookmark counts; `*` marks canonical, `=` lists aliases    |
| `tag rename OLD NEW`         | Rename a tag                                                         |
| `tag merge FROM INTO`        | Retag FROM's bookmarks as INTO; FROM becomes an alias                |
| `tag alias ALIAS TAG`        | Make ALIAS resolve to TAG on input (`--rm ALIAS` removes it)         |
| `tag canonical add\|rm\|ls`  | Mark, unmark, or list canonical tags                                 |
| `tag canonical import FILE`  | Add a canonical list: one tag per line, aliases after it             |
| `tag canonical export`       | Print the canonical list in that format                              |
| `import FILE`                | Import a browser export (Netscape HTML) or JSON lines; `-` is stdin  |
| `export [--format html]`     | Print every bookmark as JSON lines, or HTML that browsers import     |
| `suggest ID...\|--all`       | Suggest tags: domain rules, then the `[ai] tag` provider (`ai`)      |
| `suggest ls [ID]`            | List pending suggestions                                             |
| `suggest accept ID [TAG]...` | Accept a bookmark's suggestions (`--all` accepts every one)          |
| `suggest reject ID [TAG]...` | Reject suggestions; they aren't offered again for that bookmark      |
| `config path\|show`          | Print the config file path, or the effective config                  |
| `fetch ID...\|--all`         | Set titles from the pages (`--all`: bookmarks without one) (`fetch`) |
| `summarize ID...\|--all`     | Summarize pages with the `[ai] summarize` provider (`ai`)            |
| `batch status\|collect`      | Check on or apply Message Batches from `--batch` (`ai`)              |
| `model train\|status\|eval`  | Train, inspect, or measure the tag classifier (`model`)              |

`ls`, `search`, `tag ls`, and `suggest ls` take `--json` for one JSON object per line. `-n 0` shows
every result.

`import` detects the format and treats URLs already saved the way `add` does: it updates the
bookmark and merges tags. It keeps creation times, titles, tags, and notes, and skips `javascript:`
and `place:` URLs. Browser folders become tags only with `--folders-as-tags`. With `strict_tags`, an
import with non-canonical tags writes nothing unless you pass `--drop-unknown-tags`. `export` then
`import` (JSON) reproduces a database, uids included. Domain rules run on imported bookmarks as they
do on `add`; `--no-rules` skips them.

With the `fetch` feature, `add --fetch` downloads the page and takes its title (`<title>`, else
`og:title`) unless `--title` is given. A fetch that fails (offline, timeout, HTTP error, not HTML)
prints a warning and still adds the bookmark. `shlv fetch` fills in titles afterwards, a few
requests at a time (`-j`), and exits nonzero if any failed. Requests time out after 15 s, follow at
most 5 redirects, read at most 2 MiB, and send `User-Agent: shlv/VERSION`. HTTPS trusts the system's
CA bundle (`$SSL_CERT_FILE`, else the usual system paths), so a proxy CA the system trusts works;
without one, shlv uses its bundled Mozilla roots.

The database is at `$SHLV_DB`, else `$XDG_DATA_HOME/shlv/shlv.db`, else
`~/.local/share/shlv/shlv.db`.

### Config

An optional config file lives at `$SHLV_CONFIG`, else `$XDG_CONFIG_HOME/shlv/config.toml`, else
`~/.config/shlv/config.toml`. shlv only reads it, so it can be a read-only symlink. Unknown keys are
errors.

```toml
# Reject tags in add and edit that aren't canonical or an alias of a canonical tag.
strict_tags = true

# Declare the canonical list: each tag with its aliases, or a table that adds a one-line
# description of what belongs under the tag (tag suggestions match against it). When this table is
# present, even empty, the config is the source of truth: the database mirrors it on startup, and
# `tag canonical add|rm|import`, `tag alias`, `tag describe`, and renaming or merging declared tags
# are refused.
[tags.canonical]
javascript = ["js", "ecmascript"]
rust = []
infra = { aliases = ["ops"], description = "servers, deployment, Nix, CI" }

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

Without a declared list, `tag describe TAG TEXT` sets a description (`""` clears it). `tag ls` shows
descriptions after `#`, and `tag canonical export` writes them the same way
(`infra ops # servers, deployment, Nix, CI`), so `tag canonical import` reads them back.

`add` and `import` record the tags of matching rules as pending suggestions, and
`suggest ID...|--all` runs the rules on bookmarks you already have. Review them with `suggest ls`,
then `suggest accept` or `suggest reject`. Accepted tags remember that a rule added them. A rejected
suggestion isn't offered again for that bookmark, and neither is a rule's tag that you removed with
`edit`. To apply suggestions without review, set `suggestions = "auto"` under `[tags]`. Rule hosts
and tags must be normalized, like declared names (`github.com`, not `GitHub.com` or a URL). Later
tagging layers (a local model and an LLM, both opt-in) will add their suggestions to the same list.

### AI providers

With the `ai` feature, an LLM can write summaries and suggest tags. AI is off until you configure
it: there is no default provider or model. Each task names a provider, so tagging can stay local
while summaries go elsewhere:

```toml
[ai]
summarize = "local" # a [provider.*] name; leave a task out to turn it off
tag = "local"

[provider.local]
kind = "openai-compatible" # llama.cpp, Ollama, vLLM, LM Studio, and hosted APIs that speak it
base_url = "http://localhost:8080/v1"
model = "qwen2.5-3b-instruct"
# api_key_env = "OPENAI_API_KEY" # the variable holding the key; never the key itself
# timeout = 120                  # seconds per reply
```

The Claude API is `kind = "anthropic"`. The key comes from an environment variable or from a file
that shlv reads only when it sends a request (an agenix or sops-nix secret, say; shlv trims the
trailing newline and warns if every user can read the file). Set one of the two:

```toml
[provider.claude]
kind = "anthropic"
model = "claude-opus-5-5"
effort = "low" # low, medium, high, xhigh, max: less thinking, fewer tokens
api_key_env = "ANTHROPIC_API_KEY"
# api_key_file = "/run/agenix/anthropic" # instead of api_key_env
```

Requests to Anthropic cache the instructions and the canonical tag list, so a bulk run pays full
price for that prefix once. 429 and 529 responses are retried (honoring `retry-after`).

**What is sent.** For each bookmark, a request to the provider's `base_url` with the bookmark's URL,
title, note, and summary (when it has them), the page's extracted main text (up to 3000 words) when
the task fetches the page, and for tagging the canonical tag list with aliases and descriptions.
Nothing else from the database, and nothing at all unless `[ai]` names a provider. The API key is
read from the named environment variable only when a request is made.

`shlv summarize ID...` (or `--all` for every bookmark without a summary; `--force` redoes them)
fetches each page, extracts its main text, and stores the provider's two- or three-sentence summary,
which `search` then finds. Pages that can't be fetched count as failures; pages with no text (an
empty JavaScript app) are skipped. It runs two requests at a time (`-j`), commits each summary as it
arrives (an interrupted run keeps them; rerun to continue), prints token use, and exits nonzero if
any failed. It says how many bookmarks it will send where before it starts, and above 20 it needs
`--yes`.

With `[ai] tag` set, `shlv suggest ID...` also asks that provider to choose tags from the canonical
list (with aliases and descriptions; tags the bookmark has or rejected are left out), and its
choices join the rules' as suggestions with source `llm` and no confidence score, since a model's
own confidence isn't calibrated. The bookmark's summary is sent if it has one, else the page text.
`suggest --all` sends bookmarks with fewer than `--max-tags` tags (default 1: untagged ones) that
the provider hasn't tagged before (`--force` sends them again), with the same `-j`, `--yes`, and
failure reporting as `summarize`. When `[ai] summarize` and `[ai] tag` name the same provider, a
bookmark without a summary gets both from one request.

With an `anthropic` provider, `summarize --batch` and `suggest --batch` submit one Message Batch
instead of live requests: half the price, with results within 24 hours. Pages are fetched when
submitting. `shlv batch status` shows submitted batches, and `shlv batch collect` applies the
results of ended ones (from any later run, since pending batches are recorded in the database).
Failed or expired items are reported; rerunning the command without `--batch` retries them live.

Replies are checked: malformed JSON is retried once, tags not in the canonical list are dropped, and
empty or overlong summaries are rejected. If a server doesn't support JSON-schema output, shlv falls
back to JSON mode with the schema in the prompt.

### Tag classifier

With the `model` feature, shlv learns tags from your own bookmarks: a small classifier (logistic
regression over words from the title, host, URL path, summary, and note) trained on bookmarks that
have canonical tags, plus suggestions you rejected. It runs offline in microseconds and is stored in
the database. Tags with few examples fall back on matching their name, aliases, and description
(`tag describe`), so it helps from the first day.

```console
$ shlv model train      # suggest retrains it by itself when your tags change
trained on 8039 examples for 30 canonical tags in 182 ms; the model is 325 KB
$ shlv model eval       # train on your oldest 80%, test on the newest 20%
```

`model eval` reports precision and recall of the first choices, calibration (whether a 0.9 means
90%), and with `--provider` the same for the `[ai] tag` provider on a few test bookmarks, so you can
choose layers by measurement. Nothing leaves your machine unless you pass `--provider`.

With the classifier built, `suggest` runs the layers in order: domain rules, then the classifier
(suggestions with source `model` and its probability), then the `[ai] tag` provider where the
classifier is unsure. `add` runs the classifier too, with the stored model. Tune it in `[model]`:

```toml
[model]
threshold = 0.5 # offer classifier tags with at least this probability
auto_threshold = 0.9 # with suggestions = "auto", apply only from here up; the rest wait
provider = "fallback" # ask [ai] tag: "fallback" (when the classifier is unsure), "always", "never"
```

"Unsure" means nothing reached `threshold`, or a tag that did has fewer than 50 examples. Every
accepted or rejected suggestion is a training example the next time the model trains.

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
| Release binary             | 3.9 MB |
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
