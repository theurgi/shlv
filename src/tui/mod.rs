//! The TUI (plan 004, task 042): bare `shlv` on a terminal. ratatui draws, crossterm runs the
//! terminal. Everything goes through `Store`, as the CLI does.
//!
//! The terminal is restored on quit, on errors, and on panics (ratatui's panic hook). Ctrl-C
//! arrives as a key in raw mode and quits.

use std::io::{self, Write};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::DefaultTerminal;

mod app;
mod bookmarks;
mod input;
#[cfg(test)]
mod tests;
// Its jobs need the `fetch` or `ai` feature; the default build has the plumbing only.
#[cfg_attr(not(feature = "fetch"), allow(dead_code))]
mod worker;

pub use app::App;

/// How often the loop looks for the worker's reports while jobs run.
const TICK: Duration = Duration::from_millis(100);

pub fn run(opened: crate::Opened) -> Result<()> {
    let crate::Opened {
        store,
        config,
        notes,
        ..
    } = opened;
    let worker = Box::new(|| {
        let o = crate::open_store()?;
        Ok((o.store, o.config))
    });
    let mut app = App::new(store, config, worker)?;
    if !notes.is_empty() {
        app.say(notes.join("; "));
    }
    let mut screen = Screen::enter()?;
    let result = event_loop(&mut app, &mut screen.terminal);
    drop(screen);
    result
}

fn event_loop(app: &mut App, terminal: &mut DefaultTerminal) -> Result<()> {
    loop {
        terminal.draw(|f| app.draw(f))?;
        if app.quit {
            return Ok(());
        }
        // Block on the terminal, unless jobs are running: then wake up for their reports too.
        if !app.busy() || event::poll(TICK)? {
            app.event(event::read()?);
            // Handle what else is queued (fast typing, a held key) before searching and drawing.
            while !app.quit && event::poll(Duration::ZERO)? {
                app.event(event::read()?);
            }
            app.settle();
        }
        app.drain_reports(Duration::ZERO);
        if let Some(url) = app.take_open() {
            let r = suspended(terminal, || crate::open(&url));
            match r {
                Ok(()) => app.say(format!("opened {url}")),
                Err(e) => app.warn(format!("cannot open {url}: {e:#}")),
            }
        }
    }
}

/// Runs `f` with the terminal back in its normal mode (a terminal browser needs it), then
/// takes it over again.
fn suspended<T>(terminal: &mut DefaultTerminal, f: impl FnOnce() -> T) -> T {
    leave();
    let out = f();
    let _ = enter();
    let _ = terminal.clear();
    out
}

/// The terminal in raw mode on the alternate screen; restored when dropped.
struct Screen {
    terminal: DefaultTerminal,
}

impl Screen {
    fn enter() -> Result<Screen> {
        // Installs a panic hook that restores the terminal, then enters raw mode and the
        // alternate screen.
        let terminal = ratatui::try_init()?;
        let restore = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = execute!(io::stdout(), DisableBracketedPaste);
            restore(info);
        }));
        execute!(io::stdout(), EnableBracketedPaste)?;
        Ok(Screen { terminal })
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableBracketedPaste);
        ratatui::restore();
    }
}

fn enter() -> io::Result<()> {
    terminal::enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)
}

fn leave() {
    let mut out = io::stdout();
    let _ = execute!(out, DisableBracketedPaste, LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
    let _ = out.flush();
}
