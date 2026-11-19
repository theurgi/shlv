# AI providers

An LLM can write summaries and suggest tags. AI is off until you configure it: there is no default
provider or model. Each task names a provider, so tagging can stay local while summaries go
elsewhere:

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

The Claude API is `kind = "anthropic"`:

```toml
[provider.claude]
kind = "anthropic"
model = "claude-opus-5-5"
effort = "low" # low, medium, high, xhigh, max: less thinking, fewer tokens
api_key_env = "ANTHROPIC_API_KEY"
```

**API keys** come from one of three places; set at most one per provider, and never the key itself:

- `api_key_env = "ANTHROPIC_API_KEY"`: an environment variable.
- `api_key_file = "/run/agenix/anthropic"`: a file (an agenix or sops-nix secret, say). shlv trims
  the trailing newline and warns if every user can read the file.
- `api_key_cmd = ["secret-tool", "lookup", "service", "shlv"]`: a command that prints the key, for a
  secret manager. It is a program and its arguments, run without a shell. Other examples:
  `["pass", "show", "anthropic"]`, `["op", "read", "op://Private/Anthropic/credential"]`,
  `["security", "find-generic-password", "-s", "anthropic", "-w"]`.

To store a key for the `secret-tool` example (GNOME Keyring, KeePassXC, or another Secret Service
provider):

```sh
secret-tool store --label="shlv: Anthropic API key" service shlv
```

Keys are read only when a request is sent, never for other commands. The command runs once per
`shlv` process, when the first request needs the key, and its output is trimmed. Its stdin is closed
and its stderr goes to the terminal, so a prompt (a GPG pinentry, say) still works. In the TUI, the
screen is suspended while the command runs. A command that fails, prints nothing, or takes longer
than 120 seconds is an error that names the provider and the command, not the output. `config show`
shows the command, never the key.

Requests to Anthropic cache the instructions and the canonical tag list, so a bulk run pays full
price for that prefix once. 429 and 529 responses are retried (honoring `retry-after`).

**What is sent.** For each bookmark, a request to the provider's `base_url` with the bookmark's URL,
title, note, and summary (when it has them), the page's extracted main text (up to 3000 words) when
the task fetches the page, and for tagging the canonical tag list with aliases and descriptions, and
the names already proposed for it. Nothing else from the database, and nothing at all unless `[ai]`
names a provider.

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

`suggest --tag TAG` back-fills one tag: the provider sees only that tag and proposes nothing. See
[back-filling a new tag](classifier.md#back-filling-a-new-tag).

### Proposals

The `tag` provider can also propose names that aren't on the list, with a one-line description each,
when a page has a main topic the list doesn't cover. They become [proposals](config.md#proposals)
for the list, never tags on the bookmark: review them with `shlv tag proposals`. Up to three per
bookmark by default:

```toml
[ai]
tag = "local"
proposals = 3 # new names per bookmark; 0 turns proposals off
```

The prompt asks for short, general, one-concept names and lists the names already proposed (up to
100, most proposed first), so the model reuses them rather than inventing synonyms. Replies are
normalized: a name on the list (or an alias of one) becomes a suggestion of that tag, a rejected
name is dropped, and names past the cap are ignored. With a [tag style](config.md#tag-style), the
prompt asks for names in it, new names are fixed to it, and those it refuses are dropped with a
note; `[tags.style] guidelines` go in every tagging prompt, for choosing as well as proposing.
`suggest` prints the proposals for the bookmarks it was given; `suggest --all` marks them with `+`
on each bookmark's line.

With an empty list, the provider only proposes, so `suggest` works from the first bookmark: let
proposals pile up, then accept the most proposed names to start the list. With `proposals = 0` and
an empty list, `suggest` has nothing to ask the provider and says so. The classifier never proposes;
it only chooses from the list.

### Seeding the list from interests

`shlv tag seed TOPIC...` starts or grows the list from a few broad interests: the `[ai] tag`
provider expands each into candidate tags with descriptions, and you go through them one key each.

```
$ shlv tag seed nix network
nix      nix            the Nix language and package manager  k
         flake          Nix flakes                            k
         nixpkgs        the package collection                m → nix
network  wifi           wireless networking, 802.11           k
         linux          the Linux kernel and distributions    on the list
         reverse-proxy  Caddy, nginx, Traefik                 k
         proxmox        Proxmox VE                            d
```

It sends only the interests and how tags are named (the separator and length from `[tags.style]`,
and its guidelines): no bookmarks, and not your list. It says so, and where it sends them, and asks
before sending (`--yes` skips the question). Candidates are fixed to the tag style; those already on
the list are shown but not asked about.

Keys: `k` (or Enter) keeps a candidate, `r` renames it (and keeps it), `m` merges it into a tag or
another candidate (it becomes an alias; a tag already in use is merged as `tag merge` does), `d`
drops it, `e` edits its description, `a` adds interests and expands them, `b` goes back, `q`
finishes early, `?` lists the keys. At the end, Enter saves and `n` discards; Ctrl-C quits at any
point without writing anything. Kept candidates go on the list in the database, with their
descriptions (a tag's existing description is replaced only by one you edited). `-n` sets the most
candidates per interest (8).

On a terminal each answer is one keypress; otherwise each is a line whose first character is the
key, so answers can be piped in. The screen draws on stderr; stdout gets the summary.

`--save` stores the interests in the database, and `[tags] interests = [...]` in the config pins
some (see [Config](config.md#tag-health)); `shlv tag seed` with no topics expands the stored ones.
`shlv tag interests` lists them, `--add` stores more without the AI, and `--rm` forgets saved ones.
Interests and tags are independent: an interest can be kept as a tag or not, and nothing ties one to
the other. Which tags were kept under which interest is recorded either way, for tag health.

### Drafting the list from a sample

`shlv tag draft` has the `[ai] tag` provider read a sample of your bookmarks and draft the list:
tags for what recurs in the sample, with descriptions, grouped under your stored interests where
they fit (others are welcome, under `other`). With a list already, it proposes only additions, and
points at listed tags that look redundant, with what to merge each into. You go through the draft in
the same screen as `tag seed`.

```
$ shlv tag draft --yes
nix        flake          Nix flakes and lock files          k
other      home-lab       servers and services run at home   k
           photography    cameras, lenses, editing           d
redundant  js             → javascript: a synonym            m → javascript
```

The sample (`--sample`, 200) is spread across sites: one bookmark from every site in turn, larger
sites first, then a second from each, and so on. It is the same from run to run. For each bookmark
it sends the site (not the full URL), the title, and the summary if there is one; `--fetch` fetches
the pages of the rest and sends the start of their text instead. Up to 300 characters of either.
Also sent: your list with descriptions and aliases, your stored interests, and how tags are named.
Notes and the tags on bookmarks are not. It says all this, and where it sends it, before sending: up
to 20 bookmarks it asks, and above that it needs `--yes` (as `summarize --all` does).

A large sample goes in several requests of `--chunk` bookmarks (50), one after another; each sees
the names drafted so far, to reuse rather than coin synonyms of, and the drafts are merged by name.
Tags proposed by more requests come first, and `-n` (30) caps the draft. A request that fails is
reported and the rest still count. Each request's output is capped at about 60 tokens per tag, so a
small local model that starts repeating itself stops early instead of running until the server gives
up.

The keys are those of `tag seed`, without `a`. For a redundant tag, `m` merges it (Enter at the
prompt takes the suggested tag) and `k` or `d` leave it as it is; to rename a listed tag, use
`shlv tag rename`.

With an `anthropic` provider, `summarize --batch` and `suggest --batch` submit one Message Batch
instead of live requests: half the price, with results within 24 hours. Pages are fetched when
submitting. `shlv batch status` shows submitted batches, and `shlv batch collect` applies the
results of ended ones (from any later run, since pending batches are recorded in the database).
Failed or expired items are reported; rerunning the command without `--batch` retries them live.

Replies are checked: malformed JSON is retried once, tags not in the canonical list are dropped (new
names go to proposals only when asked for), and empty or overlong summaries are rejected. If a
server doesn't support JSON-schema output, shlv falls back to JSON mode with the schema in the
prompt.
