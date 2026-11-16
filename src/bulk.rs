//! Bounded parallel work for bulk network jobs (`fetch --all`, and later summaries and tags).
//!
//! The slow part (a blocking HTTP request) runs on a few worker threads; the results come back to
//! the calling thread, which applies each one as it arrives, usually in its own short
//! transaction. An interrupted run keeps everything applied so far.

use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;

/// Runs `work` on every item with at most `jobs` threads, and calls `done(index, result)` on the
/// calling thread as each finishes (in completion order). If `done` fails, no new items start and
/// that error is returned once running ones finish.
pub fn run<T, R, E>(
    items: &[T],
    jobs: usize,
    work: impl Fn(&T) -> R + Sync,
    mut done: impl FnMut(usize, R) -> Result<(), E>,
) -> Result<(), E>
where
    T: Sync,
    R: Send,
{
    let jobs = jobs.clamp(1, items.len().max(1));
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|s| {
        for _ in 0..jobs {
            let tx = tx.clone();
            let (next, stop, work) = (&next, &stop, &work);
            s.spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(i) else { break };
                    if tx.send((i, work(item))).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        for (i, r) in rx {
            if let Err(e) = done(i, r) {
                stop.store(true, Ordering::Relaxed);
                return Err(e);
            }
        }
        Ok(())
    })
}

/// A one-line progress counter on stderr, redrawn in place. Silent when stderr isn't a terminal.
pub struct Progress {
    what: &'static str,
    total: usize,
    n: usize,
    tty: bool,
}

impl Progress {
    pub fn new(what: &'static str, total: usize) -> Self {
        Progress {
            what,
            total,
            n: 0,
            tty: io::stderr().is_terminal(),
        }
    }

    /// Counts one item done and redraws.
    pub fn tick(&mut self) {
        self.n += 1;
        if self.tty {
            eprint!("\r\x1b[K{} {}/{}", self.what, self.n, self.total);
        }
    }

    /// Clears the line, so a message can be printed above it; [`Progress::tick`] redraws it.
    pub fn clear(&self) {
        if self.tty && self.n > 0 {
            eprint!("\r\x1b[K");
            let _ = io::stderr().flush();
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_everything_once() {
        let items: Vec<u32> = (0..100).collect();
        let mut seen = vec![0; 100];
        run(
            &items,
            4,
            |x| x * 2,
            |i, r| {
                assert_eq!(r, items[i] * 2);
                seen[i] += 1;
                Ok::<_, ()>(())
            },
        )
        .unwrap();
        assert!(seen.iter().all(|&n| n == 1));
        run(&[] as &[u32], 4, |x| *x, |_, _| Ok::<_, ()>(())).unwrap();
    }

    #[test]
    fn stops_on_error() {
        let items: Vec<u32> = (0..100).collect();
        let started = AtomicUsize::new(0);
        let mut done = 0;
        let r = run(
            &items,
            2,
            |_| {
                std::thread::sleep(std::time::Duration::from_millis(2));
                started.fetch_add(1, Ordering::Relaxed)
            },
            |_, _| {
                done += 1;
                if done == 3 { Err("boom") } else { Ok(()) }
            },
        );
        assert_eq!(r, Err("boom"));
        assert!(started.load(Ordering::Relaxed) < 20);
    }
}
