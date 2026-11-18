//! A small form in a box over a view: labeled one-line fields (tag fields complete with Tab) and
//! multi-line ones. Views keep their own `Form<Kind>`, `Kind` saying what submitting does.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use super::app::{App, centered, draw_line_input, edited};
use super::input::Input;

pub struct Form<K> {
    pub kind: K,
    pub fields: Vec<Field>,
    pub focus: usize,
}

pub struct Field {
    pub label: &'static str,
    pub input: Input,
    /// Tab completes tag names.
    pub tags: bool,
}

impl Field {
    pub fn line(label: &'static str, text: &str) -> Field {
        Field {
            label,
            input: Input::new(text),
            tags: false,
        }
    }

    pub fn tags(label: &'static str, text: &str) -> Field {
        Field {
            tags: true,
            ..Field::line(label, text)
        }
    }

    pub fn multiline(label: &'static str, text: &str) -> Field {
        Field {
            label,
            input: Input::multiline(text),
            tags: false,
        }
    }

    fn height(&self, focused: bool) -> u16 {
        if self.input.is_multiline() {
            5
        } else if self.tags && focused {
            2
        } else {
            1
        }
    }
}

/// What a key did to a form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormKey {
    /// Still editing.
    Edit,
    Submit,
    Cancel,
}

impl<K> Form<K> {
    pub fn new(kind: K, fields: Vec<Field>) -> Form<K> {
        Form {
            kind,
            fields,
            focus: 0,
        }
    }

    /// Field `i`'s text.
    pub fn text(&self, i: usize) -> String {
        self.fields[i].input.text().to_string()
    }

    /// The hint for the bottom line.
    pub fn hint(&self) -> &'static str {
        let field = &self.fields[self.focus];
        if field.input.is_multiline() {
            "Enter saves · Alt-Enter new line · ↑↓ field · Esc cancels"
        } else if field.tags {
            "Enter saves · Tab completes · ↑↓ field · Esc cancels"
        } else if self.fields.len() > 1 {
            "Enter saves · Tab next field · Esc cancels"
        } else {
            "Enter saves · Esc cancels"
        }
    }
}

impl App {
    /// Handles a key in a form: moving between fields, completing tag names, editing.
    pub(super) fn form_input<K>(&mut self, form: &mut Form<K>, k: KeyEvent) -> FormKey {
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let n = form.fields.len();
        match k.code {
            KeyCode::Esc => return FormKey::Cancel,
            KeyCode::Enter if !alt => return FormKey::Submit,
            KeyCode::Down => form.focus = (form.focus + 1) % n,
            KeyCode::Up | KeyCode::BackTab => form.focus = (form.focus + n - 1) % n,
            KeyCode::Tab if form.fields[form.focus].tags => {
                let names = self.tag_names().to_vec();
                if !form.fields[form.focus].input.complete(&names) {
                    self.say("no tag starts that way");
                }
            }
            KeyCode::Tab => form.focus = (form.focus + 1) % n,
            _ => {
                edited(&mut form.fields[form.focus].input, k);
            }
        }
        FormKey::Edit
    }
}

/// Draws `form` in a box centered in `area`.
pub fn draw_form<K>(f: &mut Frame, area: Rect, title: &str, form: &Form<K>) {
    let heights: Vec<u16> = form
        .fields
        .iter()
        .enumerate()
        .map(|(i, fl)| fl.height(i == form.focus))
        .collect();
    let width = 76.min(area.width.saturating_sub(2)).max(20);
    let box_area = centered(area, width, heights.iter().sum::<u16>() + 2);
    f.render_widget(Clear, box_area);
    let block = Block::bordered().title(format!(" {title} "));
    let inner = block.inner(box_area);
    f.render_widget(block, box_area);
    let rows = Layout::vertical(heights.iter().map(|&h| Constraint::Length(h))).split(inner);
    let label_width = form
        .fields
        .iter()
        .map(|fl| fl.label.len())
        .max()
        .unwrap_or(0)
        + 2;
    for (i, (field, &row)) in form.fields.iter().zip(rows.iter()).enumerate() {
        let focused = i == form.focus;
        let label = format!("{:<label_width$}", format!("{}:", field.label));
        let label_style = if focused {
            Style::new().bold()
        } else {
            Style::new().add_modifier(Modifier::DIM)
        };
        if field.input.is_multiline() {
            // The label on its own line, then the text, scrolled to keep the cursor in view.
            let [head, body] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(row);
            f.render_widget(Line::styled(label, label_style), head);
            let line = field.input.cursor_line() as u16;
            let scroll = line.saturating_sub(body.height.saturating_sub(1));
            f.render_widget(
                Paragraph::new(field.input.text().to_string()).scroll((scroll, 0)),
                body,
            );
            if focused {
                let col = (field.input.cursor_col() as u16).min(body.width.saturating_sub(1));
                f.set_cursor_position((body.x + col, body.y + line - scroll));
            }
            continue;
        }
        let [line_area, rest] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(row);
        let [l, input_area] =
            Layout::horizontal([Constraint::Length(label_width as u16), Constraint::Min(1)])
                .areas(line_area);
        f.render_widget(Line::styled(label, label_style), l);
        if focused {
            draw_line_input(f, input_area, "", &field.input);
        } else {
            f.render_widget(Line::from(field.input.text().to_string()), input_area);
        }
        if focused && field.tags && rest.height > 0 {
            let hint = match field.input.completions() {
                Some((names, i)) => {
                    let mut spans = Vec::new();
                    for (j, n) in names.iter().enumerate() {
                        let style = if j == i {
                            Style::new()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::REVERSED)
                        } else {
                            Style::new().fg(Color::Cyan)
                        };
                        spans.push(Span::styled(n.clone(), style));
                        spans.push(Span::raw(" "));
                    }
                    Line::from(spans)
                }
                None => Line::from(format!("{:label_width$}Tab completes", "")).dim(),
            };
            f.render_widget(hint, rest);
        }
    }
}
