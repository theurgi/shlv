//! The optional config file. shlv only reads it, so it can be a read-only symlink into the Nix
//! store. Commands that don't need settings never load it.

use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};

use crate::suggest::{DomainRules, SuggestMode};
use crate::tag::DeclaredTags;

/// Settings from `config.toml`. Unknown keys are errors, so typos surface; add new settings as
/// `#[serde(default)]` fields.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// `add`, `edit`, and import reject tags that aren't canonical or an alias of one.
    #[serde(default)]
    pub strict_tags: bool,
    #[serde(default, skip_serializing_if = "Tags::is_empty")]
    pub tags: Tags,
    /// Sections for features this build doesn't have yet. They're accepted, unchecked, so one
    /// config works across builds.
    #[serde(default, skip_serializing)]
    ai: Option<IgnoredAny>,
    #[serde(default, skip_serializing)]
    provider: Option<IgnoredAny>,
}

/// The `[tags]` section.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Tags {
    /// The declared canonical list, `tag = [aliases]`. When present (even empty), config is the
    /// source of truth and the database mirrors it; see [`crate::Store::sync_declared`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical: Option<DeclaredTags>,
    /// Whether tag suggestions (from domain rules, and later the model and LLM) are applied at
    /// once (`auto`) or wait for `shlv suggest accept` (`confirm`, the default).
    #[serde(default, skip_serializing_if = "SuggestMode::is_default")]
    pub suggestions: SuggestMode,
    /// Domain rules, `host = [tags]`; a host also matches its subdomains.
    #[serde(default, skip_serializing_if = "DomainRules::is_empty")]
    pub rules: DomainRules,
}

impl Tags {
    fn is_empty(&self) -> bool {
        self.canonical.is_none() && self.suggestions.is_default() && self.rules.is_empty()
    }
}

impl Config {
    /// Reads the config at `path`. A missing file means defaults.
    pub fn load(path: &Path) -> Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(text) => Config::parse(&text, path),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // A dangling symlink (say, into a garbage-collected Nix store path) is a broken
                // setup, not an absent config.
                if path.symlink_metadata().is_ok() {
                    bail!("cannot read config {}: broken symlink", path.display());
                }
                Ok(Config::default())
            }
            Err(e) => Err(e).with_context(|| format!("cannot read config {}", path.display())),
        }
    }

    /// Parses config text; errors read `PATH:LINE:COLUMN: message`.
    pub fn parse(text: &str, path: &Path) -> Result<Config> {
        let at = |pos: Option<usize>, msg: &str| match pos {
            Some(pos) => {
                let (line, col) = line_col(text, pos);
                anyhow!("{}:{line}:{col}: {msg}", path.display())
            }
            None => anyhow!("{}: {msg}", path.display()),
        };
        let config: Config = toml::from_str(text)
            .map_err(|e| at(e.span().map(|s| s.start), e.message().trim_end()))?;
        if let Some((host, msg)) = config.undeclared_rule_tag() {
            return Err(at(config.tags.rules.span(host).map(|s| s.start), &msg));
        }
        Ok(config)
    }

    /// With strict tags and a declared canonical list, rule tags must be declared tags or
    /// aliases. Returns the first offending rule's host and an error message. Without a declared
    /// list the canonical list lives in the database; see [`crate::Store::check_rules`].
    fn undeclared_rule_tag(&self) -> Option<(&str, String)> {
        let declared = self.tags.canonical.as_ref().filter(|_| self.strict_tags)?;
        self.tags.rules.iter().find_map(|(host, tags)| {
            let bad: Vec<&str> = tags
                .iter()
                .map(String::as_str)
                .filter(|t| declared.resolve(t).is_none())
                .collect();
            (!bad.is_empty()).then(|| {
                (
                    host,
                    format!(
                        "rule {host:?}: not in [tags.canonical] (strict_tags is on): {}",
                        bad.join(" ")
                    ),
                )
            })
        })
    }

    /// The effective config as TOML, defaults included.
    pub fn to_toml(&self) -> String {
        toml::to_string(self).expect("config serializes")
    }

    /// Sections present in the file that this build ignores.
    pub fn ignored_sections(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.ai.is_some() {
            v.push("ai");
        }
        if self.provider.is_some() {
            v.push("provider");
        }
        v
    }
}

/// 1-based line and column (in characters) of byte offset `at`.
fn line_col(text: &str, at: usize) -> (usize, usize) {
    let before = &text[..at.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let start = before.rfind('\n').map_or(0, |i| i + 1);
    (line, before[start..].chars().count() + 1)
}

/// Resolves the config path: `$SHLV_CONFIG`, else `$XDG_CONFIG_HOME/shlv/config.toml`, else
/// `~/.config/shlv/config.toml`.
pub fn default_path() -> Result<PathBuf> {
    resolve_path(|var| std::env::var(var).ok())
}

fn resolve_path(env: impl Fn(&str) -> Option<String>) -> Result<PathBuf> {
    let var = |name| env(name).filter(|v: &String| !v.is_empty());
    if let Some(p) = var("SHLV_CONFIG") {
        return Ok(PathBuf::from(p));
    }
    // The XDG spec says relative values are invalid and should be ignored.
    if let Some(d) = var("XDG_CONFIG_HOME").filter(|d| Path::new(d).is_absolute()) {
        return Ok(Path::new(&d).join("shlv/config.toml"));
    }
    match var("HOME") {
        Some(home) => Ok(Path::new(&home).join(".config/shlv/config.toml")),
        None => bail!("cannot locate the config: set SHLV_CONFIG, XDG_CONFIG_HOME, or HOME"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config> {
        Config::parse(text, Path::new("/c.toml"))
    }

    #[test]
    fn resolves_path() {
        let env = |pairs: &'static [(&str, &str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        let p = |pairs| resolve_path(env(pairs)).unwrap();
        assert_eq!(
            p(&[("SHLV_CONFIG", "/x.toml"), ("HOME", "/h")]),
            Path::new("/x.toml")
        );
        assert_eq!(
            p(&[("XDG_CONFIG_HOME", "/xdg"), ("HOME", "/h")]),
            Path::new("/xdg/shlv/config.toml")
        );
        assert_eq!(
            p(&[("XDG_CONFIG_HOME", "rel"), ("HOME", "/h")]),
            Path::new("/h/.config/shlv/config.toml")
        );
        assert_eq!(
            p(&[("SHLV_CONFIG", ""), ("HOME", "/h")]),
            Path::new("/h/.config/shlv/config.toml")
        );
        assert!(resolve_path(env(&[])).is_err());
    }

    #[test]
    fn parses() {
        assert!(!parse("").unwrap().strict_tags);
        assert!(parse("strict_tags = true").unwrap().strict_tags);
    }

    #[test]
    fn ignores_unbuilt_sections() {
        let c = parse(
            "strict_tags = true\n[ai]\nmodel = \"x\"\n[provider.openai]\nkey_cmd = \"pass x\"\n",
        )
        .unwrap();
        assert!(c.strict_tags);
        assert_eq!(c.ignored_sections(), ["ai", "provider"]);
        assert_eq!(c.to_toml(), "strict_tags = true\n");
    }

    #[test]
    fn errors_name_file_and_line() {
        let e = parse("\n\nstrict_tag = true\n").unwrap_err().to_string();
        assert!(
            e.starts_with("/c.toml:3:1: unknown field `strict_tag`"),
            "{e}"
        );
        let e = parse("strict_tags = 1\n").unwrap_err().to_string();
        assert!(e.starts_with("/c.toml:1:15: "), "{e}");
        let e = parse("strict_tags = \n").unwrap_err().to_string();
        assert!(e.starts_with("/c.toml:1:"), "{e}");
    }

    #[test]
    fn parses_declared_canonical_tags() {
        assert!(parse("").unwrap().tags.canonical.is_none());
        // Present but empty is still declarative.
        let c = parse("[tags.canonical]\n").unwrap();
        assert!(c.tags.canonical.unwrap().is_empty());

        let text = "[tags.canonical]\njavascript = [\"js\", \"ecmascript\"]\nrust = []\n";
        let c = parse(text).unwrap();
        let d = c.tags.canonical.as_ref().unwrap();
        let list: Vec<_> = d.iter().collect();
        assert_eq!(
            list,
            [
                ("javascript", &["ecmascript".to_string(), "js".into()][..]),
                ("rust", &[][..])
            ]
        );
        // `config show` prints it back; aliases come out sorted.
        assert_eq!(
            c.to_toml(),
            "strict_tags = false\n\n[tags.canonical]\njavascript = [\"ecmascript\", \"js\"]\nrust = []\n"
        );
        assert!(parse(&c.to_toml()).is_ok());
    }

    #[test]
    fn declared_tag_errors_have_positions() {
        let e = parse("strict_tags = true\n[tags.canonical]\n\"Web Dev\" = []\n")
            .unwrap_err()
            .to_string();
        assert!(e.starts_with("/c.toml:2:1: "), "{e}");
        assert!(
            e.contains("\"Web Dev\" is not a normalized tag name; write \"web-dev\""),
            "{e}"
        );
        let e = parse("[tags.canonical]\njavascript = [\"js\"]\njson = [\"js\"]\n")
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("alias \"js\" is declared for both \"javascript\" and \"json\""),
            "{e}"
        );
        let e = parse("[tags]\ncanon = {}\n").unwrap_err().to_string();
        assert!(e.starts_with("/c.toml:2:1: unknown field `canon`"), "{e}");
        let e = parse("[tags.canonical]\nrust = \"rs\"\n")
            .unwrap_err()
            .to_string();
        assert!(e.starts_with("/c.toml:2:8: "), "{e}");
    }

    #[test]
    fn parses_rules_and_suggestion_mode() {
        let c = parse("").unwrap();
        assert!(c.tags.rules.is_empty());
        assert_eq!(c.tags.suggestions, SuggestMode::Confirm);

        let text = "[tags]\nsuggestions = \"auto\"\n\n[tags.rules]\n\"github.com\" = [\"code\"]\n\
                    \"docs.rs\" = [\"rust\", \"docs\"]\n";
        let c = parse(text).unwrap();
        assert_eq!(c.tags.suggestions, SuggestMode::Auto);
        let rules: Vec<_> = c.tags.rules.iter().collect();
        assert_eq!(
            rules,
            [
                ("docs.rs", &["rust".to_string(), "docs".into()][..]),
                ("github.com", &["code".to_string()][..]),
            ]
        );
        assert_eq!(
            c.to_toml(),
            "strict_tags = false\n\n[tags]\nsuggestions = \"auto\"\n\n[tags.rules]\n\
             \"docs.rs\" = [\"rust\", \"docs\"]\n\"github.com\" = [\"code\"]\n"
        );
        assert!(parse(&c.to_toml()).is_ok());

        let e = parse("[tags]\nsuggestions = \"always\"\n")
            .unwrap_err()
            .to_string();
        assert!(
            e.starts_with("/c.toml:2:15: unknown variant `always`"),
            "{e}"
        );
        let e = parse("[tags.rules]\n\"GitHub.com\" = [\"code\"]\n")
            .unwrap_err()
            .to_string();
        assert!(e.starts_with("/c.toml:1:1: "), "{e}");
        assert!(
            e.contains("rule \"GitHub.com\": not a normalized host; write \"github.com\""),
            "{e}"
        );
    }

    #[test]
    fn strict_rules_must_use_declared_tags() {
        let text = "strict_tags = true\n[tags.canonical]\njavascript = [\"js\"]\ncode = []\n\
                    [tags.rules]\n\"github.com\" = [\"code\"]\n\"nodejs.org\" = [\"js\", \"node\"]\n";
        let e = parse(text).unwrap_err().to_string();
        assert_eq!(
            e,
            "/c.toml:7:16: rule \"nodejs.org\": not in [tags.canonical] (strict_tags is on): node"
        );
        // Aliases count; without strict mode, or without a declared list, nothing is checked here.
        assert!(parse(&text.replace(", \"node\"", "")).is_ok());
        assert!(parse(&text.replace("strict_tags = true", "")).is_ok());
        assert!(parse("strict_tags = true\n[tags.rules]\n\"a.com\" = [\"x\"]\n").is_ok());
    }

    #[test]
    fn line_col_counts_chars() {
        assert_eq!(line_col("ab\ncé d", 7), (2, 4));
        assert_eq!(line_col("", 0), (1, 1));
    }
}
