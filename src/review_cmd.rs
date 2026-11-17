//! `shlv review` (task 039): go through bookmarks in batches, one screen each, and settle their
//! tags. After each batch: the names it proposed (add, alias, reject), back-fill for tags just
//! added, a retrain, and the progress so far. See [`shlv::review`] for the queue and how decisions
//! are stored.

use std::collections::HashSet;
use std::io::Write;
use std::process::ExitCode;

use anyhow::Result;
use shlv::review::{self, BatchRecord, BatchStats, Decision, ReviewFilter};
use shlv::{Bookmark, Config, ProposalState, Source, Store};

use crate::screen::{Key, Screen};
use crate::{Style, plural, share};

#[derive(clap::Args)]
pub struct ReviewArgs {
    /// Bookmarks per batch
    #[arg(short = 'n', long, default_value_t = 30, value_parser = clap::value_parser!(u16).range(1..))]
    size: u16,
    /// Only bookmarks with a pending suggestion of TAG (reviewed or not)
    #[arg(long, value_name = "TAG")]
    tag: Option<String>,
    /// Only bookmarks with a pending suggestion from SOURCE (reviewed or not)
    #[arg(long, value_enum)]
    source: Option<SourceArg>,
    /// Only bookmarks whose most confident suggestion is below P (0..1), least confident first
    #[arg(long, value_name = "P", value_parser = share)]
    below: Option<f64>,
    /// Print the progress of past batches and exit
    #[arg(long, conflicts_with_all = ["tag", "source", "below"])]
    history: bool,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum SourceArg {
    Rule,
    Model,
    Llm,
}

impl From<SourceArg> for Source {
    fn from(s: SourceArg) -> Source {
        match s {
            SourceArg::Rule => Source::Rule,
            SourceArg::Model => Source::Model,
            SourceArg::Llm => Source::Llm,
        }
    }
}

/// Bookmarks scored when picking the classifier's least sure ones: a random sample keeps a batch
/// quick to start at 100k bookmarks.
#[cfg(feature = "model")]
const SAMPLE: usize = 2000;

pub fn run(
    store: &mut Store,
    config: &Config,
    args: ReviewArgs,
    out: &mut impl Write,
    style: &Style,
) -> Result<ExitCode> {
    if args.history {
        let h = store.review_history()?;
        if h.is_empty() {
            writeln!(out, "no review batches yet")?;
        } else {
            print_history(out, &h, style)?;
        }
        return Ok(ExitCode::SUCCESS);
    }
    let filter = ReviewFilter {
        tag: args.tag,
        source: args.source.map(Source::from),
        below: args.below,
    };
    let size = usize::from(args.size);
    let mut screen = Screen::open();
    writeln!(
        screen,
        "{}",
        screen.dim(
            "Tag each bookmark completely: a tag left off a reviewed bookmark teaches the \
             classifier that it doesn't apply."
        )
    )?;
    let (mut reviewed, mut skipped, mut batches) = (0, 0, 0);
    loop {
        let queue = store.review_queue(&filter)?;
        if queue.is_empty() {
            let what = if filter.is_empty() {
                "every bookmark is reviewed"
            } else {
                "no bookmarks match"
            };
            writeln!(screen, "{what}")?;
            break;
        }
        let (ids, order) = choose(store, config, &queue, &filter, size)?;
        let (batch, started) = store.start_review_batch()?;
        batches += 1;
        writeln!(
            screen,
            "\nbatch {batches}: {} of {} to review, {order}",
            ids.len(),
            queue.len()
        )?;
        offer(store, config, &ids, &mut screen)?;
        let names = Names::load(store)?;
        let mut stats = BatchStats::default();
        let mut done = Vec::new();
        let mut stop = Stop::No;
        for (i, &id) in ids.iter().enumerate() {
            let Some(b) = store.get(id)? else { continue };
            let pos = format!("[{}/{}]", i + 1, ids.len());
            match review_one(&mut screen, store, batch, &b, &pos, &names)? {
                Outcome::Reviewed { shown, accepted } => {
                    stats.reviewed += 1;
                    stats.model_shown += shown;
                    stats.model_accepted += accepted;
                    done.push(id);
                }
                Outcome::Skipped => stats.skipped += 1,
                Outcome::Stop(s) => {
                    stop = s;
                    break;
                }
            }
        }
        stats.proposed = store.proposals_since(started)?;
        if stop != Stop::Now {
            match proposals(&mut screen, store, &done)? {
                Some(added) => {
                    stats.tags_added = added.len();
                    backfill(&mut screen, store, config, &added)?;
                }
                None => stop = Stop::Now,
            }
        }
        if stop != Stop::Now {
            retrain(&mut screen, store)?;
        }
        store.finish_review_batch(batch, &stats)?;
        reviewed += stats.reviewed;
        skipped += stats.skipped;
        if stop != Stop::Now && stats.reviewed > 0 {
            writeln!(screen)?;
            print_history(&mut screen, &store.review_history()?, style)?;
        }
        if stop != Stop::No {
            break;
        }
        write!(screen, "\n{} ", screen.dim("Enter next batch  q stop"))?;
        let k = screen.key()?;
        screen.redraw()?;
        if !matches!(k, Key::Enter | Key::Char('y' | 'n' | ' ')) {
            break;
        }
    }
    drop(screen);
    write!(
        out,
        "reviewed {} in {}",
        plural(reviewed, "bookmark"),
        plural(batches, "batch")
    )?;
    if skipped > 0 {
        write!(out, ", {skipped} skipped")?;
    }
    writeln!(out)?;
    Ok(ExitCode::SUCCESS)
}

/// The next batch and how it was ordered.
#[allow(unused_variables)]
fn choose(
    store: &mut Store,
    config: &Config,
    queue: &[review::Queued],
    filter: &ReviewFilter,
    size: usize,
) -> Result<(Vec<i64>, &'static str)> {
    if !filter.is_empty() {
        let mut q: Vec<&review::Queued> = queue.iter().collect();
        q.sort_by(|a, b| a.top.total_cmp(&b.top).then(b.id.cmp(&a.id)));
        let ids = q.into_iter().take(size).map(|q| q.id).collect();
        return Ok((ids, "least confident suggestions first"));
    }
    #[cfg(feature = "model")]
    if !shlv::model::canonical_list(store)?.is_empty()
        && store.reviewed_count()? >= review::DISCOVERY
    {
        let all: Vec<i64> = queue.iter().map(|q| q.id).collect();
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |d| d.as_nanos() as u64);
        let sample = review::sample(&all, SAMPLE, seed);
        let classifier = store.classifier(true)?;
        let scores = store.review_confidence(&sample, &classifier)?;
        return Ok((
            review::least_confident(scores, size),
            "the classifier's least sure first",
        ));
    }
    let hosts = store.reviewed_hosts()?;
    Ok((
        review::discovery_order(queue, &hosts, size),
        "spread across domains, untagged first",
    ))
}

/// Suggests tags for the batch: domain rules, the classifier, then the `[ai] tag` provider for
/// bookmarks the classifier wasn't sure about (`[model] provider`) and the provider hasn't seen.
#[allow(unused_variables, unused_mut)]
fn offer(store: &mut Store, config: &Config, ids: &[i64], screen: &mut Screen) -> Result<()> {
    if !store.rules().is_empty() {
        store.apply_rules(Some(ids))?;
    }
    #[cfg(feature = "model")]
    let model_on = !shlv::model::canonical_list(store)?.is_empty();
    #[cfg(feature = "model")]
    let sure = if model_on {
        let classifier = store.classifier(true)?;
        let s = store.apply_model(Some(ids), &classifier, &config.model)?;
        (config.model.provider == shlv::config::ProviderUse::Fallback).then_some(s.sure)
    } else {
        None
    };
    #[cfg(not(feature = "model"))]
    let (model_on, sure): (bool, Option<HashSet<i64>>) = (false, None);
    #[cfg(feature = "ai")]
    {
        let provider = match config.model.provider {
            shlv::config::ProviderUse::Never if model_on => None,
            _ => crate::ai_cmd::tag_provider(config, store)?,
        };
        let usable = match &provider {
            Some(p) => p.proposes() || !store.canonical_tags()?.is_empty(),
            None => false,
        };
        if let (Some(p), true) = (provider, usable) {
            let mut targets = Vec::new();
            for &id in ids {
                if sure.as_ref().is_some_and(|s| s.contains(&id))
                    || store.suggested_by(id, Source::Llm)?
                {
                    continue;
                }
                targets.push(id);
            }
            if !targets.is_empty() {
                let llm = crate::ai_cmd::LlmArgs {
                    max_tags: 1,
                    force: false,
                    jobs: 2,
                    yes: true,
                    batch: false,
                };
                crate::ai_cmd::run_suggest_llm(
                    store, config, &p, &targets, false, &llm, None, None, screen,
                )?;
            }
        }
    }
    Ok(())
}

/// Tag names to complete from: canonical tags and their aliases, and pending proposals.
struct Names {
    /// (name, the canonical tag it stands for); sorted by name.
    canonical: Vec<(String, String)>,
    proposed: Vec<String>,
}

impl Names {
    fn load(store: &Store) -> Result<Names> {
        let mut canonical = Vec::new();
        for t in store.canonical_tags()? {
            for a in &t.aliases {
                canonical.push((a.clone(), t.name.clone()));
            }
            canonical.push((t.name.clone(), t.name));
        }
        canonical.sort();
        let proposed = store
            .proposals(ProposalState::Pending)?
            .into_iter()
            .map(|p| p.name)
            .collect();
        Ok(Names {
            canonical,
            proposed,
        })
    }

    /// Canonical tags (by name or alias) starting with `prefix`, then proposals.
    fn matching(&self, prefix: &str) -> Vec<String> {
        let p = prefix.trim().trim_start_matches('#').to_lowercase();
        let mut v: Vec<String> = Vec::new();
        for (name, tag) in &self.canonical {
            if name.starts_with(&p) && !v.contains(tag) {
                v.push(tag.clone());
            }
        }
        for name in &self.proposed {
            if name.starts_with(&p) && !v.contains(name) {
                v.push(name.clone());
            }
        }
        v
    }
}

/// What the user typed, resolved.
enum Typed {
    Canonical(String),
    /// A name off the list: a proposal.
    New(String),
}

/// Asks for a tag: a canonical tag by name, alias, or unique prefix; a pending proposal by name
/// or unique prefix; or, confirmed, a new name to propose (unless `canonical_only`).
fn ask_tag(
    screen: &mut Screen,
    store: &Store,
    names: &Names,
    prompt: &str,
    canonical_only: bool,
) -> Result<Option<Typed>> {
    let mut initial = String::new();
    loop {
        let Some(text) = screen.complete(prompt, &initial, |s| names.matching(s))? else {
            return Ok(None);
        };
        if text.is_empty() {
            return Ok(None);
        }
        let name = store.styled(&text);
        let looked = shlv::store::normalize_tag(&text);
        // An exact name or alias wins over a prefix.
        for n in [Some(looked.clone()), name.as_ref().ok().cloned()]
            .into_iter()
            .flatten()
        {
            if let Some((_, tag)) = names.canonical.iter().find(|(a, _)| *a == n) {
                return Ok(Some(Typed::Canonical(tag.clone())));
            }
            if !canonical_only && names.proposed.contains(&n) {
                return Ok(Some(Typed::New(n)));
            }
        }
        let mut found = names.matching(&looked);
        if canonical_only {
            found.retain(|t| names.canonical.iter().any(|(_, c)| c == t));
        }
        match found.len() {
            1 => {
                let t = found.remove(0);
                let canonical = names.canonical.iter().any(|(_, c)| *c == t);
                return Ok(Some(if canonical {
                    Typed::Canonical(t)
                } else {
                    Typed::New(t)
                }));
            }
            0 if canonical_only => {
                writeln!(screen, "  no canonical tag starts with {looked:?}")?;
            }
            0 => {
                let name = match name {
                    Ok(n) => n,
                    Err(e) => {
                        writeln!(screen, "  {e}")?;
                        initial = text;
                        continue;
                    }
                };
                write!(
                    screen,
                    "  #{name} isn't on the list: propose it? {} ",
                    screen.dim("y/n")
                )?;
                let k = screen.key()?;
                screen.redraw()?;
                if matches!(k, Key::Char('y') | Key::Enter) {
                    return Ok(Some(Typed::New(name)));
                }
            }
            _ => {
                let shown: Vec<&str> = found.iter().take(10).map(String::as_str).collect();
                writeln!(screen, "  {looked:?} could be: {}", shown.join(" "))?;
                initial = text;
                continue;
            }
        }
        initial.clear();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stop {
    No,
    /// `q`: end the batch here and finish it (proposals, back-fill, retrain).
    AfterBatch,
    /// Ctrl-C: leave at once.
    Now,
}

enum Outcome {
    /// Saved; how many classifier suggestions were shown, and accepted.
    Reviewed {
        shown: usize,
        accepted: usize,
    },
    Skipped,
    Stop(Stop),
}

#[derive(Clone, PartialEq)]
enum Kind {
    /// On the bookmark already.
    Current,
    /// Pending, from a layer.
    Suggested(Source, Option<f64>),
    /// Typed: a canonical tag.
    Added,
    /// Typed: a name off the list, proposed.
    New,
}

struct Row {
    tag: String,
    kind: Kind,
    on: bool,
}

const HELP: &str = "\
  Enter, y  save: the checked tags, and the unchecked suggestions rejected
  1-9       check or uncheck a tag
  t, +      add a tag: a canonical one by prefix (Tab completes), or a new name to propose
  s, n      skip: leave it for another batch
  o         open it in the browser
  q         end the batch here (its proposals, back-fill, and retrain still run)
  Ctrl-C    stop at once (what's saved stays saved)";

/// One bookmark's screen.
fn review_one(
    screen: &mut Screen,
    store: &mut Store,
    batch: i64,
    b: &Bookmark,
    pos: &str,
    names: &Names,
) -> Result<Outcome> {
    let mut rows: Vec<Row> = b
        .tags
        .iter()
        .map(|t| Row {
            tag: t.clone(),
            kind: Kind::Current,
            on: true,
        })
        .collect();
    for s in store.pending(Some(b.id))? {
        rows.push(Row {
            tag: s.tag,
            kind: Kind::Suggested(s.source, s.confidence),
            on: true,
        });
    }
    let proposed = store.proposals_for(b.id)?;
    let mut confirm_empty = false;
    let mut drawn = draw(screen, b, pos, &rows, &proposed)?;
    loop {
        let redraw = match screen.key()? {
            Key::Enter | Key::Char('y') => {
                let empty = !rows.iter().any(|r| r.on) && proposed.is_empty();
                if empty && !confirm_empty {
                    confirm_empty = true;
                    screen.redraw()?;
                    writeln!(
                        screen,
                        "  no tags: Enter again to save it without any, or t to add one"
                    )?;
                    drawn += 1;
                    write!(screen, "  {} ", screen.dim("›"))?;
                    continue;
                }
                screen.erase_up(drawn)?;
                return save(screen, store, batch, b, pos, &rows);
            }
            Key::Char(c @ '1'..='9') => {
                let i = c as usize - '1' as usize;
                match rows.get_mut(i) {
                    Some(r) => r.on = !r.on,
                    None => {
                        screen.redraw()?;
                        writeln!(screen, "  no tag {c}")?;
                        drawn += 1;
                    }
                }
                true
            }
            Key::Char('t' | '+') => {
                screen.redraw()?;
                let typed = ask_tag(screen, store, names, "  add tag: #", false)?;
                // The prompt line stays when it was answered on a line of its own.
                if !screen.keys() {
                    drawn += 1;
                }
                if let Some(t) = typed {
                    let (tag, kind) = match t {
                        Typed::Canonical(t) => (t, Kind::Added),
                        Typed::New(t) => (t, Kind::New),
                    };
                    match rows.iter_mut().find(|r| r.tag == tag) {
                        Some(r) => r.on = true,
                        None if proposed.contains(&tag) => {}
                        None => rows.push(Row {
                            tag,
                            kind,
                            on: true,
                        }),
                    }
                }
                true
            }
            Key::Char('s' | 'n') => {
                screen.erase_up(drawn)?;
                writeln!(screen, "{pos} {}  skipped", short(screen, b))?;
                return Ok(Outcome::Skipped);
            }
            Key::Char('o') => {
                screen.redraw()?;
                if let Err(e) = crate::open(&b.url) {
                    writeln!(screen, "  {e:#}")?;
                    drawn += 1;
                }
                true
            }
            Key::Char('q') | Key::Eof => {
                screen.erase_up(drawn)?;
                writeln!(screen, "{pos} {}  stopped", short(screen, b))?;
                return Ok(Outcome::Stop(Stop::AfterBatch));
            }
            Key::Interrupt => {
                screen.redraw()?;
                writeln!(screen, "stopped")?;
                return Ok(Outcome::Stop(Stop::Now));
            }
            Key::Char('?') => {
                screen.redraw()?;
                writeln!(screen, "{HELP}")?;
                drawn += HELP.lines().count();
                false
            }
            Key::Char(c) => {
                screen.redraw()?;
                writeln!(screen, "  {c:?} isn't a key here; ? lists them")?;
                drawn += 1;
                false
            }
        };
        if redraw {
            confirm_empty = false;
            screen.erase_up(drawn)?;
            drawn = draw(screen, b, pos, &rows, &proposed)?;
        } else {
            write!(screen, "  {} ", screen.dim("›"))?;
        }
    }
}

/// Draws a bookmark's screen, ending at the prompt; returns the number of lines above it.
fn draw(
    screen: &mut Screen,
    b: &Bookmark,
    pos: &str,
    rows: &[Row],
    proposed: &[String],
) -> Result<usize> {
    let s = &screen.style;
    let (id, dim, tag, reset) = (s.id, s.dim, s.tag, s.reset);
    let room = screen.width().saturating_sub(8).max(20);
    let title = b
        .title
        .as_deref()
        .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    let mut lines = vec![
        format!("{pos} {id}{}{reset}  {}", b.id, cut(&title, room)),
        format!("      {dim}{}{reset}", cut(&b.url, room)),
    ];
    let width = rows
        .iter()
        .map(|r| r.tag.chars().count())
        .chain(proposed.iter().map(|p| p.chars().count()))
        .max()
        .unwrap_or(0);
    for (i, r) in rows.iter().enumerate() {
        let mark = if r.on { "[x]" } else { "[ ]" };
        let key = if i < 9 {
            (i + 1).to_string()
        } else {
            " ".into()
        };
        let why = match &r.kind {
            Kind::Current => "tagged".to_string(),
            Kind::Suggested(src, Some(c)) if *c < 1.0 => format!("{} {c:.2}", src.as_str()),
            Kind::Suggested(src, _) => src.as_str().to_string(),
            Kind::Added => "added".into(),
            Kind::New => "new: proposed for the list".into(),
        };
        let pad = width - r.tag.chars().count();
        lines.push(format!(
            "  {key} {mark} {tag}#{}{reset}{:pad$}  {dim}{why}{reset}",
            r.tag, ""
        ));
    }
    for p in proposed {
        let pad = width - p.chars().count();
        lines.push(format!(
            "        {tag}#{p}{reset}{:pad$}  {dim}proposed (decided after the batch){reset}",
            ""
        ));
    }
    if !rows.iter().any(|r| r.on) && proposed.is_empty() {
        lines.push(format!("  {dim}no tags yet: t adds one{reset}"));
    }
    lines.push(format!(
        "  {dim}Enter save  1-9 toggle  t add  s skip  o open  q end batch  ? help{reset}"
    ));
    for l in &lines {
        writeln!(screen, "{l}")?;
    }
    write!(screen, "  {} ", screen.dim("›"))?;
    Ok(lines.len())
}

/// Saves the decision and leaves one line for the bookmark.
fn save(
    screen: &mut Screen,
    store: &mut Store,
    batch: i64,
    b: &Bookmark,
    pos: &str,
    rows: &[Row],
) -> Result<Outcome> {
    let mut d = Decision::default();
    let (mut shown, mut accepted) = (0, 0);
    for r in rows {
        let list = match (&r.kind, r.on) {
            (Kind::Current, true) => None,
            (Kind::Current, false) => Some(&mut d.remove),
            (Kind::Suggested(..), true) => Some(&mut d.accept),
            (Kind::Suggested(..), false) => Some(&mut d.reject),
            (Kind::Added, true) => Some(&mut d.add),
            (Kind::New, true) => Some(&mut d.propose),
            (Kind::Added | Kind::New, false) => None,
        };
        if let Some(l) = list {
            l.push(r.tag.clone());
        }
        if let Kind::Suggested(Source::Model, _) = r.kind {
            shown += 1;
            accepted += usize::from(r.on);
        }
    }
    let c = store.commit_review(batch, b.id, &d)?;
    let s = &screen.style;
    let mut line = format!("{pos} {}", short(screen, b));
    let on: Vec<&Row> = rows
        .iter()
        .filter(|r| r.on && !matches!(r.kind, Kind::New))
        .collect();
    if on.is_empty() && c.diverted.proposed.is_empty() {
        line.push_str("  no tags");
    }
    for r in on {
        line.push_str(&format!("  {}#{}{}", s.tag, r.tag, s.reset));
    }
    for p in &c.diverted.proposed {
        line.push_str(&format!("  {}+#{p} proposed{}", s.dim, s.reset));
    }
    for p in &c.diverted.rejected {
        line.push_str(&format!("  {}#{p} was rejected before{}", s.dim, s.reset));
    }
    let off: Vec<String> = rows
        .iter()
        .filter(|r| !r.on && matches!(r.kind, Kind::Suggested(..) | Kind::Current))
        .map(|r| format!("#{}", r.tag))
        .collect();
    if !off.is_empty() {
        line.push_str(&format!("  {}(not {}){}", s.dim, off.join(" "), s.reset));
    }
    writeln!(screen, "{line}")?;
    Ok(Outcome::Reviewed { shown, accepted })
}

/// `12  Title` (or the URL), cut short.
fn short(screen: &Screen, b: &Bookmark) -> String {
    let s = &screen.style;
    let text = b.title.as_deref().unwrap_or(&b.url);
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("{}{}{}  {}", s.id, b.id, s.reset, cut(&text, 40))
}

fn cut(text: &str, n: usize) -> String {
    match text.char_indices().nth(n.saturating_sub(1)) {
        Some((at, _)) if text.chars().count() > n => format!("{}…", &text[..at]),
        _ => text.to_string(),
    }
}

const PROPOSAL_HELP: &str = "\
  a  add it to the canonical list (its bookmarks get the tag)
  l  make it an alias of a canonical tag (its bookmarks get that tag)
  r  reject it: it won't be proposed again
  s  skip: decide later (shlv tag proposals)
  q  stop deciding for this batch";

/// The names proposed for the batch's bookmarks: add, alias, or reject each. Returns the tags
/// added to the list, or `None` on Ctrl-C.
fn proposals(screen: &mut Screen, store: &mut Store, done: &[i64]) -> Result<Option<Vec<String>>> {
    let done: HashSet<i64> = done.iter().copied().collect();
    let list: Vec<shlv::Proposal> = store
        .proposals(ProposalState::Pending)?
        .into_iter()
        .filter(|p| p.bookmarks.iter().any(|b| done.contains(b)))
        .collect();
    let mut added = Vec::new();
    if list.is_empty() {
        return Ok(Some(added));
    }
    writeln!(
        screen,
        "\n{} for the list, from this batch\n{}",
        plural(list.len(), "name"),
        screen.dim("a add  l alias  r reject  s skip  q stop  ? help")
    )?;
    let mut names = Names::load(store)?;
    for p in &list {
        let s = &screen.style;
        let mut prompt = format!(
            "  {}#{}{}  on {}",
            s.tag,
            p.name,
            s.reset,
            plural(p.bookmarks.len(), "bookmark")
        );
        if let Some(d) = &p.description {
            prompt.push_str(&format!("  {}# {d}{}", s.dim, s.reset));
        }
        prompt.push_str(&format!("  {}({}){}", s.dim, p.source.as_str(), s.reset));
        loop {
            write!(screen, "{prompt}? ")?;
            let k = screen.key()?;
            screen.redraw()?;
            let said = match k {
                Key::Char('a') => match store.accept_proposal(&p.name) {
                    Ok(d) => {
                        added.push(d.tag.clone());
                        format!("added, tagged {}", plural(d.tagged, "bookmark"))
                    }
                    Err(e) => format!("{e:#}"),
                },
                Key::Char('l') => {
                    writeln!(screen, "{prompt}")?;
                    match ask_tag(screen, store, &names, "    alias of: #", true)? {
                        Some(Typed::Canonical(tag)) => match store.alias_proposal(&p.name, &tag) {
                            Ok(d) => format!(
                                "alias of #{}, tagged {}",
                                d.tag,
                                plural(d.tagged, "bookmark")
                            ),
                            Err(e) => format!("{e:#}"),
                        },
                        _ => continue,
                    }
                }
                Key::Char('r') => {
                    store.reject_proposal(&p.name)?;
                    "rejected".into()
                }
                Key::Char('s') => "skipped".into(),
                Key::Char('q') | Key::Eof => {
                    writeln!(screen, "{prompt}  stopped")?;
                    return Ok(Some(added));
                }
                Key::Interrupt => return Ok(None),
                Key::Char('?') => {
                    writeln!(screen, "{PROPOSAL_HELP}")?;
                    continue;
                }
                Key::Enter => {
                    writeln!(screen, "  Enter does nothing here; ? lists the keys")?;
                    continue;
                }
                Key::Char(c) => {
                    writeln!(screen, "  {c:?} isn't a key here; ? lists them")?;
                    continue;
                }
            };
            writeln!(screen, "{prompt}  {said}")?;
            if said.starts_with("added") {
                names = Names::load(store)?;
            }
            break;
        }
    }
    Ok(Some(added))
}

/// Back-fills tags just added to the list on the bookmarks already reviewed: they lack the tag
/// only because it didn't exist yet, and would teach the classifier it doesn't apply.
#[allow(unused_variables)]
fn backfill(
    screen: &mut Screen,
    store: &mut Store,
    config: &Config,
    added: &[String],
) -> Result<()> {
    if added.is_empty() {
        return Ok(());
    }
    #[cfg(feature = "ai")]
    let have_ai = crate::ai_cmd::backfill_provider(config)?.is_some();
    #[cfg(not(feature = "ai"))]
    let have_ai = false;
    if !cfg!(feature = "model") && !have_ai {
        writeln!(
            screen,
            "\nback-fill needs the classifier (feature `model`) or an [ai] tag provider; check \
             reviewed bookmarks for {} with `shlv search` and `shlv edit`",
            added
                .iter()
                .map(|t| format!("#{t}"))
                .collect::<Vec<_>>()
                .join(" ")
        )?;
        return Ok(());
    }
    let scope = store.reviewed_ids()?;
    for tag in added {
        writeln!(screen, "\nback-fill #{tag} on reviewed bookmarks")?;
        crate::backfill_cmd::offer(
            store,
            config,
            tag,
            Some(&scope),
            true,
            #[cfg(feature = "ai")]
            &crate::ai_cmd::LlmArgs {
                max_tags: 1,
                force: false,
                jobs: 2,
                yes: true,
                batch: false,
            },
            screen,
        )?;
        let mut candidates = store.backfill_candidates(tag, Some(&scope))?;
        for c in &mut candidates {
            c.reviewed = c.reviewed || store.is_reviewed(c.suggestion.id)?;
        }
        let mut summary = Vec::new();
        crate::backfill_cmd::pick(store, screen, tag, &candidates, &mut summary)?;
        screen.write_all(&summary)?;
    }
    Ok(())
}

/// Retrains the classifier on the batch's decisions, so the next batch gets better suggestions.
#[allow(unused_variables)]
fn retrain(screen: &mut Screen, store: &mut Store) -> Result<()> {
    #[cfg(feature = "model")]
    if !shlv::model::canonical_list(store)?.is_empty() {
        let t = store.train_model()?;
        writeln!(
            screen,
            "classifier retrained on {} ({} ms)",
            plural(t.examples, "example"),
            t.millis
        )?;
    }
    Ok(())
}

/// Per batch: bookmarks reviewed, new proposals, tags added, and the share of the classifier's
/// suggestions accepted. Proposals tapering off and acceptance rising mean the list has settled.
fn print_history(out: &mut impl Write, h: &[BatchRecord], s: &Style) -> std::io::Result<()> {
    writeln!(
        out,
        "{}batch  reviewed  skipped  proposed  added  classifier accepted{}",
        s.dim, s.reset
    )?;
    for (i, b) in h.iter().enumerate() {
        let st = &b.stats;
        let model = if st.model_shown == 0 {
            "-".to_string()
        } else {
            format!(
                "{}/{} {}",
                st.model_accepted,
                st.model_shown,
                crate::percent(st.model_accepted, st.model_shown)
            )
        };
        writeln!(
            out,
            "{:>5}  {:>8}  {:>7}  {:>8}  {:>5}  {model}",
            i + 1,
            st.reviewed,
            st.skipped,
            st.proposed,
            st.tags_added
        )?;
    }
    Ok(())
}
