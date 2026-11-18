//! A text field: the search and command lines, form fields, and tag entry with completion.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// What a key did to a field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edited {
    /// The text changed.
    Changed,
    /// Only the cursor moved, or nothing happened.
    Moved,
    /// Not an editing key: the caller handles it.
    Ignored,
}

/// One field's text and cursor (a byte offset on a char boundary).
#[derive(Debug, Clone, Default)]
pub struct Input {
    text: String,
    cursor: usize,
    /// Accepts newlines (Alt-Enter); pasted newlines become spaces otherwise.
    multiline: bool,
    completion: Option<Completion>,
}

/// A Tab-completion in progress: further Tabs cycle through the candidates.
#[derive(Debug, Clone)]
struct Completion {
    /// Byte offset where the completed word starts (after any `+`, `-`, or `#`).
    start: usize,
    candidates: Vec<String>,
    index: usize,
}

impl Input {
    pub fn new(text: &str) -> Self {
        Input {
            text: text.to_string(),
            cursor: text.len(),
            multiline: false,
            completion: None,
        }
    }

    pub fn multiline(text: &str) -> Self {
        Input {
            multiline: true,
            ..Input::new(text)
        }
    }

    pub fn is_multiline(&self) -> bool {
        self.multiline
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// The cursor's column: characters before it on its line.
    pub fn cursor_col(&self) -> usize {
        let line_start = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
        self.text[line_start..self.cursor].chars().count()
    }

    /// The cursor's line, counting from 0.
    pub fn cursor_line(&self) -> usize {
        self.text[..self.cursor].matches('\n').count()
    }

    /// Candidates of the completion in progress, the current one first.
    pub fn completions(&self) -> Option<(&[String], usize)> {
        self.completion
            .as_ref()
            .map(|c| (c.candidates.as_slice(), c.index))
    }

    pub fn insert(&mut self, s: &str) {
        self.completion = None;
        let s = if self.multiline {
            s.replace("\r\n", "\n").replace('\r', "\n")
        } else {
            s.split(['\r', '\n']).collect::<Vec<_>>().join(" ")
        };
        self.text.insert_str(self.cursor, &s);
        self.cursor += s.len();
    }

    /// Handles an editing key: characters, Backspace and Delete, cursor moves, and the readline
    /// keys Ctrl-A, Ctrl-E, Ctrl-U, Ctrl-K, Ctrl-W.
    pub fn key(&mut self, k: KeyEvent) -> Edited {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let before = (self.text.len(), self.cursor);
        match k.code {
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.text.len(),
            KeyCode::Char('u') if ctrl => {
                self.text.replace_range(..self.cursor, "");
                self.cursor = 0;
            }
            KeyCode::Char('k') if ctrl => self.text.truncate(self.cursor),
            KeyCode::Char('w') if ctrl => {
                let head = self.text[..self.cursor].trim_end();
                let start = head.rfind(char::is_whitespace).map_or(0, |i| i + 1);
                self.text.replace_range(start..self.cursor, "");
                self.cursor = start;
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                self.insert(c.encode_utf8(&mut [0; 4]));
                return Edited::Changed;
            }
            KeyCode::Enter if alt && self.multiline => {
                self.insert("\n");
                return Edited::Changed;
            }
            KeyCode::Backspace => {
                if let Some(c) = self.text[..self.cursor].chars().next_back() {
                    self.cursor -= c.len_utf8();
                    self.text.remove(self.cursor);
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.text.len() {
                    self.text.remove(self.cursor);
                }
            }
            KeyCode::Left => {
                if let Some(c) = self.text[..self.cursor].chars().next_back() {
                    self.cursor -= c.len_utf8();
                }
            }
            KeyCode::Right => {
                if let Some(c) = self.text[self.cursor..].chars().next() {
                    self.cursor += c.len_utf8();
                }
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.text.len(),
            _ => return Edited::Ignored,
        }
        self.completion = None;
        if self.text.len() != before.0 {
            Edited::Changed
        } else {
            Edited::Moved
        }
    }

    /// Tab: completes the tag name before the cursor from `names` (in their order), or with a
    /// completion in progress moves to the next candidate. Returns false if nothing matches.
    pub fn complete(&mut self, names: &[String]) -> bool {
        if let Some(c) = &mut self.completion {
            c.index = (c.index + 1) % c.candidates.len();
            let (start, next) = (c.start, c.candidates[c.index].clone());
            let end = self.word_end(start);
            self.text.replace_range(start..end, &next);
            self.cursor = start + next.len();
            return true;
        }
        let head = &self.text[..self.cursor];
        let mut start = head.rfind(char::is_whitespace).map_or(0, |i| i + 1);
        start += self.text[start..]
            .chars()
            .take_while(|c| matches!(c, '+' | '-' | '#'))
            .map(char::len_utf8)
            .sum::<usize>();
        let prefix = shlv::store::normalize_tag(&self.text[start..self.cursor]);
        let candidates: Vec<String> = names
            .iter()
            .filter(|n| n.starts_with(&prefix) && **n != prefix)
            .take(50)
            .cloned()
            .collect();
        let Some(first) = candidates.first().cloned() else {
            return false;
        };
        let end = self.word_end(start);
        self.text.replace_range(start..end, &first);
        self.cursor = start + first.len();
        self.completion = Some(Completion {
            start,
            candidates,
            index: 0,
        });
        true
    }

    /// Where the word starting at `start` ends.
    fn word_end(&self, start: usize) -> usize {
        self.text[start..]
            .find(char::is_whitespace)
            .map_or(self.text.len(), |i| start + i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn edits_and_moves() {
        let mut i = Input::new("héllo");
        assert_eq!(i.key(key(KeyCode::Left)), Edited::Moved);
        assert_eq!(i.key(key(KeyCode::Backspace)), Edited::Changed);
        assert_eq!(i.text(), "hélo");
        i.key(key(KeyCode::Home));
        i.key(key(KeyCode::Char('>')));
        assert_eq!(i.text(), ">hélo");
        i.key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        i.insert(" big world");
        i.key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(i.text(), ">hélo big ");
        assert_eq!(i.cursor_col(), 10);
        i.insert("a\nb");
        assert_eq!(i.text(), ">hélo big a b");
    }

    #[test]
    fn completes_and_cycles() {
        let names: Vec<String> = ["rust", "rustlang", "web"].map(String::from).into();
        let mut i = Input::new("web +ru");
        assert!(i.complete(&names));
        assert_eq!(i.text(), "web +rust");
        assert!(i.complete(&names));
        assert_eq!(i.text(), "web +rustlang");
        assert!(i.complete(&names));
        assert_eq!(i.text(), "web +rust");
        i.insert(" ");
        assert!(
            i.complete(&names),
            "an empty word completes to the first name"
        );
        assert_eq!(i.text(), "web +rust rust");
        let mut i = Input::new("#zz");
        assert!(!i.complete(&names));
    }
}
