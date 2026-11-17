//! The CLI's AI commands (feature `ai`): `summarize`, and the LLM layer of `suggest`.

use std::io::Write;
use std::process::ExitCode;

use anyhow::{Result, anyhow, bail};
use shlv::ai::{self, Provider, Task};
use shlv::bulk::{self, Progress};
use shlv::choose::Item;
use shlv::config::Config;
use shlv::extract::extract;
use shlv::fetch::Fetcher;
use shlv::{Bookmark, ListQuery, ProposalSource, ProposalState, Store};

use crate::{Style, get, plural};

/// Above this many bookmarks, a bulk run needs `--yes`.
pub const CONFIRM_ABOVE: usize = 20;

/// The provider for `task`, or an error saying how to configure one.
pub fn provider(config: &Config, task: Task) -> Result<Provider> {
    for_task(config, task)?.ok_or_else(|| {
        anyhow!(
            "no provider for {}: set [ai] {} in {} (see `shlv config show`)",
            task.as_str(),
            task.as_str(),
            shlv::config::default_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "the config file".into())
        )
    })
}

/// The provider configured for `task`, with the tag style (`[tags.style]`) set.
fn for_task(config: &Config, task: Task) -> Result<Option<Provider>> {
    let p = ai::for_task(config.ai.as_ref(), &config.provider, task)?;
    Ok(p.map(|mut p| {
        p.set_style(config.tags.style.clone());
        p
    }))
}

/// One line saying where requests go, for the notice before a run.
pub fn describe_provider(config: &Config, p: &Provider) -> String {
    match config.provider.get(p.name()).map(|c| c.get_ref()) {
        Some(c) => {
            let kind = match c.kind {
                ai::Kind::OpenaiCompatible => "openai-compatible",
                ai::Kind::Anthropic => "anthropic",
            };
            let at = c
                .base_url
                .as_deref()
                .unwrap_or(ai::anthropic::DEFAULT_BASE_URL);
            format!("provider {} ({kind}, {} at {at})", p.name(), c.model)
        }
        None => format!("provider {}", p.name()),
    }
}

/// The bookmarks a bulk command works on: the ids given (all must exist), or with `all` every
/// bookmark that `pending` selects.
pub fn targets(
    store: &Store,
    ids: &[i64],
    all: bool,
    pending: impl Fn(&Bookmark) -> bool,
) -> Result<(Vec<Bookmark>, usize)> {
    let mut v = Vec::new();
    let mut skipped = 0;
    if all {
        let q = ListQuery {
            tags: Vec::new(),
            limit: usize::MAX,
        };
        store.list_each(&q, |b| {
            if pending(&b) {
                v.push(b);
            }
            Ok(())
        })?;
    } else {
        for &id in ids {
            let b = get(store, id)?;
            if pending(&b) {
                v.push(b);
            } else {
                skipped += 1;
            }
        }
    }
    Ok((v, skipped))
}

/// Says what is about to be sent where, and requires `--yes` for large runs.
pub fn confirm(n: usize, what: &str, to: &str, yes: bool) -> Result<()> {
    eprintln!("shlv: {what} {} with {to}", plural(n, "bookmark"));
    if n > CONFIRM_ABOVE && !yes {
        bail!(
            "that sends {} to the provider; pass --yes to go ahead",
            plural(n, "bookmark")
        );
    }
    Ok(())
}

/// The page's main text: `Ok(None)` when it has none (a JavaScript shell, say).
pub fn page_text(fetcher: &Fetcher, url: &str) -> Result<Option<String>> {
    let page = fetcher
        .get(url)
        .map_err(|e| anyhow!("cannot fetch the page: {e}"))?;
    let text = extract(&page.html);
    Ok((!text.is_empty()).then_some(text))
}

/// Token use, when the provider reports it.
pub fn print_usage(p: &Provider) {
    let u = p.usage();
    if u.requests == 0 {
        return;
    }
    let cached = if u.cache_read + u.cache_write > 0 {
        format!(
            " (plus {} read from and {} written to the cache)",
            u.cache_read, u.cache_write
        )
    } else {
        String::new()
    };
    eprintln!(
        "shlv: {}: {} input tokens{cached}, {} output tokens",
        plural(u.requests as usize, "request"),
        u.input,
        u.output
    );
}

enum Summarized {
    Done(String),
    NoText,
    Failed(String),
}

/// `shlv summarize`.
#[allow(clippy::too_many_arguments)]
pub fn run_summarize(
    store: &mut Store,
    config: &Config,
    ids: &[i64],
    all: bool,
    force: bool,
    jobs: usize,
    yes: bool,
    batch: bool,
    out: &mut impl Write,
    style: &Style,
) -> Result<ExitCode> {
    let provider = provider(config, Task::Summarize)?;
    if batch && !provider.supports_batches() {
        bail!(
            "--batch needs a provider of kind \"anthropic\" (the Message Batches API); {} isn't one",
            provider.name()
        );
    }
    let (targets, had) = targets(store, ids, all, |b| force || b.summary.is_none())?;
    if had > 0 {
        eprintln!(
            "shlv: skipped {} that already have a summary (--force redoes them)",
            plural(had, "bookmark")
        );
    }
    if targets.is_empty() {
        if all {
            eprintln!("shlv: nothing to summarize");
        }
        return Ok(ExitCode::SUCCESS);
    }
    confirm(
        targets.len(),
        "summarizing",
        &describe_provider(config, &provider),
        yes,
    )?;
    if batch {
        return batch_summaries(store, &provider, &targets, jobs, out);
    }

    let fetcher = Fetcher::default();
    let mut progress = Progress::new("summarizing", targets.len());
    let (mut done, mut no_text) = (0, 0);
    let mut failed: Vec<(i64, String, String)> = Vec::new();
    bulk::run(
        &targets,
        jobs,
        |b| {
            let text = match page_text(&fetcher, &b.url) {
                Ok(Some(t)) => t,
                Ok(None) => return Summarized::NoText,
                Err(e) => return Summarized::Failed(format!("{e:#}")),
            };
            let item = Item {
                text: Some(text),
                ..Item::from_bookmark(b)
            };
            match provider.summarize(&item) {
                Ok(s) => Summarized::Done(s),
                Err(e) => Summarized::Failed(format!("{e:#}")),
            }
        },
        |i, r| -> Result<()> {
            let b = &targets[i];
            progress.clear();
            match r {
                Summarized::Done(s) => {
                    // Committed one by one: an interrupted run keeps what's done.
                    if store.set_summary(b.id, Some(&s))? {
                        done += 1;
                        writeln!(out, "{}{}{}  {s}", style.id, b.id, style.reset)?;
                        out.flush()?;
                    }
                }
                Summarized::NoText => {
                    no_text += 1;
                    eprintln!("shlv: {}: skipped, no text on the page: {}", b.id, b.url);
                }
                Summarized::Failed(e) => failed.push((b.id, b.url.clone(), e)),
            }
            progress.tick();
            Ok(())
        },
    )?;
    drop(progress);
    for (id, url, e) in &failed {
        eprintln!("shlv: {id}: failed: {url}: {e}");
    }
    eprintln!(
        "shlv: summarized {} of {}: {no_text} without page text, {} failed",
        done,
        plural(targets.len(), "bookmark"),
        failed.len()
    );
    print_usage(&provider);
    Ok(if failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// `suggest` options for the LLM layer.
#[derive(clap::Args)]
pub struct LlmArgs {
    /// With --all, send only bookmarks with fewer tags than this to the [ai] tag provider
    #[arg(long, default_value_t = 1, value_name = "N")]
    pub max_tags: usize,
    /// With --all, also send bookmarks the provider has suggested tags for before
    #[arg(long)]
    pub force: bool,
    /// Requests at a time
    #[arg(short, long, default_value_t = 2, value_parser = clap::value_parser!(u16).range(1..=16))]
    pub jobs: u16,
    /// Go ahead when more than 20 bookmarks would be sent
    #[arg(short, long)]
    pub yes: bool,
    /// Submit one Message Batch (half price, results within 24 hours) instead of live requests
    #[arg(long)]
    pub batch: bool,
}

/// The `[ai] tag` provider for back-filling one tag: it only says whether the tag fits, and
/// proposes nothing.
pub fn backfill_provider(config: &Config) -> Result<Option<Provider>> {
    for_task(config, Task::Tag)
}

/// The `[ai] tag` provider, if one is configured, set to propose new names as `[ai] proposals`
/// says (see [`proposing`]).
pub fn tag_provider(config: &Config, store: &Store) -> Result<Option<Provider>> {
    let Some(mut p) = for_task(config, Task::Tag)? else {
        return Ok(None);
    };
    p.set_proposing(proposing(config, store)?);
    Ok(Some(p))
}

/// How many new names the `tag` provider may propose per bookmark (`[ai] proposals`), and the
/// names already waiting, most proposed first, for it to reuse.
pub fn proposing(config: &Config, store: &Store) -> Result<ai::Proposing> {
    let max = config
        .ai
        .as_ref()
        .map_or(ai::DEFAULT_PROPOSALS, |a| a.proposals());
    if max == 0 {
        return Ok(ai::Proposing::default());
    }
    let known = store
        .proposals(ProposalState::Pending)?
        .into_iter()
        .take(ai::MAX_KNOWN)
        .map(|p| p.name)
        .collect();
    Ok(ai::Proposing { max, known })
}

/// Whether one request can return both a summary and tags: the same provider does both tasks.
fn same_provider(config: &Config) -> bool {
    let ai = config.ai.as_ref();
    match (
        ai.and_then(|a| a.summarize.as_ref()),
        ai.and_then(|a| a.tag.as_ref()),
    ) {
        (Some(s), Some(t)) => s.get_ref() == t.get_ref(),
        _ => false,
    }
}

/// Offers an answer's list tags for a bookmark and records its new names as proposals. Returns
/// the suggestion stats (including names that turned out to be list tags) and the names newly
/// proposed for the bookmark.
fn record(
    store: &mut Store,
    bookmark: i64,
    a: &ai::Answer,
) -> Result<(shlv::OfferStats, Vec<String>)> {
    let mut st = store.offer_choices(bookmark, shlv::Source::Llm, &a.tags)?;
    let p = store.propose_tags(bookmark, ProposalSource::Llm, &a.proposed)?;
    for why in a.refused.iter().chain(&p.refused) {
        eprintln!("shlv: {bookmark}: dropped proposed tag: {why}");
    }
    st.pending += p.suggested.pending;
    st.applied += p.suggested.applied;
    Ok((st, p.proposed))
}

/// The LLM layer of `suggest`: asks the `[ai] tag` provider to choose from the canonical list,
/// and offers its choices like the other layers (source `llm`, no confidence). New names it
/// proposes become proposals for the list; with an empty list, that is all it does. With `only`,
/// the list is that one tag (back-filling it).
#[allow(clippy::too_many_arguments)]
pub fn run_suggest_llm(
    store: &mut Store,
    config: &Config,
    provider: &Provider,
    ids: &[i64],
    all: bool,
    args: &LlmArgs,
    skip: Option<&std::collections::HashSet<i64>>,
    only: Option<&str>,
    out: &mut impl Write,
) -> Result<ExitCode> {
    use shlv::Source;
    use shlv::choose::TagOption;

    let list: Vec<TagOption> = store
        .canonical_tags()?
        .iter()
        .filter(|t| only.is_none_or(|o| t.name == o))
        .map(TagOption::from)
        .collect();
    if list.is_empty() && !provider.proposes() {
        bail!(
            "the [ai] tag provider chooses from the canonical tag list, which is empty, and \
             [ai] proposals = 0 turns proposing new tags off; declare tags in [tags.canonical], \
             mark some with `shlv tag canonical add`, or allow proposals"
        );
    }
    // Explicit ids are always sent; --max-tags narrows --all.
    let (targets, _) = targets(store, ids, all, |b| !all || b.tags.len() < args.max_tags)?;
    let mut work = Vec::with_capacity(targets.len());
    for b in targets {
        if all && !args.force && store.suggested_by(b.id, Source::Llm)? {
            continue;
        }
        // The classifier was sure about it (`[model] provider = "fallback"`).
        if skip.is_some_and(|s| s.contains(&b.id)) {
            continue;
        }
        // Tags the bookmark has or rejected aren't offered to the model at all.
        let excluded = store.excluded_tags(b.id)?;
        let options: Vec<TagOption> = list
            .iter()
            .filter(|t| !excluded.contains(&t.name))
            .cloned()
            .collect();
        // With nothing left to choose, it can still propose.
        if !options.is_empty() || provider.proposes() {
            work.push((b, options));
        }
    }
    if work.is_empty() {
        if all {
            eprintln!("shlv: no bookmarks to send to the [ai] tag provider");
        }
        return Ok(ExitCode::SUCCESS);
    }
    confirm(
        work.len(),
        "suggesting tags for",
        &describe_provider(config, provider),
        args.yes,
    )?;

    let combined = same_provider(config);
    if args.batch {
        if !provider.supports_batches() {
            bail!(
                "--batch needs a provider of kind \"anthropic\" (the Message Batches API); {} \
                 isn't one",
                provider.name()
            );
        }
        let need: Vec<Bookmark> = work
            .iter()
            .filter(|(b, _)| b.summary.is_none())
            .map(|(b, _)| b.clone())
            .collect();
        let texts = fetch_texts(&need, args.jobs.into());
        let mut text_of: std::collections::HashMap<i64, Option<String>> = need
            .iter()
            .zip(texts)
            .map(|(b, t)| (b.id, t.ok().flatten()))
            .collect();
        let jobs: Vec<BatchJob> = work
            .iter()
            .map(|(b, _)| {
                let text = text_of.remove(&b.id).flatten();
                let job = if combined && text.is_some() {
                    ai::Job::Both
                } else {
                    ai::Job::Tags
                };
                BatchJob {
                    bookmark: b.id,
                    job,
                    item: Item {
                        text,
                        ..Item::from_bookmark(b)
                    },
                }
            })
            .collect();
        // One schema for the whole batch: the full list. Tags a bookmark has or rejected are
        // dropped when the results are collected.
        submit_batch(store, provider, &jobs, &list, out)?;
        return Ok(ExitCode::SUCCESS);
    }
    let fetcher = Fetcher::default();
    let mut progress = Progress::new("suggesting", work.len());
    let (mut pending, mut applied, mut proposed, mut summarized) = (0, 0, 0, 0);
    let mut failed: Vec<(i64, String, String)> = Vec::new();
    let mode = store.suggest_mode();
    bulk::run(
        &work,
        args.jobs.into(),
        |(b, options)| -> Result<ai::Answer> {
            let mut item = Item::from_bookmark(b);
            if item.summary.is_none() {
                // Without a summary, the page text; if that fails, what's left still helps.
                item.text = page_text(&fetcher, &b.url).ok().flatten();
                if combined && item.text.is_some() {
                    return provider.summarize_and_tag(&item, options);
                }
            }
            provider.tag(&item, options)
        },
        |i, r| -> Result<()> {
            let b = &work[i].0;
            progress.clear();
            match r {
                Ok(a) => {
                    if let Some(s) = &a.summary
                        && store.set_summary(b.id, Some(s))?
                    {
                        summarized += 1;
                    }
                    let (st, new) = record(store, b.id, &a)?;
                    pending += st.pending;
                    applied += st.applied;
                    proposed += new.len();
                    if all {
                        let mut line: Vec<String> = a.tags.iter().map(|c| c.tag.clone()).collect();
                        line.extend(a.proposed.iter().map(|t| format!("+{}", t.name)));
                        writeln!(out, "{}  {}", b.id, line.join(" "))?;
                        out.flush()?;
                    }
                }
                Err(e) => failed.push((b.id, b.url.clone(), format!("{e:#}"))),
            }
            progress.tick();
            Ok(())
        },
    )?;
    drop(progress);
    for (id, url, e) in &failed {
        eprintln!("shlv: {id}: failed: {url}: {e}");
    }
    let mut what = match mode {
        shlv::SuggestMode::Confirm => plural(pending, "new suggestion"),
        shlv::SuggestMode::Auto => format!("{} applied", plural(applied, "tag")),
    };
    if proposed > 0 {
        what.push_str(&format!(", {}", plural(proposed, "new proposal")));
    }
    let also = if summarized > 0 {
        format!(
            ", {} summarized in the same request",
            plural(summarized, "bookmark")
        )
    } else {
        String::new()
    };
    writeln!(
        out,
        "llm: {} sent, {} failed: {what}{also}",
        plural(work.len(), "bookmark"),
        failed.len()
    )?;
    print_usage(provider);
    Ok(if failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// `shlv batch` subcommands.
#[derive(clap::Subcommand)]
pub enum BatchCmd {
    /// Show submitted batches that haven't been collected, with their progress
    Status,
    /// Apply the results of ended batches (summaries and tag suggestions)
    Collect,
}

/// One job for a batch: the bookmark, what to ask, and what to send.
struct BatchJob {
    bookmark: i64,
    job: ai::Job,
    item: Item,
}

/// Submits `jobs` as one Message Batch and records it.
fn submit_batch(
    store: &mut Store,
    provider: &Provider,
    jobs: &[BatchJob],
    list: &[shlv::choose::TagOption],
    out: &mut impl Write,
) -> Result<()> {
    if jobs.is_empty() {
        eprintln!("shlv: nothing to submit");
        return Ok(());
    }
    let requests: Vec<(String, ai::Job, Item)> = jobs
        .iter()
        .map(|j| {
            let (s, t) = match j.job {
                ai::Job::Summary => (true, false),
                ai::Job::Tags => (false, true),
                ai::Job::Both => (true, true),
            };
            (
                shlv::batch::custom_id(j.bookmark, s, t),
                j.job,
                j.item.clone(),
            )
        })
        .collect();
    let id = provider.submit_batch(&requests, list)?;
    store.add_batch(&id, provider.name(), requests.len())?;
    writeln!(
        out,
        "submitted batch {id} ({}); collect the results with `shlv batch collect`",
        plural(requests.len(), "request")
    )?;
    Ok(())
}

/// Fetches page text for bookmarks a few at a time. Returns, in input order, the text (or
/// `None` when the page has none) or why the fetch failed.
pub(crate) fn fetch_texts(
    targets: &[Bookmark],
    jobs: usize,
) -> Vec<Result<Option<String>, String>> {
    let fetcher = Fetcher::default();
    let mut texts: Vec<Result<Option<String>, String>> = vec![Ok(None); targets.len()];
    let mut progress = Progress::new("fetching", targets.len());
    let _ = bulk::run(
        targets,
        jobs,
        |b| page_text(&fetcher, &b.url).map_err(|e| format!("{e:#}")),
        |i, r| -> Result<(), ()> {
            texts[i] = r;
            progress.tick();
            Ok(())
        },
    );
    texts
}

/// `summarize --batch`: fetch pages now, submit the summaries as a batch.
fn batch_summaries(
    store: &mut Store,
    provider: &Provider,
    targets: &[Bookmark],
    jobs: usize,
    out: &mut impl Write,
) -> Result<ExitCode> {
    let texts = fetch_texts(targets, jobs);
    let mut batch = Vec::new();
    let mut failed = 0;
    for (b, t) in targets.iter().zip(texts) {
        match t {
            Ok(Some(text)) => batch.push(BatchJob {
                bookmark: b.id,
                job: ai::Job::Summary,
                item: Item {
                    text: Some(text),
                    ..Item::from_bookmark(b)
                },
            }),
            Ok(None) => eprintln!("shlv: {}: skipped, no text on the page: {}", b.id, b.url),
            Err(e) => {
                failed += 1;
                eprintln!("shlv: {}: failed: {}: {e}", b.id, b.url);
            }
        }
    }
    submit_batch(store, provider, &batch, &[], out)?;
    Ok(if failed > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// `shlv batch status|collect`.
pub fn run_batch(
    store: &mut Store,
    config: &Config,
    cmd: BatchCmd,
    out: &mut impl Write,
) -> Result<ExitCode> {
    use shlv::choose::TagOption;

    let pending = store.pending_batches()?;
    if pending.is_empty() {
        writeln!(out, "no batches waiting")?;
        return Ok(ExitCode::SUCCESS);
    }
    let list: Vec<TagOption> = store
        .canonical_tags()?
        .iter()
        .map(TagOption::from)
        .collect();
    let mut code = ExitCode::SUCCESS;
    for pb in pending {
        let Some(pc) = config.provider.get(&pb.provider) else {
            eprintln!(
                "shlv: batch {}: provider {} is no longer configured",
                pb.id, pb.provider
            );
            code = ExitCode::FAILURE;
            continue;
        };
        let mut provider = Provider::new(&pb.provider, pc.get_ref())?;
        provider.set_style(config.tags.style.clone());
        // New names in the results are kept up to the current `[ai] proposals`.
        provider.set_proposing(ai::Proposing {
            known: Vec::new(),
            ..proposing(config, store)?
        });
        let st = provider.batch_status(&pb.id)?;
        let counts = format!(
            "{} processing, {} succeeded, {} errored, {} expired, {} canceled",
            st.processing, st.succeeded, st.errored, st.expired, st.canceled
        );
        if matches!(cmd, BatchCmd::Status) || !st.ended {
            let state = if st.ended { "ended" } else { "in progress" };
            writeln!(
                out,
                "{}  {} ({}): {state}: {counts}",
                pb.id,
                pb.provider,
                plural(pb.requests as usize, "request")
            )?;
            continue;
        }
        let results = provider.batch_results(
            &st,
            |id| {
                shlv::batch::parse_custom_id(id).map(|(_, s, t)| match (s, t) {
                    (true, true) => ai::Job::Both,
                    (true, false) => ai::Job::Summary,
                    _ => ai::Job::Tags,
                })
            },
            &list,
        )?;
        let (mut summaries, mut suggested, mut proposed, mut failed) = (0, 0, 0, Vec::new());
        for (cid, r) in results {
            let Some((id, _, tags)) = shlv::batch::parse_custom_id(&cid) else {
                failed.push((cid, "unknown custom_id".to_string()));
                continue;
            };
            match r {
                Ok(a) => {
                    if let Some(s) = &a.summary
                        && store.set_summary(id, Some(s))?
                    {
                        summaries += 1;
                    }
                    if tags && store.get(id)?.is_some() {
                        let (st, new) = record(store, id, &a)?;
                        suggested += st.pending + st.applied;
                        proposed += new.len();
                    }
                }
                Err(e) => failed.push((format!("bookmark {id}"), e)),
            }
        }
        for (what, e) in &failed {
            eprintln!("shlv: batch {}: {what}: {e}", pb.id);
        }
        let proposed = if proposed > 0 {
            format!(", {}", plural(proposed, "new proposal"))
        } else {
            String::new()
        };
        writeln!(
            out,
            "collected batch {}: {} summaries, {} tag suggestions{proposed}, {} failed",
            pb.id,
            summaries,
            suggested,
            failed.len()
        )?;
        if !failed.is_empty() {
            eprintln!(
                "shlv: rerun `shlv summarize --all` or `shlv suggest --all` (without --batch) to \
                 retry the failed ones live"
            );
            code = ExitCode::FAILURE;
        }
        store.finish_batch(&pb.id)?;
        print_usage(&provider);
    }
    Ok(code)
}

/// Bookmarks per gap group sent to the provider.
const GROUP_SAMPLE: usize = 15;

/// `tag health --name-gaps N`: asks the `[ai] tag` provider for a tag name for each of the `n`
/// largest gap groups, sending up to [`GROUP_SAMPLE`] of each group's titles and URLs. The
/// canonical list goes along, so the reply can say a listed tag already fits.
pub fn name_gaps(
    config: &Config,
    store: &Store,
    gaps: &mut shlv::health::Gaps,
    n: usize,
) -> Result<()> {
    use shlv::health::GroupName;

    let mut p = provider(config, Task::Tag)?;
    let mut proposing = proposing(config, store)?;
    proposing.max = 1;
    p.set_proposing(proposing);
    let list: Vec<shlv::choose::TagOption> = store
        .canonical_tags()?
        .iter()
        .map(shlv::choose::TagOption::from)
        .collect();
    let mut groups: Vec<(&str, &mut shlv::health::Group)> = gaps
        .by_host
        .iter_mut()
        .map(|g| ("site", g))
        .chain(gaps.by_word.iter_mut().map(|g| ("word", g)))
        .filter(|(_, g)| g.existing.is_none())
        .collect();
    groups.sort_by_key(|(_, g)| std::cmp::Reverse(g.ids.len()));
    groups.truncate(n);
    if groups.is_empty() {
        return Ok(());
    }
    eprintln!(
        "shlv: asking {} to name {}: up to {GROUP_SAMPLE} titles and URLs each",
        describe_provider(config, &p),
        plural(groups.len(), "gap group")
    );
    for (kind, g) in groups {
        let mut lines = Vec::new();
        for &id in g.ids.iter().take(GROUP_SAMPLE) {
            let b = get(store, id)?;
            lines.push(match &b.title {
                Some(t) => format!("- {t} ({})", b.url),
                None => format!("- {}", b.url),
            });
        }
        let item = Item {
            url: match kind {
                "site" => format!("https://{}/", g.key),
                _ => get(store, g.ids[0])?.url,
            },
            title: Some(format!(
                "A group of {} bookmarks with no fitting tag, sharing the {kind} {:?}; one tag \
                 for what they have in common",
                g.ids.len(),
                g.key
            )),
            note: Some(lines.join("\n")),
            summary: None,
            text: None,
        };
        match p.tag(&item, &list) {
            Ok(a) => {
                g.named = match (a.proposed.into_iter().next(), a.tags.into_iter().next()) {
                    (Some(t), _) => Some(GroupName {
                        name: t.name,
                        description: t.description,
                        listed: false,
                    }),
                    (None, Some(c)) => Some(GroupName {
                        name: c.tag,
                        description: None,
                        listed: true,
                    }),
                    (None, None) => None,
                };
            }
            Err(e) => eprintln!("shlv: cannot name gap group {:?}: {e:#}", g.key),
        }
    }
    Ok(())
}
