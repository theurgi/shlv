# Config and tags

An optional config file lives at `$SHLV_CONFIG`, else `$XDG_CONFIG_HOME/shlv/config.toml`, else
`~/.config/shlv/config.toml`. shlv only reads it, so it can be a read-only symlink. Unknown keys are
errors.

```toml
# Reject tags in add, edit, and import that aren't canonical or an alias of a canonical tag
# (true), or keep them as proposals for the list instead ("propose").
strict_tags = true

# Pin a base for the canonical list: each tag with its aliases, or a table that adds a one-line
# description of what belongs under the tag (tag suggestions match against it). The list itself
# lives in the database; shlv adds these to it on startup and keeps them there.
[tags.canonical]
javascript = ["js", "ecmascript"]
rust = []
infra = { aliases = ["ops"], description = "servers, deployment, Nix, CI" }

# Domain rules: bookmarks on a host, or any of its subdomains, get these tags suggested. With
# strict_tags, rule tags must be canonical (or aliases of canonical tags).
[tags.rules]
"developer.mozilla.org" = ["javascript"]
"docs.rs" = ["rust"]

# How tags are written; see "Tag style" below.
[tags.style]
separator = "-" # what spaces and the other separators become: "-", "_", or ""
max_length = 24
guidelines = """
Singular nouns: plugin, not plugins.
One concept per tag; combine tags rather than compounding them: rust + wifi, not rust-wifi.
"""

# Broad interests, for `tag seed` and tag health (`tag seed --save` stores more in the database).
[tags]
interests = ["nix", "network", "home-lab"]
```

The canonical list lives in the database, so `tag canonical add|rm|import`, `tag alias`,
`tag describe`, `tag rename`, and `tag merge` edit it without touching the config. A declared list
is a pinned base on top:

- On startup, declared tags become canonical, their aliases point at them, and declared descriptions
  are set. A declared alias that is a tag in the database is merged into its declared tag. This is
  skipped while the list is unchanged.
- Syncing only adds. Tags, aliases, and descriptions added in shlv stay, including new aliases of
  pinned tags and tags merged into them. A tag, alias, or description dropped from the config stays
  in the database, no longer pinned; remove it with `tag canonical rm` or `tag alias --rm` if you
  want it gone.
- What the config pins can't be undone in shlv: renaming, merging away, or unmarking a pinned tag,
  removing a declared alias, and changing a declared description are refused with a pointer to the
  config file. A pinned tag declared without a description can be described in shlv.

Declared names must already be normalized (`web-dev`, not `Web Dev`) and follow the
[tag style](#tag-style).

`tag describe TAG TEXT` sets a description (`""` clears it). `tag ls` shows descriptions after `#`,
and `tag canonical export` writes them the same way (`infra ops # servers, deployment, Nix, CI`), so
`tag canonical import` reads them back. To move tags added in shlv into the config,
`tag canonical export --toml` (or `--nix`, for the home-manager module) prints the list in the
config's shape, with `# pinned` after the tags the config already declares.

`add` and `import` record the tags of matching rules as pending suggestions, and
`suggest ID...|--all` runs the rules on bookmarks you already have. Review them with `suggest ls`,
then `suggest accept` or `suggest reject`. Accepted tags remember that a rule added them. A rejected
suggestion isn't offered again for that bookmark, and neither is a rule's tag that you removed with
`edit`. To apply suggestions without review, set `suggestions = "auto"` under `[tags]`. Rule hosts
and tags must be normalized, like declared names (`github.com`, not `GitHub.com` or a URL). Opt-in
tagging layers, the [classifier](classifier.md) and the [`[ai] tag` provider](ai.md), add their
suggestions to the same list.

## Proposals

A tag outside the canonical list can be a _proposal_ for the list: a name with an optional
description and the bookmarks it was proposed for, collected across runs. It never lands on a
bookmark until you decide. Proposals come from:

- the [`[ai] tag` provider](ai.md#proposals), which proposes names for topics the list doesn't cover
  (and, with an empty list, only proposes);
- tags you type in `add` or `edit` with `strict_tags = "propose"`: the bookmark gets its canonical
  tags, and the others become proposals for it (instead of an error, as with `strict_tags = true`);
- `import --propose-unknown-tags`, which keeps the tags `--drop-unknown-tags` would drop as
  proposals for their bookmarks (`import` does this anyway with `strict_tags = "propose"`).

Names are normalized first. A name that is a canonical tag or an alias of one isn't proposed: from
the provider it becomes a suggestion of that tag. A name that is an alias of a tag off the list is
proposed as that tag.

`tag proposals` (or `tag proposals ls`) lists them, those on the most bookmarks first, with their
descriptions and who proposed them first (`llm`, `user`, or `import`); `--json` prints one object
per line, with the bookmark ids. Each decision is one command:

- `tag proposals accept NAME...` adds the names to the canonical list, with their descriptions
  (unless the tag already has one), and tags the bookmarks they were proposed for. With a declared
  list, accepted tags go on the list in the database, like `tag canonical add`.
- `tag proposals alias NAME TAG` makes NAME an alias of TAG and tags NAME's bookmarks with TAG. If
  NAME is also a tag off the list, it is merged into TAG, as `tag merge` would.
- `tag proposals reject NAME...` remembers the names: they are never proposed again, from any source
  (a typed one is dropped with a note). A name can be rejected before anyone proposes it.
  `tag proposals ls --rejected` lists them; accepting a rejected name undoes the rejection.

Removing a proposed name from a bookmark with `edit -t -NAME` withdraws it for that bookmark.

## Review

`shlv review` goes through bookmarks in batches of 30 (`-n N`), one screen each: title, URL, the
tags it has, and the pending suggestions with their source and confidence, all checked.

```console
$ shlv review
batch 1: 30 of 812 to review, spread across domains, untagged first
[1/30] 214  Tokio: an asynchronous runtime for Rust
      https://github.com/tokio-rs/tokio
  1 [x] #code  rule
  2 [x] #rust  model 0.82
  Enter save  1-9 toggle  t add  s skip  o open  q end batch  ? help
```

One key each: Enter saves (accepting every checked suggestion, so it's "accept all" until you toggle
one), a digit checks or unchecks a tag, `t` adds a tag, `s` skips the bookmark for a later batch,
`o` opens it, `q` ends the batch, and Ctrl-C stops at once. `t` takes a canonical tag by name,
alias, or prefix (Tab completes; several matches are listed), a pending proposal by name, or, after
a `y`, a new name, which becomes a proposal for that bookmark. Each bookmark's decisions are saved
in one transaction when you press Enter: checked suggestions become tags, unchecked ones are
rejected, an unchecked tag is removed, and the bookmark is marked reviewed so later batches skip it.

Tag each bookmark completely. A tag missing from a reviewed bookmark tells the
[classifier](classifier.md) that it doesn't apply, and an unchecked suggestion is a rejection it
learns from. A bookmark with no tags asks for a second Enter.

Batches start with bookmarks without canonical tags, spread across domains (one per host per round,
hosts nothing was reviewed from first), to find the vocabulary. With the classifier built, once 100
bookmarks are reviewed, a batch is the bookmarks the classifier is least sure about instead (the tag
whose probability is nearest 0.5 decides; scored on a random 2,000 of those left). Filters narrow a
review to pending suggestions, on reviewed bookmarks too, least confident first: `--tag TAG`,
`--source rule|model|llm`, and `--below P` (the bookmark's most confident matching suggestion is
below P; an `llm` one counts as 0.5). Filters see the suggestions there are, so run `suggest --all`
first to have them everywhere.

Before a batch, its bookmarks get suggestions from domain rules, the classifier (feature `model`),
and the `[ai] tag` provider (feature `ai`) for the ones the classifier wasn't sure about and the
provider hasn't seen, as `[model] provider` says. Without AI, review works with rules and the
classifier, and new names come only from what you type. After a batch:

1. **Proposals:** the names proposed for the batch's bookmarks, with how many bookmarks each is on:
   `a` adds it to the list (its bookmarks get the tag), `l` makes it an alias of a canonical tag,
   `r` rejects it, `s` leaves it for `tag proposals`.
2. **Back-fill:** each tag just added is offered on the bookmarks reviewed before it existed, as
   [`suggest --tag`](classifier.md#back-filling-a-new-tag) does, and you answer y/n for each
   candidate. It needs the classifier or an `[ai] tag` provider.
3. **Retrain:** the classifier trains on the new decisions, so the next batch's suggestions are
   better.
4. **Progress:** a table of every batch so far: bookmarks reviewed and skipped, names proposed, tags
   added, and how many of the classifier's suggestions were accepted. When proposals taper off and
   acceptance rises, the list has settled; stop when you like. `review --history` prints the table.

## Tag style

Without `[tags.style]`, tag names are only normalized: lowercase, a leading `#` dropped, spaces
turned into `-`. Its settings add rules, checked wherever a new name enters: `add` and `edit`,
`import`, `tag canonical add|import`, `tag rename`, `tag alias`, proposals, and AI replies.

- `separator` (`"-"`, `"_"`, or `""`): what spaces, `-`, and `_` between words become. With `"_"`,
  `Home Lab` and `home-lab` become `home_lab`; with `""`, `homelab`.
- `max_length`: the most characters in a tag name. A longer name can't be fixed, so it is refused
  with the reason: `add` and `edit` fail, `import` drops the tag and says so, and an AI proposal is
  dropped with a note.
- `guidelines`: plain-language rules that code can't check (singular nouns, one concept per tag),
  sent to the AI with every tagging request; see [AI providers](ai.md#proposals).

Names that already resolve to a tag or an alias are left alone: after you set `separator = "_"`,
typing `web-dev` still finds an existing `web-dev`, and nothing is rewritten. `tag rename` applies
the style to the new name, so `tag rename web-dev web-dev` gives `web_dev`. Names in the config
(declared tags and aliases, rule tags) must already follow the style; the error gives the position
and the fixed name. Aliases are names to type, not tags, so `max_length` doesn't apply to them.

## Tag health

`shlv tag health` measures the list, to keep it as small as it can be while still finding things. It
only reports; each line names the command that would act on it (`tag merge`, `tag canonical rm`),
and nothing changes until you run one. `--json` prints the report as one JSON object.

- **Coverage:** bookmarks with any tag and with a canonical tag, tags per tagged bookmark, and how
  many bookmarks have 0, 1, 2, 3, or 4+ tags.
- **Redundant:** pairs where each tag is on at least `--redundant` (0.8) of the other's bookmarks,
  together on at least `--min-together` (3): merge candidates. The tag on fewer bookmarks is named
  first, as `tag merge` takes it.
- **Too broad:** tags on at least `--broad` (0.3) of all bookmarks; they don't narrow a search.
- **Rare:** canonical tags on at most `--rare` (2) bookmarks, oldest first, with their age.
- **Gaps:** bookmarks without a canonical tag (without any tag when there is no canonical list),
  grouped by site and by shared title words: groups of at least `--min-group` (3), at most
  `--groups` (10) of each. A word that is already a tag or alias says so: those bookmarks only need
  tagging. With the `model` feature, bookmarks the classifier would tag (at `[model] threshold`) are
  counted apart and left out of the groups; `suggest --all` tags them. With the `ai` feature,
  `--name-gaps [N]` asks the `[ai] tag` provider to name the N largest groups (5), or to pick a tag
  already on the list; it sends up to 15 titles and URLs per group, with the list.
- **Style:** names that break `[tags.style]`, a singular and a plural both present (`plugin`,
  `plugins`), and compounds of other tags (`rust-wifi` when `rust` and `wifi` exist).
- **Interests:** with stored interests (`[tags] interests`, or saved with
  [`tag seed --save`](ai.md#seeding-the-list-from-interests)), the tags under each and how many
  bookmarks have one of them, and interests with no tag under them. A tag is under an interest when
  it was kept (or merged into) while seeding that interest, or when it is named like the interest.

On 100,000 bookmarks it takes about 0.1 s, and 0.4 s when the classifier also checks 25,000 gap
bookmarks.
