//! View 2, Review (task 044): `shlv review`'s batches, one key per decision, and the queue of
//! pending suggestions. [`shlv::review`] holds the logic; this is the screen.
//!
//! A batch goes through phases: the worker picks it and runs the suggestion layers on it, then
//! each bookmark is decided in turn, then the names it proposed (add, alias, reject), then
//! back-fill for tags just added to the list, then a retrain on the worker; the batch's numbers go
//! to the history.

use std::collections::HashSet;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use shlv::review::{BatchRecord, BatchStats, Decision, ReviewFilter};
use shlv::store::normalize_tag;
use shlv::{Bookmark, Proposal, ProposalState, RuleCandidate, Source, Suggestion};

use super::app::{App, draw_line_input};
use super::form::{Field, Form, FormKey, draw_form};
use super::input::Input;
use super::pick::{Picked, Picker};
use super::worker::{Job, Output};

pub fn keys() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Tab", "batches, or the queue of pending suggestions"),
        ("", "Batches"),
        ("Enter", "start a batch"),
        (
            "f",
            "filters: a suggested tag, its source, confidence, batch size",
        ),
        ("", "In a batch"),
        (
            "Enter, y",
            "save: checked tags; unchecked suggestions are rejected",
        ),
        (
            "1-9, Space",
            "check or uncheck a tag (Space: the one at j/k)",
        ),
        ("t, +", "add a tag (Tab completes); a new name is proposed"),
        ("s, n", "skip it for now"),
        ("o", "open it"),
        ("dd", "delete it (u in Bookmarks undoes)"),
        ("R", "create the rule offered for its site"),
        (
            "q, Esc",
            "end the batch here; its names, back-fill, retrain follow",
        ),
        ("", "Names proposed, then back-fill"),
        ("a l r s", "add to the list, alias of a tag, reject, skip"),
        ("y n s", "back-fill: it has the tag, it doesn't, skip"),
        ("q, Esc", "done with this step"),
        ("", "Queue"),
        ("y n", "accept, reject the suggestion"),
        ("Y N", "accept, reject all of the bookmark's"),
        ("f", "source: all, rule, model, llm"),
        ("< >", "only below a confidence: lower, raise"),
        ("o, Ctrl-l", "open, reload"),
    ]
}

/// Batch size, as `shlv review`'s default.
const SIZE: usize = 30;
/// Suggestions the queue shows at most.
const QUEUE_MAX: usize = 5000;

pub struct Review {
    queue_tab: bool,
    phase: Phase,
    filter: ReviewFilter,
    size: usize,
    /// Finished batches; loaded when first shown.
    history: Option<Vec<BatchRecord>>,
    /// The filter form, over any phase.
    form: Option<Form<()>>,
    queue: Queue,
}

impl Default for Review {
    fn default() -> Self {
        Review {
            queue_tab: false,
            phase: Phase::Idle,
            filter: ReviewFilter::default(),
            size: SIZE,
            history: None,
            form: None,
            queue: Queue::default(),
        }
    }
}

#[derive(Default)]
enum Phase {
    #[default]
    Idle,
    /// The worker is picking a batch.
    Picking,
    Deciding(Batch),
    Proposals(Batch, Proposals),
    Backfill(Batch, Backfill),
    /// The worker is retraining the classifier.
    #[cfg_attr(not(feature = "model"), allow(dead_code))]
    Training(Batch),
}

/// A batch under way.
struct Batch {
    id: i64,
    started: i64,
    order: String,
    ids: Vec<i64>,
    /// The bookmark being decided.
    at: usize,
    current: Option<Current>,
    stats: BatchStats,
    /// Bookmarks reviewed in it.
    done: Vec<i64>,
    /// One line per bookmark decided, the last one last.
    log: Vec<Line<'static>>,
    /// Tags added to the list after it.
    added: Vec<String>,
    /// A rule worth creating for the last bookmark's site.
    offer: Option<RuleCandidate>,
    /// What the steps after the bookmarks did (back-fill, retrain), for the last message.
    notes: Vec<String>,
}

/// The bookmark being decided and its rows.
struct Current {
    bookmark: Bookmark,
    rows: Vec<Row>,
    /// Names proposed for it before (decided after the batch).
    proposed: Vec<String>,
    cursor: usize,
    /// Typing a tag to add.
    adding: Option<Input>,
    /// The first Enter on a bookmark without tags.
    confirm_empty: bool,
    /// The first `d` of `dd`.
    delete: bool,
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

struct Proposals {
    list: Vec<Proposal>,
    /// What was decided for each, shown after it.
    said: Vec<Option<String>>,
    cursor: usize,
    /// Typing the tag a name becomes an alias of.
    aliasing: Option<Input>,
}

struct Backfill {
    /// Tags still to back-fill after the current one.
    tags: Vec<String>,
    tag: String,
    scope: Vec<i64>,
    /// None while the worker offers the tag.
    picker: Option<Picker>,
}

#[derive(Default)]
struct Queue {
    loaded: bool,
    list: Vec<Suggestion>,
    /// More were pending than shown.
    more: bool,
    source: Option<Source>,
    below: Option<f64>,
    cursor: usize,
    offset: usize,
}

impl App {
    pub(super) fn review_enter(&mut self) {
        if self.review.history.is_none() {
            self.review_history();
        }
        if self.review.queue_tab && !self.review.queue.loaded {
            self.queue_load();
        }
    }

    fn review_history(&mut self) {
        match self.store.review_history() {
            Ok(h) => self.review.history = Some(h),
            Err(e) => self.warn(format!("{e:#}")),
        }
    }

    pub(super) fn review_paste(&mut self, text: &str) {
        if let Some(form) = &mut self.review.form {
            form.fields[form.focus].input.insert(text.trim());
            return;
        }
        match &mut self.review.phase {
            Phase::Deciding(b) => {
                if let Some(Current {
                    adding: Some(i), ..
                }) = &mut b.current
                {
                    i.insert(text.trim());
                }
            }
            Phase::Proposals(_, p) => {
                if let Some(i) = &mut p.aliasing {
                    i.insert(text.trim());
                }
            }
            _ => {}
        }
    }

    pub(super) fn review_key(&mut self, k: KeyEvent) -> bool {
        if let Some(mut form) = self.review.form.take() {
            match self.form_input(&mut form, k) {
                FormKey::Cancel => {}
                FormKey::Submit => {
                    if let Err(e) = self.set_filter(&form) {
                        self.warn(format!("{e:#}"));
                        self.review.form = Some(form);
                    }
                }
                FormKey::Edit => self.review.form = Some(form),
            }
            return true;
        }
        if k.code == KeyCode::Tab && !self.review_typing() {
            self.review.queue_tab = !self.review.queue_tab;
            self.review_enter();
            return true;
        }
        if self.review.queue_tab {
            return self.queue_key(k);
        }
        match std::mem::take(&mut self.review.phase) {
            Phase::Idle => {
                return self.idle_key(k);
            }
            Phase::Picking => {
                self.review.phase = Phase::Picking;
                return false;
            }
            Phase::Deciding(b) => self.deciding_key(k, b),
            Phase::Proposals(b, p) => self.proposals_key(k, b, p),
            Phase::Backfill(b, f) => self.backfill_key(k, b, f),
            Phase::Training(b) => {
                self.review.phase = Phase::Training(b);
                return false;
            }
        }
        true
    }

    /// Whether a prompt in the view has the keyboard (so Tab completes rather than switches).
    fn review_typing(&self) -> bool {
        match &self.review.phase {
            Phase::Deciding(b) => b.current.as_ref().is_some_and(|c| c.adding.is_some()),
            Phase::Proposals(_, p) => p.aliasing.is_some(),
            _ => false,
        }
    }

    fn idle_key(&mut self, k: KeyEvent) -> bool {
        if k.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        match k.code {
            KeyCode::Enter | KeyCode::Char(' ') => self.start_batch(),
            KeyCode::Char('f') => self.open_filter(),
            _ => return false,
        }
        true
    }

    fn open_filter(&mut self) {
        let f = &self.review.filter;
        let source = f.source.map_or("", Source::as_str);
        let below = f.below.map(|b| b.to_string()).unwrap_or_default();
        self.review.form = Some(Form::new(
            (),
            vec![
                Field::tags("Suggested tag", f.tag.as_deref().unwrap_or("")),
                Field::line("Source", source),
                Field::line("Below", &below),
                Field::line("Batch size", &self.review.size.to_string()),
            ],
        ));
    }

    fn set_filter(&mut self, form: &Form<()>) -> Result<()> {
        let tag = Some(normalize_tag(&form.text(0))).filter(|t| !t.is_empty());
        let source = match form.text(1).trim() {
            "" | "all" => None,
            "rule" => Some(Source::Rule),
            "model" => Some(Source::Model),
            "llm" => Some(Source::Llm),
            s => anyhow::bail!("source: rule, model, or llm (empty for any), not {s:?}"),
        };
        let below = match form.text(2).trim() {
            "" => None,
            s => match s.parse::<f64>() {
                Ok(p) if (0.0..=1.0).contains(&p) => Some(p),
                _ => anyhow::bail!("below: a confidence between 0 and 1, not {s:?}"),
            },
        };
        let size = match form.text(3).trim().parse::<usize>() {
            Ok(n) if n > 0 => n,
            _ => anyhow::bail!("batch size: a number above 0"),
        };
        if let Some(t) = &tag {
            // Fail now rather than when the batch starts.
            let name = self.store.styled(t)?;
            if self.store.tag_info(&name)?.is_none() {
                anyhow::bail!("no tag {name:?}");
            }
        }
        self.review.filter = ReviewFilter { tag, source, below };
        self.review.size = size;
        self.say(if self.review.filter.is_empty() {
            "no filters: batches of bookmarks not reviewed yet".to_string()
        } else {
            "filtered: batches of bookmarks with matching suggestions".to_string()
        });
        Ok(())
    }

    fn start_batch(&mut self) {
        self.review.phase = Phase::Picking;
        self.send_job(Job::ReviewBatch {
            filter: self.review.filter.clone(),
            size: self.review.size,
        });
    }

    /// A review job finished on the worker.
    pub(super) fn review_report(&mut self, job: Job, result: Result<(String, Output), String>) {
        let phase = std::mem::take(&mut self.review.phase);
        match (job, phase, result) {
            (Job::ReviewBatch { .. }, Phase::Picking, Ok((order, out))) => {
                let Output::Ids(ids) = out else { return };
                if ids.is_empty() {
                    self.say(if self.review.filter.is_empty() {
                        "every bookmark is reviewed"
                    } else {
                        "no bookmarks match the filters"
                    });
                    return;
                }
                if let Err(e) = self.begin(order, ids) {
                    self.warn(format!("{e:#}"));
                }
            }
            (Job::Backfill { tag, .. }, Phase::Backfill(mut b, mut f), result) if f.tag == tag => {
                if let Err(e) = result {
                    b.notes.push(format!("back-filling #{tag} failed: {e}"));
                }
                match self.picker(&f.tag, Some(&f.scope)) {
                    Ok(p) => {
                        if p.is_empty() {
                            b.notes.push(format!("no back-fill candidates for #{tag}"));
                            return self.next_backfill(b, f.tags);
                        }
                        f.picker = Some(p);
                        self.review.phase = Phase::Backfill(b, f);
                    }
                    Err(e) => {
                        self.warn(format!("{e:#}"));
                        self.next_backfill(b, f.tags);
                    }
                }
            }
            (Job::Train, Phase::Training(mut b), result) => {
                b.notes.push(match result {
                    Ok((text, _)) => text,
                    Err(e) => format!("retraining failed: {e}"),
                });
                self.finish(b);
            }
            (job, phase, result) => {
                self.review.phase = phase;
                if let Err(e) = result {
                    self.warn(format!("{} failed: {e}", job.describe()));
                }
            }
        }
        self.bookmarks_changed();
    }

    fn begin(&mut self, order: String, ids: Vec<i64>) -> Result<()> {
        let (id, started) = self.store.start_review_batch()?;
        let mut b = Batch {
            id,
            started,
            order,
            ids,
            at: 0,
            current: None,
            stats: BatchStats::default(),
            done: Vec::new(),
            log: Vec::new(),
            added: Vec::new(),
            offer: None,
            notes: Vec::new(),
        };
        self.tags_changed();
        self.load_current(&mut b)?;
        self.place(b);
        Ok(())
    }

    /// Shows the batch's next bookmark, or ends the batch after the last.
    fn place(&mut self, b: Batch) {
        if b.current.is_some() {
            self.review.phase = Phase::Deciding(b);
        } else {
            self.end_batch(b);
        }
    }

    /// Loads the bookmark at `b.at`, skipping deleted ones.
    fn load_current(&mut self, b: &mut Batch) -> Result<()> {
        b.current = None;
        while let Some(&id) = b.ids.get(b.at) {
            if let Some(bookmark) = self.store.get(id)? {
                let mut rows: Vec<Row> = bookmark
                    .tags
                    .iter()
                    .map(|t| Row {
                        tag: t.clone(),
                        kind: Kind::Current,
                        on: true,
                    })
                    .collect();
                for s in self.store.pending(Some(id))? {
                    rows.push(Row {
                        tag: s.tag,
                        kind: Kind::Suggested(s.source, s.confidence),
                        on: true,
                    });
                }
                b.current = Some(Current {
                    proposed: self.store.proposals_for(id)?,
                    bookmark,
                    rows,
                    cursor: 0,
                    adding: None,
                    confirm_empty: false,
                    delete: false,
                });
                return Ok(());
            }
            b.at += 1;
        }
        Ok(())
    }

    fn deciding_key(&mut self, k: KeyEvent, mut b: Batch) {
        let Some(mut c) = b.current.take() else {
            return self.end_batch(b);
        };
        if let Some(mut input) = c.adding.take() {
            match k.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    let text = input.text().trim().to_string();
                    if !text.is_empty() {
                        match self.typed_tag(&text) {
                            Ok((tag, kind)) => add_row(&mut c, tag, kind),
                            Err(e) => {
                                self.warn(format!("{e:#}"));
                                c.adding = Some(input);
                            }
                        }
                    }
                }
                KeyCode::Tab => {
                    let names = self.review_names();
                    if !input.complete(&names) {
                        self.say("no tag starts that way");
                    }
                    c.adding = Some(input);
                }
                _ => {
                    input.key(k);
                    c.adding = Some(input);
                }
            }
            b.current = Some(c);
            self.review.phase = Phase::Deciding(b);
            return;
        }
        let delete = std::mem::take(&mut c.delete);
        let confirm = std::mem::take(&mut c.confirm_empty);
        match k.code {
            KeyCode::Enter | KeyCode::Char('y') => {
                let empty = !c.rows.iter().any(|r| r.on) && c.proposed.is_empty();
                if empty && !confirm {
                    c.confirm_empty = true;
                    self.say("no tags: Enter again saves it without any, t adds one");
                } else {
                    return self.save(b, c);
                }
            }
            KeyCode::Char(d @ '1'..='9') => {
                let i = d as usize - '1' as usize;
                match c.rows.get_mut(i) {
                    Some(r) => {
                        r.on = !r.on;
                        c.cursor = i;
                    }
                    None => self.say(format!("no tag {d}")),
                }
            }
            KeyCode::Char(' ') => {
                if let Some(r) = c.rows.get_mut(c.cursor) {
                    r.on = !r.on;
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                c.cursor = (c.cursor + 1).min(c.rows.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => c.cursor = c.cursor.saturating_sub(1),
            KeyCode::Char('t' | '+') => c.adding = Some(Input::default()),
            KeyCode::Char('s' | 'n') => {
                b.stats.skipped += 1;
                b.log
                    .push(log_line(&c.bookmark, vec![Span::raw("skipped").dim()]));
                return self.advance(b);
            }
            KeyCode::Char('o') => self.request_open(c.bookmark.url.clone()),
            KeyCode::Char('d') if delete => {
                let id = c.bookmark.id;
                match self.store.delete_keeping(id) {
                    Ok(Some(d)) => {
                        self.remember_delete(d);
                        b.log
                            .push(log_line(&c.bookmark, vec![Span::raw("deleted").red()]));
                        self.say(format!("deleted {id} (u in Bookmarks undoes)"));
                        return self.advance(b);
                    }
                    Ok(None) => {}
                    Err(e) => self.warn(format!("{e:#}")),
                }
            }
            KeyCode::Char('d') => {
                c.delete = true;
                self.say("d again deletes this bookmark");
            }
            KeyCode::Char('R') => match b.offer.take() {
                Some(o) => {
                    if let Err(e) = self.rules_add(&o.host, &o.tag) {
                        self.warn(format!("{e:#}"));
                    }
                    // Its suggestion may be pending on this bookmark now.
                    let id = c.bookmark.id;
                    if let Ok(p) = self.store.pending(Some(id)) {
                        for s in p {
                            if !c.rows.iter().any(|r| r.tag == s.tag) {
                                c.rows.push(Row {
                                    tag: s.tag,
                                    kind: Kind::Suggested(s.source, s.confidence),
                                    on: true,
                                });
                            }
                        }
                    }
                }
                None => self.say("no rule to create"),
            },
            KeyCode::Char('q') | KeyCode::Esc => {
                b.log
                    .push(log_line(&c.bookmark, vec![Span::raw("stopped").dim()]));
                return self.end_batch(b);
            }
            _ => {}
        }
        b.current = Some(c);
        self.review.phase = Phase::Deciding(b);
    }

    /// A tag typed in the batch: a canonical tag (by name or alias), or a name to propose.
    fn typed_tag(&self, text: &str) -> Result<(String, Kind)> {
        let looked = normalize_tag(text.trim_start_matches('#'));
        let name = self.store.styled(&looked)?;
        for n in [&looked, &name] {
            if let Some(t) = self.store.tag_info(n)?.filter(|t| t.canonical) {
                return Ok((t.name, Kind::Added));
            }
        }
        Ok((name, Kind::New))
    }

    /// Completion in a batch: canonical tags and their aliases, then pending proposals.
    fn review_names(&self) -> Vec<String> {
        let mut v = Vec::new();
        if let Ok(tags) = self.store.canonical_tags() {
            for t in tags {
                v.extend(t.aliases.iter().cloned());
                v.push(t.name);
            }
        }
        if let Ok(p) = self.store.proposals(ProposalState::Pending) {
            v.extend(p.into_iter().map(|p| p.name));
        }
        v
    }

    fn save(&mut self, mut b: Batch, c: Current) {
        let mut d = Decision::default();
        let (mut shown, mut accepted) = (0, 0);
        for r in &c.rows {
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
        let id = c.bookmark.id;
        let done = match self.store.commit_review(b.id, id, &d) {
            Ok(done) => done,
            Err(e) => {
                self.warn(format!("{e:#}"));
                b.current = Some(c);
                self.review.phase = Phase::Deciding(b);
                return;
            }
        };
        b.stats.reviewed += 1;
        b.stats.model_shown += shown;
        b.stats.model_accepted += accepted;
        b.done.push(id);
        let tag = Style::new().fg(Color::Cyan);
        let mut spans = Vec::new();
        let on: Vec<&Row> = c
            .rows
            .iter()
            .filter(|r| r.on && r.kind != Kind::New)
            .collect();
        if on.is_empty() && done.diverted.proposed.is_empty() {
            spans.push(Span::raw("no tags").dim());
        }
        for r in on {
            spans.push(Span::styled(format!("#{} ", r.tag), tag));
        }
        for p in &done.diverted.proposed {
            spans.push(Span::raw(format!("+#{p} proposed ")).dim());
        }
        for p in &done.diverted.rejected {
            spans.push(Span::raw(format!("#{p} was rejected before ")).dim());
        }
        let off: Vec<String> = c
            .rows
            .iter()
            .filter(|r| !r.on && matches!(r.kind, Kind::Suggested(..) | Kind::Current))
            .map(|r| format!("#{}", r.tag))
            .collect();
        if !off.is_empty() {
            spans.push(Span::raw(format!("(not {})", off.join(" "))).dim());
        }
        b.log.push(log_line(&c.bookmark, spans));
        self.tags_changed();
        // A site tagged alike often enough: offer a rule.
        b.offer = match self.store.rule_candidates_at(&c.bookmark.url) {
            Ok(v) => v.into_iter().next(),
            Err(e) => {
                self.warn(format!("{e:#}"));
                None
            }
        };
        self.say(match &b.offer {
            Some(o) => format!(
                "saved {id}; {} of {} tagged on {} have #{}: R makes it a rule",
                o.count, o.of, o.host, o.tag
            ),
            None => format!("saved {id}"),
        });
        self.advance(b);
    }

    fn advance(&mut self, mut b: Batch) {
        b.at += 1;
        if let Err(e) = self.load_current(&mut b) {
            self.warn(format!("{e:#}"));
        }
        self.place(b);
    }

    /// The batch's bookmarks are decided (or `q`): on to the names it proposed.
    fn end_batch(&mut self, mut b: Batch) {
        b.current = None;
        b.offer = None;
        // Count the names proposed now: deciding them takes them out of the pending ones.
        match self.store.proposals_since(b.started) {
            Ok(n) => b.stats.proposed = n,
            Err(e) => self.warn(format!("{e:#}")),
        }
        let done: HashSet<i64> = b.done.iter().copied().collect();
        let list: Vec<Proposal> = match self.store.proposals(ProposalState::Pending) {
            Ok(l) => l
                .into_iter()
                .filter(|p| p.bookmarks.iter().any(|id| done.contains(id)))
                .collect(),
            Err(e) => {
                self.warn(format!("{e:#}"));
                Vec::new()
            }
        };
        if list.is_empty() {
            return self.start_backfill(b);
        }
        self.review.phase = Phase::Proposals(
            b,
            Proposals {
                said: vec![None; list.len()],
                list,
                cursor: 0,
                aliasing: None,
            },
        );
    }

    fn proposals_key(&mut self, k: KeyEvent, mut b: Batch, mut p: Proposals) {
        if let Some(mut input) = p.aliasing.take() {
            match k.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    let name = p.list[p.cursor].name.clone();
                    match self.store.alias_proposal(&name, input.text().trim()) {
                        Ok(d) => {
                            p.said[p.cursor] = Some(format!(
                                "alias of #{}, tagged {}",
                                d.tag,
                                crate::plural(d.tagged, "bookmark")
                            ));
                            self.tags_changed();
                            next_open(&mut p);
                        }
                        Err(e) => {
                            self.warn(format!("{e:#}"));
                            p.aliasing = Some(input);
                        }
                    }
                }
                KeyCode::Tab => {
                    let names: Vec<String> = match self.store.canonical_tags() {
                        Ok(t) => t
                            .into_iter()
                            .flat_map(|t| std::iter::once(t.name).chain(t.aliases))
                            .collect(),
                        Err(_) => Vec::new(),
                    };
                    if !input.complete(&names) {
                        self.say("no canonical tag starts that way");
                    }
                    p.aliasing = Some(input);
                }
                _ => {
                    input.key(k);
                    p.aliasing = Some(input);
                }
            }
            self.review.phase = Phase::Proposals(b, p);
            return;
        }
        let name = p.list[p.cursor].name.clone();
        match k.code {
            KeyCode::Char('j') | KeyCode::Down => {
                p.cursor = (p.cursor + 1).min(p.list.len() - 1);
            }
            KeyCode::Char('k') | KeyCode::Up => p.cursor = p.cursor.saturating_sub(1),
            KeyCode::Char('a') => match self.store.accept_proposal(&name) {
                Ok(d) => {
                    p.said[p.cursor] = Some(format!(
                        "added, tagged {}",
                        crate::plural(d.tagged, "bookmark")
                    ));
                    b.added.push(d.tag);
                    self.tags_changed();
                    next_open(&mut p);
                }
                Err(e) => self.warn(format!("{e:#}")),
            },
            KeyCode::Char('l') => p.aliasing = Some(Input::default()),
            KeyCode::Char('r') => match self.store.reject_proposal(&name) {
                Ok(_) => {
                    p.said[p.cursor] = Some("rejected".into());
                    next_open(&mut p);
                }
                Err(e) => self.warn(format!("{e:#}")),
            },
            KeyCode::Char('s') => {
                p.said[p.cursor] = Some("skipped".into());
                next_open(&mut p);
            }
            KeyCode::Char('q') | KeyCode::Esc => return self.start_backfill(b),
            _ => {}
        }
        if p.said.iter().all(Option::is_some) && !p.said.is_empty() {
            return self.start_backfill(b);
        }
        self.review.phase = Phase::Proposals(b, p);
    }

    /// Back-fill for the tags the batch added: they may belong on bookmarks reviewed before
    /// they existed.
    fn start_backfill(&mut self, mut b: Batch) {
        b.stats.tags_added = b.added.len();
        let tags = b.added.clone();
        if tags.is_empty() {
            return self.train(b);
        }
        if !self.can_backfill() {
            b.notes.push(format!(
                "back-fill needs the classifier (feature `model`) or an [ai] tag provider; \
                 check reviewed bookmarks for {} by hand",
                hashed(&tags)
            ));
            return self.train(b);
        }
        self.next_backfill(b, tags);
    }

    fn can_backfill(&self) -> bool {
        #[cfg(feature = "ai")]
        if matches!(crate::ai_cmd::backfill_provider(&self.config), Ok(Some(_))) {
            return true;
        }
        cfg!(feature = "model")
    }

    fn next_backfill(&mut self, b: Batch, mut tags: Vec<String>) {
        if tags.is_empty() {
            return self.train(b);
        }
        let tag = tags.remove(0);
        let scope = match self.store.reviewed_ids() {
            Ok(s) => s,
            Err(e) => {
                self.warn(format!("{e:#}"));
                return self.train(b);
            }
        };
        self.send_job(Job::Backfill {
            tag: tag.clone(),
            scope: Some(scope.clone()),
        });
        self.review.phase = Phase::Backfill(
            b,
            Backfill {
                tags,
                tag,
                scope,
                picker: None,
            },
        );
    }

    fn backfill_key(&mut self, k: KeyEvent, mut b: Batch, mut f: Backfill) {
        let Some(mut p) = f.picker.take() else {
            // Still offering on the worker.
            self.review.phase = Phase::Backfill(b, f);
            return;
        };
        if self.pick_key(&mut p, k) == Picked::Done {
            b.notes.push(p.summary());
            self.tags_changed();
            return self.next_backfill(b, f.tags);
        }
        f.picker = Some(p);
        self.review.phase = Phase::Backfill(b, f);
    }

    /// Retrains the classifier on the batch's decisions, then finishes the batch.
    fn train(&mut self, b: Batch) {
        #[cfg(feature = "model")]
        if b.stats.reviewed > 0
            && matches!(shlv::model::canonical_list(&self.store), Ok(l) if !l.is_empty())
        {
            self.send_job(Job::Train);
            self.review.phase = Phase::Training(b);
            return;
        }
        self.finish(b);
    }

    fn finish(&mut self, b: Batch) {
        if let Err(e) = self.store.finish_review_batch(b.id, &b.stats) {
            self.warn(format!("{e:#}"));
        }
        self.review_history();
        let s = &b.stats;
        let mut msg = format!(
            "batch done: reviewed {}",
            crate::plural(s.reviewed, "bookmark")
        );
        if s.skipped > 0 {
            msg.push_str(&format!(", {} skipped", s.skipped));
        }
        for n in &b.notes {
            msg.push_str(&format!("; {n}"));
        }
        msg.push_str("; Enter starts the next");
        self.say(msg);
        self.review.phase = Phase::Idle;
    }

    // The queue of pending suggestions.

    fn queue_load(&mut self) {
        let q = &mut self.review.queue;
        let (source, below) = (q.source, q.below);
        let mut list = Vec::new();
        let mut more = false;
        let r = self.store.pending_each(None, |s| {
            if source.is_some_and(|x| x != s.source)
                || below.is_some_and(|b| s.confidence.unwrap_or(0.5) >= b)
            {
                return Ok(());
            }
            if list.len() < QUEUE_MAX {
                list.push(s);
            } else {
                more = true;
            }
            Ok(())
        });
        if let Err(e) = r {
            self.warn(format!("{e:#}"));
        }
        let q = &mut self.review.queue;
        q.list = list;
        q.more = more;
        q.loaded = true;
        q.cursor = q.cursor.min(q.list.len().saturating_sub(1));
    }

    /// Suggestions changed elsewhere: the queue reads them again when shown.
    pub(super) fn queue_changed(&mut self) {
        self.review.queue.loaded = false;
    }

    fn queue_key(&mut self, k: KeyEvent) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && k.code == KeyCode::Char('l') {
            self.queue_load();
            self.say("reloaded");
            return true;
        }
        if ctrl {
            return false;
        }
        let q = &mut self.review.queue;
        let n = q.list.len();
        let current = q.list.get(q.cursor).cloned();
        match k.code {
            KeyCode::Char('j') | KeyCode::Down => {
                q.cursor = (q.cursor + 1).min(n.saturating_sub(1))
            }
            KeyCode::Char('k') | KeyCode::Up => q.cursor = q.cursor.saturating_sub(1),
            KeyCode::Char('g') | KeyCode::Home => q.cursor = 0,
            KeyCode::Char('G') | KeyCode::End => q.cursor = n.saturating_sub(1),
            KeyCode::Char('f') => {
                q.source = match q.source {
                    None => Some(Source::Rule),
                    Some(Source::Rule) => Some(Source::Model),
                    Some(Source::Model) => Some(Source::Llm),
                    Some(Source::Llm) => None,
                };
                q.cursor = 0;
                self.queue_load();
            }
            KeyCode::Char(c @ ('<' | '>')) => {
                let step = if c == '<' { -0.1 } else { 0.1 };
                let b = q.below.unwrap_or(1.0) + step;
                q.below = (b < 0.999).then(|| (b.max(0.1) * 10.0).round() / 10.0);
                q.cursor = 0;
                self.queue_load();
            }
            KeyCode::Char(c @ ('y' | 'n' | 'Y' | 'N')) => {
                let Some(s) = current else { return true };
                let one = c.is_lowercase();
                let tags = if one { vec![s.tag.clone()] } else { Vec::new() };
                let r = if c.eq_ignore_ascii_case(&'y') {
                    self.store.accept(s.id, &tags)
                } else {
                    self.store.reject(s.id, &tags)
                };
                match r {
                    Ok(done) => {
                        let verb = if c.eq_ignore_ascii_case(&'y') {
                            "accepted"
                        } else {
                            "rejected"
                        };
                        self.say(format!("{}: {verb} {}", s.id, hashed(&done)));
                        let q = &mut self.review.queue;
                        q.list
                            .retain(|x| !(x.id == s.id && (!one || x.tag == s.tag)));
                        q.cursor = q.cursor.min(q.list.len().saturating_sub(1));
                        self.bookmarks.forget(&[s.id]);
                        self.tags_changed();
                    }
                    Err(e) => self.warn(format!("{e:#}")),
                }
            }
            KeyCode::Char('o') | KeyCode::Enter => {
                if let Some(s) = current {
                    self.request_open(s.url);
                }
            }
            _ => return false,
        }
        true
    }

    pub(super) fn review_status(&self) -> Vec<Span<'static>> {
        let r = &self.review;
        if r.queue_tab {
            let q = &r.queue;
            let mut s = format!(
                "queue: {}{} pending",
                q.list.len(),
                if q.more { "+" } else { "" }
            );
            if let Some(src) = q.source {
                s.push_str(&format!(", {}", src.as_str()));
            }
            if let Some(b) = q.below {
                s.push_str(&format!(", below {b:.1}"));
            }
            return vec![Span::raw(s)];
        }
        let s = match &r.phase {
            Phase::Idle => match self.review.history.as_ref().map(Vec::len) {
                Some(n) if n > 0 => format!("{n} batches so far"),
                _ => String::new(),
            },
            Phase::Picking => "picking a batch".into(),
            Phase::Deciding(b) => format!("{}/{}", b.at + 1, b.ids.len()),
            Phase::Proposals(..) => "names proposed".into(),
            Phase::Backfill(_, f) => format!("back-fill #{}", f.tag),
            Phase::Training(_) => "retraining".into(),
        };
        let mut v = vec![Span::raw(s)];
        if !r.filter.is_empty() {
            v.push(Span::raw(format!("  {}", filter_text(&r.filter))));
        }
        v
    }

    pub(super) fn review_hint(&self) -> Option<&'static str> {
        if let Some(form) = &self.review.form {
            return Some(form.hint());
        }
        if self.review.queue_tab {
            return Some(
                "y n accept, reject · Y N all of the bookmark's · f source · < > confidence · Tab batches",
            );
        }
        Some(match &self.review.phase {
            Phase::Idle => "Enter starts a batch · f filters · Tab the suggestions queue",
            Phase::Picking | Phase::Training(_) => return None,
            Phase::Deciding(b) => match &b.current {
                Some(c) if c.adding.is_some() => "",
                _ => {
                    "Enter save · 1-9 toggle · t add · s skip · o open · dd delete · q end batch · ? help"
                }
            },
            Phase::Proposals(_, p) if p.aliasing.is_some() => "",
            Phase::Proposals(..) => "a add to the list · l alias · r reject · s skip · q done",
            Phase::Backfill(_, f) if f.picker.is_none() => return None,
            Phase::Backfill(..) => "y it has the tag · n it doesn't · s skip · o open · q done",
        })
    }

    /// The bottom line while typing a tag in the view; returns false if not.
    pub(super) fn draw_review_prompt(&self, f: &mut Frame, area: Rect) -> bool {
        if self.review.queue_tab {
            return false;
        }
        match &self.review.phase {
            Phase::Deciding(b) => {
                if let Some(Current {
                    adding: Some(i), ..
                }) = &b.current
                {
                    draw_line_input(f, area, "add tag: #", i);
                    return true;
                }
            }
            Phase::Proposals(_, p) => {
                if let Some(i) = &p.aliasing {
                    let name = &p.list[p.cursor].name;
                    draw_line_input(f, area, &format!("#{name} is an alias of: #"), i);
                    return true;
                }
            }
            _ => {}
        }
        false
    }

    pub(super) fn draw_review(&mut self, f: &mut Frame, area: Rect) {
        if self.review.queue_tab {
            self.draw_queue(f, area);
        } else {
            let lines = match &self.review.phase {
                Phase::Idle => self.idle_lines(),
                Phase::Picking => vec![
                    Line::from(""),
                    Line::from("  Picking a batch and suggesting tags for it…").dim(),
                ],
                Phase::Deciding(b) => deciding_lines(b),
                Phase::Proposals(b, p) => proposal_lines(b, p),
                Phase::Backfill(b, bf) => self.backfill_lines(b, bf),
                Phase::Training(b) => {
                    let mut l = batch_header(b);
                    l.push(Line::from("  Retraining the classifier…").dim());
                    l
                }
            };
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
        }
        if let Some(form) = &self.review.form {
            draw_form(f, area, "Review filters", form);
        }
    }

    fn idle_lines(&self) -> Vec<Line<'static>> {
        let mut l = vec![
            Line::from(""),
            Line::from(
                "  Review bookmarks in batches: settle each one's tags, one key per decision.",
            )
            .bold(),
            Line::from(
                "  Tag each bookmark completely: a tag left off a reviewed bookmark teaches the \
                 classifier that it doesn't apply.",
            )
            .dim(),
            Line::from(""),
        ];
        let batches = if self.review.filter.is_empty() {
            "  Batches: bookmarks not reviewed yet, spread across sites at first, then the \
             classifier's least sure."
                .to_string()
        } else {
            format!(
                "  Batches: bookmarks with suggestions matching {}, least confident first.",
                filter_text(&self.review.filter)
            )
        };
        l.push(Line::from(batches));
        l.push(Line::from(format!(
            "  {} bookmarks a batch. Enter starts one; f changes the filters.",
            self.review.size
        )));
        l.push(Line::from(""));
        match &self.review.history {
            Some(h) if !h.is_empty() => {
                l.push(
                    Line::from("  batch  reviewed  skipped  proposed  added  classifier accepted")
                        .dim(),
                );
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
                    l.push(Line::from(format!(
                        "  {:>5}  {:>8}  {:>7}  {:>8}  {:>5}  {model}",
                        i + 1,
                        st.reviewed,
                        st.skipped,
                        st.proposed,
                        st.tags_added
                    )));
                }
                l.push(Line::from(""));
                l.push(
                    Line::from(
                        "  Proposals tapering off and the classifier's acceptance rising mean the \
                         list has settled.",
                    )
                    .dim(),
                );
            }
            _ => l.push(Line::from("  No batches yet.").dim()),
        }
        l
    }

    fn backfill_lines(&self, b: &Batch, f: &Backfill) -> Vec<Line<'static>> {
        let mut l = batch_header(b);
        let Some(p) = &f.picker else {
            l.push(Line::from(format!("  Offering #{} on reviewed bookmarks…", f.tag)).dim());
            return l;
        };
        l.push(
            Line::from(format!(
                "  Back-fill: #{} on which of {} reviewed bookmarks? (it didn't exist when they \
                 were reviewed)",
                f.tag,
                p.len()
            ))
            .bold(),
        );
        l.extend(p.lines());
        l
    }

    fn draw_queue(&mut self, f: &mut Frame, area: Rect) {
        let q = &mut self.review.queue;
        if q.list.is_empty() {
            f.render_widget(
                Paragraph::new(vec![
                    Line::from(""),
                    Line::from(
                        "  No pending suggestions match. f changes the source, < > the confidence.",
                    )
                    .dim(),
                ]),
                area,
            );
            return;
        }
        let [list, foot] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(u16::from(q.more))])
                .areas(area);
        let h = list.height as usize;
        // Lines: a bookmark's header, then its suggestions.
        let mut lines = Vec::new();
        let mut at = 0;
        let mut last = None;
        for (i, s) in q.list.iter().enumerate() {
            if last != Some(s.id) {
                last = Some(s.id);
                let title = s.title.as_deref().unwrap_or(&s.url);
                lines.push(Line::from(vec![
                    Span::styled(format!("  {:>6}  ", s.id), Style::new().fg(Color::Yellow)),
                    Span::raw(one_line(title)),
                    Span::raw(format!("  {}", host(&s.url))).dim(),
                ]));
            }
            if i == q.cursor {
                at = lines.len();
            }
            let conf = s.confidence.map(|c| format!(" {c:.2}")).unwrap_or_default();
            lines.push(selected(
                Line::from(vec![
                    Span::raw("          "),
                    Span::styled(format!("#{}", s.tag), Style::new().fg(Color::Green)),
                    Span::raw(format!("  {}{conf}", s.source.as_str())).dim(),
                ]),
                i == q.cursor,
            ));
        }
        // Keep the header of the current bookmark in view when it fits.
        if at < q.offset + 1 {
            q.offset = at.saturating_sub(1);
        } else if at >= q.offset + h {
            q.offset = at + 1 - h;
        }
        f.render_widget(Paragraph::new(lines).scroll((q.offset as u16, 0)), list);
        if q.more {
            f.render_widget(
                Line::from(format!("  showing the first {QUEUE_MAX}")).dim(),
                foot,
            );
        }
    }
}

/// Moves to the next proposal not decided yet.
fn next_open(p: &mut Proposals) {
    let n = p.list.len();
    for step in 1..=n {
        let i = (p.cursor + step) % n;
        if p.said[i].is_none() {
            p.cursor = i;
            return;
        }
    }
}

fn add_row(c: &mut Current, tag: String, kind: Kind) {
    match c.rows.iter().position(|r| r.tag == tag) {
        Some(i) => {
            c.rows[i].on = true;
            c.cursor = i;
        }
        None if c.proposed.contains(&tag) => {}
        None => {
            c.rows.push(Row {
                tag,
                kind,
                on: true,
            });
            c.cursor = c.rows.len() - 1;
        }
    }
}

fn batch_header(b: &Batch) -> Vec<Line<'static>> {
    vec![
        Line::from(format!("  Batch: {}", b.order)).dim(),
        Line::from(""),
    ]
}

fn deciding_lines(b: &Batch) -> Vec<Line<'static>> {
    let mut l = batch_header(b);
    let Some(c) = &b.current else {
        return l;
    };
    let bm = &c.bookmark;
    l.push(Line::from(vec![
        Span::raw(format!("  [{}/{}] ", b.at + 1, b.ids.len())),
        Span::styled(bm.id.to_string(), Style::new().fg(Color::Yellow)),
        Span::raw("  "),
        Span::raw(one_line(bm.title.as_deref().unwrap_or(""))).bold(),
    ]));
    l.push(Line::from(format!("        {}", bm.url)).fg(Color::Blue));
    if let Some(s) = &bm.summary {
        l.push(Line::from(format!("        {}", one_line(s))).dim());
    }
    l.push(Line::from(""));
    let width = c
        .rows
        .iter()
        .map(|r| r.tag.chars().count())
        .chain(c.proposed.iter().map(|p| p.chars().count()))
        .max()
        .unwrap_or(0);
    let tag = Style::new().fg(Color::Cyan);
    for (i, r) in c.rows.iter().enumerate() {
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
        let line = Line::from(vec![
            Span::raw(format!("    {key} {mark} ")),
            Span::styled(format!("#{}", r.tag), tag),
            Span::raw(format!("{:pad$}  ", "")),
            Span::raw(why).dim(),
        ]);
        l.push(selected(line, i == c.cursor));
    }
    for p in &c.proposed {
        let pad = width - p.chars().count();
        l.push(Line::from(vec![
            Span::raw("          "),
            Span::styled(format!("#{p}"), tag),
            Span::raw(format!("{:pad$}  ", "")),
            Span::raw("proposed (decided after the batch)").dim(),
        ]));
    }
    if !c.rows.iter().any(|r| r.on) && c.proposed.is_empty() {
        l.push(Line::from("    no tags yet: t adds one").dim());
    }
    if let Some(o) = &b.offer {
        l.push(Line::from(""));
        l.push(Line::from(vec![
            Span::raw("  Rule? ").bold(),
            Span::raw(format!("{} → ", o.host)),
            Span::styled(format!("#{}", o.tag), tag),
            Span::raw(format!(
                "  ({} of {} tagged bookmarks there have it)  R creates it",
                o.count, o.of
            ))
            .dim(),
        ]));
    }
    if !b.log.is_empty() {
        l.push(Line::from(""));
        l.push(Line::from("  Done in this batch").dim());
        for line in b.log.iter().rev().take(8) {
            l.push(line.clone());
        }
    }
    l
}

fn proposal_lines(b: &Batch, p: &Proposals) -> Vec<Line<'static>> {
    let mut l = batch_header(b);
    l.push(
        Line::from(format!(
            "  {} for the list, from this batch: a add · l alias · r reject · s skip",
            crate::plural(p.list.len(), "name")
        ))
        .bold(),
    );
    for (i, x) in p.list.iter().enumerate() {
        let mut spans = vec![
            Span::raw("    "),
            Span::styled(format!("#{}", x.name), Style::new().fg(Color::Cyan)),
            Span::raw(format!(
                "  on {}",
                crate::plural(x.bookmarks.len(), "bookmark")
            )),
        ];
        if let Some(d) = &x.description {
            spans.push(Span::raw(format!("  # {d}")).dim());
        }
        spans.push(Span::raw(format!("  ({})", x.source.as_str())).dim());
        if let Some(s) = &p.said[i] {
            spans.push(Span::raw(format!("  {s}")).fg(Color::Green));
        }
        l.push(selected(Line::from(spans), i == p.cursor));
    }
    l
}

/// `12  Title  tags…`: a bookmark decided in the batch.
fn log_line(b: &Bookmark, what: Vec<Span<'static>>) -> Line<'static> {
    let text = b.title.as_deref().unwrap_or(&b.url);
    let mut spans = vec![
        Span::styled(format!("    {:>6}  ", b.id), Style::new().fg(Color::Yellow)),
        Span::raw(format!("{}  ", cut(&one_line(text), 40))),
    ];
    spans.extend(what);
    Line::from(spans)
}

fn filter_text(f: &ReviewFilter) -> String {
    let mut parts = Vec::new();
    if let Some(t) = &f.tag {
        parts.push(format!("#{t}"));
    }
    if let Some(s) = f.source {
        parts.push(format!("from {}", s.as_str()));
    }
    if let Some(b) = f.below {
        parts.push(format!("below {b}"));
    }
    parts.join(", ")
}

fn selected(line: Line<'static>, on: bool) -> Line<'static> {
    if on {
        line.style(Style::new().add_modifier(Modifier::REVERSED))
    } else {
        line
    }
}

fn hashed(tags: &[String]) -> String {
    tags.iter()
        .map(|t| format!("#{t}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn cut(text: &str, n: usize) -> String {
    match text.char_indices().nth(n.saturating_sub(1)) {
        Some((at, _)) if text.chars().count() > n => format!("{}…", &text[..at]),
        _ => text.to_string(),
    }
}

fn host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    host.strip_prefix("www.").unwrap_or(host)
}

#[cfg(test)]
impl App {
    /// The batch's phase, for tests.
    pub fn review_phase(&self) -> &'static str {
        match &self.review.phase {
            Phase::Idle => "idle",
            Phase::Picking => "picking",
            Phase::Deciding(_) => "deciding",
            Phase::Proposals(..) => "proposals",
            Phase::Backfill(_, f) if f.picker.is_none() => "offering",
            Phase::Backfill(..) => "backfill",
            Phase::Training(_) => "training",
        }
    }
}

impl App {
    /// `:review`: starts a batch unless one is under way.
    pub(super) fn run_review(&mut self) {
        self.review.queue_tab = false;
        match self.review.phase {
            Phase::Idle => self.start_batch(),
            _ => self.say("a batch is under way"),
        }
    }
}
