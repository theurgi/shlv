//! View 5, Bootstrap (task 046): interests, and with the `ai` feature, `tag seed` (expand
//! interests into candidate tags) and `tag draft` (draft tags from a sample of bookmarks), with
//! the same keep / rename / merge / drop decisions. Sending asks first and says what is sent.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use shlv::{Candidate, Decision, Interest, SeedName, Seeded};

use super::app::{App, draw_line_input};
use super::form::{Field, Form, FormKey, draw_form};
use super::input::Input;
use super::worker::{Job, Output};

pub fn keys() -> Vec<(&'static str, &'static str)> {
    vec![
        ("", "Interests"),
        ("a", "add interests (saved; [tags] interests pins more)"),
        ("dd", "forget one (not one pinned in config)"),
        ("s", "seed: expand the interests into candidate tags (ai)"),
        ("D", "draft tags from a sample of bookmarks (ai)"),
        ("", "Candidates"),
        ("y, Enter", "keep: put it on the list"),
        ("d", "drop"),
        ("r", "rename, then keep"),
        ("m", "merge into a tag or candidate (it becomes an alias)"),
        ("e", "edit the description"),
        ("a", "add interests and expand them (seed)"),
        ("w", "write the decisions"),
        ("Esc Esc", "discard them"),
    ]
}

/// Candidates per interest, as `tag seed`'s default.
const PER: usize = 8;

#[derive(Default)]
pub struct Bootstrap {
    interests: Option<Vec<Interest>>,
    cursor: usize,
    mode: Mode,
}

#[derive(Default)]
enum Mode {
    #[default]
    Normal,
    /// The first `d` of `dd`.
    Delete,
    Form(Form<FormKind>),
    /// The worker is asking the provider.
    Waiting(String),
    Deciding(Deciding),
}

enum FormKind {
    Interests,
    Draft,
}

struct Deciding {
    /// From `tag seed` (more interests can be added), else `tag draft`.
    seed: bool,
    cands: Vec<Candidate>,
    cursor: usize,
    prompt: Option<(Prompt, Input)>,
    /// Interests expanded so far.
    expanded: Vec<String>,
    /// More candidates are on the way.
    waiting: bool,
    /// The first Esc.
    discard: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Prompt {
    Rename,
    Merge,
    Describe,
    Interests,
}

impl App {
    pub(super) fn bootstrap_enter(&mut self) {
        if self.bootstrap.interests.is_none() {
            self.interests_load();
        }
    }

    fn interests_load(&mut self) {
        match self.store.interests() {
            Ok(i) => {
                self.bootstrap.cursor = self.bootstrap.cursor.min(i.len().saturating_sub(1));
                self.bootstrap.interests = Some(i);
            }
            Err(e) => self.warn(format!("{e:#}")),
        }
    }

    pub(super) fn bootstrap_paste(&mut self, text: &str) {
        match &mut self.bootstrap.mode {
            Mode::Form(form) => form.fields[form.focus].input.insert(text.trim()),
            Mode::Deciding(d) => {
                if let Some((_, i)) = &mut d.prompt {
                    i.insert(text.trim());
                }
            }
            _ => {}
        }
    }

    pub(super) fn bootstrap_key(&mut self, k: KeyEvent) -> bool {
        match std::mem::take(&mut self.bootstrap.mode) {
            Mode::Normal => self.interests_key(k, false),
            Mode::Delete => self.interests_key(k, true),
            Mode::Form(mut form) => {
                match self.form_input(&mut form, k) {
                    FormKey::Cancel => {}
                    FormKey::Submit => {
                        if let Err(e) = self.bootstrap_submit(&form) {
                            self.warn(format!("{e:#}"));
                            self.bootstrap.mode = Mode::Form(form);
                        }
                    }
                    FormKey::Edit => self.bootstrap.mode = Mode::Form(form),
                }
                true
            }
            Mode::Waiting(what) => {
                self.bootstrap.mode = Mode::Waiting(what);
                false
            }
            Mode::Deciding(d) => {
                self.deciding(k, d);
                true
            }
        }
    }

    fn interests_key(&mut self, k: KeyEvent, delete: bool) -> bool {
        if k.modifiers.contains(KeyModifiers::CONTROL) {
            if k.code == KeyCode::Char('l') {
                self.interests_load();
                self.say("reloaded");
                return true;
            }
            return false;
        }
        let list = self.bootstrap.interests.clone().unwrap_or_default();
        let n = list.len();
        let b = &mut self.bootstrap;
        match k.code {
            KeyCode::Char('j') | KeyCode::Down => {
                b.cursor = (b.cursor + 1).min(n.saturating_sub(1))
            }
            KeyCode::Char('k') | KeyCode::Up => b.cursor = b.cursor.saturating_sub(1),
            KeyCode::Char('a') => {
                b.mode = Mode::Form(Form::new(
                    FormKind::Interests,
                    vec![Field::line("Interests", "")],
                ));
            }
            KeyCode::Char('d') if delete => {
                let Some(i) = list.get(b.cursor) else {
                    return true;
                };
                match self.store.forget_interests(std::slice::from_ref(&i.name)) {
                    Ok(_) => {
                        self.say(format!("forgot interest {}", i.name));
                        self.interests_load();
                    }
                    Err(e) => self.warn(format!("{e:#}")),
                }
            }
            KeyCode::Char('d') => {
                if let Some(i) = list.get(b.cursor) {
                    let msg = format!("d again forgets interest {}", i.name);
                    b.mode = Mode::Delete;
                    self.say(msg);
                }
            }
            KeyCode::Char('s') => {
                let names: Vec<String> = list.into_iter().map(|i| i.name).collect();
                if names.is_empty() {
                    self.warn("no interests to expand: a adds some");
                } else {
                    self.ask_seed(names);
                }
            }
            KeyCode::Char('D') => {
                b.mode = Mode::Form(Form::new(
                    FormKind::Draft,
                    vec![
                        Field::line("Sample", "200"),
                        Field::line("New tags at most", "30"),
                    ],
                ));
            }
            _ => return false,
        }
        true
    }

    fn bootstrap_submit(&mut self, form: &Form<FormKind>) -> anyhow::Result<()> {
        match form.kind {
            FormKind::Interests => {
                let names = interests_of(&form.text(0));
                if names.is_empty() {
                    anyhow::bail!("an interest, please: nix, home-lab, …");
                }
                let n = self.store.save_interests(&names)?;
                self.interests_load();
                self.say(format!(
                    "saved {} (s expands them into tags)",
                    crate::plural(n, "new interest")
                ));
            }
            FormKind::Draft => {
                let number = |i: usize, what: &str| -> anyhow::Result<usize> {
                    match form.text(i).trim().parse::<usize>() {
                        Ok(n) if n > 0 => Ok(n),
                        _ => anyhow::bail!("{what}: a number above 0"),
                    }
                };
                let (sample, max) = (number(0, "sample")?, number(1, "new tags")?);
                self.ask_draft(sample, max.min(100))?;
            }
        }
        Ok(())
    }

    /// Shows what `tag seed` sends, and sends it on `y`.
    #[allow(unused_variables)]
    pub(super) fn ask_seed(&mut self, interests: Vec<String>) {
        #[cfg(feature = "ai")]
        {
            let p = match crate::ai_cmd::provider(&self.config, shlv::ai::Task::Tag) {
                Ok(p) => p,
                Err(e) => return self.warn(format!("{e:#}")),
            };
            let to = crate::ai_cmd::describe_provider(&self.config, &p);
            let mut buf = Vec::new();
            let _ = crate::seed_cmd::notice(&mut buf, &self.config, &to, &interests);
            self.switch(super::app::View::Bootstrap);
            self.confirm(
                lines(&buf),
                Job::Seed {
                    interests,
                    per: PER,
                },
            );
        }
        #[cfg(not(feature = "ai"))]
        self.warn("tag seed needs the `ai` feature");
    }

    /// Shows what `tag draft` sends, and sends it on `y`.
    #[allow(unused_variables)]
    pub(super) fn ask_draft(&mut self, sample: usize, max: usize) -> anyhow::Result<()> {
        #[cfg(feature = "ai")]
        {
            let plan =
                crate::draft_cmd::Plan::new(&self.store, &self.config, sample, 50, max, false, 4)?;
            let mut buf = Vec::new();
            plan.notice(&self.config, &mut buf)?;
            self.switch(super::app::View::Bootstrap);
            self.confirm(
                lines(&buf),
                Job::Draft {
                    sample: plan.len(),
                    max,
                },
            );
            Ok(())
        }
        #[cfg(not(feature = "ai"))]
        anyhow::bail!("tag draft needs the `ai` feature")
    }

    /// A confirmed seed or draft was sent.
    pub(super) fn bootstrap_sent(&mut self, job: &Job) {
        let what = job.describe();
        match &mut self.bootstrap.mode {
            Mode::Deciding(d) if matches!(job, Job::Seed { .. }) => d.waiting = true,
            _ => self.bootstrap.mode = Mode::Waiting(what),
        }
    }

    /// A seed or draft came back.
    pub(super) fn bootstrap_report(&mut self, job: Job, result: Result<(String, Output), String>) {
        let (notes, cands) = match result {
            Ok((notes, Output::Candidates(c))) => (notes, c),
            Ok(_) => return,
            Err(e) => {
                if let Mode::Deciding(d) = &mut self.bootstrap.mode {
                    d.waiting = false;
                } else {
                    self.bootstrap.mode = Mode::Normal;
                }
                return self.warn(format!("{} failed: {e}", job.describe()));
            }
        };
        let (seed, interests) = match &job {
            Job::Seed { interests, .. } => (true, interests.clone()),
            _ => (false, Vec::new()),
        };
        match std::mem::take(&mut self.bootstrap.mode) {
            Mode::Deciding(mut d) if seed && d.seed => {
                let before = d.cands.len();
                for c in cands {
                    if !d.cands.iter().any(|o| o.name == c.name) {
                        d.cands.push(c);
                    }
                }
                d.expanded.extend(interests);
                d.waiting = false;
                let n = d.cands.len() - before;
                if before < d.cands.len() {
                    d.cursor = before;
                }
                self.bootstrap.mode = Mode::Deciding(d);
                self.say(join_notes(
                    format!("added {}", crate::plural(n, "candidate")),
                    &notes,
                ));
            }
            _ if cands.iter().all(|c| c.decision == Decision::Listed) => {
                let what = if cands.is_empty() {
                    "no candidates"
                } else {
                    "every candidate is on the list already"
                };
                self.say(join_notes(what.to_string(), &notes));
            }
            _ => {
                let mut d = Deciding {
                    seed,
                    cands,
                    cursor: 0,
                    prompt: None,
                    expanded: interests,
                    waiting: false,
                    discard: false,
                };
                d.cursor = next_open(&d.cands, 0).unwrap_or(0);
                self.say(join_notes(
                    format!("{} to decide", crate::plural(d.cands.len(), "candidate")),
                    &notes,
                ));
                self.bootstrap.mode = Mode::Deciding(d);
            }
        }
    }

    fn deciding(&mut self, k: KeyEvent, mut d: Deciding) {
        if let Some((p, mut input)) = d.prompt.take() {
            match k.code {
                KeyCode::Esc => {}
                KeyCode::Enter => match self.answer(&mut d, p, input.text().trim()) {
                    Ok(()) => {}
                    Err(e) => {
                        self.warn(e);
                        d.prompt = Some((p, input));
                    }
                },
                KeyCode::Tab if p == Prompt::Merge => {
                    let mut names: Vec<String> = match self.store.canonical_tags() {
                        Ok(t) => t.into_iter().map(|t| t.name).collect(),
                        Err(_) => Vec::new(),
                    };
                    names.extend(
                        d.cands
                            .iter()
                            .filter(|c| c.hint.is_none() && c.decision != Decision::Drop)
                            .map(|c| c.name.clone()),
                    );
                    if !input.complete(&names) {
                        self.say("no tag or candidate starts that way");
                    }
                    d.prompt = Some((p, input));
                }
                _ => {
                    input.key(k);
                    d.prompt = Some((p, input));
                }
            }
            self.bootstrap.mode = Mode::Deciding(d);
            return;
        }
        let discard = std::mem::take(&mut d.discard);
        let n = d.cands.len();
        let i = d.cursor;
        let listed = d
            .cands
            .get(i)
            .is_some_and(|c| c.decision == Decision::Listed);
        match k.code {
            KeyCode::Char('j') | KeyCode::Down => d.cursor = (i + 1).min(n.saturating_sub(1)),
            KeyCode::Char('k') | KeyCode::Up => d.cursor = i.saturating_sub(1),
            KeyCode::Char('g') => d.cursor = 0,
            KeyCode::Char('G') => d.cursor = n.saturating_sub(1),
            _ if listed
                && matches!(
                    k.code,
                    KeyCode::Char('y' | 'd' | 'r' | 'm' | 'e') | KeyCode::Enter
                ) =>
            {
                self.say(format!("{} is on the list already", d.cands[i].name));
            }
            KeyCode::Char('y') | KeyCode::Enter => decide(&mut d, Decision::Keep),
            KeyCode::Char('d') => decide(&mut d, Decision::Drop),
            KeyCode::Char('r') if d.cands[i].hint.is_some() => self.say(format!(
                "{} is on the list: the Tags view renames it; m merges it",
                d.cands[i].name
            )),
            KeyCode::Char('r') => d.prompt = Some((Prompt::Rename, Input::new(&d.cands[i].name))),
            KeyCode::Char('m') => d.prompt = Some((Prompt::Merge, Input::default())),
            KeyCode::Char('e') => {
                let text = d.cands[i].description.clone().unwrap_or_default();
                d.prompt = Some((Prompt::Describe, Input::new(&text)));
            }
            KeyCode::Char('a') if d.seed && !d.waiting => {
                d.prompt = Some((Prompt::Interests, Input::default()));
            }
            KeyCode::Char('w') => return self.write_seed(d),
            KeyCode::Esc if discard => {
                self.say("nothing written");
                return;
            }
            KeyCode::Esc => {
                d.discard = true;
                self.say("Esc again discards the decisions; w writes them");
            }
            _ => {}
        }
        self.bootstrap.mode = Mode::Deciding(d);
    }

    /// Applies a prompt's answer; an error keeps the prompt open.
    fn answer(&mut self, d: &mut Deciding, p: Prompt, text: &str) -> Result<(), String> {
        let i = d.cursor;
        match p {
            Prompt::Rename => {
                if text.is_empty() {
                    return Ok(());
                }
                let n = self.store.seed_name(text).map_err(|e| format!("{e:#}"))?;
                if n.listed() {
                    return Err(format!(
                        "{} is already on the list; m merges into it",
                        n.name()
                    ));
                }
                let new = n.name().to_string();
                if d.cands
                    .iter()
                    .enumerate()
                    .any(|(j, c)| j != i && c.name == new)
                {
                    return Err(format!("{new} is another candidate; m merges into it"));
                }
                // Candidates merged into it follow the new name.
                let old = std::mem::replace(&mut d.cands[i].name, new.clone());
                for c in d.cands.iter_mut() {
                    if c.decision == Decision::Merge(old.clone()) {
                        c.decision = Decision::Merge(new.clone());
                    }
                    if let Some(h) = &mut c.hint
                        && h.into == old
                    {
                        h.into.clone_from(&new);
                    }
                }
                decide(d, Decision::Keep);
            }
            Prompt::Merge => {
                let raw = match (text.is_empty(), &d.cands[i].hint) {
                    (false, _) => text.to_string(),
                    (true, Some(h)) => h.into.clone(),
                    (true, None) => return Ok(()),
                };
                let target = self.store.seed_name(&raw).map_err(|e| format!("{e:#}"))?;
                let name = target.name().to_string();
                if name == d.cands[i].name {
                    return Err(format!("can't merge {name} into itself"));
                }
                let candidate = d.cands.iter().find(|c| c.name == name && c.hint.is_none());
                match (candidate, &target) {
                    (Some(c), _) if c.decision == Decision::Drop => {
                        return Err(format!("{name} was dropped; keep it first"));
                    }
                    (Some(_), _) | (None, SeedName::Tag { .. }) => {
                        decide(d, Decision::Merge(name));
                    }
                    (None, SeedName::New(_)) => return Err(format!("no tag or candidate {name}")),
                }
            }
            Prompt::Describe => {
                if !text.is_empty() {
                    let desc = shlv::tag::check_description(text)?;
                    d.cands[i].description = desc;
                    d.cands[i].edited = true;
                }
            }
            Prompt::Interests => {
                let new: Vec<String> = interests_of(text)
                    .into_iter()
                    .filter(|x| !d.expanded.contains(x))
                    .collect();
                if new.is_empty() {
                    return Ok(());
                }
                // As `tag seed`'s `a`: sends the interests and naming rules only.
                d.waiting = true;
                self.send_job(Job::Seed {
                    interests: new,
                    per: PER,
                });
                self.say("expanding; sends the interests and naming rules only");
            }
        }
        Ok(())
    }

    fn write_seed(&mut self, d: Deciding) {
        match self.store.apply_seed(&d.cands, &[]) {
            Ok(s) => {
                let verb = if d.seed { "seeded" } else { "drafted" };
                let mut msg = seeded(verb, &s);
                if !s.skipped.is_empty() {
                    msg.push_str(&format!("; skipped: {}", s.skipped.join("; ")));
                }
                self.say(msg);
                self.tags_changed();
                self.interests_load();
            }
            Err(e) => {
                self.warn(format!("{e:#}"));
                self.bootstrap.mode = Mode::Deciding(d);
            }
        }
    }

    pub(super) fn bootstrap_status(&self) -> Vec<Span<'static>> {
        let s = match &self.bootstrap.mode {
            Mode::Deciding(d) => {
                let open = d
                    .cands
                    .iter()
                    .filter(|c| c.decision == Decision::Undecided)
                    .count();
                format!("{} candidates, {open} undecided", d.cands.len())
            }
            Mode::Waiting(w) => w.clone(),
            _ => crate::plural(
                self.bootstrap.interests.as_ref().map_or(0, Vec::len),
                "interest",
            ),
        };
        vec![Span::raw(s)]
    }

    pub(super) fn bootstrap_hint(&self) -> Option<&'static str> {
        Some(match &self.bootstrap.mode {
            Mode::Form(f) => f.hint(),
            Mode::Deciding(d) if d.prompt.is_some() => return None,
            Mode::Deciding(d) if d.seed => {
                "y keep · d drop · r rename · m merge · e describe · a add interests · w write · Esc discard"
            }
            Mode::Deciding(_) => {
                "y keep · d drop · r rename · m merge · e describe · w write · Esc discard"
            }
            Mode::Waiting(_) => return None,
            _ if cfg!(feature = "ai") => {
                "a add interests · dd forget · s seed tags from them · D draft tags from bookmarks"
            }
            _ => "a add interests · dd forget (seed and draft need the ai feature)",
        })
    }

    pub(super) fn draw_bootstrap_prompt(&self, f: &mut Frame, area: Rect) -> bool {
        let Mode::Deciding(d) = &self.bootstrap.mode else {
            return false;
        };
        let Some((p, input)) = &d.prompt else {
            return false;
        };
        let name = d.cands.get(d.cursor).map_or("", |c| c.name.as_str());
        let label = match p {
            Prompt::Rename => format!("rename {name} to: "),
            Prompt::Merge => match d.cands.get(d.cursor).and_then(|c| c.hint.as_ref()) {
                Some(h) => format!("merge {name} into [{}]: ", h.into),
                None => format!("merge {name} into: "),
            },
            Prompt::Describe => format!("description of {name}: "),
            Prompt::Interests => "add interests: ".to_string(),
        };
        draw_line_input(f, area, &label, input);
        true
    }

    pub(super) fn draw_bootstrap(&mut self, f: &mut Frame, area: Rect) {
        let mut l: Vec<Line> = Vec::new();
        let mut at = 0;
        match &self.bootstrap.mode {
            Mode::Deciding(d) => {
                at = candidate_lines(d, &mut l);
            }
            Mode::Waiting(w) => {
                l.push(Line::from(""));
                l.push(Line::from(format!("  {w}…")).dim());
            }
            _ => {
                l.push(Line::from(""));
                l.push(Line::from("  Interests").bold());
                l.push(
                    Line::from("  Broad topics you bookmark: s expands them into candidate tags.")
                        .dim(),
                );
                let list = self.bootstrap.interests.as_deref().unwrap_or_default();
                if list.is_empty() {
                    l.push(Line::from("  None yet: a adds some.").dim());
                }
                for (i, x) in list.iter().enumerate() {
                    let from = match (x.saved, x.pinned) {
                        (true, true) => "saved, pinned in config",
                        (false, true) => "pinned in config",
                        _ => "",
                    };
                    let mut line = Line::from(vec![
                        Span::raw(format!("    {}", x.name)),
                        Span::raw(format!("  {from}")).fg(Color::Magenta),
                    ]);
                    if i == self.bootstrap.cursor {
                        line = line.style(Style::new().add_modifier(Modifier::REVERSED));
                    }
                    l.push(line);
                }
            }
        }
        let h = area.height as usize;
        let scroll = (at + 2).saturating_sub(h);
        f.render_widget(Paragraph::new(l).scroll((scroll as u16, 0)), area);
        if let Mode::Form(form) = &self.bootstrap.mode {
            let title = match form.kind {
                FormKind::Interests => "Add interests (spaces or commas between)",
                FormKind::Draft => "Draft tags from a sample of bookmarks",
            };
            draw_form(f, area, title, form);
        }
    }
}

/// The candidates by group; returns the cursor's line.
fn candidate_lines(d: &Deciding, l: &mut Vec<Line<'static>>) -> usize {
    let mut at = 0;
    let what = if d.seed { "tag seed" } else { "tag draft" };
    l.push(
        Line::from(format!(
            "  Candidates from {what}: y keeps, d drops, w writes"
        ))
        .bold(),
    );
    if d.waiting {
        l.push(Line::from("  expanding more interests…").dim());
    }
    let width = d
        .cands
        .iter()
        .map(|c| c.name.chars().count())
        .max()
        .unwrap_or(0)
        .min(24);
    let mut group: Option<&str> = None;
    for (i, c) in d.cands.iter().enumerate() {
        let g = group_of(c);
        if group != Some(g) {
            group = Some(g);
            l.push(Line::from(""));
            l.push(Line::from(format!("  {g}")).bold());
        }
        let about = match &c.hint {
            Some(h) => match &h.reason {
                Some(r) => format!("→ {}: {r}", h.into),
                None => format!("→ {}", h.into),
            },
            None => c.description.clone().unwrap_or_default(),
        };
        let mark = match (&c.decision, &c.hint) {
            (Decision::Keep | Decision::Drop, Some(_)) => "left as is".to_string(),
            (Decision::Undecided, _) => String::new(),
            (Decision::Keep, None) => "keep".into(),
            (Decision::Drop, None) => "drop".into(),
            (Decision::Merge(into), _) => format!("merge → {into}"),
            (Decision::Listed, _) => "on the list".into(),
        };
        let mark_style = match c.decision {
            Decision::Keep | Decision::Merge(_) => Style::new().fg(Color::Green),
            Decision::Drop => Style::new().fg(Color::Red),
            _ => Style::new().add_modifier(Modifier::DIM),
        };
        let mut line = Line::from(vec![
            Span::styled(format!("    {:<12}", mark), mark_style),
            Span::styled(format!("{:<width$}", c.name), Style::new().fg(Color::Cyan)),
            Span::raw(format!("  {about}")).dim(),
        ]);
        if i == d.cursor {
            line = line.style(Style::new().add_modifier(Modifier::REVERSED));
            at = l.len();
        }
        l.push(line);
    }
    let n = |f: &dyn Fn(&Candidate) -> bool| d.cands.iter().filter(|c| f(c)).count();
    let keep = n(&|c| c.decision == Decision::Keep && c.hint.is_none());
    let merge = n(&|c| matches!(c.decision, Decision::Merge(_)));
    let drop = n(&|c| c.decision == Decision::Drop && c.hint.is_none());
    let open = n(&|c| c.decision == Decision::Undecided);
    l.push(Line::from(""));
    l.push(
        Line::from(format!(
            "  {keep} to keep, {merge} to merge, {drop} dropped, {open} undecided (left out)"
        ))
        .dim(),
    );
    at
}

/// Sets the current candidate's decision and moves to the next undecided one.
fn decide(d: &mut Deciding, decision: Decision) {
    d.cands[d.cursor].decision = decision;
    if let Some(n) = next_open(&d.cands, d.cursor + 1) {
        d.cursor = n;
    }
}

/// The first undecided candidate from `i` on.
fn next_open(cands: &[Candidate], i: usize) -> Option<usize> {
    (i..cands.len()).find(|&j| cands[j].decision == Decision::Undecided)
}

/// The group a candidate is shown under: its interest, `redundant` for a listed tag that looks
/// redundant, `other` for none.
fn group_of(c: &Candidate) -> &str {
    match (&c.hint, c.interest.as_str()) {
        (Some(_), _) => "redundant",
        (None, "") => "other",
        (None, i) => i,
    }
}

/// Interests as typed: split on whitespace and commas, normalized, deduplicated in order.
fn interests_of(text: &str) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for w in text.split([',', ' ', '\t']) {
        let i = shlv::seed::normalize_interest(w);
        if !i.is_empty() && !v.contains(&i) {
            v.push(i);
        }
    }
    v
}

/// `seeded: 3 new tags on the list (a b c), 1 alias (x → y)`.
fn seeded(verb: &str, s: &Seeded) -> String {
    let mut m = format!(
        "{verb}: {} on the list",
        crate::plural(s.kept.len(), "new tag")
    );
    if !s.kept.is_empty() {
        m.push_str(&format!(" ({})", s.kept.join(" ")));
    }
    if !s.merged.is_empty() {
        let list: Vec<String> = s.merged.iter().map(|(a, b)| format!("{a} → {b}")).collect();
        m.push_str(&format!(
            ", {} ({})",
            crate::plural(s.merged.len(), "alias"),
            list.join(", ")
        ));
    }
    if s.described > 0 {
        m.push_str(&format!(", {}", crate::plural(s.described, "description")));
    }
    m
}

fn join_notes(msg: String, notes: &str) -> String {
    if notes.is_empty() {
        msg
    } else {
        format!("{msg}; {notes}")
    }
}

#[cfg_attr(not(feature = "ai"), allow(dead_code))]
fn lines(buf: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(buf)
        .lines()
        .map(String::from)
        .collect()
}

impl App {
    /// Interests changed (a `:` command): the list is read again.
    pub(super) fn interests_changed(&mut self) {
        self.bootstrap.interests = None;
        if self.view == super::app::View::Bootstrap {
            self.interests_load();
        }
    }
}
