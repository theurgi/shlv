//! The optional config file. shlv only reads it, so it can be a read-only symlink into the Nix
//! store. Commands that don't need settings never load it.

use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
#[cfg(feature = "ai")]
use std::collections::BTreeMap;

#[cfg(not(feature = "ai"))]
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
#[cfg(feature = "ai")]
use toml::Spanned;

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
    /// The tag classifier (feature `model`): thresholds and when to ask the provider. Parsed in
    /// every build so one config works across builds.
    #[serde(default, skip_serializing_if = "ModelConfig::is_default")]
    pub model: ModelConfig,
    /// Which provider does which AI task.
    #[cfg(feature = "ai")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai: Option<crate::ai::AiConfig>,
    /// AI providers by name.
    #[cfg(feature = "ai")]
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub provider: BTreeMap<String, Spanned<crate::ai::ProviderConfig>>,
    /// Without the `ai` feature, its sections are accepted unchecked, so one config works
    /// across builds.
    #[cfg(not(feature = "ai"))]
    #[serde(default, skip_serializing)]
    ai: Option<IgnoredAny>,
    #[cfg(not(feature = "ai"))]
    #[serde(default, skip_serializing)]
    provider: Option<IgnoredAny>,
}

/// The `[model]` section: how the tag classifier's suggestions are used (feature `model`).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ModelConfig {
    /// Offer classifier tags with at least this probability (025 measured 67% precision at 0.5).
    pub threshold: f64,
    /// With `[tags] suggestions = "auto"`, apply classifier tags only from this probability up
    /// (84% precision at 0.9 in 025 and 022); the rest wait for review.
    pub auto_threshold: f64,
    /// When `suggest` also asks the `[ai] tag` provider.
    pub provider: ProviderUse,
}

impl Default for ModelConfig {
    fn default() -> Self {
        ModelConfig {
            threshold: 0.5,
            auto_threshold: 0.9,
            provider: ProviderUse::Fallback,
        }
    }
}

impl ModelConfig {
    fn is_default(&self) -> bool {
        *self == ModelConfig::default()
    }

    fn check(&self) -> Result<(), String> {
        for (name, v) in [
            ("threshold", self.threshold),
            ("auto_threshold", self.auto_threshold),
        ] {
            if !(0.0..=1.0).contains(&v) {
                return Err(format!("[model] {name} must be between 0 and 1, not {v}"));
            }
        }
        Ok(())
    }
}

/// When the `[ai] tag` provider runs, with the classifier built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderUse {
    /// For every bookmark `suggest` handles.
    Always,
    /// Only where the classifier is unsure: nothing at or above `threshold`, or its likeliest
    /// tags have too few examples to trust.
    #[default]
    Fallback,
    /// Never; the classifier and rules only.
    Never,
}
/// The `[tags]` section.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Tags {
    /// The declared canonical list, `tag = [aliases]`: a pinned base that the database's list is
    /// built on; see [`crate::Store::pin`].
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
        if let Err(msg) = config.model.check() {
            return Err(at(None, &msg));
        }
        #[cfg(feature = "ai")]
        if let Some((pos, msg)) = config.bad_ai() {
            return Err(at(Some(pos), &msg));
        }
        Ok(config)
    }

    /// The effective config as TOML, defaults included.
    pub fn to_toml(&self) -> String {
        toml::to_string(self).expect("config serializes")
    }

    /// Sections present in the file that this build ignores.
    pub fn ignored_sections(&self) -> Vec<&'static str> {
        #[cfg(not(feature = "ai"))]
        {
            let mut v = Vec::new();
            if self.ai.is_some() {
                v.push("ai");
            }
            if self.provider.is_some() {
                v.push("provider");
            }
            v
        }
        #[cfg(feature = "ai")]
        Vec::new()
    }

    /// A provider section that breaks its kind's rules, or an `[ai]` task naming a provider
    /// that doesn't exist: the byte offset to report and the message.
    #[cfg(feature = "ai")]
    fn bad_ai(&self) -> Option<(usize, String)> {
        for (name, p) in &self.provider {
            if let Err(e) = p.get_ref().check() {
                return Some((p.span().start, format!("[provider.{name}]: {e}")));
            }
        }
        let ai = self.ai.as_ref()?;
        [("summarize", &ai.summarize), ("tag", &ai.tag)]
            .into_iter()
            .find_map(|(task, name)| {
                let name = name.as_ref()?;
                (!self.provider.contains_key(name.get_ref())).then(|| {
                    (
                        name.span().start,
                        format!(
                            "[ai] {task} = {:?}: no such provider; define [provider.{}]",
                            name.get_ref(),
                            name.get_ref()
                        ),
                    )
                })
            })
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

    #[cfg(not(feature = "ai"))]
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

    #[cfg(feature = "ai")]
    #[test]
    fn parses_ai_providers() {
        let text = "[ai]\nsummarize = \"claude\"\ntag = \"local\"\n\n\
                    [provider.local]\nkind = \"openai-compatible\"\n\
                    base_url = \"http://localhost:11434/v1\"\nmodel = \"qwen\"\n\n\
                    [provider.claude]\nkind = \"anthropic\"\nmodel = \"m\"\n\
                    api_key_env = \"ANTHROPIC_API_KEY\"\ntimeout = 30\n";
        let c = parse(text).unwrap();
        assert!(c.ignored_sections().is_empty());
        let ai = c.ai.as_ref().unwrap();
        assert_eq!(ai.tag.as_ref().unwrap().get_ref(), "local");
        let local = c.provider["local"].get_ref();
        assert_eq!(local.kind, crate::ai::Kind::OpenaiCompatible);
        assert_eq!(local.api_key_env, None);
        assert_eq!(c.provider["claude"].get_ref().timeout().as_secs(), 30);
        // `config show` prints providers (by name, sorted) and reads back.
        let shown = c.to_toml();
        assert!(
            shown.contains("[provider.claude]\nkind = \"anthropic\""),
            "{shown}"
        );
        assert!(
            shown.contains("api_key_env = \"ANTHROPIC_API_KEY\""),
            "{shown}"
        );
        parse(&shown).unwrap();
        // AI stays off until configured.
        assert!(parse("").unwrap().ai.is_none());
    }

    #[cfg(feature = "ai")]
    #[test]
    fn ai_errors_have_positions() {
        let local = "[provider.local]\nkind = \"openai-compatible\"\nbase_url = \"http://h/v1\"\n\
                     model = \"m\"\n";
        let e = |t: &str| parse(t).unwrap_err().to_string();
        let err = e(&format!("[ai]\ntag = \"locl\"\n{local}"));
        assert!(
            err.starts_with(
                "/c.toml:2:7: [ai] tag = \"locl\": no such provider; define [provider.locl]"
            ),
            "{err}"
        );
        let err = e(&format!("{local}api_key = \"sk-x\"\n"));
        assert!(
            err.starts_with("/c.toml:5:1: unknown field `api_key`"),
            "{err}"
        );
        let err = e("[provider.x]\nkind = \"openai\"\nmodel = \"m\"\n");
        assert!(
            err.starts_with("/c.toml:2:8: unknown variant `openai`"),
            "{err}"
        );
        let err = e("[provider.x]\nkind = \"anthropic\"\n");
        assert!(err.contains("missing field `model`"), "{err}");
        let err = e("[provider.x]\nkind = \"openai-compatible\"\nmodel = \"m\"\n");
        assert!(
            err.starts_with(
                "/c.toml:1:1: [provider.x]: kind \"openai-compatible\" needs `base_url`"
            ),
            "{err}"
        );
        let err = e("[ai]\nmodel = \"x\"\n");
        assert!(
            err.starts_with("/c.toml:2:1: unknown field `model`"),
            "{err}"
        );
    }

    #[cfg(feature = "ai")]
    #[test]
    fn api_key_file_is_exclusive_and_shown_as_a_path() {
        let p = "[provider.c]\nkind = \"anthropic\"\nmodel = \"m\"\n";
        let c = parse(&format!("{p}api_key_file = \"/run/agenix/anthropic\"\n")).unwrap();
        assert!(
            c.to_toml()
                .contains("api_key_file = \"/run/agenix/anthropic\"")
        );
        let e = parse(&format!("{p}api_key_file = \"/k\"\napi_key_env = \"K\"\n"))
            .unwrap_err()
            .to_string();
        assert_eq!(
            e,
            "/c.toml:1:1: [provider.c]: set `api_key_env` or `api_key_file`, not both"
        );
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
        // Present but empty is a declared (empty) list.
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
    fn declared_table_form_with_descriptions() {
        let text = "[tags.canonical]\nrust = [\"rs\"]\n\
                    infra = { aliases = [\"ops\"], description = \"servers, deployment, Nix, CI\" }\n\
                    web = { description = \" browsers \" }\n";
        let c = parse(text).unwrap();
        let d = c.tags.canonical.as_ref().unwrap();
        assert_eq!(
            d.iter().collect::<Vec<_>>(),
            [
                ("infra", &["ops".to_string()][..]),
                ("rust", &["rs".to_string()][..]),
                ("web", &[][..])
            ]
        );
        assert_eq!(d.description("infra"), Some("servers, deployment, Nix, CI"));
        assert_eq!(d.description("web"), Some("browsers"));
        assert_eq!(d.description("rust"), None);
        // `config show` writes the table form only where there's a description, and reads back.
        let shown = c.to_toml();
        assert!(shown.contains("rust = [\"rs\"]"), "{shown}");
        let again = parse(&shown).unwrap();
        assert_eq!(again.tags.canonical.as_ref().unwrap(), d);

        let e = parse("[tags.canonical]\ninfra = { alias = [\"ops\"] }\n")
            .unwrap_err()
            .to_string();
        assert!(e.starts_with("/c.toml:2:11: unknown field `alias`"), "{e}");
        let e = parse("[tags.canonical]\ninfra = { description = \"a\\nb\" }\n")
            .unwrap_err()
            .to_string();
        assert!(
            e.starts_with("/c.toml:2:9: a tag description must be one line"),
            "{e}"
        );
        let e = parse("[tags.canonical]\ninfra = 3\n")
            .unwrap_err()
            .to_string();
        assert!(
            e.starts_with("/c.toml:2:9: invalid type: integer `3`, expected a list of aliases"),
            "{e}"
        );
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
    fn line_col_counts_chars() {
        assert_eq!(line_col("ab\ncé d", 7), (2, 4));
        assert_eq!(line_col("", 0), (1, 1));
    }
}
