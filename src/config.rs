//! The optional config file. shlv only reads it, so it can be a read-only symlink into the Nix
//! store. Commands that don't need settings never load it.

use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};

/// Settings from `config.toml`. Unknown keys are errors, so typos surface; add new settings as
/// `#[serde(default)]` fields.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// `add`, `edit`, and import reject tags that aren't canonical or an alias of one.
    #[serde(default)]
    pub strict_tags: bool,
    /// Sections for features this build doesn't have yet. They're accepted, unchecked, so one
    /// config works across builds.
    #[serde(default, skip_serializing)]
    ai: Option<IgnoredAny>,
    #[serde(default, skip_serializing)]
    provider: Option<IgnoredAny>,
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
        toml::from_str(text).map_err(|e| {
            let msg = e.message().trim_end();
            match e.span() {
                Some(span) => {
                    let (line, col) = line_col(text, span.start);
                    anyhow!("{}:{line}:{col}: {msg}", path.display())
                }
                None => anyhow!("{}: {msg}", path.display()),
            }
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
    fn line_col_counts_chars() {
        assert_eq!(line_col("ab\ncé d", 7), (2, 4));
        assert_eq!(line_col("", 0), (1, 1));
    }
}
