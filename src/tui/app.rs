//! The application: views, the help overlay, the command line, the status line, and the
//! worker's reports. Key handling and drawing are plain functions of this state, so tests drive
//! it with key events and render it to a `TestBackend`.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use shlv::Store;
use shlv::config::Config;

use super::bookmarks::Bookmarks;
use super::input::{Edited, Input};
use super::rules::Rules;
use super::worker::{Job, OpenStore, Report, Worker};

/// The views, switched with `1`-`6` or `:name`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Bookmarks,
    Review,
    Tags,
    Rules,
    Bootstrap,
    Status,
}

impl View {
    pub const ALL: [View; 6] = [
        View::Bookmarks,
        View::Review,
        View::Tags,
        View::Rules,
        View::Bootstrap,
        View::Status,
    ];

    pub fn name(self) -> &'static str {
        match self {
            View::Bookmarks => "bookmarks",
            View::Review => "review",
            View::Tags => "tags",
            View::Rules => "rules",
            View::Bootstrap => "bootstrap",
            View::Status => "status",
        }
    }

    fn title(self) -> &'static str {
        match self {
            View::Bookmarks => "Bookmarks",
            View::Review => "Review",
            View::Tags => "Tags",
            View::Rules => "Rules",
            View::Bootstrap => "Bootstrap",
            View::Status => "Status",
        }
    }

    /// What a placeholder view will hold, and the task that builds it.
    fn placeholder(self) -> &'static str {
        match self {
            View::Bookmarks => "",
            View::Review => "Review batches and the suggestions queue: task 044.",
            View::Tags => "The tag list, edits, proposals, health, and back-fill: task 045.",
            View::Rules => "",
            View::Bootstrap => "Interests, tag seed, and tag draft: task 046.",
            View::Status => "Classifier, background jobs, and config: task 046.",
        }
    }

    fn by_name(name: &str) -> Option<View> {
        View::ALL
            .into_iter()
            .find(|v| v.name() == name || (name.len() >= 2 && v.name().starts_with(name)))
    }
}

/// How a message on the status line reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Info,
    Error,
}

/// A job sent to the worker and not finished.
struct Running {
    job: Job,
    step: String,
}

pub struct App {
    pub store: Store,
    pub config: Config,
    pub view: View,
    pub bookmarks: Bookmarks,
    pub rules: Rules,
    help: bool,
    command: Option<Input>,
    message: Option<(String, Tone)>,
    worker: Worker,
    jobs: BTreeMap<u64, Running>,
    /// Jobs sent in the current batch and how many of them finished, for "3/10".
    batch: (usize, usize),
    pub quit: bool,
    /// A URL to open: the event loop suspends the screen for the browser.
    open: Option<String>,
    /// Tag names and aliases for completion, most used first; loaded when first needed.
    tag_names: Option<Vec<String>>,
}

impl App {
    pub fn new(store: Store, config: Config, worker: OpenStore) -> Result<App> {
        let mut app = App {
            store,
            config,
            view: View::Bookmarks,
            bookmarks: Bookmarks::default(),
            rules: Rules::default(),
            help: false,
            command: None,
            message: None,
            worker: Worker::new(worker),
            jobs: BTreeMap::new(),
            batch: (0, 0),
            quit: false,
            open: None,
            tag_names: None,
        };
        app.reload()?;
        Ok(app)
    }

    pub fn say(&mut self, text: impl Into<String>) {
        self.message = Some((text.into(), Tone::Info));
    }

    pub fn warn(&mut self, text: impl Into<String>) {
        self.message = Some((text.into(), Tone::Error));
    }

    #[cfg(test)]
    pub fn message(&self) -> Option<&(String, Tone)> {
        self.message.as_ref()
    }

    /// The URL to open, if a key asked for one.
    pub fn take_open(&mut self) -> Option<String> {
        self.open.take()
    }

    pub(super) fn request_open(&mut self, url: String) {
        self.open = Some(url);
    }

    /// Whether worker jobs are running (the event loop then also waits for their reports).
    pub fn busy(&self) -> bool {
        !self.jobs.is_empty()
    }

    #[cfg_attr(not(feature = "fetch"), allow(dead_code))]
    pub(super) fn send_job(&mut self, job: Job) {
        if self.jobs.is_empty() {
            self.batch = (0, 0);
        }
        let step = format!("{} {}", job.verb(), job.id());
        let n = self.worker.send(job.clone());
        self.jobs.insert(n, Running { job, step });
        self.batch.0 += 1;
    }

    /// Applies the worker's reports, waiting up to `wait` for the first.
    pub fn drain_reports(&mut self, wait: Duration) {
        let mut wait = wait;
        while let Some(r) = self.worker.recv(wait) {
            wait = Duration::ZERO;
            self.report(r);
        }
    }

    /// Waits for every running job to finish, for tests.
    #[cfg(all(test, feature = "fetch"))]
    pub fn finish_jobs(&mut self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while self.busy() && std::time::Instant::now() < deadline {
            self.drain_reports(Duration::from_millis(50));
        }
        assert!(!self.busy(), "jobs still running");
    }

    fn report(&mut self, r: Report) {
        let Some(running) = self.jobs.get_mut(&r.job()) else {
            return;
        };
        match r {
            Report::Progress { text, .. } => running.step = text,
            Report::Done { job, id, text } => {
                self.jobs.remove(&job);
                self.batch.1 += 1;
                self.bookmarks.forget(&[id]);
                self.say(format!("{id}: {text}"));
            }
            Report::Failed { job, id, error } => {
                let verb = self.jobs.remove(&job).map_or("", |r| r.job.verb());
                self.batch.1 += 1;
                self.warn(format!("{verb} {id} failed: {error}"));
            }
        }
    }

    /// Tag names (most used first) and then aliases, for completion.
    pub(super) fn tag_names(&mut self) -> &[String] {
        if self.tag_names.is_none() {
            let names = match self.store.tags(shlv::TagSort::Count) {
                Ok(tags) => {
                    let mut v: Vec<String> = tags.iter().map(|t| t.name.clone()).collect();
                    v.extend(tags.into_iter().flat_map(|t| t.aliases));
                    v
                }
                Err(e) => {
                    self.warn(format!("cannot list tags: {e:#}"));
                    Vec::new()
                }
            };
            self.tag_names = Some(names);
        }
        self.tag_names.as_deref().unwrap_or_default()
    }

    /// Tags may have changed: completion reloads them.
    pub(super) fn tags_changed(&mut self) {
        self.tag_names = None;
        self.rules_changed();
    }

    /// Shows a view, loading what it lists if it hasn't yet.
    pub(super) fn switch(&mut self, v: View) {
        self.view = v;
        self.message = None;
        if v == View::Rules {
            self.rules_enter();
        }
    }

    /// A view's hint for the bottom line while it is in the middle of something.
    fn view_hint(&self) -> Option<&'static str> {
        match self.view {
            View::Rules => self.rules_hint(),
            _ => None,
        }
    }

    /// Handles a terminal event.
    pub fn event(&mut self, ev: Event) {
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => self.key(k),
            Event::Paste(text) => self.paste(&text),
            _ => {}
        }
    }

    fn paste(&mut self, text: &str) {
        if let Some(c) = &mut self.command {
            c.insert(text);
        } else {
            match self.view {
                View::Bookmarks => self.bookmarks_paste(text),
                View::Rules => self.rules_paste(text),
                _ => {}
            }
        }
    }

    pub fn key(&mut self, k: KeyEvent) {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.help {
            self.help = false;
            return;
        }
        if self.command.is_some() {
            self.command_key(k);
            return;
        }
        // A view in the middle of something (typing a search, a form) takes every key.
        let taken = match self.view {
            View::Bookmarks => self.bookmarks_key(k),
            View::Rules => self.rules_key(k),
            _ => false,
        };
        if taken {
            return;
        }
        if !k.modifiers.difference(KeyModifiers::SHIFT).is_empty() {
            return;
        }
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char(':') => self.command = Some(Input::default()),
            KeyCode::Char(c @ '1'..='6') => self.switch(View::ALL[c as usize - '1' as usize]),
            _ => {}
        }
    }

    fn command_key(&mut self, k: KeyEvent) {
        let Some(input) = &mut self.command else {
            return;
        };
        match k.code {
            KeyCode::Esc => self.command = None,
            KeyCode::Enter => {
                let text = input.text().trim().to_string();
                self.command = None;
                self.run_command(&text);
            }
            KeyCode::Backspace if input.text().is_empty() => self.command = None,
            _ => {
                input.key(k);
            }
        }
    }

    /// Runs a `:` command.
    pub fn run_command(&mut self, text: &str) {
        let mut words = text.split_whitespace();
        let Some(cmd) = words.next() else {
            return;
        };
        match cmd {
            "q" | "q!" | "quit" | "qa" | "wq" | "x" => self.quit = true,
            "help" | "h" => self.help = true,
            name => match View::by_name(name) {
                Some(v) => self.switch(v),
                None => self.warn(format!("unknown command :{text} (? lists the keys)")),
            },
        }
    }

    pub fn draw(&mut self, f: &mut Frame) {
        let [header, body, status, prompt] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(f.area());
        self.draw_header(f, header);
        match self.view {
            View::Bookmarks => self.draw_bookmarks(f, body),
            View::Rules => self.draw_rules(f, body),
            v => f.render_widget(
                Paragraph::new(vec![
                    Line::from(""),
                    Line::from(format!("  {}", v.placeholder())).dim(),
                ]),
                body,
            ),
        }
        self.draw_status(f, status);
        self.draw_prompt(f, prompt);
        if self.help {
            self.draw_help(f);
        }
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let mut spans = Vec::new();
        for (i, v) in View::ALL.into_iter().enumerate() {
            let label = format!(" {} {} ", i + 1, v.title());
            spans.push(if v == self.view {
                Span::styled(label, Style::new().add_modifier(Modifier::REVERSED))
            } else {
                Span::raw(label)
            });
        }
        f.render_widget(Line::from(spans), area);
    }

    fn draw_status(&self, f: &mut Frame, area: Rect) {
        let mut left = vec![Span::styled(
            format!(" {} ", self.view.title()),
            Style::new().add_modifier(Modifier::BOLD),
        )];
        match self.view {
            View::Bookmarks => left.extend(self.bookmarks_status()),
            View::Rules => left.extend(self.rules_status()),
            _ => {}
        }
        let mut right = Vec::new();
        if let Some(r) = self.jobs.values().next() {
            let (sent, done) = self.batch;
            let count = if sent > 1 {
                format!(" {}/{sent}", done + 1)
            } else {
                String::new()
            };
            right.push(Span::styled(
                format!("⟳{count} {} ", r.step),
                Style::new().fg(Color::Yellow),
            ));
        }
        right.push(Span::raw("? help "));
        let right = Line::from(right);
        let width = right.width() as u16;
        let [l, r] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(width)]).areas(area);
        let bar = Style::new().add_modifier(Modifier::REVERSED);
        f.render_widget(Line::from(left).style(bar), l);
        f.render_widget(right.style(bar), r);
    }

    fn draw_prompt(&self, f: &mut Frame, area: Rect) {
        if let Some(input) = &self.command {
            draw_line_input(f, area, ":", input);
        } else if self.view == View::Bookmarks && self.draw_bookmarks_prompt(f, area) {
            // The view drew its prompt or hint.
        } else if let Some(hint) = self.view_hint() {
            f.render_widget(Line::from(hint).dim(), area);
        } else if let Some((text, tone)) = &self.message {
            let style = match tone {
                Tone::Info => Style::new(),
                Tone::Error => Style::new().fg(Color::Red),
            };
            f.render_widget(Line::styled(text.as_str(), style), area);
        }
    }

    fn draw_help(&self, f: &mut Frame) {
        let mut lines = Vec::new();
        let mut section = |title: &str, keys: &[(&str, &str)]| {
            lines.push(Line::from(title.to_string()).bold());
            for (k, what) in keys {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {k:<14}"), Style::new().fg(Color::Cyan)),
                    Span::raw(what.to_string()),
                ]));
            }
            lines.push(Line::from(""));
        };
        section(
            "Everywhere",
            &[
                ("1-6", "switch view"),
                (":", "command line (:q, :bookmarks, :review, …)"),
                ("?", "this help (any key closes it)"),
                ("q, Ctrl-C", "quit"),
            ],
        );
        match self.view {
            View::Bookmarks => section(self.view.title(), &super::bookmarks::keys()),
            View::Rules => section(self.view.title(), &super::rules::keys()),
            v => section(v.title(), &[("", v.placeholder())]),
        }
        lines.pop();
        let height = (lines.len() as u16 + 2).min(f.area().height);
        let width = 70.min(f.area().width);
        let area = centered(f.area(), width, height);
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::bordered().title(" Keys ")),
            area,
        );
    }
}

/// A one-line prompt: `label` then the text, with the cursor placed in it.
pub fn draw_line_input(f: &mut Frame, area: Rect, label: &str, input: &Input) {
    let label_width = Line::from(label).width() as u16;
    let col = input.cursor_col() as u16;
    // Scroll a long line so the cursor stays visible.
    let room = area.width.saturating_sub(label_width + 1);
    let skip = col.saturating_sub(room);
    let text: String = input.text().chars().skip(skip as usize).collect();
    f.render_widget(
        Line::from(vec![Span::raw(label.to_string()), Span::raw(text)]),
        area,
    );
    f.set_cursor_position((area.x + label_width + col - skip, area.y));
}

/// A `width` × `height` rectangle centered in `area`.
pub fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// Whether a key edited `input` (so the caller is done with it).
pub fn edited(input: &mut Input, k: KeyEvent) -> bool {
    input.key(k) != Edited::Ignored
}
