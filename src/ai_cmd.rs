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
use shlv::{Bookmark, ListQuery, Store};

use crate::{Style, get, plural};

/// Above this many bookmarks, a bulk run needs `--yes`.
pub const CONFIRM_ABOVE: usize = 20;

/// The provider for `task`, or an error saying how to configure one.
pub fn provider(config: &Config, task: Task) -> Result<Provider> {
    ai::for_task(config.ai.as_ref(), &config.provider, task)?.ok_or_else(|| {
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
    out: &mut impl Write,
    style: &Style,
) -> Result<ExitCode> {
    let provider = provider(config, Task::Summarize)?;
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
}

/// The `[ai] tag` provider, if one is configured.
pub fn tag_provider(config: &Config) -> Result<Option<Provider>> {
    ai::for_task(config.ai.as_ref(), &config.provider, Task::Tag)
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

struct Tagged {
    tags: Vec<shlv::choose::Choice>,
    /// Set when the same request also summarized the page.
    summary: Option<String>,
}

/// The LLM layer of `suggest`: asks the `[ai] tag` provider to choose from the canonical list,
/// and offers its choices like the other layers (source `llm`, no confidence).
pub fn run_suggest_llm(
    store: &mut Store,
    config: &Config,
    provider: &Provider,
    ids: &[i64],
    all: bool,
    args: &LlmArgs,
    out: &mut impl Write,
) -> Result<ExitCode> {
    use shlv::Source;
    use shlv::choose::TagOption;

    let list: Vec<TagOption> = store
        .canonical_tags()?
        .iter()
        .map(TagOption::from)
        .collect();
    if list.is_empty() {
        bail!(
            "the [ai] tag provider chooses from the canonical tag list, which is empty; declare \
             tags in [tags.canonical] or mark some with `shlv tag canonical add`"
        );
    }
    // Explicit ids are always sent; --max-tags narrows --all.
    let (targets, _) = targets(store, ids, all, |b| !all || b.tags.len() < args.max_tags)?;
    let mut work = Vec::with_capacity(targets.len());
    for b in targets {
        if all && !args.force && store.suggested_by(b.id, Source::Llm)? {
            continue;
        }
        // Tags the bookmark has or rejected aren't offered to the model at all.
        let excluded = store.excluded_tags(b.id)?;
        let options: Vec<TagOption> = list
            .iter()
            .filter(|t| !excluded.contains(&t.name))
            .cloned()
            .collect();
        if !options.is_empty() {
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
    let fetcher = Fetcher::default();
    let mut progress = Progress::new("suggesting", work.len());
    let (mut pending, mut applied, mut summarized) = (0, 0, 0);
    let mut failed: Vec<(i64, String, String)> = Vec::new();
    let mode = store.suggest_mode();
    bulk::run(
        &work,
        args.jobs.into(),
        |(b, options)| -> Result<Tagged> {
            let mut item = Item::from_bookmark(b);
            if item.summary.is_none() {
                // Without a summary, the page text; if that fails, what's left still helps.
                item.text = page_text(&fetcher, &b.url).ok().flatten();
                if combined && item.text.is_some() {
                    let (summary, tags) = provider.summarize_and_tag(&item, options)?;
                    return Ok(Tagged {
                        tags,
                        summary: Some(summary),
                    });
                }
            }
            Ok(Tagged {
                tags: provider.choose_tags(&item, options)?,
                summary: None,
            })
        },
        |i, r| -> Result<()> {
            let b = &work[i].0;
            progress.clear();
            match r {
                Ok(t) => {
                    if let Some(s) = &t.summary
                        && store.set_summary(b.id, Some(s))?
                    {
                        summarized += 1;
                    }
                    let st = store.offer_choices(b.id, Source::Llm, &t.tags)?;
                    pending += st.pending;
                    applied += st.applied;
                    if all {
                        let names: Vec<&str> = t.tags.iter().map(|c| c.tag.as_str()).collect();
                        writeln!(out, "{}  {}", b.id, names.join(" "))?;
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
    let what = match mode {
        shlv::SuggestMode::Confirm => plural(pending, "new suggestion"),
        shlv::SuggestMode::Auto => format!("{} applied", plural(applied, "tag")),
    };
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
