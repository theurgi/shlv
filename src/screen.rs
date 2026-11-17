//! The interactive screen (task 034, reused by `edit --pick`, `suggest --tag --pick`, `tag draft`, and `review`): a
//! plain terminal prompt loop, no TUI library.
//!
//! On a terminal, answers are single keypresses: the terminal is put in non-canonical mode with
//! `stty` (`-icanon -echo -isig`, so Ctrl-C arrives as a key rather than a signal and the
//! terminal is always restored), and back in its saved mode for text input and on drop. When
//! stdin isn't a terminal (or `stty` fails), each answer is a line and its first character is the
//! key: scripts and tests drive the screen that way. A command whose stdin carries data reads
//! keys from `/dev/tty` instead ([`Screen::open_tty`]).
//!
//! The screen draws on stderr, so stdout keeps only the command's result.

use std::fs::File;
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::process::{Command, Stdio};

use crate::Style;

/// A keypress (or answer line, without a terminal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    /// Ctrl-C.
    Interrupt,
    /// End of input (Ctrl-D, or the end of piped answers).
    Eof,
}

pub struct Screen {
    raw: Option<Raw>,
    input: Box<dyn BufRead>,
    out: io::Stderr,
    pub style: Style,
}

impl Screen {
    /// Opens the screen on stdin and stderr: single keys on a terminal, lines otherwise.
    pub fn open() -> Screen {
        let tty = io::stdin().is_terminal() && io::stderr().is_terminal();
        Screen {
            raw: if tty { Raw::enter(None) } else { None },
            input: Box::new(io::stdin().lock()),
            out: io::stderr(),
            style: Style::detect(io::stderr().is_terminal()),
        }
    }

    /// Opens the screen on the controlling terminal (`/dev/tty`) and stderr, for commands whose
    /// stdin carries data. Fails without a terminal.
    pub fn open_tty() -> io::Result<Screen> {
        let tty = File::open("/dev/tty")?;
        let raw = Raw::enter(Some(tty.try_clone()?));
        Ok(Screen {
            raw,
            input: Box::new(BufReader::new(tty)),
            out: io::stderr(),
            style: Style::detect(io::stderr().is_terminal()),
        })
    }

    /// `text` dimmed, when colors are on.
    pub fn dim(&self, text: &str) -> String {
        format!("{}{text}{}", self.style.dim, self.style.reset)
    }

    /// Whether answers are single keys (and lines can be redrawn).
    pub fn keys(&self) -> bool {
        self.raw.is_some()
    }

    /// Terminal width in columns (80 without a terminal).
    pub fn width(&self) -> usize {
        self.raw.as_ref().map_or(80, |r| r.cols)
    }

    /// Reads one key (or one answer line).
    pub fn key(&mut self) -> io::Result<Key> {
        self.out.flush()?;
        if self.raw.is_none() {
            let mut line = String::new();
            if self.input.read_line(&mut line)? == 0 {
                return Ok(Key::Eof);
            }
            return Ok(match line.trim().chars().next() {
                None => Key::Enter,
                Some(c) => Key::Char(c),
            });
        }
        let mut b = [0u8; 4];
        if self.input.read(&mut b[..1])? == 0 {
            return Ok(Key::Eof);
        }
        Ok(match b[0] {
            0x03 => Key::Interrupt,
            0x04 => Key::Eof,
            b'\r' | b'\n' => Key::Enter,
            b0 => {
                // The rest of a UTF-8 character.
                let n = match b0 {
                    0xc0..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf7 => 4,
                    _ => 1,
                };
                self.input.read_exact(&mut b[1..n])?;
                std::str::from_utf8(&b[..n])
                    .ok()
                    .and_then(|s| s.chars().next())
                    .map_or(Key::Char('\u{fffd}'), Key::Char)
            }
        })
    }

    /// Reads a line of text after `prompt`, with the terminal in its usual mode (echo, line
    /// editing, Ctrl-C). `None` at the end of input; the text is trimmed.
    pub fn line(&mut self, prompt: &str) -> io::Result<Option<String>> {
        if let Some(r) = &self.raw {
            r.cooked();
        }
        write!(self.out, "{prompt}")?;
        self.out.flush()?;
        let mut line = String::new();
        let n = self.input.read_line(&mut line);
        if let Some(r) = &self.raw {
            r.raw();
        }
        Ok((n? > 0).then(|| line.trim().to_string()))
    }

    /// Clears the current line when redrawing is possible (on a terminal), so a prompt can be
    /// replaced by the answer; otherwise starts a new line.
    pub fn redraw(&mut self) -> io::Result<()> {
        if self.keys() {
            write!(self.out, "\r\x1b[2K")
        } else {
            writeln!(self.out)
        }
    }

    /// Reads a tag name after `prompt`, showing what `matches` returns for the text so far and
    /// completing their common prefix on Tab. On a terminal it edits in place (Backspace,
    /// Ctrl-U; Esc or Ctrl-C cancels); otherwise it reads a line. `None` if cancelled or at the
    /// end of input; the text is trimmed.
    pub fn complete(
        &mut self,
        prompt: &str,
        initial: &str,
        matches: impl Fn(&str) -> Vec<String>,
    ) -> io::Result<Option<String>> {
        if !self.keys() {
            let line = self.line(prompt);
            writeln!(self.out)?;
            return line;
        }
        let mut buf = initial.to_string();
        loop {
            let found = matches(&buf);
            let hint = if buf.is_empty() || found.is_empty() {
                String::new()
            } else {
                let mut h: Vec<&str> = found.iter().take(6).map(String::as_str).collect();
                if found.len() > 6 {
                    h.push("…");
                }
                format!("  {}", h.join(" "))
            };
            let room = self
                .width()
                .saturating_sub(prompt.chars().count() + buf.chars().count());
            let hint: String = hint.chars().take(room.saturating_sub(1)).collect();
            let back = hint.chars().count();
            write!(self.out, "\r\x1b[2K{prompt}{buf}{}", self.dim(&hint))?;
            if back > 0 {
                write!(self.out, "\x1b[{back}D")?;
            }
            match self.key()? {
                Key::Enter => {
                    writeln!(self.out)?;
                    return Ok(Some(buf.trim().to_string()));
                }
                Key::Interrupt | Key::Eof | Key::Char('\x1b') => {
                    write!(self.out, "\r\x1b[2K")?;
                    return Ok(None);
                }
                Key::Char('\x7f' | '\x08') => {
                    buf.pop();
                }
                Key::Char('\x15') => buf.clear(),
                Key::Char('\t') => {
                    if let Some(p) = common_prefix(&found)
                        && p.len() > buf.len()
                    {
                        buf = p;
                    }
                }
                Key::Char(c) if !c.is_control() => buf.push(c),
                Key::Char(_) => {}
            }
        }
    }

    /// Erases the `n` lines above the cursor and the current one, so a block can be redrawn.
    /// On a terminal; otherwise it starts a new line and the block is printed again below.
    pub fn erase_up(&mut self, n: usize) -> io::Result<()> {
        if !self.keys() {
            return writeln!(self.out);
        }
        if n > 0 {
            write!(self.out, "\x1b[{n}A")?;
        }
        write!(self.out, "\r\x1b[J")
    }
}

impl Write for Screen {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.out.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// The terminal in non-canonical mode; restored on drop.
struct Raw {
    /// `stty -g` before.
    saved: String,
    cols: usize,
    /// The terminal, when it isn't stdin.
    tty: Option<File>,
}

const RAW: &[&str] = &["-icanon", "-echo", "-isig", "min", "1", "time", "0"];

impl Raw {
    /// Puts the terminal (`tty`, else stdin) in non-canonical mode.
    fn enter(tty: Option<File>) -> Option<Raw> {
        let saved = stty(tty.as_ref(), &["-g"])?;
        let cols = stty(tty.as_ref(), &["size"])
            .and_then(|s| s.split_whitespace().nth(1)?.parse().ok())
            .filter(|&c| c > 0)
            .unwrap_or(80);
        stty(tty.as_ref(), RAW)?;
        Some(Raw { saved, cols, tty })
    }

    fn cooked(&self) {
        stty(self.tty.as_ref(), &[&self.saved]);
    }

    fn raw(&self) {
        stty(self.tty.as_ref(), RAW);
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        self.cooked();
    }
}

/// Runs `stty` on the terminal (`tty`, else our stdin); its output, if it succeeded.
fn stty(tty: Option<&File>, args: &[&str]) -> Option<String> {
    let stdin = match tty {
        Some(f) => Stdio::from(f.try_clone().ok()?),
        None => Stdio::inherit(),
    };
    let o = Command::new("stty")
        .args(args)
        .stdin(stdin)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// The longest prefix all of `names` share, if there are any.
fn common_prefix(names: &[String]) -> Option<String> {
    let first = names.first()?;
    let mut end = first.len();
    for n in &names[1..] {
        end = first
            .char_indices()
            .zip(n.chars())
            .find(|((_, a), b)| a != b)
            .map_or(end.min(n.len()), |((i, _), _)| i.min(end));
    }
    Some(first[..end].to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn common_prefix() {
        let v = |s: &[&str]| s.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            super::common_prefix(&v(&["rust", "rustc", "ruby"])),
            Some("ru".into())
        );
        assert_eq!(super::common_prefix(&v(&["web"])), Some("web".into()));
        assert_eq!(super::common_prefix(&v(&["a", "b"])), Some("".into()));
        assert_eq!(super::common_prefix(&[]), None);
    }
}
