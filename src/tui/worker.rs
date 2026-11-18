//! The worker thread: slow work (fetching a title, summarizing) off the UI thread, with its own
//! database connection, reporting progress and results as events.
//!
//! Jobs run one at a time, in the order sent. The thread starts with the first job, so a session
//! that never sends one costs nothing.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use shlv::Store;
use shlv::config::Config;

/// Opens the worker's own store, configured as the UI's is.
pub type OpenStore = Box<dyn FnOnce() -> Result<(Store, Config)> + Send>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    /// Fetch the page and set the bookmark's title from it (feature `fetch`).
    #[cfg_attr(not(feature = "fetch"), allow(dead_code))]
    FetchTitle { id: i64, url: String },
    /// Summarize the page with the `[ai] summarize` provider (feature `ai`).
    #[cfg_attr(not(feature = "ai"), allow(dead_code))]
    Summarize { id: i64, url: String },
}

impl Job {
    pub fn id(&self) -> i64 {
        match self {
            Job::FetchTitle { id, .. } | Job::Summarize { id, .. } => *id,
        }
    }

    pub fn verb(&self) -> &'static str {
        match self {
            Job::FetchTitle { .. } => "fetching the title of",
            Job::Summarize { .. } => "summarizing",
        }
    }
}

/// What the worker says about a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    /// A step of a job, for the status line.
    Progress {
        job: u64,
        text: String,
    },
    /// The job is done and the bookmark changed (or not, as `text` says).
    Done {
        job: u64,
        id: i64,
        text: String,
    },
    Failed {
        job: u64,
        id: i64,
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
        let id = job.id();
        let progress = |text: String| {
            let _ = reports.send(Report::Progress { job: n, text });
        };
        let report = match &mut opened {
            Err(e) => Report::Failed {
                job: n,
                id,
                error: format!("cannot open the database: {e}"),
            },
            Ok((store, config)) => match run(&mut ctx, store, config, &job, &progress) {
                Ok(text) => Report::Done { job: n, id, text },
                Err(e) => Report::Failed {
                    job: n,
                    id,
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

#[allow(unused_variables)]
fn run(
    ctx: &mut Context,
    store: &mut Store,
    config: &Config,
    job: &Job,
    progress: &dyn Fn(String),
) -> Result<String> {
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
            Ok(format!("title: {title}"))
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
            Ok("summarized".into())
        }
        #[allow(unreachable_patterns)]
        _ => anyhow::bail!("this build can't do that"),
    }
}
