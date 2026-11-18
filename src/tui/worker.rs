//! The worker thread: slow work (fetching a title, summarizing) off the UI thread, with its own
//! database connection, reporting progress and results as events.
//!
//! Jobs run one at a time, in the order sent. The thread starts with the first job, so a session
//! that never sends one costs nothing.

use std::io::{self, Write};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use shlv::Store;
use shlv::config::Config;
use shlv::health::{Health, HealthOptions};
use shlv::review::ReviewFilter;

/// Opens the worker's own store, configured as the UI's is.
pub type OpenStore = Box<dyn FnOnce() -> Result<(Store, Config)> + Send>;

#[derive(Debug, Clone, PartialEq)]
pub enum Job {
    /// Fetch the page and set the bookmark's title from it (feature `fetch`).
    #[cfg_attr(not(feature = "fetch"), allow(dead_code))]
    FetchTitle { id: i64, url: String },
    /// Summarize the page with the `[ai] summarize` provider (feature `ai`).
    #[cfg_attr(not(feature = "ai"), allow(dead_code))]
    Summarize { id: i64, url: String },
    /// Pick the next review batch and run the suggestion layers on it: rules, the classifier,
    /// the `[ai] tag` provider. Done with the batch's ids, and how they were ordered.
    ReviewBatch { filter: ReviewFilter, size: usize },
    /// Offer one tag (`suggest --tag`): the classifier, the provider; on the given bookmarks, or
    /// all of them.
    #[cfg_attr(not(any(feature = "model", feature = "ai")), allow(dead_code))]
    Backfill {
        tag: String,
        scope: Option<Vec<i64>>,
    },
    /// Measure the tag list (`tag health`).
    Health,
    /// Retrain the classifier (feature `model`).
    #[cfg_attr(not(feature = "model"), allow(dead_code))]
    Train,
}

impl Job {
    /// What the job does, for the status line until it reports a step.
    pub fn describe(&self) -> String {
        match self {
            Job::FetchTitle { id, .. } => format!("fetching the title of {id}"),
            Job::Summarize { id, .. } => format!("summarizing {id}"),
            Job::ReviewBatch { .. } => "picking a batch".into(),
            Job::Backfill { tag, .. } => format!("back-filling #{tag}"),
            Job::Train => "training the classifier".into(),
            Job::Health => "measuring the tag list".into(),
        }
    }
}

/// What a job made, besides a message.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Output {
    #[default]
    Nothing,
    /// Bookmarks: those a review batch picked.
    Ids(Vec<i64>),
    Health(Box<Health>),
}

/// What the worker says about a job.
#[derive(Debug, Clone, PartialEq)]
pub enum Report {
    /// A step of a job, for the status line.
    Progress {
        job: u64,
        text: String,
    },
    /// The job is done: what it did, and what it made.
    Done {
        job: u64,
        text: String,
        out: Output,
    },
    Failed {
        job: u64,
        error: String,
    },
}

impl Report {
    pub fn job(&self) -> u64 {
        match self {
            Report::Progress { job, .. }
            | Report::Done { job, .. }
            | Report::Failed { job, .. } => *job,
        }
    }
}

pub struct Worker {
    open: Option<OpenStore>,
    jobs: Option<Sender<(u64, Job)>>,
    reports: Option<Receiver<Report>>,
    next: u64,
}

impl Worker {
    pub fn new(open: OpenStore) -> Self {
        Worker {
            open: Some(open),
            jobs: None,
            reports: None,
            next: 1,
        }
    }

    /// Queues a job; returns its number.
    pub fn send(&mut self, job: Job) -> u64 {
        if self.jobs.is_none() {
            let (jobs_tx, jobs_rx) = mpsc::channel();
            let (reports_tx, reports_rx) = mpsc::channel();
            let open = self.open.take().expect("worker started once");
            thread::spawn(move || work(open, jobs_rx, reports_tx));
            self.jobs = Some(jobs_tx);
            self.reports = Some(reports_rx);
        }
        let n = self.next;
        self.next += 1;
        // A send fails only if the thread is gone; `recv` reports that.
        let _ = self.jobs.as_ref().expect("started").send((n, job));
        n
    }

    /// The next report, waiting up to `wait` (or not at all for zero).
    pub fn recv(&self, wait: Duration) -> Option<Report> {
        let r = self.reports.as_ref()?;
        if wait.is_zero() {
            return r.try_recv().ok();
        }
        r.recv_timeout(wait).ok()
    }
}

fn work(open: OpenStore, jobs: Receiver<(u64, Job)>, reports: Sender<Report>) {
    let mut opened = match open() {
        Ok(o) => Ok(o),
        Err(e) => Err(format!("{e:#}")),
    };
    let mut ctx = Context::default();
    while let Ok((n, job)) = jobs.recv() {
        let progress = |text: String| {
            let _ = reports.send(Report::Progress { job: n, text });
        };
        let report = match &mut opened {
            Err(e) => Report::Failed {
                job: n,
                error: format!("cannot open the database: {e}"),
            },
            Ok((store, config)) => match run(&mut ctx, store, config, &job, &progress) {
                Ok((text, out)) => Report::Done { job: n, text, out },
                Err(e) => Report::Failed {
                    job: n,
                    error: format!("{e:#}"),
                },
            },
        };
        if reports.send(report).is_err() {
            return;
        }
    }
}

/// What the worker keeps between jobs.
#[derive(Default)]
struct Context {
    #[cfg(feature = "fetch")]
    fetcher: Option<shlv::fetch::Fetcher>,
}

/// Turns what a CLI helper prints into progress reports, a line at a time.
struct Lines<'a> {
    progress: &'a dyn Fn(String),
    line: Vec<u8>,
}

impl Write for Lines<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        for &b in buf {
            if b == b'\n' || b == b'\r' {
                let text = String::from_utf8_lossy(&self.line).trim().to_string();
                if !text.is_empty() {
                    (self.progress)(text);
                }
                self.line.clear();
            } else {
                self.line.push(b);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[allow(unused_variables)]
fn run(
    ctx: &mut Context,
    store: &mut Store,
    config: &Config,
    job: &Job,
    progress: &dyn Fn(String),
) -> Result<(String, Output)> {
    let mut out = Lines {
        progress,
        line: Vec::new(),
    };
    match job {
        #[cfg(feature = "fetch")]
        Job::FetchTitle { id, url } => {
            let fetcher = ctx.fetcher.get_or_insert_with(Default::default);
            progress(format!("fetching {url}"));
            let page = fetcher
                .get(url)
                .map_err(|e| anyhow::anyhow!("cannot fetch {url}: {e}"))?;
            let Some(title) = shlv::fetch::title(&page.html) else {
                anyhow::bail!("{url} has no title");
            };
            let edit = shlv::Edit {
                title: Some(title.clone()),
                ..Default::default()
            };
            if !store.edit(*id, &edit)? {
                anyhow::bail!("bookmark {id} is gone");
            }
            Ok((format!("title: {title}"), Output::Nothing))
        }
        #[cfg(feature = "ai")]
        Job::Summarize { id, url } => {
            let provider = crate::ai_cmd::provider(config, shlv::ai::Task::Summarize)?;
            let fetcher = ctx.fetcher.get_or_insert_with(Default::default);
            progress(format!("fetching {url}"));
            let Some(text) = crate::ai_cmd::page_text(fetcher, url)? else {
                anyhow::bail!("no text on the page {url}");
            };
            let Some(b) = store.get(*id)? else {
                anyhow::bail!("bookmark {id} is gone");
            };
            progress(format!("asking {}", provider.name()));
            let item = shlv::choose::Item {
                text: Some(text),
                ..shlv::choose::Item::from_bookmark(&b)
            };
            let summary = provider.summarize(&item)?;
            if !store.set_summary(*id, Some(&summary))? {
                anyhow::bail!("bookmark {id} is gone");
            }
            Ok(("summarized".into(), Output::Nothing))
        }
        Job::ReviewBatch { filter, size } => {
            store.reload_rules()?;
            let queue = store.review_queue(filter)?;
            if queue.is_empty() {
                return Ok((String::new(), Output::Ids(Vec::new())));
            }
            progress(format!("ordering {} bookmarks", queue.len()));
            let (ids, order) = crate::review_cmd::choose(store, config, &queue, filter, *size)?;
            progress("suggesting tags".into());
            crate::review_cmd::offer(store, config, &ids, &mut out)?;
            Ok((
                format!("{} of {} to review, {order}", ids.len(), queue.len()),
                Output::Ids(ids),
            ))
        }
        #[cfg(any(feature = "model", feature = "ai"))]
        Job::Backfill { tag, scope } => {
            crate::backfill_cmd::offer(
                store,
                config,
                tag,
                scope.as_deref(),
                true,
                #[cfg(feature = "ai")]
                &crate::ai_cmd::LlmArgs {
                    max_tags: 1,
                    force: false,
                    jobs: 2,
                    yes: true,
                    batch: false,
                },
                &mut out,
            )?;
            Ok((format!("offered #{tag}"), Output::Nothing))
        }
        #[cfg(feature = "model")]
        Job::Train => {
            let t = store.train_model()?;
            Ok((
                format!(
                    "classifier retrained on {} ({} ms)",
                    crate::plural(t.examples, "example"),
                    t.millis
                ),
                Output::Nothing,
            ))
        }
        Job::Health => {
            progress("measuring the tag list".into());
            let h = health(store, config)?;
            Ok((String::new(), Output::Health(Box::new(h))))
        }
        #[allow(unreachable_patterns)]
        _ => anyhow::bail!("this build can't do that"),
    }
}

/// `tag health` with its default thresholds; with the classifier built and a canonical list,
/// bookmarks it can place aren't gaps.
#[allow(unused_variables)]
fn health(store: &mut Store, config: &Config) -> Result<Health> {
    let opts = HealthOptions::default();
    #[cfg(feature = "model")]
    if !shlv::model::canonical_list(store)?.is_empty() {
        let c = store.classifier(false)?;
        let t = config.model.threshold;
        let place =
            move |item: &shlv::choose::Item| c.probabilities(item).iter().any(|(_, p)| *p >= t);
        return store.health(&opts, Some(&place));
    }
    store.health(&opts, None)
}
