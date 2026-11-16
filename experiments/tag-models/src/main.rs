//! Task 025: which tag-choosing layer works? An experiment on HetRec 2011 Delicious, not shipped.
//!
//! ```text
//! tag-models prep DATA_DIR OUT_DIR       # dataset.jsonl from the HetRec files, time-ordered
//! tag-models run CANDIDATE OUT_DIR       # predictions/CANDIDATE.jsonl
//! tag-models eval OUT_DIR                # metrics table (Markdown) for every predictions file
//! ```
//!
//! Candidates: `lexical` (zero-shot), `nb-few`, `lr-few`, `nb`, `lr` (hashed tokens, trained on
//! 5 examples per tag or the whole train split), `lr+lexical` (the cold-start blend 022 would
//! ship), `laya-noul`, `laya-choice`, `laya-hybrid` (decision model on the local llama.cpp
//! `/systemone` endpoint), and `llm` (a small local LLM through shlv's openai-compatible
//! provider). Slow candidates run on a fixed sample of the test split (`SAMPLE` items).
//!
//! Endpoints come from `$LLAMA_CPP_BASE_URL` (an OpenAI-style `/v1` root); the LLM model from
//! `$LLM_MODEL` (default `Sharp-MiniCPM5-2B`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// Items from the end of the test split that slow candidates run on.
const SAMPLE: usize = 300;
/// Share of items (oldest first) used for training.
const TRAIN_SHARE: f64 = 0.8;
/// Examples per tag in the few-shot setting.
const FEW: usize = 5;
/// Hash buckets: `$TAG_BUCKETS_LOG2` (default 18).
fn buckets() -> usize {
    static B: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *B.get_or_init(|| {
        1 << std::env::var("TAG_BUCKETS_LOG2")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(18)
    })
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["prep", data, out] => prep(Path::new(data), Path::new(out)),
        ["run", cand, out] => run(cand, Path::new(out)),
        ["eval", out] => eval(Path::new(out)),
        _ => bail!("usage: tag-models prep DATA OUT | run CANDIDATE OUT | eval OUT"),
    }
}

// ---------------------------------------------------------------------------------------------
// Data

/// A canonical tag as the experiment's "user" declared it.
#[derive(Clone)]
struct Tag {
    name: String,
    aliases: Vec<String>,
    description: String,
}

fn canonical() -> Result<Vec<Tag>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("canonical.toml");
    let text = fs::read_to_string(&path)?;
    let config = shlv::Config::parse(&text, &path)?;
    let d = config.tags.canonical.context("no [tags.canonical]")?;
    Ok(d.iter()
        .map(|(name, aliases)| Tag {
            name: name.to_string(),
            aliases: aliases.to_vec(),
            description: d.description(name).unwrap_or("").to_string(),
        })
        .collect())
}

#[derive(Clone)]
struct Item {
    id: u64,
    ts: i64,
    title: String,
    url: String,
    tags: Vec<String>,
}

fn item_json(i: &Item) -> Value {
    json!({"id": i.id, "ts": i.ts, "title": i.title, "url": i.url, "tags": i.tags})
}

fn read_items(out: &Path) -> Result<Vec<Item>> {
    let f = fs::File::open(out.join("dataset.jsonl")).context("run prep first")?;
    BufReader::new(f)
        .lines()
        .map(|l| {
            let v: Value = serde_json::from_str(&l?)?;
            Ok(Item {
                id: v["id"].as_u64().unwrap(),
                ts: v["ts"].as_i64().unwrap(),
                title: v["title"].as_str().unwrap().to_string(),
                url: v["url"].as_str().unwrap().to_string(),
                tags: v["tags"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| t.as_str().unwrap().to_string())
                    .collect(),
            })
        })
        .collect()
}

/// Reads the HetRec files, maps raw tags to the canonical list through its aliases, keeps
/// bookmarks with at least one canonical tag, and writes them oldest first.
fn prep(data: &Path, out: &Path) -> Result<()> {
    let tags = canonical()?;
    let mut map: HashMap<String, String> = HashMap::new();
    for t in &tags {
        map.insert(t.name.clone(), t.name.clone());
        for a in &t.aliases {
            map.insert(a.clone(), t.name.clone());
        }
    }
    let read = |name: &str| -> Result<String> {
        let bytes = fs::read(data.join(name)).with_context(|| name.to_string())?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    };
    let raw_tags: HashMap<u64, String> = read("tags.dat")?
        .lines()
        .skip(1)
        .filter_map(|l| {
            let (id, v) = l.split_once('\t')?;
            Some((id.parse().ok()?, shlv::store::normalize_tag(v)))
        })
        .collect();
    let bookmarks: HashMap<u64, (String, String)> = read("bookmarks.dat")?
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            Some((
                f[0].parse().ok()?,
                (f.get(2)?.to_string(), f.get(3)?.to_string()),
            ))
        })
        .collect();
    let mut first: HashMap<u64, i64> = HashMap::new();
    let mut labels: HashMap<u64, HashSet<String>> = HashMap::new();
    for l in read("user_taggedbookmarks-timestamps.dat")?.lines().skip(1) {
        let f: Vec<&str> = l.split('\t').collect();
        let (Ok(b), Ok(t), Ok(ts)) = (
            f[1].parse::<u64>(),
            f[2].parse::<u64>(),
            f[3].parse::<i64>(),
        ) else {
            continue;
        };
        let e = first.entry(b).or_insert(ts);
        *e = (*e).min(ts);
        if let Some(c) = raw_tags.get(&t).and_then(|r| map.get(r)) {
            labels.entry(b).or_default().insert(c.clone());
        }
    }
    let mut items: Vec<Item> = labels
        .into_iter()
        .filter_map(|(id, tags)| {
            let (title, url) = bookmarks.get(&id)?;
            let mut tags: Vec<String> = tags.into_iter().collect();
            tags.sort();
            Some(Item {
                id,
                ts: first[&id],
                title: title.clone(),
                url: url.clone(),
                tags,
            })
        })
        .collect();
    items.sort_by_key(|i| (i.ts, i.id));
    fs::create_dir_all(out)?;
    let mut f = fs::File::create(out.join("dataset.jsonl"))?;
    for i in &items {
        writeln!(f, "{}", item_json(i))?;
    }
    let n_tags: usize = items.iter().map(|i| i.tags.len()).sum();
    eprintln!(
        "{} bookmarks with canonical tags ({} canonical tags, {:.2} per bookmark)",
        items.len(),
        tags.len(),
        n_tags as f64 / items.len() as f64
    );
    Ok(())
}

/// Train and test splits by time, and the few-shot train set.
struct Splits {
    train: Vec<Item>,
    test: Vec<Item>,
    few: Vec<Item>,
}

fn splits(items: Vec<Item>, tags: &[Tag]) -> Splits {
    let cut = (items.len() as f64 * TRAIN_SHARE) as usize;
    let (train, test) = items.split_at(cut);
    // Few-shot: the first FEW training examples of each tag (a user's first tagged bookmarks).
    let mut seen: HashMap<&str, usize> = HashMap::new();
    let mut few = Vec::new();
    for i in train {
        let wanted = i
            .tags
            .iter()
            .any(|t| seen.get(t.as_str()).copied().unwrap_or(0) < FEW);
        if wanted {
            for t in &i.tags {
                *seen.entry(t).or_default() += 1;
            }
            few.push(i.clone());
        }
    }
    let _ = tags;
    Splits {
        train: train.to_vec(),
        test: test.to_vec(),
        few,
    }
}

/// The last SAMPLE test items, spread evenly over the test split.
fn sample(test: &[Item]) -> Vec<Item> {
    let step = (test.len() / SAMPLE).max(1);
    test.iter().step_by(step).take(SAMPLE).cloned().collect()
}

// ---------------------------------------------------------------------------------------------
// Features: what 022 would hash.

const STOP: &[&str] = &[
    "the", "and", "for", "with", "you", "your", "from", "how", "what", "this", "that", "are",
    "www", "com", "org", "net", "http", "https", "html", "htm", "php", "index", "home", "page",
    "of", "to", "in", "on", "a", "an", "is", "it", "by", "at", "or", "be", "as", "de",
];

fn words(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|w| {
            w.len() >= 2 && !STOP.contains(&w.as_str()) && !w.chars().all(|c| c.is_ascii_digit())
        })
}

/// Tokens: title words, host labels, path words (prefixed by where they came from).
fn tokens(i: &Item) -> Vec<String> {
    let mut v: Vec<String> = words(&i.title).map(|w| format!("t:{w}")).collect();
    let rest = i.url.split_once("://").map_or(i.url.as_str(), |(_, r)| r);
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host.to_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() >= 2 {
        v.push(format!("d:{}", labels[labels.len() - 2..].join(".")));
    }
    v.extend(words(&host).map(|w| format!("h:{w}")));
    v.extend(
        words(path)
            .filter(|w| w.len() >= 3)
            .map(|w| format!("p:{w}")),
    );
    v.sort();
    v.dedup();
    v
}

/// Plain words (no prefixes) for lexical matching.
fn plain_words(i: &Item) -> HashSet<String> {
    let mut s: HashSet<String> = words(&i.title).collect();
    s.extend(words(&i.url));
    s
}

fn hash(s: &str) -> usize {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (h as usize) % buckets()
}

fn features(i: &Item) -> Vec<usize> {
    let mut f: Vec<usize> = tokens(i).iter().map(|t| hash(t)).collect();
    f.sort_unstable();
    f.dedup();
    f
}

// ---------------------------------------------------------------------------------------------
// Candidates. Each returns, per item, a score in [0, 1] for every tag it considered.

type Scores = BTreeMap<String, f64>;

/// Zero-shot: word overlap with the tag's name and aliases (weight 1) and description (0.4).
fn lexical(items: &[Item], tags: &[Tag]) -> Vec<Scores> {
    let terms: Vec<Vec<(String, f64)>> = tags
        .iter()
        .map(|t| {
            let mut v: Vec<(String, f64)> = Vec::new();
            for n in std::iter::once(&t.name).chain(&t.aliases) {
                for w in n.split(['-', '_']) {
                    v.push((w.to_string(), 1.0));
                }
                v.push((n.replace(['-', '_'], ""), 1.0));
            }
            for w in words(&t.description) {
                if !v.iter().any(|(x, _)| *x == w) {
                    v.push((w, 0.4));
                }
            }
            v
        })
        .collect();
    items
        .iter()
        .map(|i| {
            let ws = plain_words(i);
            tags.iter()
                .zip(&terms)
                .filter_map(|(t, terms)| {
                    let s: f64 = terms
                        .iter()
                        .filter(|(w, _)| ws.contains(w))
                        .map(|(_, x)| x)
                        .sum();
                    (s > 0.0).then(|| (t.name.clone(), 1.0 - (-s).exp()))
                })
                .collect()
        })
        .collect()
}

/// One-vs-rest logistic regression on hashed binary features, plain SGD with L2.
struct Lr {
    w: Vec<Vec<f32>>,
    b: Vec<f32>,
}

impl Lr {
    fn train(items: &[Item], tags: &[Tag], epochs: usize) -> Lr {
        let feats: Vec<Vec<usize>> = items.iter().map(features).collect();
        let mut w = vec![vec![0f32; buckets()]; tags.len()];
        let mut b = vec![0f32; tags.len()];
        let (lr, l2) = (0.1f32, 1e-6f32);
        let mut order: Vec<usize> = (0..items.len()).collect();
        let mut rng = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..epochs {
            for k in (1..order.len()).rev() {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                order.swap(k, (rng % (k as u64 + 1)) as usize);
            }
            for &n in &order {
                let f = &feats[n];
                for (t, tag) in tags.iter().enumerate() {
                    let y = if items[n].tags.contains(&tag.name) {
                        1.0
                    } else {
                        0.0
                    };
                    let z: f32 = b[t] + f.iter().map(|&j| w[t][j]).sum::<f32>();
                    let g = 1.0 / (1.0 + (-z).exp()) - y;
                    b[t] -= lr * g;
                    for &j in f {
                        w[t][j] -= lr * (g + l2 * w[t][j]);
                    }
                }
            }
        }
        Lr { w, b }
    }

    fn predict(&self, items: &[Item], tags: &[Tag]) -> Vec<Scores> {
        items
            .iter()
            .map(|i| {
                let f = features(i);
                tags.iter()
                    .enumerate()
                    .map(|(t, tag)| {
                        let z = self.b[t] + f.iter().map(|&j| self.w[t][j]).sum::<f32>();
                        (tag.name.clone(), 1.0 / (1.0 + (-f64::from(z)).exp()))
                    })
                    .collect()
            })
            .collect()
    }

    /// Weights that matter, for the size estimate.
    fn nonzero(&self) -> usize {
        self.w.iter().flatten().filter(|x| x.abs() > 0.05).count()
    }
}

/// Bernoulli-free multinomial naive Bayes per tag (tag vs not), Laplace smoothing; the
/// posterior from the log odds.
fn nb(train: &[Item], test: &[Item], tags: &[Tag]) -> Vec<Scores> {
    let feats: Vec<Vec<usize>> = train.iter().map(features).collect();
    let total: HashMap<usize, f64> = feats.iter().flatten().fold(HashMap::new(), |mut m, &f| {
        *m.entry(f).or_default() += 1.0;
        m
    });
    let all_count: f64 = total.values().sum();
    let vocab = total.len() as f64;
    let models: Vec<(f64, HashMap<usize, f64>, f64)> = tags
        .iter()
        .map(|tag| {
            let mut pos: HashMap<usize, f64> = HashMap::new();
            let mut n_pos = 0.0;
            for (i, f) in train.iter().zip(&feats) {
                if i.tags.contains(&tag.name) {
                    n_pos += 1.0;
                    for &x in f {
                        *pos.entry(x).or_default() += 1.0;
                    }
                }
            }
            let pos_count: f64 = pos.values().sum();
            let prior = (n_pos + 1.0) / (train.len() as f64 + 2.0);
            (prior, pos, pos_count)
        })
        .collect();
    test.iter()
        .map(|i| {
            let f = features(i);
            tags.iter()
                .zip(&models)
                .map(|(tag, (prior, pos, pos_count))| {
                    let mut lo = (prior / (1.0 - prior)).ln();
                    for x in &f {
                        let p = pos.get(x).copied().unwrap_or(0.0);
                        let a = total.get(x).copied().unwrap_or(0.0);
                        let p_pos = (p + 1.0) / (pos_count + vocab);
                        let p_neg = (a - p + 1.0) / (all_count - pos_count + vocab);
                        lo += (p_pos / p_neg).ln();
                    }
                    (tag.name.clone(), 1.0 / (1.0 + (-lo).exp()))
                })
                .collect()
        })
        .collect()
}

/// The decision model on llama.cpp's `/systemone` endpoint.
struct Laya {
    agent: ureq::Agent,
    url: String,
}

impl Laya {
    fn new() -> Result<Laya> {
        let base = std::env::var("LLAMA_CPP_BASE_URL").context("set LLAMA_CPP_BASE_URL")?;
        Ok(Laya {
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(std::time::Duration::from_secs(600)))
                .build()
                .new_agent(),
            url: format!("{}/systemone", base.trim_end_matches('/')),
        })
    }

    fn ask(&self, item: &Item, questions: Value) -> Result<Value> {
        let body = json!({
            "model": "Laya",
            "state": {"title": item.title, "url": item.url},
            "questions": questions,
        });
        let mut r = self.agent.post(&self.url).send(body.to_string())?;
        let v: Value = serde_json::from_str(&r.body_mut().read_to_string()?)?;
        Ok(v["answers"].clone())
    }

    /// One yes/no question per tag.
    fn noul(&self, item: &Item, tags: &[&Tag]) -> Result<Scores> {
        let q: serde_json::Map<String, Value> = tags
            .iter()
            .map(|t| {
                (
                    t.name.clone(),
                    json!({"type": "noul", "instructions": format!(
                        "Does the tag \"{}\" ({}) fit this bookmark?", t.name, t.description)}),
                )
            })
            .collect();
        let a = self.ask(item, Value::Object(q))?;
        Ok(tags
            .iter()
            .map(|t| (t.name.clone(), a[&t.name]["noul"].as_f64().unwrap_or(0.0)))
            .collect())
    }

    /// One multi-option choice over all tags: the primary tag's distribution.
    fn choice(&self, item: &Item, tags: &[Tag]) -> Result<Scores> {
        let criteria: serde_json::Map<String, Value> = tags
            .iter()
            .map(|t| (t.name.clone(), json!(t.description)))
            .collect();
        let a = self.ask(
            item,
            json!({"primary": {"type": "choice",
                "instructions": "Which tag fits this bookmark best?", "criteria": criteria}}),
        )?;
        Ok(a["primary"]["probabilities"]
            .as_object()
            .context("no probabilities")?
            .iter()
            .map(|(k, v)| (k.clone(), v.as_f64().unwrap_or(0.0)))
            .collect())
    }
}

fn run(cand: &str, out: &Path) -> Result<()> {
    let tags = canonical()?;
    let s = splits(read_items(out)?, &tags);
    let test = s.test.clone();
    let slow = sample(&test);
    let dir = out.join("predictions");
    fs::create_dir_all(&dir)?;
    let mut w = fs::File::create(dir.join(format!("{cand}.jsonl")))?;
    let mut write = |items: &[Item], scores: &[Scores], ms: &[f64], meta: Value| -> Result<()> {
        writeln!(w, "{}", json!({"meta": meta}))?;
        for ((i, s), ms) in items.iter().zip(scores).zip(ms) {
            writeln!(w, "{}", json!({"id": i.id, "scores": s, "ms": ms}))?;
        }
        Ok(())
    };
    let per = |t: Instant, n: usize| vec![t.elapsed().as_secs_f64() * 1000.0 / n as f64; n];
    match cand {
        "lexical" => {
            let t = Instant::now();
            let sc = lexical(&test, &tags);
            write(&test, &sc, &per(t, test.len()), json!({"setting": "cold"}))?;
        }
        "nb" | "nb-few" => {
            let train = if cand == "nb" { &s.train } else { &s.few };
            let t0 = Instant::now();
            // NB has no separate training step worth timing apart from counting.
            let sc = nb(train, &test, &tags);
            let ms = per(t0, test.len());
            write(
                &test,
                &sc,
                &ms,
                json!({"setting": if cand == "nb" {"full"} else {"few"},
                "train": train.len()}),
            )?;
        }
        "lr" | "lr-few" | "lr+lexical" | "lr+lexical-few" => {
            let few = cand.ends_with("few");
            let train = if few { &s.few } else { &s.train };
            let epochs = if few { 30 } else { 5 };
            let t0 = Instant::now();
            let model = Lr::train(train, &tags, epochs);
            let train_ms = t0.elapsed().as_secs_f64() * 1000.0;
            let t = Instant::now();
            let mut sc = model.predict(&test, &tags);
            let ms = per(t, test.len());
            if cand.starts_with("lr+lexical") {
                // Cold-start blend: a tag with few training examples leans on the lexical score.
                let lex = lexical(&test, &tags);
                let counts: HashMap<&str, usize> = tags
                    .iter()
                    .map(|t| {
                        (
                            t.name.as_str(),
                            train.iter().filter(|i| i.tags.contains(&t.name)).count(),
                        )
                    })
                    .collect();
                for (s, l) in sc.iter_mut().zip(&lex) {
                    for (tag, p) in s.iter_mut() {
                        let wgt = (counts[tag.as_str()] as f64 / 50.0).min(1.0);
                        let lx = l.get(tag).copied().unwrap_or(0.0);
                        *p = wgt * *p + (1.0 - wgt) * lx;
                    }
                }
            }
            write(
                &test,
                &sc,
                &ms,
                json!({
                    "setting": if few {"few"} else {"full"}, "buckets": buckets(),
                    "train": train.len(), "train_ms": train_ms, "nonzero_weights": model.nonzero(),
                }),
            )?;
        }
        "laya-noul" | "laya-choice" | "laya-hybrid" => {
            let laya = Laya::new()?;
            let all: Vec<&Tag> = tags.iter().collect();
            let mut sc = Vec::new();
            let mut ms = Vec::new();
            for (n, i) in slow.iter().enumerate() {
                let t = Instant::now();
                let s = match cand {
                    "laya-noul" => laya.noul(i, &all)?,
                    "laya-choice" => laya.choice(i, &tags)?,
                    _ => {
                        // Primary by choice, then yes/no on the 8 likeliest only.
                        let c = laya.choice(i, &tags)?;
                        let mut ranked: Vec<(&String, &f64)> = c.iter().collect();
                        ranked.sort_by(|a, b| b.1.total_cmp(a.1));
                        let top: Vec<&Tag> = ranked
                            .iter()
                            .take(8)
                            .filter_map(|(k, _)| tags.iter().find(|t| &&t.name == k))
                            .collect();
                        laya.noul(i, &top)?
                    }
                };
                ms.push(t.elapsed().as_secs_f64() * 1000.0);
                sc.push(s);
                if n % 25 == 0 {
                    eprintln!("{cand}: {n}/{}", slow.len());
                }
            }
            write(
                &slow,
                &sc,
                &ms,
                json!({"setting": "cold", "sample": slow.len()}),
            )?;
        }
        "llm" => {
            let base = std::env::var("LLAMA_CPP_BASE_URL").context("set LLAMA_CPP_BASE_URL")?;
            let model = std::env::var("LLM_MODEL").unwrap_or("Sharp-MiniCPM5-2B".into());
            let p = shlv::ai::Provider::new(
                "llm",
                &shlv::ai::ProviderConfig {
                    kind: shlv::ai::Kind::OpenaiCompatible,
                    model: model.clone(),
                    base_url: Some(base),
                    api_key_env: None,
                    api_key_file: None,
                    timeout: Some(600),
                    effort: None,
                },
            )?;
            let list: Vec<shlv::choose::TagOption> = tags
                .iter()
                .map(|t| shlv::choose::TagOption {
                    name: t.name.clone(),
                    aliases: Vec::new(),
                    description: Some(t.description.clone()),
                })
                .collect();
            let mut sc = Vec::new();
            let mut ms = Vec::new();
            let mut failed = 0;
            for (n, i) in slow.iter().enumerate() {
                let item = shlv::choose::Item {
                    url: i.url.clone(),
                    title: Some(i.title.clone()),
                    ..Default::default()
                };
                let t = Instant::now();
                let chosen = p.choose_tags(&item, &list).unwrap_or_else(|e| {
                    eprintln!("llm: {}: {e:#}", i.id);
                    failed += 1;
                    Vec::new()
                });
                ms.push(t.elapsed().as_secs_f64() * 1000.0);
                // No probabilities: rank order only, as 1.0, 0.99, ...
                sc.push(
                    chosen
                        .iter()
                        .enumerate()
                        .map(|(k, c)| (c.tag.clone(), 1.0 - k as f64 * 0.01))
                        .collect(),
                );
                if n % 25 == 0 {
                    eprintln!("llm: {n}/{}", slow.len());
                }
            }
            let u = p.usage();
            write(
                &slow,
                &sc,
                &ms,
                json!({"setting": "cold", "sample": slow.len(), "model": model,
                "failed": failed, "input_tokens": u.input, "output_tokens": u.output,
                "no_probabilities": true}),
            )?;
        }
        _ => bail!("unknown candidate {cand}"),
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Metrics

fn eval(out: &Path) -> Result<()> {
    let tags = canonical()?;
    let items = read_items(out)?;
    let s = splits(items, &tags);
    let truth: HashMap<u64, HashSet<String>> = s
        .test
        .iter()
        .map(|i| (i.id, i.tags.iter().cloned().collect()))
        .collect();
    // Common and rare: thirds of the canonical tags by training frequency.
    let mut freq: Vec<(String, usize)> = tags
        .iter()
        .map(|t| {
            (
                t.name.clone(),
                s.train.iter().filter(|i| i.tags.contains(&t.name)).count(),
            )
        })
        .collect();
    freq.sort_by(|a, b| b.1.cmp(&a.1));
    let third = freq.len() / 3;
    let common: HashSet<String> = freq[..third].iter().map(|f| f.0.clone()).collect();
    let rare: HashSet<String> = freq[freq.len() - third..]
        .iter()
        .map(|f| f.0.clone())
        .collect();
    println!(
        "train {} / test {} / few {} items; common tags: {}; rare tags: {}\n",
        s.train.len(),
        s.test.len(),
        s.few.len(),
        freq[..third]
            .iter()
            .map(|f| format!("{}({})", f.0, f.1))
            .collect::<Vec<_>>()
            .join(" "),
        freq[freq.len() - third..]
            .iter()
            .map(|f| format!("{}({})", f.0, f.1))
            .collect::<Vec<_>>()
            .join(" "),
    );
    println!(
        "| candidate | setting | n | P@1 | R@3 | P@0.5 | R@0.5 | recall@3 common | recall@3 rare | ECE | P(p≥0.9) | cov(p≥0.9) | ms/item |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    // `SAMPLE_ONLY=1`: every candidate on the slow candidates' sample, for a fair comparison.
    let only: Option<HashSet<u64>> =
        std::env::var_os("SAMPLE_ONLY").map(|_| sample(&s.test).iter().map(|i| i.id).collect());
    let mut files: Vec<PathBuf> = fs::read_dir(out.join("predictions"))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    files.sort();
    for f in files {
        let name = f.file_stem().unwrap().to_string_lossy().to_string();
        let mut meta = Value::Null;
        let mut preds: Vec<(u64, Vec<(String, f64)>, f64)> = Vec::new();
        for l in BufReader::new(fs::File::open(&f)?).lines() {
            let v: Value = serde_json::from_str(&l?)?;
            if v.get("meta").is_some() {
                meta = v["meta"].clone();
                continue;
            }
            if let Some(only) = &only
                && !only.contains(&v["id"].as_u64().unwrap())
            {
                continue;
            }
            let mut sc: Vec<(String, f64)> = v["scores"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, p)| (k.clone(), p.as_f64().unwrap()))
                .collect();
            sc.sort_by(|a, b| b.1.total_cmp(&a.1));
            preds.push((
                v["id"].as_u64().unwrap(),
                sc,
                v["ms"].as_f64().unwrap_or(0.0),
            ));
        }
        let m = metrics(
            &preds,
            &truth,
            &tags,
            &common,
            &rare,
            meta.get("no_probabilities").is_some(),
        );
        println!(
            "| {name} | {} | {} | {} |",
            meta["setting"].as_str().unwrap_or(""),
            preds.len(),
            m.join(" | ")
        );
        eprintln!("{name}: {meta}");
    }
    Ok(())
}

fn metrics(
    preds: &[(u64, Vec<(String, f64)>, f64)],
    truth: &HashMap<u64, HashSet<String>>,
    tags: &[Tag],
    common: &HashSet<String>,
    rare: &HashSet<String>,
    no_probs: bool,
) -> Vec<String> {
    let n = preds.len() as f64;
    let (mut p1, mut r3, mut tp5, mut pred5, mut true_n, mut ms) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    // Per-tag recall@3 for the common/rare split.
    let mut hit3: HashMap<&str, (f64, f64)> = HashMap::new();
    // Calibration bins over every (item, tag) pair the candidate scored.
    let mut bins = [(0.0f64, 0.0f64, 0.0f64); 10];
    let (mut hi_n, mut hi_ok) = (0.0, 0.0);
    for (id, sc, t) in preds {
        let tr = &truth[id];
        ms += t;
        true_n += tr.len() as f64;
        if sc.first().is_some_and(|(k, _)| tr.contains(k)) {
            p1 += 1.0;
        }
        let top3: Vec<&str> = sc.iter().take(3).map(|(k, _)| k.as_str()).collect();
        r3 += top3.iter().filter(|k| tr.contains(**k)).count() as f64 / tr.len() as f64;
        for g in tr {
            let e = hit3.entry(g.as_str()).or_default();
            e.1 += 1.0;
            if top3.contains(&g.as_str()) {
                e.0 += 1.0;
            }
        }
        for (k, p) in sc {
            let y = if tr.contains(k) { 1.0 } else { 0.0 };
            if *p >= 0.5 {
                pred5 += 1.0;
                tp5 += y;
            }
            if *p >= 0.9 {
                hi_n += 1.0;
                hi_ok += y;
            }
            let b = ((p * 10.0) as usize).min(9);
            bins[b].0 += 1.0;
            bins[b].1 += p;
            bins[b].2 += y;
        }
        // Tags the candidate didn't score count as p = 0 for calibration.
        for t in tags {
            if !sc.iter().any(|(k, _)| *k == t.name) {
                bins[0].0 += 1.0;
                if tr.contains(&t.name) {
                    bins[0].2 += 1.0;
                }
            }
        }
    }
    let total: f64 = bins.iter().map(|b| b.0).sum();
    let ece: f64 = bins
        .iter()
        .filter(|b| b.0 > 0.0)
        .map(|b| (b.0 / total) * ((b.1 / b.0) - (b.2 / b.0)).abs())
        .sum();
    let macro_recall = |set: &HashSet<String>| {
        let v: Vec<f64> = set
            .iter()
            .filter_map(|t| {
                hit3.get(t.as_str())
                    .filter(|h| h.1 > 0.0)
                    .map(|h| h.0 / h.1)
            })
            .collect();
        v.iter().sum::<f64>() / v.len().max(1) as f64
    };
    let f = |x: f64| format!("{x:.3}");
    vec![
        f(p1 / n),
        f(r3 / n),
        if pred5 > 0.0 {
            f(tp5 / pred5)
        } else {
            "-".into()
        },
        f(tp5 / true_n),
        f(macro_recall(common)),
        f(macro_recall(rare)),
        if no_probs { "n/a".into() } else { f(ece) },
        if no_probs || hi_n == 0.0 {
            "-".into()
        } else {
            f(hi_ok / hi_n)
        },
        if no_probs { "n/a".into() } else { f(hi_n / n) },
        format!("{:.1}", ms / n),
    ]
}
