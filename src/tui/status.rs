//! View 6, Status (task 046): where the config and database are, the classifier's state and
//! measurements, and the background jobs, with keys to start and cancel them.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::app::App;
use super::worker::Job;

pub fn keys() -> Vec<(&'static str, &'static str)> {
    vec![
        ("j k", "select a job"),
        ("t", "train the classifier"),
        ("e", "measure it (model eval)"),
        ("F", "fetch titles for bookmarks without one"),
        ("S", "summarize bookmarks without a summary (asks first)"),
        ("x", "cancel the selected job"),
        ("X", "cancel every job"),
        ("Ctrl-l", "reload"),
    ]
}

#[derive(Default)]
pub struct Status {
    /// `model status`, read when the view is shown.
    classifier: Option<Vec<String>>,
    /// `model eval`'s report, once run.
    pub(super) eval: Option<Vec<String>>,
    cursor: usize,
}

impl App {
    pub(super) fn status_enter(&mut self) {
        self.status.classifier = Some(self.classifier_lines());
    }

    fn classifier_lines(&mut self) -> Vec<String> {
        let mut buf = Vec::new();
        let r = crate::model_cmd::run(
            &mut self.store,
            &self.config,
            crate::model_cmd::ModelCmd::Status,
            &mut buf,
        );
        match r {
            Ok(_) => String::from_utf8_lossy(&buf)
                .lines()
                .map(String::from)
                .collect(),
            Err(e) => vec![format!("{e:#}")],
        }
    }

    pub(super) fn status_key(&mut self, k: KeyEvent) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && k.code == KeyCode::Char('l') {
            self.status_enter();
            self.say("reloaded");
            return true;
        }
        if ctrl {
            return false;
        }
        let n = self.job_list().len();
        match k.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.status.cursor = (self.status.cursor + 1).min(n.saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.status.cursor = self.status.cursor.saturating_sub(1);
            }
            KeyCode::Char('x') => match self.job_list().get(self.status.cursor) {
                Some(&(n, _)) => self.cancel_job(n),
                None => self.say("no jobs"),
            },
            KeyCode::Char('X') => self.cancel_all(),
            KeyCode::Char('t') => self.run_command("train"),
            KeyCode::Char('e') => self.run_command("eval"),
            KeyCode::Char('F') => self.run_command("fetch all"),
            KeyCode::Char('S') => self.run_command("summarize all"),
            _ => return false,
        }
        true
    }

    /// A Status job finished.
    pub(super) fn status_report(&mut self, job: Job, result: Result<Vec<String>, String>) {
        match (job, result) {
            (Job::Eval, Ok(lines)) => {
                self.status.eval = Some(lines);
                self.say("measured the classifier");
            }
            (job, Err(e)) => self.warn(format!("{} failed: {e}", job.describe())),
            _ => {}
        }
    }

    pub(super) fn status_status(&self) -> Vec<Span<'static>> {
        let n = self.job_list().len();
        vec![Span::raw(match n {
            0 => "no jobs".to_string(),
            1 => "1 job".to_string(),
            n => format!("{n} jobs"),
        })]
    }

    pub(super) fn draw_status_view(&mut self, f: &mut Frame, area: Rect) {
        let label = |s: &str| Span::styled(format!("  {s:<12}"), Style::new().bold());
        let mut l = vec![Line::from("")];
        let path = |p: Option<&std::path::Path>| {
            p.map_or("(none)".to_string(), |p| p.display().to_string())
        };
        l.push(Line::from(vec![
            label("config"),
            Span::raw(path(self.config_path.as_deref())),
        ]));
        l.push(Line::from(vec![
            label("database"),
            Span::raw(path(self.store.path().as_deref())),
        ]));
        l.push(Line::from(""));
        l.push(Line::from("  Classifier").bold());
        for line in self.status.classifier.iter().flatten() {
            l.push(Line::from(format!("  {line}")));
        }
        if let Some(eval) = &self.status.eval {
            l.push(Line::from(""));
            l.push(Line::from("  Measured (model eval)").bold());
            for line in eval {
                l.push(Line::from(format!("  {line}")));
            }
        }
        l.push(Line::from(""));
        l.push(Line::from("  Jobs").bold());
        let jobs = self.job_list();
        if jobs.is_empty() {
            l.push(Line::from("  none running").dim());
        }
        for (i, (n, step)) in jobs.iter().enumerate() {
            let mut line = Line::from(vec![
                Span::styled(format!("  {n:>4}  "), Style::new().fg(Color::Yellow)),
                Span::raw(step.clone()),
            ]);
            if i == self.status.cursor {
                line = line.style(Style::new().add_modifier(Modifier::REVERSED));
            }
            l.push(line);
        }
        f.render_widget(Paragraph::new(l).wrap(Wrap { trim: false }), area);
    }
}
