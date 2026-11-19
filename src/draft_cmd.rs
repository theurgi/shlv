//! `shlv tag draft` (task 035): the `[ai] tag` provider drafts the tag list from a sample of
//! bookmarks spread across sites, guided by the list and stored interests; the user goes through
//! the draft in `tag seed`'s screen.

use std::io::Write;
use std::process::ExitCode;
use std::time::Instant;

use anyhow::{Result, bail};
use shlv::ai::{self, DraftGuide, DraftItem, Task};
use shlv::choose::TagOption;
use shlv::config::Config;
use shlv::draft::{self, Sample};
use shlv::{Candidate, Decision, Hint, Store};

use crate::ai_cmd::{CONFIRM_ABOVE, describe_provider, fetch_texts, print_usage, provider};
use crate::screen::{Key, Screen};
use crate::seed_cmd::{End, report, review};
use crate::{Style, plural};

#[derive(clap::Args)]
pub struct DraftArgs {
    /// Bookmarks to send, spread across sites
    #[arg(short, long, value_name = "N", default_value_t = 200,
          value_parser = clap::value_parser!(u32).range(1..))]
    sample: u32,
    /// Bookmarks per request: a larger sample goes in several requests whose drafts are merged
    #[arg(long, value_name = "N", default_value_t = 50,
          value_parser = clap::value_parser!(u32).range(1..))]
    chunk: u32,
    /// Most new tags in the draft
    #[arg(short = 'n', long, value_name = "N", default_value_t = 30,
          value_parser = clap::value_parser!(u8).range(1..=100))]
    max: u8,
    /// Fetch the pages of sampled bookmarks without a summary, and send the start of their text
    #[arg(long)]
    fetch: bool,
    /// Pages fetched at a time, with --fetch
    #[arg(short, long, default_value_t = 4, value_parser = clap::value_parser!(u16).range(1..=16))]
    jobs: u16,
    /// Send without asking; needed when the sample has more than 20 bookmarks
    #[arg(short, long)]
    yes: bool,
}

/// What a draft sends, worked out before anything is sent.
pub(crate) struct Plan {
    provider: ai::Provider,
    to: String,
    list: Vec<TagOption>,
    interests: Vec<String>,
    sample: Sample,
    chunk: usize,
    max: usize,
    fetch: bool,
    jobs: usize,
}

impl Plan {
    /// Picks the sample; fails without a provider or bookmarks.
    pub(crate) fn new(
        store: &Store,
        config: &Config,
        sample: usize,
        chunk: usize,
        max: usize,
        fetch: bool,
        jobs: usize,
    ) -> Result<Plan> {
        let provider = provider(config, Task::Tag)?;
        let list: Vec<TagOption> = store
            .canonical_tags()?
            .iter()
            .map(TagOption::from)
            .collect();
        let interests: Vec<String> = store.interests()?.into_iter().map(|i| i.name).collect();
        let sample = store.draft_sample(sample)?;
        if sample.bookmarks.is_empty() {
            bail!("no bookmarks to draft from: add or import some first");
        }
        Ok(Plan {
            to: describe_provider(config, &provider),
            provider,
            list,
            interests,
            sample,
            chunk,
            max,
            fetch,
            jobs,
        })
    }

    /// Bookmarks in the sample.
    pub(crate) fn len(&self) -> usize {
        self.sample.bookmarks.len()
    }

    /// Says exactly what is sent, and where.
    pub(crate) fn notice(&self, config: &Config, out: &mut impl Write) -> std::io::Result<()> {
        notice(
            out,
            config,
            &self.to,
            &self.sample,
            self.chunk,
            &self.list,
            &self.interests,
            self.fetch,
        )
    }

    /// Sends the sample (in requests of `chunk`), merges the drafts, and returns the candidates
    /// to decide; says how each request went on `out`. Fails only if every request does.
    pub(crate) fn run(&self, store: &Store, out: &mut impl Write) -> Result<Vec<Candidate>> {
        let n = self.len();
        let chunk = self.chunk;
        let items = items(&self.sample, self.fetch, self.jobs);
        let requests = n.div_ceil(chunk);
        let mut drafts = Vec::new();
        let mut known: Vec<String> = Vec::new();
        let mut last_err = None;
        for (k, part) in items.chunks(chunk).enumerate() {
            let started = Instant::now();
            let guide = DraftGuide {
                list: &self.list,
                interests: &self.interests,
                known: &known,
                max: self.max,
            };
            let r = self.provider.draft(part, &guide);
            let took = format!("({:.1} s)", started.elapsed().as_secs_f64());
            let which = if requests > 1 {
                format!("request {} of {requests}", k + 1)
            } else {
                "draft".into()
            };
            match r {
                Ok(d) => {
                    let mut what = plural(d.tags.len(), "tag");
                    if !d.redundant.is_empty() {
                        what.push_str(&format!(", {} redundant", d.redundant.len()));
                    }
                    writeln!(out, "{which}: {what} {took}")?;
                    for w in &d.refused {
                        writeln!(out, "dropped: {w}")?;
                    }
                    for t in &d.tags {
                        if known.len() < ai::MAX_KNOWN && !known.contains(&t.name) {
                            known.push(t.name.clone());
                        }
                    }
                    drafts.push(d);
                }
                Err(e) => {
                    writeln!(out, "{which}: shlv: {e:#} {took}")?;
                    last_err = Some(e);
                }
            }
        }
        if drafts.is_empty() {
            return Err(last_err.expect("every request failed"));
        }
        let merged = draft::merge(&drafts, self.max);
        if merged.cut > 0 {
            writeln!(
                out,
                "{} proposed by fewer requests left out (-n sets how many tags)",
                plural(merged.cut, "tag")
            )?;
        }
        candidates(store, &merged, &self.interests)
    }

    pub(crate) fn print_usage(&self) {
        print_usage(&self.provider);
    }
}

/// `shlv tag draft`.
pub fn run_draft(
    store: &mut Store,
    config: &Config,
    args: DraftArgs,
    out: &mut impl Write,
    style: &Style,
) -> Result<ExitCode> {
    let plan = Plan::new(
        store,
        config,
        args.sample as usize,
        args.chunk as usize,
        usize::from(args.max),
        args.fetch,
        args.jobs.into(),
    )?;
    let n = plan.len();
    let mut screen = Screen::open();
    plan.notice(config, &mut screen)?;
    if n > CONFIRM_ABOVE && !args.yes {
        bail!(
            "that sends {} to the provider; pass --yes to go ahead (--sample sets how many)",
            plural(n, "bookmark")
        );
    }
    if !args.yes {
        write!(screen, "send? [Y/n] ")?;
        let k = screen.key()?;
        screen.redraw()?;
        if !matches!(k, Key::Enter | Key::Char('y' | 'Y')) {
            writeln!(screen, "nothing sent")?;
            return Ok(ExitCode::SUCCESS);
        }
    }
    let ran = plan.run(store, &mut screen);
    let mut cands = match ran {
        Ok(c) => c,
        Err(e) => {
            plan.print_usage();
            return Err(e);
        }
    };
    if cands.is_empty() {
        writeln!(screen, "the draft has nothing to add")?;
        plan.print_usage();
        return Ok(ExitCode::SUCCESS);
    }
    let end = review(&mut screen, &mut cands, None, |raw| store.seed_name(raw))?;
    if end == End::Abort {
        writeln!(screen, "nothing written")?;
        plan.print_usage();
        return Ok(ExitCode::SUCCESS);
    }
    let s = store.apply_seed(&cands, &[])?;
    report(out, "drafted", &s, None, style)?;
    plan.print_usage();
    Ok(if s.skipped.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Says exactly what is sent, and where.
#[allow(clippy::too_many_arguments)]
fn notice(
    screen: &mut impl Write,
    config: &Config,
    to: &str,
    sample: &Sample,
    chunk: usize,
    list: &[TagOption],
    interests: &[String],
    fetch: bool,
) -> std::io::Result<()> {
    let n = sample.bookmarks.len();
    let requests = n.div_ceil(chunk);
    let how = if requests > 1 {
        format!(", in {requests} requests of up to {chunk}")
    } else {
        String::new()
    };
    let summaries = sample
        .bookmarks
        .iter()
        .filter(|b| b.summary.is_some())
        .count();
    let have = if summaries == 1 { "has" } else { "have" };
    let text = if fetch {
        format!(
            "its summary ({summaries} {have} one) or else the start of its page text, fetched now"
        )
    } else {
        format!("its summary ({summaries} {have} one; --fetch sends page text for the rest)")
    };
    let list = if list.is_empty() {
        "no tag list yet: it drafts one".to_string()
    } else {
        format!(
            "your tag list ({}, with descriptions and aliases)",
            plural(list.len(), "tag")
        )
    };
    let interests = if interests.is_empty() {
        "no interests (none stored)".to_string()
    } else {
        format!("your interests: {}", interests.join(", "))
    };
    let g = match config.tags.style.guidelines() {
        Some(g) => format!(
            "your [tags.style] guidelines ({})",
            plural(g.lines().count(), "line")
        ),
        None => "no guidelines ([tags.style] has none)".into(),
    };
    writeln!(
        screen,
        "shlv: tag draft sends to {to}:\n  \
         {n} of {} bookmarks, from {} of {} sites{how}: each one's site, title, and {text}, up \
         to {} characters\n  \
         {list}\n  \
         {interests}\n  \
         how tags are named (the separator and length from [tags.style]), and {g}\n  \
         nothing else: no full URLs, notes, or the tags on bookmarks",
        sample.total,
        sample.sites(),
        sample.total_sites,
        ai::DRAFT_TEXT,
    )
}

/// What is sent per bookmark: its site, title, and summary, or with `fetch`, the page text of
/// those without one (a page that can't be fetched sends the rest).
fn items(sample: &Sample, fetch: bool, jobs: usize) -> Vec<DraftItem> {
    let need: Vec<shlv::Bookmark> = if fetch {
        sample
            .bookmarks
            .iter()
            .filter(|b| b.summary.is_none())
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    let mut texts: std::collections::HashMap<i64, String> = need
        .iter()
        .zip(fetch_texts(&need, jobs))
        .filter_map(|(b, t)| Some((b.id, t.ok().flatten()?)))
        .collect();
    sample
        .bookmarks
        .iter()
        .map(|b| DraftItem {
            site: draft::site(&b.url),
            title: b.title.clone(),
            text: b.summary.clone().or_else(|| texts.remove(&b.id)),
        })
        .collect()
}

/// The merged draft as candidates for the review: new tags grouped by interest (in the order
/// stored, those under none last), then listed tags that look redundant. A name that turns out
/// to be on the list is left out; a redundancy whose target is neither listed nor drafted too.
fn candidates(
    store: &Store,
    merged: &draft::Merged,
    interests: &[String],
) -> Result<Vec<Candidate>> {
    let mut cands: Vec<Candidate> = Vec::new();
    for (t, _) in &merged.tags {
        let Ok(n) = store.seed_name(&t.name) else {
            continue;
        };
        if n.listed() || cands.iter().any(|c| c.name == n.name()) {
            continue;
        }
        cands.push(Candidate {
            interest: t.interest.clone().unwrap_or_default(),
            name: n.name().to_string(),
            description: t.description.clone(),
            edited: false,
            decision: Decision::Undecided,
            hint: None,
        });
    }
    let rank = |c: &Candidate| {
        interests
            .iter()
            .position(|i| *i == c.interest)
            .unwrap_or(interests.len())
    };
    cands.sort_by_key(rank);
    for r in &merged.redundant {
        let listed = |name: &str| store.seed_name(name).is_ok_and(|n| n.listed());
        if !listed(&r.tag) || !(listed(&r.into) || cands.iter().any(|c| c.name == r.into)) {
            continue;
        }
        cands.push(Candidate {
            interest: String::new(),
            name: r.tag.clone(),
            description: None,
            edited: false,
            decision: Decision::Undecided,
            hint: Some(Hint {
                into: r.into.clone(),
                reason: r.reason.clone(),
            }),
        });
    }
    Ok(cands)
}
