# Config and tags

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
and tags must be normalized, like declared names (`github.com`, not `GitHub.com` or a URL). Opt-in
tagging layers, the [classifier](classifier.md) and the [`[ai] tag` provider](ai.md), add their
suggestions to the same list.
