//! API keys: a value read when the provider was built (`api_key_env`, `api_key_file`), or a
//! command run when the first request needs it (`api_key_cmd`, task 048).
//!
//! A command's output is cached for the life of the process, so a secret manager is asked once
//! per run however many providers or requests use it. Failures aren't cached: the next request
//! asks again (after the keyring is unlocked, say).

use std::collections::HashMap;
use std::fmt;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};

/// How long `api_key_cmd` may take: long enough to type a passphrase into a pinentry.
pub const KEY_CMD_TIMEOUT: Duration = Duration::from_secs(120);

/// Where a provider's key comes from. `Debug` never shows the key.
#[derive(Clone)]
pub enum ApiKey {
    Value(String),
    Cmd(KeyCmd),
}

impl ApiKey {
    /// The key, running the command if this is the first time it's needed.
    pub fn get(&self) -> Result<String> {
        match self {
            ApiKey::Value(k) => Ok(k.clone()),
            ApiKey::Cmd(c) => c.key(),
        }
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiKey::Value(_) => f.write_str("ApiKey::Value(..)"),
            ApiKey::Cmd(c) => write!(f, "ApiKey::Cmd({:?})", c.argv),
        }
    }
}

/// `api_key_cmd`: a program and its arguments, run without a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyCmd {
    /// The provider it belongs to, for the note shown while it runs.
    pub provider: String,
    pub argv: Vec<String>,
    pub timeout: Duration,
}

/// Runs a key command somewhere other than here (the TUI runs it with the terminal suspended).
pub type Runner = Box<dyn Fn(&KeyCmd) -> Result<String> + Send + Sync>;

static RUNNER: OnceLock<Runner> = OnceLock::new();

/// Keys from commands, by argv. Held while a command runs, so concurrent requests wait for one
/// run instead of starting their own.
static CACHE: Mutex<Option<HashMap<Vec<String>, String>>> = Mutex::new(None);

/// Routes key commands through `runner` from now on. Only the first call has an effect.
pub fn set_runner(runner: Runner) {
    let _ = RUNNER.set(runner);
}

impl KeyCmd {
    pub fn new(provider: &str, argv: &[String]) -> KeyCmd {
        KeyCmd {
            provider: provider.to_string(),
            argv: argv.to_vec(),
            timeout: KEY_CMD_TIMEOUT,
        }
    }

    /// The key: cached, or from running the command (through the runner, if one is set).
    pub fn key(&self) -> Result<String> {
        let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        let cache = cache.get_or_insert_with(HashMap::new);
        if let Some(k) = cache.get(&self.argv) {
            return Ok(k.clone());
        }
        let key = match RUNNER.get() {
            Some(r) => r(self)?,
            None => self.run()?,
        };
        cache.insert(self.argv.clone(), key.clone());
        Ok(key)
    }

    /// Runs the command now: stdin closed, stderr passed through (for prompts), stdout trimmed.
    /// Errors name the command, never its output.
    pub fn run(&self) -> Result<String> {
        let what = self.describe();
        let Some((program, args)) = self.argv.split_first() else {
            bail!("{what} is empty");
        };
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| anyhow!("cannot run {what}: {e}"))?;
        // Read stdout on a thread so a chatty command can't fill the pipe and stall.
        let mut stdout = child.stdout.take().expect("stdout is piped");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let r = stdout.read_to_end(&mut buf).map(|_| buf);
            let _ = tx.send(r);
        });
        let deadline = Instant::now() + self.timeout;
        let status = loop {
            if let Some(s) = child.try_wait()? {
                break s;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!("{what} timed out after {:?}", self.timeout);
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        if !status.success() {
            bail!("{what} failed ({status})");
        }
        // A child it left running may still hold stdout open; don't wait past the deadline.
        let left = deadline.saturating_duration_since(Instant::now());
        let out = match rx.recv_timeout(left.max(Duration::from_millis(100))) {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => bail!("cannot read the output of {what}: {e}"),
            Err(_) => bail!("{what} timed out after {:?}", self.timeout),
        };
        let Ok(text) = String::from_utf8(out) else {
            bail!("{what} printed something that isn't UTF-8");
        };
        let key = text.trim();
        if key.is_empty() {
            bail!("{what} printed nothing");
        }
        Ok(key.to_string())
    }

    /// `api_key_cmd ["secret-tool", "lookup", ...]`, for messages.
    pub fn describe(&self) -> String {
        format!("api_key_cmd {:?}", self.argv)
    }
}
