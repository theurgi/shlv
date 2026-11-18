//! The in-binary tag classifier (feature `model`): chooses tags from the canonical list, with
//! probabilities, offline, in microseconds. Task 025 picked the approach on a public dataset
//! (HetRec 2011 Delicious): one-vs-rest logistic regression over hashed tokens beat naive Bayes
//! and was well calibrated, and a lexical match against tag names, aliases, and descriptions is
//! the best cold start.
//!
//! - **Features:** hashed tokens from the title (`t:`), the registrable domain (`d:`), host
//!   labels (`h:`), URL path words (`p:`), the summary (`s:`), and the note (`n:`), into
//!   [`BUCKETS`] buckets.
//! - **Training examples:** bookmarks with at least one canonical tag (positive for their tags,
//!   negative for the rest) and rejected suggestions (negative for that tag only). Untagged
//!   bookmarks say nothing about tags, so they aren't examples.
//! - **Cold start:** a tag with fewer than [`FULL_AT`] examples blends in the lexical score, whose
//!   probabilities are the precision 025 measured for each kind of match.

use std::collections::{HashMap, HashSet};

use anyhow::{Result, bail};

use crate::choose::{Choice, ChooseTags, Item, TagOption};

/// Hash buckets. 025: 2^16 was within 0.01 of 2^18 at P@1 and a quarter of the size.
pub const BUCKETS: usize = 1 << 16;

/// Examples of a tag at which the trained model fully replaces the lexical score. 038: at 50, P@1
/// dipped at 1k-2k bookmarks (0.41, against 0.43 at 500), as tags lost the lexical score before
/// their model was good; 100 gave 0.45 and 0.44 there, the same from 5k up, and 0.01 less at 200.
pub const FULL_AT: usize = 100;

/// Choices below this probability aren't returned by [`ChooseTags`].
pub const MIN_REPORTED: f64 = 0.05;

const EPOCHS: usize = 5;
const LEARNING_RATE: f32 = 0.1;
const L2: f32 = 1e-6;
/// Weights smaller than this are dropped when the model is stored.
const PRUNE: f32 = 0.2;

const STOP: &[&str] = &[
    "the", "and", "for", "with", "you", "your", "from", "how", "what", "this", "that", "are",
    "www", "com", "org", "net", "http", "https", "html", "htm", "php", "index", "home", "page",
    "of", "to", "in", "on", "an", "is", "it", "by", "at", "or", "be", "as", "de",
];

fn words(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|w| {
            w.chars().count() >= 2
                && !STOP.contains(&w.as_str())
                && !w.chars().all(|c| c.is_ascii_digit())
        })
}

/// The tokens a bookmark is described by, before hashing.
pub fn tokens(item: &Item) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    let mut add = |prefix: &str, text: &Option<String>| {
        if let Some(t) = text {
            v.extend(words(t).map(|w| format!("{prefix}{w}")));
        }
    };
    add("t:", &item.title);
    add("s:", &item.summary);
    add("n:", &item.note);
    let rest = item
        .url
        .split_once("://")
        .map_or(item.url.as_str(), |(_, r)| r);
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host
        .rsplit_once('@')
        .map_or(host, |(_, h)| h)
        .split(':')
        .next()
        .unwrap_or("")
        .to_lowercase();
    let labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
    if labels.len() >= 2 {
        v.push(format!("d:{}", labels[labels.len() - 2..].join(".")));
    }
    v.extend(words(&host).map(|w| format!("h:{w}")));
    v.extend(
        words(path)
            .filter(|w| w.chars().count() >= 3)
            .map(|w| format!("p:{w}")),
    );
    v.sort();
    v.dedup();
    v
}

fn hash(s: &str) -> u32 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (h % BUCKETS as u64) as u32
}

/// Hashed features of a bookmark: sorted, distinct bucket numbers.
pub fn features(item: &Item) -> Vec<u32> {
    let mut f: Vec<u32> = tokens(item).iter().map(|t| hash(t)).collect();
    f.sort_unstable();
    f.dedup();
    f
}

/// The zero-shot layer: bookmark words against each tag's name, aliases, and description.
pub struct Lexical {
    /// Per tag: name and alias terms, and description words.
    terms: Vec<(HashSet<String>, HashSet<String>)>,
}

impl Lexical {
    pub fn new(list: &[TagOption]) -> Self {
        let terms = list
            .iter()
            .map(|t| {
                let mut names = HashSet::new();
                for n in std::iter::once(&t.name).chain(&t.aliases) {
                    names.extend(n.split(['-', '_']).map(str::to_string));
                    names.insert(n.replace(['-', '_'], ""));
                }
                let desc: HashSet<String> = t
                    .description
                    .as_deref()
                    .map(|d| words(d).filter(|w| !names.contains(w)).collect())
                    .unwrap_or_default();
                (names, desc)
            })
            .collect();
        Lexical { terms }
    }

    /// One probability per tag, in list order. The values are the precision 025 measured on
    /// Delicious for each kind of match: two or more name or alias terms 0.65, one 0.45, two or
    /// more description words 0.15, one 0.05. Lexical guesses never reach auto-apply levels.
    pub fn probabilities(&self, item: &Item) -> Vec<f64> {
        let mut ws: HashSet<String> = HashSet::new();
        for t in [&item.title, &item.summary, &item.note]
            .into_iter()
            .flatten()
        {
            ws.extend(words(t));
        }
        ws.extend(words(&item.url));
        self.terms
            .iter()
            .map(|(names, desc)| {
                let n = names.iter().filter(|w| ws.contains(*w)).count();
                let d = desc.iter().filter(|w| ws.contains(*w)).count();
                match (n, d) {
                    (2.., _) => 0.65,
                    (1, _) => 0.45,
                    (0, 2..) => 0.15,
                    (0, 1) => 0.05,
                    _ => 0.0,
                }
            })
            .collect()
    }
}

/// A training example.
#[derive(Debug, Clone)]
pub struct Example {
    pub features: Vec<u32>,
    pub positive: HashSet<String>,
    /// `None`: every other tag is a negative (a tagged bookmark). `Some`: only these are (a
    /// rejected suggestion on a bookmark that may be untagged otherwise).
    pub negative: Option<HashSet<String>>,
}

/// One tag's trained weights.
#[derive(Debug, Clone, PartialEq)]
pub struct TagModel {
    pub name: String,
    /// Positive examples seen in training.
    pub positives: u32,
    pub bias: f32,
    /// Sparse weights, by bucket.
    pub weights: Vec<(u16, f32)>,
}

impl TagModel {
    /// The log-odds of the tag for a bookmark's hashed features.
    pub fn logit(&self, features: &[u32]) -> f32 {
        let mut z = self.bias;
        for &f in features {
            if let Ok(i) = self
                .weights
                .binary_search_by_key(&f, |(j, _)| u32::from(*j))
            {
                z += self.weights[i].1;
            }
        }
        z
    }
}

/// A trained classifier: per-tag logistic regression.
#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    pub tags: Vec<TagModel>,
    /// Training examples used.
    pub examples: u32,
}

/// Trains a model for the tags in `list` (plain SGD, a fixed seed, so the same data gives the
/// same model).
pub fn train(examples: &[Example], list: &[TagOption]) -> Model {
    let n_tags = list.len();
    let mut w = vec![vec![0f32; BUCKETS]; n_tags];
    let mut b = vec![0f32; n_tags];
    let mut order: Vec<usize> = (0..examples.len()).collect();
    let mut rng = 0x2545_f491_4f6c_dd1du64;
    for _ in 0..EPOCHS {
        for k in (1..order.len()).rev() {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            order.swap(k, (rng % (k as u64 + 1)) as usize);
        }
        for &n in &order {
            let ex = &examples[n];
            for (t, tag) in list.iter().enumerate() {
                let y = if ex.positive.contains(&tag.name) {
                    1.0
                } else if ex
                    .negative
                    .as_ref()
                    .is_none_or(|neg| neg.contains(&tag.name))
                {
                    0.0
                } else {
                    continue;
                };
                let wt = &mut w[t];
                let z: f32 = b[t] + ex.features.iter().map(|&j| wt[j as usize]).sum::<f32>();
                let g = 1.0 / (1.0 + (-z).exp()) - y;
                b[t] -= LEARNING_RATE * g;
                for &j in &ex.features {
                    let j = j as usize;
                    wt[j] -= LEARNING_RATE * (g + L2 * wt[j]);
                }
            }
        }
    }
    let tags = list
        .iter()
        .enumerate()
        .map(|(t, tag)| TagModel {
            name: tag.name.clone(),
            positives: examples
                .iter()
                .filter(|e| e.positive.contains(&tag.name))
                .count() as u32,
            bias: b[t],
            weights: quantize(&w[t]),
        })
        .collect();
    Model {
        tags,
        examples: examples.len() as u32,
    }
}

/// Prunes small weights and rounds the rest to 8-bit steps of the tag's largest weight, as
/// stored; training returns these values, so evaluation measures the shipped model.
fn quantize(w: &[f32]) -> Vec<(u16, f32)> {
    let max = w.iter().fold(0f32, |m, x| m.max(x.abs()));
    if max == 0.0 {
        return Vec::new();
    }
    let step = max / 127.0;
    w.iter()
        .enumerate()
        .filter(|(_, x)| x.abs() >= PRUNE)
        .map(|(j, x)| (j as u16, (x / step).round().clamp(-127.0, 127.0) * step))
        .filter(|(_, x)| *x != 0.0)
        .collect()
}
const MAGIC: &[u8; 8] = b"SHLVLR02";

impl Model {
    /// A compact little-endian encoding, for storing in the database.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            16 + self
                .tags
                .iter()
                .map(|t| 32 + t.weights.len() * 6)
                .sum::<usize>(),
        );
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.examples.to_le_bytes());
        out.extend_from_slice(&(self.tags.len() as u32).to_le_bytes());
        for t in &self.tags {
            out.extend_from_slice(&(t.name.len() as u16).to_le_bytes());
            out.extend_from_slice(t.name.as_bytes());
            out.extend_from_slice(&t.positives.to_le_bytes());
            out.extend_from_slice(&t.bias.to_le_bytes());
            // Weights are multiples of one step (see `quantize`): stored as 8-bit counts of it.
            let step = t.weights.iter().fold(0f32, |m, (_, x)| m.max(x.abs())) / 127.0;
            out.extend_from_slice(&step.to_le_bytes());
            out.extend_from_slice(&(t.weights.len() as u32).to_le_bytes());
            for (j, x) in &t.weights {
                out.extend_from_slice(&j.to_le_bytes());
                out.push((x / step).round() as i8 as u8);
            }
        }
        out
    }

    pub fn from_bytes(b: &[u8]) -> Result<Model> {
        let mut r = Reader { b, at: 0 };
        if r.take(8)? != MAGIC {
            bail!("not a shlv model (or an older format; retrain with `shlv model train`)");
        }
        let examples = r.u32()?;
        let n = r.u32()? as usize;
        let mut tags = Vec::with_capacity(n.min(10_000));
        for _ in 0..n {
            let len = r.u16()? as usize;
            let name = String::from_utf8(r.take(len)?.to_vec())?;
            let positives = r.u32()?;
            let bias = r.f32()?;
            let step = r.f32()?;
            let nw = r.u32()? as usize;
            let mut weights = Vec::with_capacity(nw.min(BUCKETS));
            for _ in 0..nw {
                let j = r.u16()?;
                let q = r.take(1)?[0] as i8;
                weights.push((j, f32::from(q) * step));
            }
            tags.push(TagModel {
                name,
                positives,
                bias,
                weights,
            });
        }
        Ok(Model { tags, examples })
    }
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let s = self
            .b
            .get(self.at..self.at + n)
            .ok_or_else(|| anyhow::anyhow!("truncated model"))?;
        self.at += n;
        Ok(s)
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into()?))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }
    fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(self.take(4)?.try_into()?))
    }
}

/// The classifier as a tagging layer: the trained model where a tag has examples, the lexical
/// score where it has few or none.
pub struct Classifier {
    list: Vec<TagOption>,
    lexical: Lexical,
    /// Per list entry, its trained weights (matched by name), if any.
    trained: Vec<Option<TagModel>>,
}

impl Classifier {
    pub fn new(list: &[TagOption], model: Option<&Model>) -> Self {
        let by_name: HashMap<&str, &TagModel> = model
            .map(|m| m.tags.iter().map(|t| (t.name.as_str(), t)).collect())
            .unwrap_or_default();
        Classifier {
            list: list.to_vec(),
            lexical: Lexical::new(list),
            trained: list
                .iter()
                .map(|t| by_name.get(t.name.as_str()).map(|m| (*m).clone()))
                .collect(),
        }
    }

    /// Every tag's probability, in list order.
    pub fn probabilities(&self, item: &Item) -> Vec<(String, f64)> {
        let f = features(item);
        let lex = self.lexical.probabilities(item);
        self.list
            .iter()
            .zip(&self.trained)
            .zip(lex)
            .map(|((tag, m), lx)| {
                let p = match m {
                    Some(m) if m.positives > 0 => {
                        let p = 1.0 / (1.0 + (-f64::from(m.logit(&f))).exp());
                        let w = (f64::from(m.positives) / FULL_AT as f64).min(1.0);
                        w * p + (1.0 - w) * lx
                    }
                    _ => lx,
                };
                (tag.name.clone(), p)
            })
            .collect()
    }

    /// The classifier narrowed to one tag of its list (for back-filling it), or `None` if the
    /// tag isn't on it.
    pub fn only(&self, tag: &str) -> Option<Classifier> {
        let i = self.list.iter().position(|t| t.name == tag)?;
        let list = &self.list[i..=i];
        Some(Classifier {
            list: list.to_vec(),
            lexical: Lexical::new(list),
            trained: vec![self.trained[i].clone()],
        })
    }

    /// Whether a tag's probability comes only from the lexical match (it has no examples). A tag
    /// that isn't cold counts toward "sure" for the provider fallback even below [`FULL_AT`]: 041
    /// measured stricter rules on 200-5k bookmarks and they sent more bookmarks to the provider
    /// without raising the precision of the rest.
    pub fn is_cold(&self, tag: &str) -> bool {
        self.list
            .iter()
            .zip(&self.trained)
            .find(|(t, _)| t.name == tag)
            .is_none_or(|(_, m)| m.as_ref().is_none_or(|m| m.positives == 0))
    }
}

impl ChooseTags for Classifier {
    fn choose_tags(&self, item: &Item, list: &[TagOption]) -> Result<Vec<Choice>> {
        let allowed: HashSet<&str> = list.iter().map(|t| t.name.as_str()).collect();
        let mut v: Vec<Choice> = self
            .probabilities(item)
            .into_iter()
            .filter(|(t, p)| *p >= MIN_REPORTED && allowed.contains(t.as_str()))
            .map(|(tag, p)| Choice {
                tag,
                confidence: Some(p),
            })
            .collect();
        v.sort_by(|a, b| {
            b.confidence
                .unwrap_or(0.0)
                .total_cmp(&a.confidence.unwrap_or(0.0))
        });
        Ok(v)
    }
}

/// What [`Store::train_model`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trained {
    pub examples: usize,
    pub tags: usize,
    /// Size of the stored model.
    pub bytes: usize,
    pub millis: u128,
}

/// A stored model and when it was trained.
pub struct Stored {
    pub model: Model,
    pub trained_at: i64,
    pub bytes: usize,
    /// Whether the tags changed since (it would be retrained before use by `suggest`).
    pub stale: bool,
}

/// The canonical list as the classifier sees it: names, aliases, descriptions. Unlike
/// [`crate::Store::canonical_tags`] it doesn't count bookmarks, which costs milliseconds at 100k.
pub fn canonical_list(store: &crate::Store) -> Result<Vec<TagOption>> {
    let mut stmt = store.conn().prepare_cached(
        "SELECT t.name, t.description, (SELECT group_concat(a.alias, ' ' ORDER BY a.alias) \
         FROM tag_alias a WHERE a.tag_id = t.id) FROM tag t \
         WHERE t.canonical AND t.deleted_at IS NULL ORDER BY t.name",
    )?;
    let rows = stmt.query_map([], |r| {
        let aliases: Option<String> = r.get(2)?;
        Ok(TagOption {
            name: r.get(0)?,
            description: r.get(1)?,
            aliases: aliases
                .map(|a| a.split(' ').map(str::to_string).collect())
                .unwrap_or_default(),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

impl crate::Store {
    /// Training examples: bookmarks with canonical tags, and rejected suggestions. Each comes
    /// with its creation time, for time-ordered evaluation.
    pub fn training_examples(&self) -> Result<Vec<(i64, Item, Example)>> {
        let list = canonical_list(self)?;
        let canonical: HashSet<&str> = list.iter().map(|t| t.name.as_str()).collect();
        let mut stmt = self.conn().prepare(
            "SELECT b.id, b.url, b.title, b.note, b.summary, b.created_at, \
             (SELECT group_concat(t.name, ' ') FROM bookmark_tag bt JOIN tag t ON t.id = bt.tag_id \
              WHERE bt.bookmark_id = b.id AND t.canonical), \
             (SELECT group_concat(t.name, ' ') FROM tag_suggestion s JOIN tag t ON t.id = s.tag_id \
              WHERE s.bookmark_id = b.id AND s.state = 'rejected' AND t.canonical) \
             FROM bookmark b WHERE b.deleted_at IS NULL ORDER BY b.created_at, b.id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?,
            ))
        })?;
        let split = |s: Option<String>| -> HashSet<String> {
            s.map(|s| {
                s.split(' ')
                    .filter(|t| canonical.contains(t))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
        };
        let mut out = Vec::new();
        for row in rows {
            let (url, title, note, summary, created, tags, rejected) = row?;
            let item = Item {
                url,
                title,
                note,
                summary,
                text: None,
            };
            let (positive, rejected) = (split(tags), split(rejected));
            if positive.is_empty() && rejected.is_empty() {
                continue;
            }
            let f = features(&item);
            if !positive.is_empty() {
                out.push((
                    created,
                    item,
                    Example {
                        features: f,
                        positive,
                        negative: None,
                    },
                ));
            } else {
                out.push((
                    created,
                    item,
                    Example {
                        features: f,
                        positive: HashSet::new(),
                        negative: Some(rejected),
                    },
                ));
            }
        }
        Ok(out)
    }

    /// What the training data looks like now: tag assignments, rejections, the canonical list,
    /// and the latest bookmark change. A model built from other data is stale.
    pub fn model_fingerprint(&self) -> Result<String> {
        let (tags, rejected, changed): (i64, i64, Option<i64>) = self.conn().query_row(
            "SELECT (SELECT count(*) FROM bookmark_tag), \
             (SELECT count(*) FROM tag_suggestion WHERE state = 'rejected'), \
             (SELECT max(updated_at) FROM bookmark)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let list: Vec<String> = canonical_list(self)?
            .into_iter()
            .map(|t| match t.description {
                Some(d) => format!("{}#{d}", t.name),
                None => t.name,
            })
            .collect();
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in list.join("\n").bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        Ok(format!(
            "v1:{tags}:{rejected}:{}:{h:016x}",
            changed.unwrap_or(0)
        ))
    }

    /// Trains the classifier on every example and stores it.
    pub fn train_model(&mut self) -> Result<Trained> {
        let t = std::time::Instant::now();
        let list = canonical_list(self)?;
        let examples: Vec<Example> = self
            .training_examples()?
            .into_iter()
            .map(|(_, _, e)| e)
            .collect();
        let model = train(&examples, &list);
        let bytes = model.to_bytes();
        let fp = self.model_fingerprint()?;
        self.conn().execute(
            "INSERT OR REPLACE INTO model (name, data, fingerprint, trained_at) \
             VALUES ('tags', ?1, ?2, ?3)",
            rusqlite::params![bytes, fp, crate::store::now_ms()],
        )?;
        Ok(Trained {
            examples: examples.len(),
            tags: list.len(),
            bytes: bytes.len(),
            millis: t.elapsed().as_millis(),
        })
    }

    /// The stored model, if there is one.
    pub fn stored_model(&self) -> Result<Option<Stored>> {
        self.load_model(true)
    }

    /// Loads the stored model; `check_stale` compares its fingerprint with the data now, which
    /// scans the tag tables (milliseconds at 100k bookmarks), so `add` skips it.
    fn load_model(&self, check_stale: bool) -> Result<Option<Stored>> {
        use rusqlite::OptionalExtension;
        let row: Option<(Vec<u8>, String, i64)> = self
            .conn()
            .query_row(
                "SELECT data, fingerprint, trained_at FROM model WHERE name = 'tags'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((data, fp, trained_at)) = row else {
            return Ok(None);
        };
        Ok(Some(Stored {
            bytes: data.len(),
            model: Model::from_bytes(&data)?,
            trained_at,
            stale: check_stale && fp != self.model_fingerprint()?,
        }))
    }

    /// The classifier for the current canonical list: the stored model, retrained first if
    /// `retrain_stale` and the tags changed since (or there is none). `None` model means
    /// lexical only.
    pub fn classifier(&mut self, retrain_stale: bool) -> Result<Classifier> {
        let stored = self.load_model(retrain_stale)?;
        let model = match stored {
            Some(s) if !s.stale => Some(s.model),
            Some(s) if !retrain_stale => Some(s.model),
            None if !retrain_stale => None,
            _ => {
                self.train_model()?;
                self.load_model(false)?.map(|s| s.model)
            }
        };
        Ok(Classifier::new(&canonical_list(self)?, model.as_ref()))
    }
}

/// What [`crate::Store::apply_model`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelStats {
    pub checked: usize,
    /// New pending suggestions.
    pub pending: usize,
    /// Tags applied (auto mode, at or above `auto_threshold`).
    pub applied: usize,
    /// Bookmarks the classifier was sure about: a tag at or above the threshold, and every such
    /// tag has at least one example (see [`Classifier::is_cold`]). The provider fallback skips
    /// these.
    pub sure: HashSet<i64>,
}

/// Back-fill offers a tag from this probability up, below `[model] threshold`: a tag new to the
/// list has no examples, and the lexical score of one name match is 0.45, of two description
/// words 0.15. One description word (0.05, about as often right) is left out.
pub const BACKFILL_MIN: f64 = 0.1;

impl crate::Store {
    /// Back-fills canonical tag `tag` ([`crate::Store::backfill_tag`]): runs the classifier for
    /// that tag alone on the given bookmarks (or all) and offers it from [`BACKFILL_MIN`] up, the
    /// way [`crate::Store::apply_model`] offers tags (auto mode applies it only from
    /// `config.auto_threshold`). Bookmarks with the tag, or that rejected it, are skipped. Rank
    /// the result with [`crate::Store::backfill_candidates`].
    pub fn backfill_model(
        &mut self,
        tag: &str,
        ids: Option<&[i64]>,
        classifier: &Classifier,
        config: &crate::config::ModelConfig,
    ) -> Result<ModelStats> {
        let Some(one) = classifier.only(tag) else {
            bail!("{tag:?} isn't on the classifier's canonical list");
        };
        let config = crate::config::ModelConfig {
            threshold: BACKFILL_MIN,
            ..*config
        };
        self.apply_model(ids, &one, &config)
    }

    /// Runs the classifier on the given bookmarks (or all) and offers its tags at or above
    /// `config.threshold` as source `model`, with their probability, in one transaction. In auto
    /// mode only tags at or above `config.auto_threshold` are applied; the others wait.
    pub fn apply_model(
        &mut self,
        ids: Option<&[i64]>,
        classifier: &Classifier,
        config: &crate::config::ModelConfig,
    ) -> Result<ModelStats> {
        use crate::suggest::{Offered, Source, SuggestMode, offer};

        let mode = self.suggest_mode();
        let now = crate::store::now_ms();
        let tx = self.conn.transaction()?;
        let rows: Vec<(i64, Item)> = {
            let sql = "SELECT id, url, title, note, summary FROM bookmark WHERE deleted_at IS NULL";
            let read = |r: &rusqlite::Row| -> rusqlite::Result<(i64, Item)> {
                Ok((
                    r.get(0)?,
                    Item {
                        url: r.get(1)?,
                        title: r.get(2)?,
                        note: r.get(3)?,
                        summary: r.get(4)?,
                        text: None,
                    },
                ))
            };
            match ids {
                None => tx
                    .prepare(sql)?
                    .query_map([], read)?
                    .collect::<rusqlite::Result<_>>()?,
                Some(ids) => {
                    let mut stmt = tx.prepare(&format!("{sql} AND id = ?1"))?;
                    let mut v = Vec::with_capacity(ids.len());
                    for &id in ids {
                        match rusqlite::OptionalExtension::optional(stmt.query_row([id], read))? {
                            Some(row) => v.push(row),
                            None => bail!("no bookmark with id {id}"),
                        }
                    }
                    v
                }
            }
        };
        let mut stats = ModelStats::default();
        let mut tagged = Vec::new();
        for (id, item) in rows {
            stats.checked += 1;
            let likely: Vec<(String, f64)> = classifier
                .probabilities(&item)
                .into_iter()
                .filter(|(_, p)| *p >= config.threshold)
                .collect();
            if !likely.is_empty() && likely.iter().all(|(t, _)| !classifier.is_cold(t)) {
                stats.sure.insert(id);
            }
            let mut applied = false;
            for (tag, p) in likely {
                let Some(tag_id) = crate::store::resolve_tag(&tx, &tag)? else {
                    continue;
                };
                let m = if mode == SuggestMode::Auto && p >= config.auto_threshold {
                    SuggestMode::Auto
                } else {
                    SuggestMode::Confirm
                };
                match offer(&tx, id, tag_id, Source::Model, Some(p), m, now)? {
                    Offered::Pending => stats.pending += 1,
                    Offered::Applied => {
                        stats.applied += 1;
                        applied = true;
                    }
                    Offered::Skipped => {}
                }
            }
            if applied {
                tagged.push(id);
            }
        }
        crate::store::touch_many(&tx, &tagged, now)?;
        tx.commit()?;
        Ok(stats)
    }
}
/// Ranking and calibration metrics for a tagging layer, as 025 measured them.
#[derive(Debug, Clone, Default)]
pub struct Metrics {
    pub items: usize,
    /// The first choice is one of the bookmark's tags.
    pub precision_at_1: f64,
    /// Sampling error of `precision_at_1`: the half-width of its 95% Wilson interval. About
    /// ±0.06 at 300 test bookmarks, ±0.13 at 50.
    pub precision_at_1_margin: f64,
    /// Share of the bookmark's tags among the first three choices.
    pub recall_at_3: f64,
    /// Precision and recall of choices with p ≥ 0.5.
    pub precision_at_half: f64,
    pub recall_at_half: f64,
    /// Expected calibration error over every (bookmark, tag) pair (10 bins); `None` for layers
    /// without probabilities. Easy negatives (p near 0, rightly) dominate it, so it stays small
    /// even when the offered choices are off.
    pub ece: Option<f64>,
    /// Expected calibration error over the offered choices only (p ≥ the `offer_at` given to
    /// [`evaluate`]); `None` without probabilities or without offered choices.
    pub ece_offered: Option<f64>,
    /// The offered choices by probability, in tenths: what was predicted against what was right.
    pub reliability: Vec<ReliabilityBin>,
    /// Precision of choices with p ≥ 0.9, and how many there are per bookmark.
    pub precision_at_09: Option<f64>,
    pub per_item_at_09: f64,
}

/// One row of a reliability table: offered choices with `from <= p < to`.
#[derive(Debug, Clone, PartialEq)]
pub struct ReliabilityBin {
    pub from: f64,
    pub to: f64,
    pub choices: usize,
    pub mean_p: f64,
    pub precision: f64,
}

/// Half-width of the 95% Wilson score interval of a proportion `p` over `n` trials.
pub fn wilson_margin(p: f64, n: f64) -> f64 {
    let z2 = 1.96f64 * 1.96;
    1.96 * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt() / (1.0 + z2 / n)
}

/// Calibration bins: (choices, sum of p, right ones).
type Bins = [(f64, f64, f64); 10];

fn ece(bins: &Bins) -> Option<f64> {
    let total: f64 = bins.iter().map(|b| b.0).sum();
    (total > 0.0).then(|| {
        bins.iter()
            .filter(|b| b.0 > 0.0)
            .map(|b| (b.0 / total) * ((b.1 - b.2) / b.0).abs())
            .sum()
    })
}

/// Evaluates choices against the true tags. `n_tags` is the size of the list, for calibration:
/// tags a layer didn't return count as p = 0. `offer_at` is the probability from which choices
/// are offered (`[model] threshold`), for [`Metrics::ece_offered`] and the reliability table.
pub fn evaluate(
    results: &[(Vec<Choice>, HashSet<String>)],
    n_tags: usize,
    offer_at: f64,
) -> Metrics {
    let mut m = Metrics {
        items: results.len(),
        ..Default::default()
    };
    if results.is_empty() {
        return m;
    }
    let (mut tp5, mut pred5, mut truth_n, mut hi, mut hi_ok) = (0.0, 0.0, 0.0, 0.0, 0.0);
    let mut bins: Bins = [(0.0, 0.0, 0.0); 10];
    let mut offered: Bins = [(0.0, 0.0, 0.0); 10];
    let mut have_probs = true;
    for (choices, truth) in results {
        truth_n += truth.len() as f64;
        if choices.first().is_some_and(|c| truth.contains(&c.tag)) {
            m.precision_at_1 += 1.0;
        }
        if !truth.is_empty() {
            m.recall_at_3 += choices
                .iter()
                .take(3)
                .filter(|c| truth.contains(&c.tag))
                .count() as f64
                / truth.len() as f64;
        }
        for c in choices {
            let y = if truth.contains(&c.tag) { 1.0 } else { 0.0 };
            let Some(p) = c.confidence else {
                have_probs = false;
                pred5 += 1.0;
                tp5 += y;
                continue;
            };
            if p >= 0.5 {
                pred5 += 1.0;
                tp5 += y;
            }
            if p >= 0.9 {
                hi += 1.0;
                hi_ok += y;
            }
            let b = ((p * 10.0) as usize).min(9);
            bins[b].0 += 1.0;
            bins[b].1 += p;
            bins[b].2 += y;
            if p >= offer_at {
                offered[b].0 += 1.0;
                offered[b].1 += p;
                offered[b].2 += y;
            }
        }
        let missing = n_tags.saturating_sub(choices.len()) as f64;
        let missed_true = truth
            .iter()
            .filter(|t| !choices.iter().any(|c| &c.tag == *t))
            .count() as f64;
        bins[0].0 += missing;
        bins[0].2 += missed_true;
    }
    let n = results.len() as f64;
    m.precision_at_1 /= n;
    m.precision_at_1_margin = wilson_margin(m.precision_at_1, n);
    m.recall_at_3 /= n;
    m.precision_at_half = if pred5 > 0.0 { tp5 / pred5 } else { 0.0 };
    m.recall_at_half = if truth_n > 0.0 { tp5 / truth_n } else { 0.0 };
    if have_probs {
        m.ece = ece(&bins);
        m.ece_offered = ece(&offered);
        m.reliability = offered
            .iter()
            .enumerate()
            .filter(|(_, b)| b.0 > 0.0)
            .map(|(k, b)| ReliabilityBin {
                from: (k as f64 / 10.0).max(offer_at),
                to: (k + 1) as f64 / 10.0,
                choices: b.0 as usize,
                mean_p: b.1 / b.0,
                precision: b.2 / b.0,
            })
            .collect();
        m.precision_at_09 = (hi > 0.0).then(|| hi_ok / hi);
        m.per_item_at_09 = hi / n;
    }
    m
}
#[cfg(test)]
mod tests {
    use super::*;

    fn tag(name: &str, aliases: &[&str], d: &str) -> TagOption {
        TagOption {
            name: name.into(),
            aliases: aliases.iter().map(|a| a.to_string()).collect(),
            description: (!d.is_empty()).then(|| d.to_string()),
        }
    }

    fn item(title: &str, url: &str) -> Item {
        Item {
            url: url.into(),
            title: Some(title.into()),
            ..Default::default()
        }
    }

    fn list() -> Vec<TagOption> {
        vec![
            tag("rust", &["rs"], "the Rust programming language"),
            tag("cooking", &["recipes"], "food and recipes"),
            tag("infra", &["ops"], "servers, deployment, Nix, CI"),
        ]
    }

    #[test]
    fn tokens_cover_title_host_and_path() {
        let t = tokens(&item(
            "The Tokio Tutorial",
            "https://user@docs.tokio.rs:443/tokio/tutorial/spawning",
        ));
        assert_eq!(
            t,
            [
                "d:tokio.rs",
                "h:docs",
                "h:rs",
                "h:tokio",
                "p:spawning",
                "p:tokio",
                "p:tutorial",
                "t:tokio",
                "t:tutorial"
            ]
        );
    }

    #[test]
    fn lexical_cold_start() {
        let lex = Lexical::new(&list());
        let p = lex.probabilities(&item("Deploying with Nix", "https://example.com/ops/ci"));
        // infra: alias "ops" (one name term), description words "nix", "ci", "deployment".
        assert_eq!(p, [0.0, 0.0, 0.45]);
        let p = lex.probabilities(&item("Rust recipes", "https://rs.example/"));
        assert_eq!(p, [0.65, 0.45, 0.0]);
    }

    fn examples() -> Vec<Example> {
        let mut v = Vec::new();
        for i in 0..240 {
            let (title, tag) = if i % 2 == 0 {
                (format!("borrow checker lifetimes {i}"), "rust")
            } else {
                (format!("pasta sauce dinner {i}"), "cooking")
            };
            v.push(Example {
                features: features(&item(&title, &format!("https://site{i}.example/"))),
                positive: [tag.to_string()].into(),
                negative: None,
            });
        }
        v
    }

    #[test]
    fn trains_and_predicts() {
        let m = train(&examples(), &list());
        assert_eq!(m.examples, 240);
        assert_eq!(m.tags[0].positives, 120);
        let c = Classifier::new(&list(), Some(&m));
        let p: HashMap<String, f64> = c
            .probabilities(&item(
                "lifetimes and the borrow checker",
                "https://x.example/",
            ))
            .into_iter()
            .collect();
        assert!(p["rust"] > 0.8, "{p:?}");
        assert!(p["cooking"] < 0.2, "{p:?}");
        // infra has no examples: lexical only.
        assert!(c.is_cold("infra") && !c.is_cold("rust"));
        let chosen = c
            .choose_tags(&item("borrow checker", "https://y.example/"), &list())
            .unwrap();
        assert_eq!(chosen[0].tag, "rust");
    }

    #[test]
    fn rejections_only_train_their_tag() {
        let mut ex = examples();
        let f = features(&item("pasta borrow", "https://z.example/"));
        for _ in 0..20 {
            ex.push(Example {
                features: f.clone(),
                positive: HashSet::new(),
                negative: Some(["rust".to_string()].into()),
            });
        }
        let m = train(&ex, &list());
        let c = Classifier::new(&list(), Some(&m));
        let p: HashMap<String, f64> = c
            .probabilities(&item("pasta borrow", "https://z.example/"))
            .into_iter()
            .collect();
        assert!(p["rust"] < 0.3, "{p:?}");
    }

    #[test]
    fn calibration_of_offered_choices() {
        let c = |tag: &str, p: f64| Choice {
            tag: tag.into(),
            confidence: Some(p),
        };
        let truth = |t: &[&str]| t.iter().map(|t| t.to_string()).collect::<HashSet<_>>();
        // Four bookmarks, 50 tags each: offered choices at 0.95 (half right) and 0.55 (right),
        // and many easy negatives that dilute the all-pairs ECE.
        let results = vec![
            (vec![c("a", 0.95), c("b", 0.55)], truth(&["b"])),
            (vec![c("a", 0.95), c("b", 0.2)], truth(&["a"])),
            (vec![c("b", 0.55)], truth(&["b"])),
            (vec![c("c", 0.05)], truth(&["a"])),
        ];
        let m = evaluate(&results, 50, 0.5);
        assert_eq!(m.precision_at_1, 0.5);
        assert_eq!(m.precision_at_09, Some(0.5));
        assert_eq!(
            m.reliability,
            [
                ReliabilityBin {
                    from: 0.5,
                    to: 0.6,
                    choices: 2,
                    mean_p: 0.55,
                    precision: 1.0
                },
                ReliabilityBin {
                    from: 0.9,
                    to: 1.0,
                    choices: 2,
                    mean_p: 0.95,
                    precision: 0.5
                },
            ]
        );
        // Offered: |0.55 - 1| and |0.95 - 0.5|, equally weighted.
        assert!((m.ece_offered.unwrap() - 0.45).abs() < 1e-9);
        assert!(m.ece.unwrap() < 0.02, "{:?}", m.ece);
        assert!((m.precision_at_1_margin - wilson_margin(0.5, 4.0)).abs() < 1e-12);
        // Near the bounds, the interval stays wide (the normal approximation gives 0).
        assert!((wilson_margin(1.0, 33.0) - 0.052).abs() < 0.001);
        assert!((wilson_margin(0.6, 300.0) - 0.055).abs() < 0.001);
        assert!((wilson_margin(0.5, 50.0) - 0.13).abs() < 0.005);
        // Offered from 0.6: only the 0.95s, and the first bin starts at the threshold.
        let m = evaluate(&results, 50, 0.6);
        assert_eq!(m.reliability.len(), 1);
        assert!((m.ece_offered.unwrap() - 0.45).abs() < 1e-9);
        let m = evaluate(&results, 50, 0.52);
        assert_eq!(m.reliability[0].from, 0.52);
    }

    #[test]
    fn round_trips_bytes() {
        let m = train(&examples(), &list());
        let b = m.to_bytes();
        assert_eq!(Model::from_bytes(&b).unwrap(), m);
        assert!(Model::from_bytes(b"nope").is_err());
        assert!(Model::from_bytes(&b[..b.len() - 3]).is_err());
    }
}
