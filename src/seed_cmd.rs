//! `shlv tag seed` (feature `ai`, task 034): expand interests into candidate tags with the
//! `[ai] tag` provider and go through them one key each.

use std::io::{self, Write};
use std::process::ExitCode;

use anyhow::Result;
use shlv::ai::{Provider, Task};
use shlv::config::Config;
use shlv::seed::normalize_interest;
use shlv::{Candidate, Decision, SeedName, Seeded, Store};

use crate::ai_cmd::{describe_provider, print_usage, provider};
use crate::screen::{Key, Screen};
use crate::{Style, plural};

#[derive(clap::Args)]
pub struct SeedArgs {
    /// Interests to expand, such as `nix network home-lab` (default: the stored ones)
    #[arg(value_name = "TOPIC")]
    topics: Vec<String>,
    /// Store the interests in the database, for tag health and later drafting
    #[arg(long)]
    save: bool,
    /// Most candidates per interest
    #[arg(short = 'n', long, default_value_t = 8, value_parser = clap::value_parser!(u8).range(1..=30))]
    per: u8,
    /// Send without asking first
    #[arg(short, long)]
    yes: bool,
}

/// `shlv tag seed`.
pub fn run_seed(
    store: &mut Store,
    config: &Config,
    args: SeedArgs,
    out: &mut impl Write,
    style: &Style,
) -> Result<ExitCode> {
    let provider = provider(config, Task::Tag)?;
    let mut interests = interests_of(args.topics.iter().map(String::as_str));
    if interests.is_empty() {
        interests = store.interests()?.into_iter().map(|i| i.name).collect();
        if interests.is_empty() {
            anyhow::bail!(
                "name some interests to expand, such as `shlv tag seed nix network home-lab`; \
                 --save stores them, and [tags] interests in the config pins some"
            );
        }
    }
    let to = describe_provider(config, &provider);
    let mut screen = Screen::open();
    notice(&mut screen, config, &to, &interests)?;
    if !args.yes {
        write!(screen, "send? [Y/n] ")?;
        let k = screen.key()?;
        screen.redraw()?;
        if !matches!(k, Key::Enter | Key::Char('y' | 'Y')) {
            writeln!(screen, "nothing sent")?;
            return Ok(ExitCode::SUCCESS);
        }
    }
    let per = usize::from(args.per);
    let store_ref = &*store;
    let expand = |names: &[String]| expand(&provider, store_ref, names, per);
    let mut cands = Vec::new();
    let mut session = Session {
        expanded: Vec::new(),
        to,
    };
    session.add(&mut screen, &mut cands, &interests, &expand)?;
    let mut add = |screen: &mut Screen, cands: &mut Vec<Candidate>| {
        add_interests(screen, cands, &mut session, &expand)
    };
    let end = review(&mut screen, &mut cands, Some(&mut add), |raw| {
        store_ref.seed_name(raw)
    })?;
    if end == End::Abort {
        writeln!(screen, "nothing written")?;
        print_usage(&provider);
        return Ok(ExitCode::SUCCESS);
    }
    let save = if args.save {
        session.expanded.clone()
    } else {
        Vec::new()
    };
    let s = store.apply_seed(&cands, &save)?;
    report(out, "seeded", &s, args.save.then_some(s.saved), style)?;
    print_usage(&provider);
    Ok(if s.skipped.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// What was written: skipped decisions on stderr, the rest as one line on `out`, starting with
/// `verb`. `saved`: new interests stored, when asked to store them.
pub(crate) fn report(
    out: &mut impl Write,
    verb: &str,
    s: &Seeded,
    saved: Option<usize>,
    style: &Style,
) -> io::Result<()> {
    for why in &s.skipped {
        eprintln!("shlv: {why}");
    }
    let t = |n: &str| format!("{}{n}{}", style.tag, style.reset);
    write!(
        out,
        "{verb}: {} on the list",
        plural(s.kept.len(), "new tag")
    )?;
    if !s.kept.is_empty() {
        let names: Vec<String> = s.kept.iter().map(|n| t(n)).collect();
        write!(out, " ({})", names.join(" "))?;
    }
    if !s.merged.is_empty() {
        let m: Vec<String> = s
            .merged
            .iter()
            .map(|(a, b)| format!("{a} → {}", t(b)))
            .collect();
        write!(
            out,
            ", {} ({})",
            plural(s.merged.len(), "alias"),
            m.join(", ")
        )?;
    }
    if s.described > 0 {
        write!(out, ", {}", plural(s.described, "description"))?;
    }
    if let Some(n) = saved {
        write!(out, ", {} saved", plural(n, "new interest"))?;
    }
    writeln!(out)
}

/// Interests as typed: split on whitespace and commas, normalized, deduplicated in order.
fn interests_of<'a>(words: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for w in words.flat_map(|w| w.split([',', ' ', '\t'])) {
        let i = normalize_interest(w);
        if !i.is_empty() && !v.contains(&i) {
            v.push(i);
        }
    }
    v
}

/// Says exactly what is sent, and where.
fn notice(screen: &mut Screen, config: &Config, to: &str, interests: &[String]) -> io::Result<()> {
    let g = match config.tags.style.guidelines() {
        Some(g) => format!(
            "your [tags.style] guidelines ({})",
            plural(g.lines().count(), "line")
        ),
        None => "no guidelines ([tags.style] has none)".into(),
    };
    writeln!(
        screen,
        "shlv: tag seed sends to {to}:\n  \
         interests: {}\n  \
         how tags are named (the separator and length from [tags.style]), and {g}\n  \
         nothing else: no bookmarks, no tags from your list",
        interests.join(", ")
    )
}

/// The provider's candidates for `interests`, styled and resolved against the database, with
/// notes on names that couldn't be used and interests that got none.
fn expand(
    provider: &Provider,
    store: &Store,
    interests: &[String],
    per: usize,
) -> Result<(Vec<Candidate>, Vec<String>)> {
    let seeds = provider.seed(interests, per)?;
    let mut notes: Vec<String> = seeds
        .refused
        .iter()
        .map(|w| format!("dropped: {w}"))
        .collect();
    let mut cands = Vec::new();
    for (interest, tags) in seeds.groups {
        if tags.is_empty() {
            notes.push(format!("no candidates for {interest}"));
        }
        for t in tags {
            match store.seed_name(&t.name) {
                Ok(n) => cands.push(Candidate {
                    interest: interest.clone(),
                    decision: if n.listed() {
                        Decision::Listed
                    } else {
                        Decision::Undecided
                    },
                    name: n.name().to_string(),
                    description: t.description,
                    edited: false,
                    hint: None,
                }),
                Err(e) => notes.push(format!("dropped {:?}: {e:#}", t.name)),
            }
        }
    }
    Ok((cands, notes))
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum End {
    Save,
    Abort,
}

/// What the review needs besides the candidates: interests sent so far, and where they go.
struct Session {
    expanded: Vec<String>,
    to: String,
}

type Expand<'a> = dyn Fn(&[String]) -> Result<(Vec<Candidate>, Vec<String>)> + 'a;

impl Session {
    /// Expands interests not expanded yet and appends their candidates (names already among the
    /// candidates are left out). Fails only on the provider's error.
    fn add(
        &mut self,
        screen: &mut Screen,
        cands: &mut Vec<Candidate>,
        interests: &[String],
        expand: &Expand,
    ) -> Result<()> {
        let new: Vec<String> = interests
            .iter()
            .filter(|i| !self.expanded.contains(i))
            .cloned()
            .collect();
        if new.is_empty() {
            writeln!(screen, "already expanded: {}", interests.join(", "))?;
            return Ok(());
        }
        writeln!(screen, "expanding {} with {}…", new.join(", "), self.to)?;
        let (more, notes) = expand(&new)?;
        for n in notes {
            let n = screen.dim(&n);
            writeln!(screen, "{n}")?;
        }
        for c in more {
            if !cands.iter().any(|o| o.name == c.name) {
                cands.push(c);
            }
        }
        self.expanded.extend(new);
        Ok(())
    }
}

const HELP: &str = "\
  k, Enter  keep: put it on the list
  r         rename, then keep
  m         merge into a tag or another candidate (it becomes an alias)
  d         drop
  e         edit the description
  b         back to the previous candidate
  q         finish: save what's decided so far
  Ctrl-C    quit without saving";

const HELP_ADD: &str = "  a         add interests and expand them";

const HELP_HINT: &str = "\
  For a listed tag that looks redundant: m merges it (Enter at the prompt takes the suggested
  tag); k and d leave it as it is.";

/// Adds candidates during the review (`tag seed`: expands more interests).
pub(crate) type Add<'a> = dyn FnMut(&mut Screen, &mut Vec<Candidate>) -> Result<()> + 'a;

/// The group a candidate is shown under: its interest, `redundant` for a listed tag that looks
/// redundant, `other` for none.
fn group(c: &Candidate) -> &str {
    match (&c.hint, c.interest.as_str()) {
        (Some(_), _) => "redundant",
        (None, "") => "other",
        (None, i) => i,
    }
}

/// Goes through the candidates one key each (`tag seed`, `tag draft`). Candidates already on the
/// list are shown, not asked about. With `add`, the `a` key adds more.
pub(crate) fn review(
    screen: &mut Screen,
    cands: &mut Vec<Candidate>,
    mut add: Option<&mut Add>,
    name_of: impl Fn(&str) -> Result<SeedName>,
) -> Result<End> {
    let a = if add.is_some() {
        "a add interest  "
    } else {
        ""
    };
    let keys = screen.dim(&format!(
        "k keep  r rename  m merge into  d drop  e edit description  {a}b back  q finish  ? help"
    ));
    writeln!(screen, "\n{keys}")?;
    let mut i = 0;
    // The group whose name was last shown in the first column.
    let mut shown: Option<String> = None;
    loop {
        if i >= cands.len() {
            match finish(screen, cands, add.is_some())? {
                Finish::Save => return Ok(End::Save),
                Finish::Abort => return Ok(End::Abort),
                Finish::Back => {
                    i = prev_open(cands, cands.len()).unwrap_or(0);
                    shown = None;
                }
                Finish::Add => {
                    if let Some(add) = add.as_deref_mut()
                        && let Err(e) = add(screen, cands)
                    {
                        writeln!(screen, "shlv: {e:#}")?;
                    }
                }
            }
            continue;
        }
        let first = shown.as_deref() != Some(group(&cands[i]));
        shown = Some(group(&cands[i]).to_string());
        let line = row(screen, cands, i, first);
        if cands[i].decision == Decision::Listed {
            writeln!(screen, "{line}{}", mark(screen, &cands[i]))?;
            i += 1;
            continue;
        }
        let now = match &cands[i].decision {
            Decision::Undecided => String::new(),
            _ => format!("{} ", screen.dim(&format!("[{}]", plain_mark(&cands[i])))),
        };
        let q = screen.dim("?");
        write!(screen, "{line}{now}{q} ")?;
        let key = screen.key()?;
        let decided = match key {
            Key::Enter | Key::Char('k') => Some(Decision::Keep),
            Key::Char('d') => Some(Decision::Drop),
            Key::Char('r') if cands[i].hint.is_some() => {
                screen.redraw()?;
                writeln!(
                    screen,
                    "{} is on the list: `shlv tag rename` renames it; m merges it",
                    cands[i].name
                )?;
                None
            }
            Key::Char('r') => {
                screen.redraw()?;
                rename(screen, cands, i, &name_of)?.map(|new| {
                    // Candidates merged into it follow the new name.
                    let old = std::mem::replace(&mut cands[i].name, new.clone());
                    for c in cands.iter_mut() {
                        if c.decision == Decision::Merge(old.clone()) {
                            c.decision = Decision::Merge(new.clone());
                        }
                        if let Some(h) = &mut c.hint
                            && h.into == old
                        {
                            h.into.clone_from(&new);
                        }
                    }
                    Decision::Keep
                })
            }
            Key::Char('m') => {
                screen.redraw()?;
                merge_target(screen, cands, i, &name_of)?
            }
            Key::Char('e') => {
                screen.redraw()?;
                if let Some(d) = screen.line(&format!("description of {}: ", cands[i].name))?
                    && !d.is_empty()
                {
                    match shlv::tag::check_description(&d) {
                        Ok(d) => {
                            cands[i].description = d;
                            cands[i].edited = true;
                        }
                        Err(e) => writeln!(screen, "{e}")?,
                    }
                }
                None
            }
            Key::Char('a') if add.is_some() => {
                screen.redraw()?;
                if let Some(add) = add.as_deref_mut()
                    && let Err(e) = add(screen, cands)
                {
                    writeln!(screen, "shlv: {e:#}")?;
                }
                None
            }
            Key::Char('b') => {
                screen.redraw()?;
                if let Some(p) = prev_open(cands, i) {
                    i = p;
                }
                shown = None;
                continue;
            }
            Key::Char('q') | Key::Eof => {
                screen.redraw()?;
                i = cands.len();
                continue;
            }
            Key::Interrupt => {
                screen.redraw()?;
                return Ok(End::Abort);
            }
            Key::Char('?') => {
                screen.redraw()?;
                writeln!(screen, "{HELP}")?;
                if add.is_some() {
                    writeln!(screen, "{HELP_ADD}")?;
                }
                if cands.iter().any(|c| c.hint.is_some()) {
                    writeln!(screen, "{HELP_HINT}")?;
                }
                None
            }
            Key::Char(c) => {
                screen.redraw()?;
                writeln!(screen, "{c:?} isn't a key here; ? lists them")?;
                None
            }
        };
        match decided {
            Some(d) => {
                cands[i].decision = d;
                screen.redraw()?;
                let line = row(screen, cands, i, first);
                writeln!(screen, "{line}{}", mark(screen, &cands[i]))?;
                i += 1;
            }
            // Asked again, under its group.
            None => shown = None,
        }
    }
}

/// The index of the last candidate before `i` that is asked about.
fn prev_open(cands: &[Candidate], i: usize) -> Option<usize> {
    (0..i)
        .rev()
        .find(|&j| cands[j].decision != Decision::Listed)
}

/// What the description column shows: the description, or for a redundant tag, the suggestion.
fn about(c: &Candidate) -> String {
    match &c.hint {
        Some(h) => match &h.reason {
            Some(r) => format!("→ {}: {r}", h.into),
            None => format!("→ {}", h.into),
        },
        None => c.description.clone().unwrap_or_default(),
    }
}

/// One candidate's line, up to its decision: the group (on the first of it), name, and
/// description, cut to the terminal's width.
fn row(screen: &Screen, cands: &[Candidate], i: usize, first: bool) -> String {
    let s = &screen.style;
    let iw = cands
        .iter()
        .map(|c| group(c).chars().count())
        .max()
        .unwrap_or(0);
    let nw = cands
        .iter()
        .map(|c| c.name.chars().count())
        .max()
        .unwrap_or(0)
        .min(24);
    let c = &cands[i];
    let g = if first { group(c) } else { "" };
    let room = screen.width().saturating_sub(iw + nw + 20).max(20);
    let desc = about(c);
    let desc = match desc.char_indices().nth(room) {
        Some((at, _)) => format!("{}…", &desc[..at]),
        None => desc,
    };
    let pad = room.min(
        cands
            .iter()
            .map(|c| about(c).chars().count())
            .max()
            .unwrap_or(0),
    );
    format!(
        "{g:<iw$}  {}{:<nw$}{}  {}{desc:<pad$}{}  ",
        s.tag, c.name, s.reset, s.dim, s.reset
    )
}

fn plain_mark(c: &Candidate) -> String {
    match (&c.decision, &c.hint) {
        (Decision::Keep | Decision::Drop, Some(_)) => "left as is".into(),
        (Decision::Undecided, _) => String::new(),
        (Decision::Keep, None) => "k".into(),
        (Decision::Drop, None) => "d".into(),
        (Decision::Merge(into), _) => format!("m → {into}"),
        (Decision::Listed, _) => "on the list".into(),
    }
}

fn mark(screen: &Screen, c: &Candidate) -> String {
    match &c.decision {
        Decision::Listed => format!("{}on the list{}", screen.style.dim, screen.style.reset),
        _ => plain_mark(c),
    }
}

/// Asks for a new name: a valid one that isn't on the list or another candidate.
fn rename(
    screen: &mut Screen,
    cands: &[Candidate],
    i: usize,
    name_of: &impl Fn(&str) -> Result<SeedName>,
) -> Result<Option<String>> {
    let Some(raw) = screen.line(&format!("rename {} to: ", cands[i].name))? else {
        return Ok(None);
    };
    if raw.is_empty() {
        return Ok(None);
    }
    let n = match name_of(&raw) {
        Ok(n) => n,
        Err(e) => {
            writeln!(screen, "{e:#}")?;
            return Ok(None);
        }
    };
    if n.listed() {
        writeln!(
            screen,
            "{} is already on the list; m merges into it",
            n.name()
        )?;
        return Ok(None);
    }
    if cands
        .iter()
        .enumerate()
        .any(|(j, c)| j != i && c.name == n.name())
    {
        writeln!(
            screen,
            "{} is another candidate; m merges into it",
            n.name()
        )?;
        return Ok(None);
    }
    Ok(Some(n.name().to_string()))
}

/// Asks what to merge into: another candidate (not dropped) or an existing tag. For a redundant
/// tag, an empty answer takes the suggested one.
fn merge_target(
    screen: &mut Screen,
    cands: &[Candidate],
    i: usize,
    name_of: &impl Fn(&str) -> Result<SeedName>,
) -> Result<Option<Decision>> {
    let hint = cands[i].hint.as_ref().map(|h| h.into.as_str());
    let prompt = match hint {
        Some(h) => format!("merge {} into [{h}]: ", cands[i].name),
        None => format!("merge {} into: ", cands[i].name),
    };
    let Some(mut raw) = screen.line(&prompt)? else {
        return Ok(None);
    };
    if raw.is_empty() {
        match hint {
            Some(h) => raw = h.to_string(),
            None => return Ok(None),
        }
    }
    let target = match name_of(&raw) {
        Ok(n) => n,
        Err(e) => {
            writeln!(screen, "{e:#}")?;
            return Ok(None);
        }
    };
    let name = target.name();
    if name == cands[i].name {
        writeln!(screen, "can't merge {name} into itself")?;
        return Ok(None);
    }
    let candidate = cands.iter().find(|c| c.name == name && c.hint.is_none());
    match (candidate, &target) {
        (Some(c), _) if c.decision == Decision::Drop => {
            writeln!(screen, "{name} was dropped; keep it first (b goes back)")?;
            Ok(None)
        }
        (Some(_), _) | (None, SeedName::Tag { .. }) => Ok(Some(Decision::Merge(name.into()))),
        (None, SeedName::New(_)) => {
            writeln!(screen, "no tag or candidate {name}")?;
            Ok(None)
        }
    }
}

/// Asks for more interests and expands them.
fn add_interests(
    screen: &mut Screen,
    cands: &mut Vec<Candidate>,
    session: &mut Session,
    expand: &Expand,
) -> Result<()> {
    let Some(line) = screen.line("add interests: ")? else {
        return Ok(());
    };
    let new = interests_of(std::iter::once(line.as_str()));
    if new.is_empty() {
        return Ok(());
    }
    let note = screen.dim("sends the interests and naming rules only");
    writeln!(screen, "{note}")?;
    session.add(screen, cands, &new, expand)
}

enum Finish {
    Save,
    Abort,
    Back,
    Add,
}

/// The end of the list: counts, then save, discard, go back, or (with `add`) add interests.
fn finish(screen: &mut Screen, cands: &[Candidate], add: bool) -> Result<Finish> {
    let n = |f: &dyn Fn(&Candidate) -> bool| cands.iter().filter(|c| f(c)).count();
    let keep = n(&|c| c.decision == Decision::Keep && c.hint.is_none());
    let merge = n(&|c| matches!(c.decision, Decision::Merge(_)));
    let drop = n(&|c| c.decision == Decision::Drop && c.hint.is_none());
    let open = n(&|c| c.decision == Decision::Undecided);
    let mut what = format!("\n{keep} to keep, {merge} to merge, {drop} dropped");
    if open > 0 {
        what.push_str(&format!(", {open} undecided (left out)"));
    }
    writeln!(screen, "{what}")?;
    loop {
        let more = screen.dim(if add {
            "b back  a add interests"
        } else {
            "b back"
        });
        write!(screen, "save? [Y/n]  {more} ")?;
        let k = screen.key()?;
        screen.redraw()?;
        return Ok(match k {
            Key::Enter | Key::Char('y' | 'Y' | 'q') | Key::Eof => Finish::Save,
            Key::Char('n') | Key::Interrupt => Finish::Abort,
            Key::Char('b') => Finish::Back,
            Key::Char('a') if add => Finish::Add,
            _ => continue,
        });
    }
}
