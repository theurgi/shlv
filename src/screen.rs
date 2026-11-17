//! The interactive screen (task 034, reused by `tag draft` and `review`): a plain terminal
//! prompt loop, no TUI library.
//!
//! On a terminal, answers are single keypresses: the terminal is put in non-canonical mode with
//! `stty` (`-icanon -echo -isig`, so Ctrl-C arrives as a key rather than a signal and the
//! terminal is always restored), and back in its saved mode for text input and on drop. When
//! stdin isn't a terminal (or `stty` fails), each answer is a line and its first character is the
//! key: scripts and tests drive the screen that way.
//!
//! The screen draws on stderr, so stdout keeps only the command's result.

use std::io::{self, BufRead, IsTerminal, Read, Write};
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
    input: io::StdinLock<'static>,
    out: io::Stderr,
    pub style: Style,
}

impl Screen {
    /// Opens the screen on stdin and stderr: single keys on a terminal, lines otherwise.
    pub fn open() -> Screen {
        let tty = io::stdin().is_terminal() && io::stderr().is_terminal();
        Screen {
            raw: if tty { Raw::enter() } else { None },
            input: io::stdin().lock(),
            out: io::stderr(),
            style: Style::detect(io::stderr().is_terminal()),
        }
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
}

const RAW: &[&str] = &["-icanon", "-echo", "-isig", "min", "1", "time", "0"];

impl Raw {
    fn enter() -> Option<Raw> {
        let saved = stty(&["-g"])?;
        let cols = stty(&["size"])
            .and_then(|s| s.split_whitespace().nth(1)?.parse().ok())
            .filter(|&c| c > 0)
            .unwrap_or(80);
        stty(RAW)?;
        Some(Raw { saved, cols })
    }

    fn cooked(&self) {
        stty(&[&self.saved]);
    }

    fn raw(&self) {
        stty(RAW);
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        self.cooked();
    }
}

/// Runs `stty` on the terminal (its stdin is ours); its output, if it succeeded.
fn stty(args: &[&str]) -> Option<String> {
    let o = Command::new("stty")
        .args(args)
        .stdin(Stdio::inherit())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}
