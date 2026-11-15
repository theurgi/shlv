//! Generates synthetic bookmarks for benchmarking.
//!
//! Usage: `cargo run --release --example gen -- <count> <db-path>`
//!
//! Deterministic (fixed seed). Words, hosts, and tags follow skewed distributions so the search
//! index looks like a real one: a few very common terms, a long tail of rare ones.

use shlv::{NewBookmark, Store};

/// Real words first, so benchmark queries have something to hit; the rest are synthetic.
const WORDS: &[&str] = &[
    "the",
    "guide",
    "rust",
    "how",
    "to",
    "introduction",
    "python",
    "async",
    "with",
    "and",
    "web",
    "linux",
    "javascript",
    "database",
    "design",
    "performance",
    "nix",
    "tutorial",
    "learning",
    "server",
    "api",
    "programming",
    "security",
    "cloud",
    "testing",
    "data",
    "machine",
    "model",
    "react",
    "kubernetes",
    "docker",
    "git",
    "functional",
    "systems",
    "network",
    "memory",
    "compiler",
    "type",
    "error",
    "handling",
    "best",
    "practices",
    "deep",
    "dive",
    "notes",
    "blog",
    "review",
    "news",
    "recipe",
    "travel",
    "music",
    "history",
    "science",
    "climate",
    "economics",
];
const SYLLABLES: &[&str] = &[
    "ka", "lo", "mi", "ne", "su", "ta", "ri", "po", "ve", "do", "ga", "zu", "bel", "ton", "mar",
    "lis", "qua", "tor", "fen", "dra", "ix", "ul", "om", "es",
];
const TLDS: &[&str] = &["com", "org", "io", "dev", "net", "co.uk", "de", "rs"];
const TAGS: &[&str] = &[
    "rust",
    "python",
    "web",
    "linux",
    "nix",
    "database",
    "security",
    "ai",
    "design",
    "devops",
    "reading",
    "video",
    "tool",
    "reference",
    "tutorial",
    "news",
    "javascript",
    "go",
    "cooking",
    "travel",
    "music",
    "science",
    "history",
    "math",
    "career",
    "productivity",
    "hardware",
    "networking",
    "testing",
    "performance",
];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// Skewed toward small indexes: index 0 is the most likely.
    fn skewed(&mut self, n: usize) -> usize {
        ((n as f64) * self.unit().powi(3)) as usize
    }
}

struct Gen {
    rng: Rng,
    vocab: Vec<String>,
    hosts: Vec<String>,
}

impl Gen {
    fn new() -> Self {
        let mut rng = Rng(0x5eed_1234_abcd_0042);
        let mut vocab: Vec<String> = WORDS.iter().map(|w| w.to_string()).collect();
        while vocab.len() < 20_000 {
            let n = 2 + rng.below(3);
            vocab.push(
                (0..n)
                    .map(|_| SYLLABLES[rng.below(SYLLABLES.len())])
                    .collect(),
            );
        }
        let hosts = (0..5_000)
            .map(|i| {
                let name: String = (0..2 + rng.below(3))
                    .map(|_| SYLLABLES[rng.below(SYLLABLES.len())])
                    .collect();
                let sub = if i % 7 == 0 { "blog." } else { "" };
                format!("{sub}{name}{i}.{}", TLDS[rng.below(TLDS.len())])
            })
            .collect();
        Gen { rng, vocab, hosts }
    }

    fn word(&mut self) -> &str {
        let i = self.rng.skewed(self.vocab.len());
        &self.vocab[i]
    }

    fn words(&mut self, min: usize, max: usize) -> String {
        let n = min + self.rng.below(max - min + 1);
        (0..n)
            .map(|_| self.word().to_string())
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn bookmark(&mut self, i: usize) -> NewBookmark {
        let host = self.hosts[self.rng.skewed(self.hosts.len())].clone();
        let path = (0..1 + self.rng.below(3))
            .map(|_| self.word().to_string())
            .collect::<Vec<_>>()
            .join("/");
        let query = if self.rng.below(5) == 0 {
            format!("?id={i}&utm_source=feed")
        } else {
            String::new()
        };
        let url = format!("https://{host}/{path}-{i}{query}");
        let mut title = self.words(3, 10);
        if let Some(first) = title.get_mut(..1) {
            first.make_ascii_uppercase();
        }
        let note = (self.rng.below(10) < 3).then(|| self.words(5, 25));
        let tags = (0..self.rng.below(5))
            .map(|_| TAGS[self.rng.skewed(TAGS.len())].to_string())
            .collect();
        NewBookmark {
            url,
            title: (self.rng.below(20) != 0).then_some(title),
            note,
            tags,
        }
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let [_, count, path] = args.as_slice() else {
        anyhow::bail!("usage: gen <count> <db-path>");
    };
    let count: usize = count.parse()?;
    let mut store = Store::open(path.as_ref())?;
    let mut generator = Gen::new();
    let start = std::time::Instant::now();
    for i in 0..count {
        store.add(&generator.bookmark(i))?;
    }
    // Spread creation times over ten years, keeping id order roughly chronological.
    let span: i64 = 10 * 365 * 86_400_000;
    store.conn().execute(
        "UPDATE bookmark SET created_at = created_at - ?1 + id * (?1 / ?2) \
         + abs(random() % 3600000)",
        rusqlite::params![span, count as i64],
    )?;
    store.conn().execute_batch(
        "UPDATE bookmark SET updated_at = created_at; \
         PRAGMA wal_checkpoint(TRUNCATE); VACUUM;",
    )?;
    eprintln!("{count} bookmarks in {:.1?}", start.elapsed());
    Ok(())
}
