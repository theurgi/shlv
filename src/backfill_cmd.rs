//! `shlv suggest --tag TAG` (task 037): back-fill one canonical tag. A tag new to the list is
//! missing from every bookmark reviewed before it, and the classifier would learn from them that
//! it doesn't apply. This offers only that tag: the classifier from a low probability up (a new
//! tag has only its lexical score), and the `[ai] tag` provider if configured, then lists the
//! candidates (reviewed bookmarks first) or goes through them one key each (`--pick`). `review`
//! runs the same steps after a batch adds tags.

use std::io::Write;
use std::process::ExitCode;

use anyhow::{Result, bail};
use shlv::{BackfillCandidate, Config, Store};

use crate::screen::{Key, Screen};
use crate::{Style, id_width, plural, print_pending};

/// What to back-fill, and where.
pub struct Backfill<'a> {
    /// The tag as typed: a canonical tag or an alias of one.
    pub tag: &'a str,
    pub ids: &'a [i64],
    pub all: bool,
    pub pick: bool,
}

pub fn run(
    store: &mut Store,
    config: &Config,
    b: Backfill,
    #[cfg(feature = "ai")] llm: &crate::ai_cmd::LlmArgs,
    out: &mut impl Write,
    style: &Style,
) -> Result<ExitCode> {
    let tag = store.backfill_tag(b.tag)?;
    let scope = (!b.all).then_some(b.ids);
    let code = offer(
        store,
        config,
        &tag,
        scope,
        b.all,
        #[cfg(feature = "ai")]
        llm,
        out,
    )?;
    let candidates = store.backfill_candidates(&tag, scope)?;
    if b.pick {
        out.flush()?;
        pick(store, &mut Screen::open(), &tag, &candidates, out)?;
    } else {
        list(store, &tag, &candidates, out, style)?;
    }
    Ok(code)
}

/// Offers `tag` with every layer there is: the classifier on `scope` (the given bookmarks, or
/// all), then the provider on `scope`, or with `unoffered`, on reviewed bookmarks the classifier
/// passed over.
#[allow(unused_variables, unused_mut)]
pub(crate) fn offer(
    store: &mut Store,
    config: &Config,
    tag: &str,
    scope: Option<&[i64]>,
    unoffered: bool,
    #[cfg(feature = "ai")] llm: &crate::ai_cmd::LlmArgs,
    out: &mut impl Write,
) -> Result<ExitCode> {
    let model_on = cfg!(feature = "model");
    // `provider = "never"` keeps the provider out wherever the classifier runs.
    #[cfg(feature = "ai")]
    let provider = if model_on && config.model.provider == shlv::config::ProviderUse::Never {
        None
    } else {
        crate::ai_cmd::backfill_provider(config)?
    };
    #[cfg(not(feature = "ai"))]
    let provider: Option<()> = None;
    if !model_on && provider.is_none() {
        bail!(
            "back-filling a tag needs the classifier (feature `model`) or an [ai] tag provider \
             (feature `ai`)"
        );
    }
    #[cfg(feature = "model")]
    {
        let classifier = store.classifier(true)?;
        let s = store.backfill_model(tag, scope, &classifier, &config.model)?;
        let what = match store.suggest_mode() {
            shlv::SuggestMode::Confirm => plural(s.pending, "new suggestion"),
            shlv::SuggestMode::Auto => format!(
                "{} applied, {}",
                plural(s.applied, "tag"),
                plural(s.pending, "new suggestion")
            ),
        };
        writeln!(
            out,
            "classifier checked {} for #{tag}: {what}",
            plural(s.checked, "bookmark")
        )?;
    }
    #[cfg(feature = "ai")]
    if let Some(p) = provider {
        // With --all, only reviewed bookmarks the classifier passed over: they're the examples
        // that teach it, and the rest get the tag offered as they're reviewed.
        let targets = if unoffered {
            store.backfill_unoffered(tag)?
        } else {
            scope.unwrap_or_default().to_vec()
        };
        out.flush()?;
        return crate::ai_cmd::run_suggest_llm(
            store,
            config,
            &p,
            &targets,
            false,
            llm,
            None,
            Some(tag),
            out,
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// The candidates, ranked, one per line.
fn list(
    store: &Store,
    tag: &str,
    candidates: &[BackfillCandidate],
    out: &mut impl Write,
    style: &Style,
) -> Result<()> {
    if candidates.is_empty() {
        writeln!(out, "no candidates for #{tag}")?;
        return Ok(());
    }
    let width = id_width(store)?;
    let reviewed = candidates.iter().filter(|c| c.reviewed).count();
    writeln!(
        out,
        "{} for #{tag}, {reviewed} of them reviewed (listed first):",
        plural(candidates.len(), "candidate")
    )?;
    for c in candidates {
        print_pending(out, std::slice::from_ref(&c.suggestion), width, style)?;
    }
    writeln!(
        out,
        "(shlv suggest --tag {tag} --pick ... to go through them, or suggest accept/reject ID {tag})"
    )?;
    Ok(())
}

const HELP: &str = "\
  y, Enter  it has the tag: accept
  n         it doesn't: reject (not offered again)
  s         skip: leave it pending
  q         stop here (answers so far are saved)";

/// Asks about each candidate, saving each answer as it's given. `review` runs it for the tags a
/// batch added.
pub(crate) fn pick(
    store: &mut Store,
    screen: &mut Screen,
    tag: &str,
    candidates: &[BackfillCandidate],
    out: &mut impl Write,
) -> Result<()> {
    if candidates.is_empty() {
        writeln!(out, "no candidates for #{tag}")?;
        return Ok(());
    }
    let width = id_width(store)?;
    let keys = screen.dim("y accept  n reject  s skip  q stop  ? help");
    writeln!(
        screen,
        "#{tag} on which of {}?\n{keys}",
        plural(candidates.len(), "candidate")
    )?;
    let (mut accepted, mut rejected) = (0, 0);
    let mut i = 0;
    while i < candidates.len() {
        let c = &candidates[i];
        let Some(b) = store.get(c.suggestion.id)? else {
            i += 1;
            continue;
        };
        let line = crate::edit_cmd::row(screen, &b, width);
        let mut why = c.suggestion.source.as_str().to_string();
        if let Some(p) = c.suggestion.confidence {
            why.push_str(&format!(" {p:.2}"));
        }
        if c.reviewed {
            why.push_str(", reviewed");
        }
        let prompt = format!("  #{tag} {}", screen.dim(&format!("({why})")));
        writeln!(screen, "{line}")?;
        write!(screen, "{prompt}? ")?;
        let answer = loop {
            match screen.key()? {
                Key::Enter | Key::Char('y') => break Some('y'),
                Key::Char('n') => break Some('n'),
                Key::Char('s') => break Some('s'),
                Key::Char('q') | Key::Eof | Key::Interrupt => break None,
                Key::Char('?') => {
                    screen.redraw()?;
                    writeln!(screen, "{HELP}")?;
                }
                Key::Char(c) => {
                    screen.redraw()?;
                    writeln!(screen, "{c:?} isn't a key here; ? lists them")?;
                }
            }
            write!(screen, "{prompt}? ")?;
        };
        screen.redraw()?;
        let Some(a) = answer else {
            writeln!(screen, "{prompt}  stopped")?;
            break;
        };
        let tags = [tag.to_string()];
        match a {
            'y' => {
                store.accept(b.id, &tags)?;
                accepted += 1;
            }
            'n' => {
                store.reject(b.id, &tags)?;
                rejected += 1;
            }
            _ => {}
        }
        let said = match a {
            'y' => "accepted",
            'n' => "rejected",
            _ => "skipped",
        };
        writeln!(screen, "{prompt}  {said}")?;
        i += 1;
    }
    let left = candidates.len() - accepted - rejected;
    writeln!(
        out,
        "#{tag}: {accepted} accepted, {rejected} rejected, {left} still pending"
    )?;
    Ok(())
}
