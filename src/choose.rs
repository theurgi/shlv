//! The "choose tags" call shared by every tagging layer that picks from the canonical list (the
//! in-binary classifier, LLM providers, decision-model providers): a bookmark and the list in,
//! tags with probabilities out. Domain rules (layer 1) don't use it; they map hosts to tags.

use std::collections::HashMap;

use anyhow::Result;

use crate::store::Bookmark;
use crate::tag::TagInfo;

/// What a layer knows about a bookmark.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Item {
    pub url: String,
    pub title: Option<String>,
    pub note: Option<String>,
    pub summary: Option<String>,
    /// The page's main text, when it was fetched for this call (never stored).
    pub text: Option<String>,
}

impl Item {
    pub fn from_bookmark(b: &Bookmark) -> Self {
        Item {
            url: b.url.clone(),
            title: b.title.clone(),
            note: b.note.clone(),
            summary: b.summary.clone(),
            text: None,
        }
    }
}

/// One canonical tag a layer may choose.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagOption {
    pub name: String,
    pub aliases: Vec<String>,
    pub description: Option<String>,
}

impl From<&TagInfo> for TagOption {
    fn from(t: &TagInfo) -> Self {
        TagOption {
            name: t.name.clone(),
            aliases: t.aliases.clone(),
            description: t.description.clone(),
        }
    }
}

/// A chosen tag, with the layer's probability that it applies when it has one.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub tag: String,
    pub confidence: Option<f64>,
}

/// A tagging layer that chooses from the canonical list.
pub trait ChooseTags {
    /// Tags from `list` that fit `item`, most likely first. Implementations must only return
    /// names from `list`; [`Canonical::resolve`] helps.
    fn choose_tags(&self, item: &Item, list: &[TagOption]) -> Result<Vec<Choice>>;
}

/// Name and alias lookup over a canonical list.
pub struct Canonical<'a> {
    names: HashMap<&'a str, &'a str>,
}

impl<'a> Canonical<'a> {
    pub fn new(list: &'a [TagOption]) -> Self {
        let mut names = HashMap::new();
        for t in list {
            names.insert(t.name.as_str(), t.name.as_str());
            for a in &t.aliases {
                names.insert(a.as_str(), t.name.as_str());
            }
        }
        Canonical { names }
    }

    /// The canonical name for `name` (normalized first), if it or an alias is in the list.
    pub fn resolve(&self, name: &str) -> Option<&'a str> {
        let n = crate::store::normalize_tag(name);
        self.names.get(n.as_str()).copied()
    }

    /// Resolves raw choices: unknown names are dropped (returned second), aliases become their
    /// tags, and a tag chosen twice keeps its first (most likely) entry with the higher
    /// confidence.
    pub fn filter(&self, raw: Vec<Choice>) -> (Vec<Choice>, Vec<String>) {
        let mut out: Vec<Choice> = Vec::new();
        let mut unknown = Vec::new();
        for c in raw {
            match self.resolve(&c.tag) {
                Some(name) => match out.iter_mut().find(|o| o.tag == name) {
                    Some(o) => {
                        o.confidence = match (o.confidence, c.confidence) {
                            (Some(a), Some(b)) => Some(a.max(b)),
                            (a, b) => a.or(b),
                        }
                    }
                    None => out.push(Choice {
                        tag: name.to_string(),
                        confidence: c.confidence,
                    }),
                },
                None => unknown.push(c.tag),
            }
        }
        (out, unknown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_choices() {
        let list = [
            TagOption {
                name: "javascript".into(),
                aliases: vec!["js".into()],
                description: None,
            },
            TagOption {
                name: "rust".into(),
                ..Default::default()
            },
        ];
        let c = Canonical::new(&list);
        assert_eq!(c.resolve(" JS "), Some("javascript"));
        assert_eq!(c.resolve("#rust"), Some("rust"));
        assert_eq!(c.resolve("go"), None);
        let ch = |t: &str, p: Option<f64>| Choice {
            tag: t.into(),
            confidence: p,
        };
        let (ok, unknown) = c.filter(vec![
            ch("js", Some(0.4)),
            ch("go", Some(0.9)),
            ch("Rust", None),
            ch("javascript", Some(0.7)),
        ]);
        assert_eq!(ok, [ch("javascript", Some(0.7)), ch("rust", None)]);
        assert_eq!(unknown, ["go"]);
    }
}
