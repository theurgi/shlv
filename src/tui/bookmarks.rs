//! View 1, Bookmarks: the whole library newest first (or a search's results), a detail pane, and
//! the actions on one bookmark or several marked ones.
//!
//! Only ids are held for the list; rows are loaded as they scroll into view, so the first screen
//! of a 100k library costs one id query and a screenful of rows.

use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap};

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use shlv::store::normalize_tag;
use shlv::{Bookmark, Deleted, Edit, NewBookmark};

use super::app::{App, draw_line_input};
use super::form::{Field, Form, FormKey, draw_form};
use super::input::{Edited, Input};
#[cfg(feature = "fetch")]
use super::worker::Job;

/// The view's keys, for `?`.
pub fn keys() -> Vec<(&'static str, &'static str)> {
    let mut keys = vec![
        ("j k, ↓ ↑", "down, up"),
        ("g G", "top, bottom"),
        ("Ctrl-d Ctrl-u", "half a page down, up"),
        (
            "/",
            "search (#tag, site:, AND/OR/NOT); Enter keeps it, Esc drops it",
        ),
        ("Enter, o", "open in the browser"),
        ("Tab", "toggle the detail pane"),
        ("a", "add a bookmark"),
        ("e", "edit title and note"),
        ("t", "edit tags (Tab completes)"),
        ("dd", "delete"),
        ("u", "undo the last delete"),
        ("v", "mark (t, dd, y, n act on all marked)"),
        ("y n", "accept, reject the pending suggestions"),
        ("s", "go through the suggestions one by one"),
    ];
    if cfg!(feature = "fetch") {
        keys.push(("F", "fetch the title"));
    }
    if cfg!(feature = "ai") {
        keys.push(("S", "summarize ([ai] summarize provider)"));
    }
    keys.extend([
        ("Ctrl-l", "reload (after changes from elsewhere)"),
        ("Esc", "clear the marks, then the search"),
    ]);
    keys
}

#[derive(Default)]
pub struct Bookmarks {
    /// The listed bookmarks, in order.
    pub ids: Vec<i64>,
    /// Live bookmarks in the library.
    total: usize,
    pub cursor: usize,
    /// The first row shown.
    offset: usize,
    /// Rows shown at the last draw.
    page: usize,
    /// Loaded rows, by id.
    rows: HashMap<i64, Bookmark>,
    /// The search applied ("" lists everything).
    pub query: String,
    search_error: Option<String>,
    /// The search text changed and hasn't run yet; see [`App::settle`].
    typed: bool,
    mode: Mode,
    pub marked: BTreeSet<i64>,
    hide_detail: bool,
    /// Deletes of this session, the last one last.
    undo: Vec<Vec<Deleted>>,
}

#[derive(Default)]
enum Mode {
    #[default]
    Normal,
    /// Typing a search; `before` comes back on Esc.
    Search {
        input: Input,
        before: String,
    },
    /// The first `d` of `dd`.
    Delete,
    /// Going through the current bookmark's pending suggestions; the one selected.
    Suggest(usize),
    Form(Form<FormKind>),
}

enum FormKind {
    Add,
    Edit {
        id: i64,
        title: String,
        note: String,
    },
    /// One bookmark (with its tags as they were) or several (`+tag -tag`).
    Tags {
        ids: Vec<i64>,
        old: Option<Vec<String>>,
    },
}

impl Bookmarks {
    /// Drops loaded rows, so they are read again.
    pub fn forget(&mut self, ids: &[i64]) {
        for id in ids {
            self.rows.remove(id);
        }
    }

    pub fn current(&self) -> Option<i64> {
        self.ids.get(self.cursor).copied()
    }

    /// The marked bookmarks, else the current one.
    fn targets(&self) -> Vec<i64> {
        if self.marked.is_empty() {
            self.current().into_iter().collect()
        } else {
            self.ids
                .iter()
                .copied()
                .filter(|id| self.marked.contains(id))
                .collect()
        }
    }

    fn move_to(&mut self, i: isize) {
        let last = self.ids.len().saturating_sub(1) as isize;
        self.cursor = i.clamp(0, last.max(0)) as usize;
    }

    fn move_by(&mut self, d: isize) {
        self.move_to(self.cursor as isize + d);
    }
}

impl App {
    /// Reads the list again (the search, or everything), keeping the cursor on its bookmark if
    /// it's still listed.
    pub fn reload(&mut self) -> Result<()> {
        self.reload_at(self.bookmarks.current())
    }

    fn reload_at(&mut self, keep: Option<i64>) -> Result<()> {
        let b = &mut self.bookmarks;
        let all = self.store.list_ids()?;
        b.total = all.len();
        b.ids = if b.query.trim().is_empty() {
            all
        } else {
            self.store.search_ids(&b.query, usize::MAX)?
        };
        b.rows.clear();
        let live: BTreeSet<i64> = b.ids.iter().copied().collect();
        b.marked.retain(|id| live.contains(id));
        match keep.and_then(|id| b.ids.iter().position(|&i| i == id)) {
            Some(i) => b.cursor = i,
            None => b.move_to(b.cursor as isize),
        }
        Ok(())
    }

    /// Runs the search typed since the last call. The event loop calls this once the keys
    /// queued so far are handled, so fast typing (or a held key) runs one search, not one per
    /// key: a one-letter prefix ranks most of a 100k library (50-80 ms).
    pub fn settle(&mut self) {
        if !std::mem::take(&mut self.bookmarks.typed) {
            return;
        }
        if let Mode::Search { input, .. } = &self.bookmarks.mode {
            let q = input.text().to_string();
            self.search(&q);
        }
    }

    /// Runs the search being typed. An unfinished query (an open quote, a dangling `OR`) keeps
    /// the last results and says why on the status line.
    fn search(&mut self, query: &str) {
        let result = if query.trim().is_empty() {
            self.store.list_ids()
        } else {
            self.store.search_ids(query, usize::MAX)
        };
        let b = &mut self.bookmarks;
        match result {
            Ok(ids) => {
                b.ids = ids;
                b.query = query.to_string();
                b.search_error = None;
                b.cursor = 0;
                b.offset = 0;
                b.rows.clear();
            }
            Err(e) => b.search_error = Some(format!("{e:#}")),
        }
    }

    pub(super) fn bookmarks_paste(&mut self, text: &str) {
        match &mut self.bookmarks.mode {
            Mode::Search { input, .. } => {
                input.insert(text);
                self.bookmarks.typed = true;
            }
            Mode::Form(form) => form.fields[form.focus].input.insert(text),
            // Pasting a URL in the list starts adding it.
            _ => {
                self.open_add();
                if let Mode::Form(form) = &mut self.bookmarks.mode {
                    form.fields[0].input.insert(text.trim());
                }
            }
        }
    }

    /// Handles a key in the Bookmarks view; returns false for keys it leaves to the app (`q`,
    /// `?`, `:`, view numbers).
    pub(super) fn bookmarks_key(&mut self, k: KeyEvent) -> bool {
        let mode = std::mem::take(&mut self.bookmarks.mode);
        match mode {
            Mode::Normal => return self.normal_key(k),
            Mode::Search { mut input, before } => match k.code {
                KeyCode::Esc => {
                    self.bookmarks.typed = false;
                    self.search(&before);
                    self.bookmarks.search_error = None;
                }
                KeyCode::Enter => {
                    if std::mem::take(&mut self.bookmarks.typed) {
                        self.search(input.text());
                    }
                    // Keep what last worked.
                    self.bookmarks.search_error = None;
                }
                _ => {
                    if input.key(k) == Edited::Changed {
                        self.bookmarks.typed = true;
                    }
                    self.bookmarks.mode = Mode::Search { input, before };
                }
            },
            Mode::Delete => match k.code {
                KeyCode::Char('d') => self.delete(),
                _ => self.message_clear(),
            },
            Mode::Suggest(i) => self.suggest_key(k, i),
            Mode::Form(form) => self.form_key(k, form),
        }
        true
    }

    fn message_clear(&mut self) {
        self.say("");
    }

    fn normal_key(&mut self, k: KeyEvent) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let half = (self.bookmarks.page.max(2) / 2) as isize;
        let b = &mut self.bookmarks;
        match k.code {
            KeyCode::Char('d') if ctrl => b.move_by(half),
            KeyCode::Char('u') if ctrl => b.move_by(-half),
            KeyCode::Char('f') if ctrl => b.move_by(2 * half),
            KeyCode::Char('b') if ctrl => b.move_by(-2 * half),
            KeyCode::Char('l') if ctrl => {
                if let Err(e) = self.reload() {
                    self.warn(format!("{e:#}"));
                } else {
                    self.tags_changed();
                    self.say("reloaded");
                }
            }
            _ if ctrl => return false,
            KeyCode::Char('j') | KeyCode::Down => b.move_by(1),
            KeyCode::Char('k') | KeyCode::Up => b.move_by(-1),
            KeyCode::PageDown => b.move_by(2 * half),
            KeyCode::PageUp => b.move_by(-2 * half),
            KeyCode::Char('g') | KeyCode::Home => b.move_to(0),
            KeyCode::Char('G') | KeyCode::End => b.move_to(isize::MAX),
            KeyCode::Tab => b.hide_detail = !b.hide_detail,
            KeyCode::Char('/') => {
                b.mode = Mode::Search {
                    input: Input::new(&b.query),
                    before: b.query.clone(),
                };
            }
            KeyCode::Esc => {
                if !b.marked.is_empty() {
                    b.marked.clear();
                } else if !b.query.is_empty() {
                    self.search("");
                }
            }
            KeyCode::Enter | KeyCode::Char('o') => {
                if let Some(id) = b.current() {
                    match self.store.get(id) {
                        Ok(Some(bm)) => self.request_open(bm.url),
                        Ok(None) => self.warn(format!("bookmark {id} is gone (Ctrl-l reloads)")),
                        Err(e) => self.warn(format!("{e:#}")),
                    }
                }
            }
            KeyCode::Char('v') => {
                if let Some(id) = b.current() {
                    if !b.marked.remove(&id) {
                        b.marked.insert(id);
                    }
                    b.move_by(1);
                    let n = b.marked.len();
                    self.say(format!("{n} marked (Esc clears)"));
                }
            }
            KeyCode::Char('d') => {
                if b.current().is_some() {
                    b.mode = Mode::Delete;
                    let n = b.targets().len();
                    let what = if n == 1 {
                        "this bookmark".to_string()
                    } else {
                        format!("{n} marked bookmarks")
                    };
                    self.say(format!("d again deletes {what}"));
                }
            }
            KeyCode::Char('u') => self.undo(),
            KeyCode::Char('a') => self.open_add(),
            KeyCode::Char('e') => self.open_edit(),
            KeyCode::Char('t') => self.open_tags(),
            KeyCode::Char('y') => self.decide_all(true),
            KeyCode::Char('n') => self.decide_all(false),
            KeyCode::Char('s') => match b.current().map(|id| self.store.pending(Some(id))) {
                Some(Ok(p)) if !p.is_empty() => {
                    self.bookmarks.mode = Mode::Suggest(0);
                    self.bookmarks.hide_detail = false;
                }
                Some(Err(e)) => self.warn(format!("{e:#}")),
                _ => self.say("no pending suggestions"),
            },
            #[cfg(feature = "fetch")]
            KeyCode::Char('F') => {
                for (id, url) in self.target_urls() {
                    self.send_job(Job::FetchTitle { id, url });
                }
            }
            #[cfg(feature = "ai")]
            KeyCode::Char('S') => {
                if let Err(e) = crate::ai_cmd::provider(&self.config, shlv::ai::Task::Summarize) {
                    self.warn(format!("{e:#}"));
                } else {
                    for (id, url) in self.target_urls() {
                        self.send_job(Job::Summarize { id, url });
                    }
                }
            }
            _ => return false,
        }
        true
    }

    /// The URLs of the bookmarks an action works on.
    #[cfg(feature = "fetch")]
    fn target_urls(&mut self) -> Vec<(i64, String)> {
        let mut v = Vec::new();
        for id in self.bookmarks.targets() {
            match self.store.get(id) {
                Ok(Some(b)) => v.push((id, b.url)),
                Ok(None) => {}
                Err(e) => self.warn(format!("{e:#}")),
            }
        }
        v
    }

    fn delete(&mut self) {
        let ids = self.bookmarks.targets();
        let mut kept = Vec::new();
        for &id in &ids {
            match self.store.delete_keeping(id) {
                Ok(Some(d)) => kept.push(d),
                Ok(None) => {}
                Err(e) => {
                    self.warn(format!("cannot delete {id}: {e:#}"));
                    break;
                }
            }
        }
        if kept.is_empty() {
            return;
        }
        let gone: BTreeSet<i64> = kept.iter().map(|d| d.bookmark.id).collect();
        let b = &mut self.bookmarks;
        b.ids.retain(|id| !gone.contains(id));
        b.total -= gone.len();
        b.marked.clear();
        b.move_to(b.cursor as isize);
        let what = match &kept[..] {
            [d] => format!("deleted {}", d.bookmark.id),
            many => format!("deleted {} bookmarks", many.len()),
        };
        b.undo.push(kept);
        self.tags_changed();
        self.say(format!("{what} (u undoes)"));
    }

    fn undo(&mut self) {
        let Some(kept) = self.bookmarks.undo.pop() else {
            self.say("nothing to undo");
            return;
        };
        let mut back = 0;
        for d in &kept {
            match self.store.undelete(d) {
                Ok(true) => back += 1,
                Ok(false) => {}
                Err(e) => self.warn(format!("cannot restore {}: {e:#}", d.bookmark.id)),
            }
        }
        let first = kept.first().map(|d| d.bookmark.id);
        if let Err(e) = self.reload_at(first) {
            self.warn(format!("{e:#}"));
            return;
        }
        self.tags_changed();
        if back > 0 {
            let s = if back == 1 { "" } else { "s" };
            self.say(format!("restored {back} bookmark{s}"));
        }
    }

    /// `y` and `n`: accepts or rejects every pending suggestion of the targets.
    fn decide_all(&mut self, accept: bool) {
        let mut done = Vec::new();
        for id in self.bookmarks.targets() {
            let pending = match self.store.pending(Some(id)) {
                Ok(p) => p,
                Err(e) => return self.warn(format!("{e:#}")),
            };
            if pending.is_empty() {
                continue;
            }
            let r = if accept {
                self.store.accept(id, &[])
            } else {
                self.store.reject(id, &[])
            };
            match r {
                Ok(tags) => done.extend(tags),
                Err(e) => return self.warn(format!("{id}: {e:#}")),
            }
            self.bookmarks.forget(&[id]);
        }
        if done.is_empty() {
            self.say("no pending suggestions");
            return;
        }
        self.tags_changed();
        let verb = if accept { "accepted" } else { "rejected" };
        self.say(format!("{verb} {}", hashed(&done)));
    }

    fn suggest_key(&mut self, k: KeyEvent, i: usize) {
        let Some(id) = self.bookmarks.current() else {
            return;
        };
        let pending = match self.store.pending(Some(id)) {
            Ok(p) => p,
            Err(e) => return self.warn(format!("{e:#}")),
        };
        let mut i = i.min(pending.len().saturating_sub(1));
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return,
            KeyCode::Char('j') | KeyCode::Down => i = (i + 1).min(pending.len() - 1),
            KeyCode::Char('k') | KeyCode::Up => i = i.saturating_sub(1),
            KeyCode::Char(c @ ('y' | 'n')) => {
                let Some(s) = pending.get(i) else { return };
                let tags = [s.tag.clone()];
                let r = if c == 'y' {
                    self.store.accept(id, &tags)
                } else {
                    self.store.reject(id, &tags)
                };
                match r {
                    Ok(_) => {
                        let verb = if c == 'y' { "accepted" } else { "rejected" };
                        self.say(format!("{verb} #{}", s.tag));
                        self.bookmarks.forget(&[id]);
                        self.tags_changed();
                    }
                    Err(e) => self.warn(format!("{e:#}")),
                }
                if pending.len() == 1 {
                    return;
                }
            }
            _ => {}
        }
        self.bookmarks.mode = Mode::Suggest(i);
    }

    fn open_add(&mut self) {
        self.bookmarks.mode = Mode::Form(Form {
            kind: FormKind::Add,
            fields: vec![
                Field::line("URL", ""),
                Field::line("Title", ""),
                Field::tags("Tags", ""),
            ],
            focus: 0,
        });
    }

    fn open_edit(&mut self) {
        let Some(id) = self.bookmarks.current() else {
            return;
        };
        let b = match self.store.get(id) {
            Ok(Some(b)) => b,
            Ok(None) => return self.warn(format!("bookmark {id} is gone")),
            Err(e) => return self.warn(format!("{e:#}")),
        };
        let title = b.title.unwrap_or_default();
        let note = b.note.unwrap_or_default();
        self.bookmarks.mode = Mode::Form(Form {
            fields: vec![
                Field::line("Title", &title),
                Field::multiline("Note", &note),
            ],
            kind: FormKind::Edit { id, title, note },
            focus: 0,
        });
    }

    fn open_tags(&mut self) {
        let ids = self.bookmarks.targets();
        let (old, text) = match ids[..] {
            [] => return,
            [id] => match self.store.get(id) {
                Ok(Some(b)) => {
                    let mut text = b.tags.join(" ");
                    if !text.is_empty() {
                        text.push(' ');
                    }
                    (Some(b.tags), text)
                }
                Ok(None) => return self.warn(format!("bookmark {id} is gone")),
                Err(e) => return self.warn(format!("{e:#}")),
            },
            _ => (None, String::new()),
        };
        let label = if old.is_some() {
            "Tags"
        } else {
            "+add -remove"
        };
        self.bookmarks.mode = Mode::Form(Form {
            kind: FormKind::Tags { ids, old },
            fields: vec![Field::tags(label, &text)],
            focus: 0,
        });
    }

    fn form_key(&mut self, k: KeyEvent, mut form: Form<FormKind>) {
        match self.form_input(&mut form, k) {
            FormKey::Cancel => return,
            FormKey::Submit if self.submit(&form) => return,
            _ => {}
        }
        self.bookmarks.mode = Mode::Form(form);
    }

    /// Saves a form; returns false (keeping it open) if that failed.
    fn submit(&mut self, form: &Form<FormKind>) -> bool {
        let text = |i: usize| form.fields[i].input.text().to_string();
        let r = match &form.kind {
            FormKind::Add => self.add(text(0), text(1), &text(2)),
            FormKind::Edit { id, title, note } => {
                let (t, n) = (text(0), text(1));
                if t == *title && n == *note {
                    self.say("nothing changed");
                    return true;
                }
                let edit = Edit {
                    title: (t != *title).then_some(t),
                    note: (n != *note).then_some(n),
                    ..Default::default()
                };
                match self.store.edit(*id, &edit) {
                    Ok(true) => {
                        self.bookmarks.forget(&[*id]);
                        self.say(format!("updated {id}"));
                        Ok(())
                    }
                    Ok(false) => Err(anyhow::anyhow!("bookmark {id} is gone (Ctrl-l reloads)")),
                    Err(e) => Err(e),
                }
            }
            FormKind::Tags { ids, old } => self.retag(ids, old.as_deref(), &text(0)),
        };
        match r {
            Ok(()) => true,
            Err(e) => {
                self.warn(format!("{e:#}"));
                false
            }
        }
    }

    fn add(&mut self, url: String, title: String, tags: &str) -> Result<()> {
        if url.trim().is_empty() {
            anyhow::bail!("a URL, please");
        }
        let tags: Vec<String> = tags
            .split_whitespace()
            .map(|t| t.trim_start_matches('+').to_string())
            .collect();
        let added = self.store.add(&NewBookmark {
            url: url.trim().to_string(),
            title: Some(title).filter(|t| !t.trim().is_empty()),
            note: None,
            tags,
        })?;
        let id = added.id;
        crate::classify_added(&mut self.store, &self.config, id)?;
        self.tags_changed();
        self.reload_at(Some(id))?;
        let mut msg = if added.created {
            format!("added {id}")
        } else {
            format!("updated {id} (already saved)")
        };
        if self.bookmarks.current() != Some(id) {
            msg.push_str(", not in this search (Esc clears it)");
        }
        let pending: Vec<String> = self
            .store
            .pending(Some(id))?
            .into_iter()
            .map(|s| s.tag)
            .collect();
        if !pending.is_empty() {
            msg.push_str(&format!("; suggested {} (y accepts)", hashed(&pending)));
        }
        if !added.diverted.proposed.is_empty() {
            msg.push_str(&format!(
                "; proposed {} (not canonical)",
                hashed(&added.diverted.proposed)
            ));
        }
        if !added.diverted.rejected.is_empty() {
            msg.push_str(&format!(
                "; dropped {} (rejected names)",
                hashed(&added.diverted.rejected)
            ));
        }
        self.say(msg);
        #[cfg(feature = "fetch")]
        if let Some(b) = self.store.get(id)?.filter(|b| b.title.is_none()) {
            self.send_job(Job::FetchTitle { id, url: b.url });
        }
        Ok(())
    }

    /// Applies a tag form: with one bookmark's `old` tags the text is its new tag list (`-tag`
    /// also removes); for several, `+tag` (or bare) adds and `-tag` removes.
    fn retag(&mut self, ids: &[i64], old: Option<&[String]>, text: &str) -> Result<()> {
        let mut edit = Edit::default();
        let mut listed = Vec::new();
        for w in text.split_whitespace() {
            if let Some(t) = w.strip_prefix('-') {
                edit.remove_tags.push(t.to_string());
            } else {
                let t = w.strip_prefix('+').unwrap_or(w);
                listed.push(normalize_tag(t));
                edit.add_tags.push(t.to_string());
            }
        }
        if let Some(old) = old {
            edit.add_tags.retain(|t| !old.contains(&normalize_tag(t)));
            edit.remove_tags
                .extend(old.iter().filter(|t| !listed.contains(t)).cloned());
        }
        if edit.add_tags.is_empty() && edit.remove_tags.is_empty() {
            self.say("nothing changed");
            return Ok(());
        }
        let done = self.store.edit_many(ids, &edit)?;
        self.bookmarks.forget(ids);
        self.tags_changed();
        let mut parts = Vec::new();
        let count = |n: usize| {
            if ids.len() > 1 {
                format!(" ({n})")
            } else {
                String::new()
            }
        };
        for (t, n) in &done.added {
            parts.push(format!("+#{t}{}", count(*n)));
        }
        for (t, n) in &done.removed {
            parts.push(format!("-#{t}{}", count(*n)));
        }
        let mut msg = match ids {
            [id] => format!("{id}: {}", parts.join(" ")),
            _ => format!("{} bookmarks: {}", ids.len(), parts.join(" ")),
        };
        if !done.diverted.proposed.is_empty() {
            msg.push_str(&format!(
                "; proposed {} (not canonical)",
                hashed(&done.diverted.proposed)
            ));
        }
        self.say(msg);
        Ok(())
    }

    pub(super) fn bookmarks_status(&self) -> Vec<Span<'static>> {
        let b = &self.bookmarks;
        let mut v = Vec::new();
        if !b.query.is_empty() {
            v.push(Span::raw(format!("/{} ", b.query)));
        }
        let pos = if b.ids.is_empty() { 0 } else { b.cursor + 1 };
        let mut count = format!("{pos}/{}", b.ids.len());
        if b.ids.len() != b.total {
            count.push_str(&format!(" of {}", b.total));
        }
        v.push(Span::raw(count));
        if !b.marked.is_empty() {
            v.push(Span::raw(format!("  {} marked", b.marked.len())));
        }
        if let Some(e) = &b.search_error {
            v.push(Span::styled(
                format!("  {e}"),
                Style::new()
                    .fg(Color::Red)
                    .remove_modifier(Modifier::REVERSED),
            ));
        }
        v
    }

    /// Draws the search being typed on the bottom line; returns false if there's none.
    pub(super) fn draw_bookmarks_prompt(&self, f: &mut Frame, area: Rect) -> bool {
        match &self.bookmarks.mode {
            Mode::Search { input, .. } => {
                draw_line_input(f, area, "/", input);
                true
            }
            _ => false,
        }
    }

    /// The keys of what the view is in the middle of, for the bottom line.
    pub(super) fn bookmarks_hint(&self) -> Option<&'static str> {
        Some(match &self.bookmarks.mode {
            Mode::Normal | Mode::Delete | Mode::Search { .. } => return None,
            Mode::Suggest(_) => "y accept · n reject · j/k move · Esc done",
            Mode::Form(form) => match form.kind {
                FormKind::Edit { .. } => {
                    "Enter saves · Alt-Enter new line · ↑↓ field · Esc cancels"
                }
                _ => form.hint(),
            },
        })
    }

    pub(super) fn draw_bookmarks(&mut self, f: &mut Frame, area: Rect) {
        let (list, detail) = if self.bookmarks.hide_detail || self.bookmarks.ids.is_empty() {
            (area, None)
        } else if area.width >= 100 {
            let [l, d] =
                Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                    .areas(area);
            (l, Some(d))
        } else {
            let [l, d] = Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
                .areas(area);
            (l, Some(d))
        };
        self.draw_list(f, list);
        if let Some(d) = detail {
            self.draw_detail(f, d);
        }
        if let Mode::Form(form) = &self.bookmarks.mode {
            let title = match &form.kind {
                FormKind::Add => "Add a bookmark".to_string(),
                FormKind::Edit { id, .. } => format!("Edit {id}"),
                FormKind::Tags { ids, .. } => match ids[..] {
                    [id] => format!("Tags of {id}"),
                    _ => format!("Tags of {} bookmarks", ids.len()),
                },
            };
            draw_form(f, area, &title, form);
        }
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        let b = &mut self.bookmarks;
        let h = area.height as usize;
        b.page = h;
        if b.ids.is_empty() {
            let text = if b.total == 0 {
                "  No bookmarks yet: a adds one."
            } else {
                "  Nothing matches. Esc drops the search."
            };
            f.render_widget(
                Paragraph::new(vec![Line::from(""), Line::from(text).dim()]),
                area,
            );
            return;
        }
        if b.cursor < b.offset {
            b.offset = b.cursor;
        } else if b.cursor >= b.offset + h {
            b.offset = b.cursor + 1 - h;
        }
        // A cache that has grown past a few pages starts over.
        if b.rows.len() > 4096 {
            b.rows.clear();
        }
        let end = (b.offset + h).min(b.ids.len());
        let width = b.ids.iter().max().map_or(1, |m| m.to_string().len());
        let mut lines = Vec::with_capacity(h);
        for i in b.offset..end {
            let id = b.ids[i];
            if let Entry::Vacant(slot) = b.rows.entry(id) {
                match self.store.get(id) {
                    Ok(Some(row)) => {
                        slot.insert(row);
                    }
                    Ok(None) => {
                        lines.push(Line::from(format!("  {id:>width$}  (deleted)")).dim());
                        continue;
                    }
                    Err(e) => {
                        lines.push(Line::from(format!("  {id:>width$}  {e:#}")).red());
                        continue;
                    }
                }
            }
            let row = &b.rows[&id];
            let mark = if b.marked.contains(&id) { "▌ " } else { "  " };
            let mut spans = vec![
                Span::styled(mark, Style::new().fg(Color::Magenta)),
                Span::styled(format!("{id:>width$}  "), Style::new().fg(Color::Yellow)),
            ];
            match &row.title {
                Some(t) => {
                    spans.push(Span::raw(one_line(t)));
                    spans.push(Span::styled(
                        format!("  {}", host(&row.url)),
                        Style::new().add_modifier(Modifier::DIM),
                    ));
                }
                None => spans.push(Span::raw(row.url.clone())),
            }
            for t in &row.tags {
                spans.push(Span::styled(format!(" #{t}"), Style::new().fg(Color::Cyan)));
            }
            let mut line = Line::from(spans);
            if i == b.cursor {
                line = line.style(Style::new().add_modifier(Modifier::REVERSED));
            }
            lines.push(line);
        }
        f.render_widget(Paragraph::new(lines), area);
    }

    fn draw_detail(&self, f: &mut Frame, area: Rect) {
        let Some(id) = self.bookmarks.current() else {
            return;
        };
        let lines = match self.detail_lines(id) {
            Ok(l) => l,
            Err(e) => vec![Line::from(format!("{e:#}")).red()],
        };
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::bordered().border_style(Style::new().add_modifier(Modifier::DIM))),
            area,
        );
    }

    fn detail_lines(&self, id: i64) -> Result<Vec<Line<'static>>> {
        let Some(b) = self.store.get(id)? else {
            return Ok(vec![Line::from("(deleted)").dim()]);
        };
        let label = |s: &str| Span::styled(format!("{s:<10}"), Style::new().bold());
        let tag = Style::new().fg(Color::Cyan);
        let mut lines = vec![
            Line::from(b.title.clone().unwrap_or_else(|| "(no title)".into())).bold(),
            Line::from(b.url.clone()).fg(Color::Blue),
            Line::from(""),
            Line::from(vec![
                label("id"),
                Span::styled(b.id.to_string(), Style::new().fg(Color::Yellow)),
            ]),
            Line::from(vec![
                label("added"),
                Span::raw(crate::format_time(b.created_at)),
            ]),
        ];
        if b.updated_at != b.created_at {
            lines.push(Line::from(vec![
                label("updated"),
                Span::raw(crate::format_time(b.updated_at)),
            ]));
        }
        let mut tags = vec![label("tags")];
        for t in self.store.tag_sources(id)? {
            tags.push(Span::styled(format!("#{}", t.tag), tag));
            if t.source != "user" {
                tags.push(Span::styled(
                    format!(" ({})", source(&t.source, t.confidence)),
                    Style::new().add_modifier(Modifier::DIM),
                ));
            }
            tags.push(Span::raw(" "));
        }
        if tags.len() == 1 {
            tags.push(Span::raw("none").dim());
        }
        lines.push(Line::from(tags));
        let pending = self.store.pending(Some(id))?;
        if !pending.is_empty() {
            let selected = match self.bookmarks.mode {
                Mode::Suggest(i) => Some(i.min(pending.len() - 1)),
                _ => None,
            };
            let mut spans = vec![label("suggested")];
            for (i, s) in pending.iter().enumerate() {
                let mut style = Style::new().fg(Color::Green);
                if selected == Some(i) {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                spans.push(Span::styled(format!("#{}", s.tag), style));
                spans.push(Span::styled(
                    format!(" ({}) ", source(s.source.as_str(), s.confidence)),
                    Style::new().add_modifier(Modifier::DIM),
                ));
            }
            if selected.is_none() {
                spans.push(Span::raw("y/n/s").dim());
            }
            lines.push(Line::from(spans));
        }
        let proposed = self.store.proposals_for(id)?;
        if !proposed.is_empty() {
            let mut spans = vec![label("proposed")];
            spans.extend(proposed.iter().map(|p| Span::raw(format!("#{p} "))));
            lines.push(Line::from(spans));
        }
        for (name, text) in [("Note", &b.note), ("Summary", &b.summary)] {
            if let Some(text) = text {
                lines.push(Line::from(""));
                lines.push(Line::from(name.to_string()).bold());
                lines.extend(text.lines().map(|l| Line::from(l.to_string())));
            }
        }
        Ok(lines)
    }
}

/// `rule`, `model 0.82`: where a tag came from.
fn source(source: &str, confidence: Option<f64>) -> String {
    match confidence {
        Some(c) => format!("{source} {c:.2}"),
        None => source.to_string(),
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

/// The host of a URL, for the list (the detail pane has the full URL).
fn host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    host.strip_prefix("www.").unwrap_or(host)
}

impl App {
    /// Switches to the Bookmarks view showing a search's results (a jump from another view).
    pub(super) fn show_bookmarks(&mut self, query: &str) {
        self.switch(super::app::View::Bookmarks);
        self.bookmarks.mode = Mode::Normal;
        self.search(query);
    }

    /// Bookmarks changed elsewhere (tags, suggestions): rows are read again when drawn.
    pub(super) fn bookmarks_changed(&mut self) {
        self.bookmarks.rows.clear();
    }
}

impl App {
    /// A bookmark deleted elsewhere in the TUI joins the undo list.
    pub(super) fn remember_delete(&mut self, d: Deleted) {
        let id = d.bookmark.id;
        let b = &mut self.bookmarks;
        b.ids.retain(|&i| i != id);
        b.total = b.total.saturating_sub(1);
        b.marked.remove(&id);
        b.move_to(b.cursor as isize);
        b.undo.push(vec![d]);
        self.tags_changed();
    }
}
