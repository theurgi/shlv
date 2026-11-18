//! Going through a tag's back-fill candidates one key each (as `suggest --tag --pick`): Review
//! uses it for tags a batch added, Tags for any canonical tag.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use shlv::BackfillCandidate;

use super::app::App;

pub struct Picker {
    pub tag: String,
    candidates: Vec<BackfillCandidate>,
    said: Vec<Option<&'static str>>,
    cursor: usize,
}

/// Whether the picker is done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Picked {
    Going,
    Done,
}

impl Picker {
    pub fn new(tag: String, candidates: Vec<BackfillCandidate>) -> Picker {
        Picker {
            said: vec![None; candidates.len()],
            tag,
            candidates,
            cursor: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    pub fn len(&self) -> usize {
        self.candidates.len()
    }

    /// `#tag: 2 accepted, 1 rejected, 3 still pending`.
    pub fn summary(&self) -> String {
        let count = |w: &str| self.said.iter().filter(|s| **s == Some(w)).count();
        let (a, r) = (count("accepted"), count("rejected"));
        let left = self.said.len() - a - r;
        format!(
            "#{}: {a} accepted, {r} rejected, {left} still pending",
            self.tag
        )
    }

    /// The candidates, one line each, reviewed bookmarks first.
    pub fn lines(&self) -> Vec<Line<'static>> {
        let mut l = Vec::new();
        for (i, c) in self.candidates.iter().enumerate() {
            let s = &c.suggestion;
            let title = s.title.as_deref().unwrap_or(&s.url);
            let mut why = s.source.as_str().to_string();
            if let Some(p) = s.confidence {
                why.push_str(&format!(" {p:.2}"));
            }
            if c.reviewed {
                why.push_str(", reviewed");
            }
            let mut spans = vec![
                Span::styled(format!("  {:>6}  ", s.id), Style::new().fg(Color::Yellow)),
                Span::raw(title.split_whitespace().collect::<Vec<_>>().join(" ")),
                Span::raw(format!("  ({why})")).dim(),
            ];
            if let Some(said) = self.said[i] {
                spans.push(Span::raw(format!("  {said}")).fg(Color::Green));
            }
            let mut line = Line::from(spans);
            if i == self.cursor {
                line = line.style(Style::new().add_modifier(Modifier::REVERSED));
            }
            l.push(line);
        }
        l
    }

    /// The line the cursor is on, for scrolling.
    pub fn cursor(&self) -> usize {
        self.cursor
    }
}

impl App {
    /// A tag's candidates after offering it ([`super::worker::Job::Backfill`]): on `scope`, or
    /// everywhere.
    pub(super) fn picker(&self, tag: &str, scope: Option<&[i64]>) -> Result<Picker> {
        let mut c = self.store.backfill_candidates(tag, scope)?;
        for x in &mut c {
            x.reviewed = x.reviewed || self.store.is_reviewed(x.suggestion.id)?;
        }
        Ok(Picker::new(tag.to_string(), c))
    }

    /// `y` (or Enter) accepts the tag for the candidate, `n` rejects it, `s` skips, `o` opens,
    /// `q` or Esc stops; done once every candidate is decided.
    pub(super) fn pick_key(&mut self, p: &mut Picker, k: KeyEvent) -> Picked {
        let n = p.candidates.len();
        if n == 0 {
            return Picked::Done;
        }
        let id = p.candidates[p.cursor].suggestion.id;
        let tags = [p.tag.clone()];
        let answer = |p: &mut Picker, said| {
            p.said[p.cursor] = Some(said);
            p.cursor = (p.cursor + 1).min(n - 1);
        };
        match k.code {
            KeyCode::Char('j') | KeyCode::Down => p.cursor = (p.cursor + 1).min(n - 1),
            KeyCode::Char('k') | KeyCode::Up => p.cursor = p.cursor.saturating_sub(1),
            KeyCode::Char('y') | KeyCode::Enter => match self.store.accept(id, &tags) {
                Ok(_) => answer(p, "accepted"),
                Err(e) => self.warn(format!("{e:#}")),
            },
            KeyCode::Char('n') => match self.store.reject(id, &tags) {
                Ok(_) => answer(p, "rejected"),
                Err(e) => self.warn(format!("{e:#}")),
            },
            KeyCode::Char('s') => answer(p, "skipped"),
            KeyCode::Char('o') => {
                if let Ok(Some(b)) = self.store.get(id) {
                    self.request_open(b.url);
                }
            }
            KeyCode::Char('q') | KeyCode::Esc => return Picked::Done,
            _ => {}
        }
        self.bookmarks.forget(&[id]);
        if p.said.iter().all(Option::is_some) {
            Picked::Done
        } else {
            Picked::Going
        }
    }
}
