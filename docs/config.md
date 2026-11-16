# Config and tags

An optional config file lives at `$SHLV_CONFIG`, else `$XDG_CONFIG_HOME/shlv/config.toml`, else
`~/.config/shlv/config.toml`. shlv only reads it, so it can be a read-only symlink. Unknown keys are
errors.

```toml
# Reject tags in add and edit that aren't canonical or an alias of a canonical tag.
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

Declared names must already be normalized (`web-dev`, not `Web Dev`).

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
