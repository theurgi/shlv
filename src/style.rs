//! Tag style (`[tags.style]`): mechanical rules applied wherever a tag name enters (typed tags,
//! import, config, proposals, AI output), and plain-language guidelines sent to the AI.
//!
//! Without a style, names are only normalized ([`crate::store::normalize_tag`]). With one, a name
//! that can be fixed is fixed (`Sci_Fi` becomes `sci-fi` with `separator = "-"`), and one that
//! can't (too long) is refused with the reason. Names that already resolve to a tag or alias are
//! left alone: style governs new names, and tag health reports old ones that break it.

use serde::{Deserialize, Serialize};

use crate::store::normalize_tag;

/// What spaces and the other word separators (`-`, `_`) in a tag name become.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum Separator {
    #[serde(rename = "-")]
    Hyphen,
    #[serde(rename = "_")]
    Underscore,
    /// Words run together.
    #[serde(rename = "")]
    None,
}

impl Separator {
    pub fn as_str(self) -> &'static str {
        match self {
            Separator::Hyphen => "-",
            Separator::Underscore => "_",
            Separator::None => "",
        }
    }
}

/// The mechanical rules. The default is no rules: names are only normalized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TagStyle {
    pub separator: Option<Separator>,
    /// Most characters in a tag name.
    pub max_length: Option<usize>,
}

impl TagStyle {
    pub fn is_default(&self) -> bool {
        *self == TagStyle::default()
    }

    /// Normalizes `name` and fixes what can be fixed: with a separator, runs of whitespace, `-`,
    /// and `_` become one separator, and none is left at either end.
    pub fn fix(&self, name: &str) -> String {
        let n = normalize_tag(name);
        let Some(sep) = self.separator else { return n };
        n.split(['-', '_'])
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>()
            .join(sep.as_str())
    }

    /// Why a (fixed) name can't be used, if it can't.
    pub fn check(&self, name: &str) -> Result<(), String> {
        match self.max_length {
            Some(max) if name.chars().count() > max => Err(format!(
                "{name:?} is longer than [tags.style] max_length = {max}"
            )),
            _ => Ok(()),
        }
    }

    /// How an existing name breaks the style, if it does: for tag health and config.
    pub fn violation(&self, name: &str) -> Option<String> {
        let fixed = self.fix(name);
        if fixed != name {
            return Some(format!("{name:?} breaks [tags.style]; write {fixed:?}"));
        }
        self.check(name).err()
    }
}

/// The `[tags.style]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StyleConfig {
    /// What spaces and the other separators become: `"-"`, `"_"`, or `""`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub separator: Option<Separator>,
    /// Most characters in a tag name; 0 is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<std::num::NonZeroUsize>,
    /// Plain-language rules sent to the AI with every tagging request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guidelines: Option<String>,
}

impl StyleConfig {
    pub fn is_empty(&self) -> bool {
        *self == StyleConfig::default()
    }

    pub fn tag_style(&self) -> TagStyle {
        TagStyle {
            separator: self.separator,
            max_length: self.max_length.map(std::num::NonZeroUsize::get),
        }
    }

    /// The guidelines, trimmed, if any.
    pub fn guidelines(&self) -> Option<&str> {
        self.guidelines
            .as_deref()
            .map(str::trim)
            .filter(|g| !g.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style(sep: Option<Separator>, max: Option<usize>) -> TagStyle {
        TagStyle {
            separator: sep,
            max_length: max,
        }
    }

    #[test]
    fn default_is_plain_normalization() {
        let s = TagStyle::default();
        assert_eq!(s.fix(" Sci Fi "), "sci-fi");
        assert_eq!(s.fix("sci_fi"), "sci_fi");
        assert_eq!(s.check(&"x".repeat(200)), Ok(()));
        assert_eq!(s.violation("sci_fi"), None);
    }

    #[test]
    fn fixes_separators() {
        let h = style(Some(Separator::Hyphen), None);
        assert_eq!(h.fix("Sci Fi"), "sci-fi");
        assert_eq!(h.fix("sci_fi"), "sci-fi");
        assert_eq!(h.fix("-a - _b_ "), "a-b");
        assert_eq!(h.fix("c++"), "c++");
        assert_eq!(h.fix("---"), "");
        let u = style(Some(Separator::Underscore), None);
        assert_eq!(u.fix("home-lab"), "home_lab");
        assert_eq!(u.fix("#Home Lab"), "home_lab");
        let n = style(Some(Separator::None), None);
        assert_eq!(n.fix("home-lab"), "homelab");
        assert_eq!(n.fix("Home Lab"), "homelab");
    }

    #[test]
    fn checks_length_in_chars() {
        let s = style(None, Some(4));
        assert_eq!(s.check("rust"), Ok(()));
        assert_eq!(s.check("café"), Ok(()));
        assert_eq!(
            s.check("rusty").unwrap_err(),
            "\"rusty\" is longer than [tags.style] max_length = 4"
        );
    }

    #[test]
    fn reports_violations() {
        let s = style(Some(Separator::Underscore), Some(8));
        assert_eq!(s.violation("home_lab"), None);
        assert_eq!(
            s.violation("home-lab").unwrap(),
            "\"home-lab\" breaks [tags.style]; write \"home_lab\""
        );
        assert!(s.violation("security").is_none());
        assert!(
            s.violation("networking")
                .unwrap()
                .contains("max_length = 8")
        );
    }

    #[test]
    fn parses_section() {
        let c: StyleConfig =
            toml::from_str("separator = \"\"\nmax_length = 3\nguidelines = \" x \"\n").unwrap();
        assert_eq!(c.tag_style(), style(Some(Separator::None), Some(3)));
        assert_eq!(c.guidelines(), Some("x"));
        assert!(toml::from_str::<StyleConfig>("separator = \".\"").is_err());
        assert!(toml::from_str::<StyleConfig>("sep = \"-\"").is_err());
    }
}
