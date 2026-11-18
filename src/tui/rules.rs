//! View 4, Rules: domain rules (saved, and pinned in config) with what each tagged, and the rules
//! worth offering, where several bookmarks of a site are tagged the same way (task 043).

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use shlv::rules::{OFFER_MIN, OFFER_SHARE};
use shlv::{RuleCandidate, RuleInfo};

use super::app::{App, View};
use super::form::{Field, Form, FormKey, draw_form};

pub fn keys() -> Vec<(&'static str, &'static str)> {
    vec![
        ("j k, g G", "move, top, bottom"),
        ("Enter", "the bookmarks on the rule's site with its tag"),
        ("a", "add a rule (a host, or a URL on it, and tags)"),
        ("dd", "remove a rule (not one pinned in config)"),
        ("y", "create an offered rule (and run it on the site)"),
        ("n", "decline an offered rule"),
        ("Ctrl-l", "reload"),
    ]
}

#[derive(Default)]
pub struct Rules {
    /// Loaded when the view is first shown, and again after changes.
    loaded: bool,
    list: Vec<RuleInfo>,
    offers: Vec<RuleCandidate>,
    /// Over the rules, then the offers.
    cursor: usize,
    offset: usize,
    mode: Mode,
}

#[derive(Default)]
enum Mode {
    #[default]
    Normal,
    /// The first `d` of `dd`.
    Delete,
    Add(Form<()>),
}

/// The row under the cursor.
enum Row<'a> {
    Rule(&'a RuleInfo),
    Offer(&'a RuleCandidate),
}

impl Rules {
    fn len(&self) -> usize {
        self.list.len() + self.offers.len()
    }

    fn row(&self, i: usize) -> Option<Row<'_>> {
        match self.list.get(i) {
            Some(r) => Some(Row::Rule(r)),
            None => self.offers.get(i - self.list.len()).map(Row::Offer),
        }
    }

    fn move_to(&mut self, i: isize) {
        let last = self.len().saturating_sub(1) as isize;
        self.cursor = i.clamp(0, last.max(0)) as usize;
    }
}

impl App {
    /// Loads the view's data unless it has it.
    pub(super) fn rules_enter(&mut self) {
        if !self.rules.loaded {
            self.rules_reload();
        }
    }

    pub(super) fn rules_reload(&mut self) {
        if let Err(e) = self.rules_load() {
            self.warn(format!("{e:#}"));
        }
    }

    fn rules_load(&mut self) -> Result<()> {
        self.store.reload_rules()?;
        let r = &mut self.rules;
        r.list = self.store.rule_list()?;
        r.offers = self.store.rule_candidates(OFFER_MIN, OFFER_SHARE)?;
        r.loaded = true;
        r.move_to(r.cursor as isize);
        Ok(())
    }

    /// Rules may have changed (a tag renamed, a rule offered in review).
    pub(super) fn rules_changed(&mut self) {
        self.rules.loaded = false;
        if self.view == View::Rules {
            self.rules_reload();
        }
    }

    pub(super) fn rules_paste(&mut self, text: &str) {
        if let Mode::Add(form) = &mut self.rules.mode {
            form.fields[form.focus].input.insert(text.trim());
        }
    }

    pub(super) fn rules_key(&mut self, k: KeyEvent) -> bool {
        match std::mem::take(&mut self.rules.mode) {
            Mode::Normal => return self.rules_normal_key(k),
            Mode::Delete => {
                if k.code == KeyCode::Char('d') {
                    self.rules_remove();
                } else {
                    self.say("");
                }
            }
            Mode::Add(mut form) => match self.form_input(&mut form, k) {
                FormKey::Cancel => {}
                FormKey::Submit => {
                    if let Err(e) = self.rules_add(&form.text(0), &form.text(1)) {
                        self.warn(format!("{e:#}"));
                        self.rules.mode = Mode::Add(form);
                    }
                }
                FormKey::Edit => self.rules.mode = Mode::Add(form),
            },
        }
        true
    }

    fn rules_normal_key(&mut self, k: KeyEvent) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let r = &mut self.rules;
        match k.code {
            KeyCode::Char('l') if ctrl => {
                self.rules_reload();
                self.say("reloaded");
            }
            _ if ctrl => return false,
            KeyCode::Char('j') | KeyCode::Down => r.move_to(r.cursor as isize + 1),
            KeyCode::Char('k') | KeyCode::Up => r.move_to(r.cursor as isize - 1),
            KeyCode::Char('g') | KeyCode::Home => r.move_to(0),
            KeyCode::Char('G') | KeyCode::End => r.move_to(isize::MAX),
            KeyCode::Char('a') => {
                r.mode = Mode::Add(Form::new(
                    (),
                    vec![Field::line("Host", ""), Field::tags("Tags", "")],
                ));
            }
            KeyCode::Char('d') => {
                let Some(Row::Rule(rule)) = r.row(r.cursor) else {
                    return true;
                };
                let what = format!("{} → #{}", rule.host, rule.tag);
                if rule.pinned {
                    self.warn(format!("{what} is pinned by [tags.rules] in the config"));
                } else {
                    r.mode = Mode::Delete;
                    self.say(format!("d again removes the rule {what}"));
                }
            }
            KeyCode::Enter => {
                let (host, tag) = match r.row(r.cursor) {
                    Some(Row::Rule(x)) => (x.host.clone(), x.tag.clone()),
                    Some(Row::Offer(x)) => (x.host.clone(), x.tag.clone()),
                    None => return true,
                };
                self.show_bookmarks(&format!("site:{host} #{tag}"));
            }
            KeyCode::Char('y') => {
                if let Some(Row::Offer(o)) = r.row(r.cursor) {
                    let (host, tag) = (o.host.clone(), o.tag.clone());
                    if let Err(e) = self.rules_add(&host, &tag) {
                        self.warn(format!("{e:#}"));
                    }
                }
            }
            KeyCode::Char('n') => {
                if let Some(Row::Offer(o)) = r.row(r.cursor) {
                    let (host, tag) = (o.host.clone(), o.tag.clone());
                    match self.store.decline_rule(&host, &tag) {
                        Ok(()) => {
                            self.rules_reload();
                            self.say(format!("won't offer {host} → #{tag} again"));
                        }
                        Err(e) => self.warn(format!("{e:#}")),
                    }
                }
            }
            _ => return false,
        }
        true
    }

    /// Saves a rule and runs the rules on its site's bookmarks.
    pub(super) fn rules_add(&mut self, host: &str, tags: &str) -> Result<()> {
        let tags: Vec<String> = tags.split_whitespace().map(String::from).collect();
        let added = self.store.add_rule(host, &tags)?;
        let mut msg = String::new();
        if !added.added.is_empty() {
            msg = format!("added rule {} → {}", added.host, hashed(&added.added));
            let s = self.store.apply_rules_on(&added.host)?;
            if s.pending > 0 {
                msg.push_str(&format!(
                    "; {} new suggestion{}",
                    s.pending,
                    plural_s(s.pending)
                ));
            }
            if s.tagged > 0 {
                msg.push_str(&format!(
                    "; tagged {} bookmark{}",
                    s.tagged,
                    plural_s(s.tagged)
                ));
            }
        }
        if !added.existing.is_empty() {
            if !msg.is_empty() {
                msg.push_str("; ");
            }
            msg.push_str(&format!(
                "{} already gets {}",
                added.host,
                hashed(&added.existing)
            ));
        }
        self.bookmarks_changed();
        self.tags_changed();
        self.rules_load()?;
        // Keep the cursor on the new rule.
        if let Some(i) =
            self.rules.list.iter().position(|r| {
                r.host == added.host && added.added.first().is_some_and(|t| *t == r.tag)
            })
        {
            self.rules.cursor = i;
        }
        self.say(msg);
        Ok(())
    }

    fn rules_remove(&mut self) {
        let Some(Row::Rule(rule)) = self.rules.row(self.rules.cursor) else {
            return;
        };
        let (host, tag) = (rule.host.clone(), rule.tag.clone());
        match self.store.remove_rule(&host, std::slice::from_ref(&tag)) {
            Ok(r) => {
                let mut msg = format!("removed rule {host} → #{tag}");
                if r.dropped > 0 {
                    msg.push_str(&format!(
                        "; dropped {} pending suggestion{}",
                        r.dropped,
                        plural_s(r.dropped)
                    ));
                }
                self.bookmarks_changed();
                self.rules_reload();
                self.say(msg);
            }
            Err(e) => self.warn(format!("{e:#}")),
        }
    }

    pub(super) fn rules_status(&self) -> Vec<Span<'static>> {
        let r = &self.rules;
        let mut s = format!("{} rule{}", r.list.len(), plural_s(r.list.len()));
        if !r.offers.is_empty() {
            s.push_str(&format!(", {} offered", r.offers.len()));
        }
        vec![Span::raw(s)]
    }

    pub(super) fn rules_hint(&self) -> Option<&'static str> {
        match &self.rules.mode {
            Mode::Normal | Mode::Delete => None,
            Mode::Add(form) => Some(form.hint()),
        }
    }

    pub(super) fn draw_rules(&mut self, f: &mut Frame, area: Rect) {
        let r = &mut self.rules;
        let mut lines: Vec<Line> = Vec::new();
        // Which line each row is on, for scrolling.
        let mut at = Vec::new();
        let host_width = r
            .list
            .iter()
            .map(|x| x.host.len())
            .chain(r.offers.iter().map(|x| x.host.len()))
            .max()
            .unwrap_or(0);
        let tag_width = r
            .list
            .iter()
            .map(|x| x.tag.len())
            .chain(r.offers.iter().map(|x| x.tag.len()))
            .max()
            .unwrap_or(0)
            + 1;
        let tag = Style::new().fg(Color::Cyan);
        let dim = Style::new().add_modifier(Modifier::DIM);
        if r.list.is_empty() {
            lines.push(Line::from(""));
            lines.push(
                Line::from("  No domain rules yet: a adds one (bookmarks on a host get its tags).")
                    .dim(),
            );
        }
        for (i, rule) in r.list.iter().enumerate() {
            let mut count = format!("{} bookmark{}", rule.tagged, plural_s(rule.tagged));
            if rule.pending > 0 {
                count.push_str(&format!(" ({} pending)", rule.pending));
            }
            let mut spans = vec![
                Span::raw(format!("  {:host_width$}  ", rule.host)),
                Span::styled(format!("{:tag_width$}", format!("#{}", rule.tag)), tag),
                Span::raw(format!("  {count}")),
            ];
            if rule.pinned {
                spans.push(Span::styled("  pinned", Style::new().fg(Color::Magenta)));
            }
            at.push(lines.len());
            lines.push(selected(Line::from(spans), i == r.cursor));
        }
        if !r.offers.is_empty() {
            lines.push(Line::from(""));
            lines.push(
                Line::from("  Offered: sites you tagged the same way (y creates, n declines)")
                    .bold(),
            );
            for (j, o) in r.offers.iter().enumerate() {
                let spans = vec![
                    Span::raw(format!("  {:host_width$}  ", o.host)),
                    Span::styled(format!("{:tag_width$}", format!("#{}", o.tag)), tag),
                    Span::styled(
                        format!("  on {} of {} tagged bookmarks", o.count, o.of),
                        dim,
                    ),
                ];
                at.push(lines.len());
                lines.push(selected(Line::from(spans), r.list.len() + j == r.cursor));
            }
        }
        let h = area.height as usize;
        let line = at.get(r.cursor).copied().unwrap_or(0);
        if line < r.offset {
            r.offset = line;
        } else if line >= r.offset + h {
            r.offset = line + 1 - h;
        }
        f.render_widget(Paragraph::new(lines).scroll((r.offset as u16, 0)), area);
        if let Mode::Add(form) = &self.rules.mode {
            draw_form(f, area, "Add a rule", form);
        }
    }
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

fn plural_s(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}
