//! AI providers (feature `ai`): summaries and tag choices from an LLM, configured as
//! `[provider.NAME]` and picked per task in `[ai]`. Nothing is sent anywhere unless a provider is
//! configured.
//!
//! How the pieces fit: a provider *kind* is a transport ([`Llm`]: one structured-output request
//! in, the model's JSON text out). [`Provider`] wraps one, owns the prompts, schemas, and output
//! checks, and offers the calls the features use:
//!
//! - [`Provider::choose_tags`], the "choose tags" call shared by every tagging layer
//!   ([`crate::choose::ChooseTags`]);
//! - [`Provider::summarize`];
//! - [`Provider::summarize_and_tag`], both in one request, for when `[ai] summarize` and `tag`
//!   name the same provider and a bookmark needs both. It is an optimization; results go through
//!   the same checks.

pub mod anthropic;
pub mod key;
pub mod openai;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use toml::Spanned;

use crate::choose::{Canonical, Choice, ChooseTags, Item, NewTag, TagOption};
use crate::draft::{DraftTag, Drafted, Redundant};
use crate::style::{StyleConfig, TagStyle};
pub use key::{ApiKey, KeyCmd};

/// Longest summary accepted, in characters.
pub const MAX_SUMMARY: usize = 1000;

/// Longest page text sent, in characters (the extract is already capped at 3000 words).
const MAX_TEXT: usize = 24_000;

/// The `[ai]` section: which provider does which task. A task without one is off.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AiConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summarize: Option<Spanned<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<Spanned<String>>,
    /// Most new tag names the `tag` provider may propose per bookmark, alongside tags from the
    /// list (default 3; 0 turns proposals off). They become proposals, never tags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposals: Option<usize>,
}

/// A `[provider.NAME]` section.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub kind: Kind,
    /// Required: there is no default model.
    pub model: String,
    /// The API root, such as `http://localhost:11434/v1`. Required for `openai-compatible`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// The environment variable holding the API key; read only when the provider is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// A file holding the API key (an agenix or sops-nix secret, say); read only when the
    /// provider is used. Exclusive with `api_key_env` and `api_key_cmd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_file: Option<String>,
    /// A command printing the API key (`["secret-tool", "lookup", "service", "shlv"]`), run
    /// without a shell when the first request needs the key, then cached for the process.
    /// Exclusive with `api_key_env` and `api_key_file`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_cmd: Option<Vec<String>>,
    /// Seconds to wait for one response (default 120; local models can be slow).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    /// `anthropic` only: `output_config.effort` (`low`, `medium`, `high`, `xhigh`, `max`). Lower
    /// effort means less thinking and fewer tokens; tagging and short summaries need little.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    OpenaiCompatible,
    Anthropic,
}

impl ProviderConfig {
    /// Checks what serde can't: per-kind requirements.
    pub fn check(&self) -> Result<(), String> {
        if self.model.trim().is_empty() {
            return Err("`model` is empty".into());
        }
        if self.kind == Kind::OpenaiCompatible && self.base_url.is_none() {
            return Err("kind \"openai-compatible\" needs `base_url`".into());
        }
        if let Some(u) = &self.base_url
            && !(u.starts_with("http://") || u.starts_with("https://"))
        {
            return Err(format!(
                "`base_url` must be an http or https URL, not {u:?}"
            ));
        }
        let sources = [
            self.api_key_env.is_some(),
            self.api_key_file.is_some(),
            self.api_key_cmd.is_some(),
        ];
        if sources.into_iter().filter(|&s| s).count() > 1 {
            return Err("set only one of `api_key_env`, `api_key_file`, and `api_key_cmd`".into());
        }
        if let Some(c) = &self.api_key_cmd
            && c.first().is_none_or(|p| p.trim().is_empty())
        {
            return Err("`api_key_cmd` needs a program: [\"program\", \"arg\", ...]".into());
        }
        if let Some(e) = &self.effort {
            if self.kind != Kind::Anthropic {
                return Err("`effort` applies only to kind \"anthropic\"".into());
            }
            if !["low", "medium", "high", "xhigh", "max"].contains(&e.as_str()) {
                return Err(format!(
                    "`effort` must be low, medium, high, xhigh, or max, not {e:?}"
                ));
            }
        }
        if self.timeout == Some(0) {
            return Err("`timeout` must be at least 1 (seconds)".into());
        }
        Ok(())
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout.unwrap_or(120))
    }

    /// Where provider `name`'s API key comes from. `api_key_env` and `api_key_file` are read
    /// now; `api_key_cmd` runs when [`ApiKey::get`] first needs it. `None` when no key is
    /// configured (local servers).
    pub fn api_key(&self, name: &str) -> Result<Option<ApiKey>> {
        if let Some(argv) = &self.api_key_cmd {
            return Ok(Some(ApiKey::Cmd(KeyCmd::new(name, argv))));
        }
        if let Some(path) = &self.api_key_file {
            return read_key_file(Path::new(path)).map(|k| Some(ApiKey::Value(k)));
        }
        let Some(var) = &self.api_key_env else {
            return Ok(None);
        };
        match std::env::var(var) {
            Ok(k) if !k.trim().is_empty() => Ok(Some(ApiKey::Value(k.trim().to_string()))),
            _ => bail!("environment variable {var} (api_key_env) is not set"),
        }
    }
}

/// Reads a key file: surrounding whitespace (the trailing newline) is trimmed. Warns when other
/// users can read it, as ssh does for private keys.
fn read_key_file(path: &Path) -> Result<String> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read api_key_file {}", path.display()))?;
    let key = text.trim();
    if key.is_empty() {
        bail!("api_key_file {} is empty", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(m) = std::fs::metadata(path)
            && m.permissions().mode() & 0o004 != 0
        {
            eprintln!(
                "shlv: warning: api_key_file {} is readable by every user (mode {:o}); \
                 chmod o-r it",
                path.display(),
                m.permissions().mode() & 0o777
            );
        }
    }
    Ok(key.to_string())
}

/// A task a provider does, for error messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Task {
    Summarize,
    Tag,
}

impl Task {
    pub fn as_str(self) -> &'static str {
        match self {
            Task::Summarize => "summarize",
            Task::Tag => "tag",
        }
    }
}

/// One structured-output request.
#[derive(Debug, Clone)]
pub struct Request {
    /// Instructions and, for tags, the canonical list: the stable prefix.
    pub system: String,
    /// The bookmark.
    pub user: String,
    /// A name for the schema (some APIs require one).
    pub schema_name: &'static str,
    /// JSON Schema the reply must match.
    pub schema: Value,
    /// On a retry: the previous reply and what was wrong with it.
    pub retry: Option<(String, String)>,
    /// Most output tokens, when the call needs a bound tighter than the transport's default (a
    /// small model looping inside a JSON array otherwise runs until the server gives up).
    pub max_tokens: Option<u32>,
}

/// Tokens used so far, as the API reports them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub requests: u64,
    /// Input tokens not read from the cache.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// A provider kind: sends a request, returns the model's reply text (JSON, unchecked).
pub trait Llm: Send + Sync {
    fn complete(&self, req: &Request) -> Result<String>;

    /// Tokens used by this transport so far.
    fn usage(&self) -> Usage {
        Usage::default()
    }

    /// The Message Batches API, for kinds that have one.
    fn batches(&self) -> Option<&anthropic::Anthropic> {
        None
    }
}

/// New tag names per bookmark the `tag` provider may propose unless `[ai] proposals` says
/// otherwise.
pub const DEFAULT_PROPOSALS: usize = 3;

impl AiConfig {
    /// `[ai] proposals`, or its default.
    pub fn proposals(&self) -> usize {
        self.proposals.unwrap_or(DEFAULT_PROPOSALS)
    }
}

/// Asking for new tag names alongside tags from the list (task 031). Off by default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Proposing {
    /// Most new names per bookmark; 0 asks for none.
    pub max: usize,
    /// Names already waiting as proposals, for the model to reuse rather than coin synonyms of.
    pub known: Vec<String>,
}

/// Most pending proposal names put in the prompt.
pub const MAX_KNOWN: usize = 100;

/// A configured provider.
pub struct Provider {
    name: String,
    llm: Box<dyn Llm>,
    proposing: Proposing,
    style: StyleConfig,
}

impl Provider {
    /// Builds provider `name` from its config. Reads its API key now, except from `api_key_cmd`:
    /// that runs when the first request needs it.
    pub fn new(name: &str, config: &ProviderConfig) -> Result<Provider> {
        let at = || format!("provider {name}");
        config.check().map_err(|e| anyhow!(e)).with_context(at)?;
        let key = config.api_key(name).with_context(at)?;
        let llm: Box<dyn Llm> = match config.kind {
            Kind::OpenaiCompatible => Box::new(openai::OpenAi::new(config, key)),
            Kind::Anthropic => match key {
                Some(key) => Box::new(anthropic::Anthropic::new(config, key)),
                None => bail!(
                    "provider {name}: kind \"anthropic\" needs an API key: set api_key_env, \
                     api_key_file, or api_key_cmd"
                ),
            },
        };
        Ok(Provider::with_llm(name, llm))
    }

    /// A provider over any transport (tests use this with fakes).
    pub fn with_llm(name: &str, llm: Box<dyn Llm>) -> Provider {
        Provider {
            name: name.to_string(),
            llm,
            proposing: Proposing::default(),
            style: StyleConfig::default(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Asks for new tag names in tag requests from now on; see [`Proposing`].
    pub fn set_proposing(&mut self, p: Proposing) {
        self.proposing = p;
    }

    /// The tag style (`[tags.style]`) for tag requests from now on: its rules and guidelines go
    /// in the prompt, and new names in replies are fixed by it, or dropped if it refuses them
    /// ([`Answer::refused`]).
    pub fn set_style(&mut self, style: StyleConfig) {
        self.style = style;
    }

    /// Whether tag requests ask for new names.
    pub fn proposes(&self) -> bool {
        self.proposing.max > 0
    }

    /// Tokens used by this provider so far.
    pub fn usage(&self) -> Usage {
        self.llm.usage()
    }

    /// A short summary of the bookmark.
    pub fn summarize(&self, item: &Item) -> Result<String> {
        Ok(self
            .run(Job::Summary, item, &[])?
            .summary
            .unwrap_or_default())
    }

    /// Tags from `list` that fit the bookmark. Unknown names in the reply are dropped and aliases
    /// resolved; confidence is `None` (LLMs' stated confidence isn't calibrated).
    pub fn choose_tags(&self, item: &Item, list: &[TagOption]) -> Result<Vec<Choice>> {
        Ok(self.tag(item, list)?.tags)
    }

    /// Tags from `list` that fit the bookmark, as [`Provider::choose_tags`] returns them, and,
    /// when proposing ([`Provider::set_proposing`]), new names for the list. `list` may be empty
    /// when proposing; then the reply has only new names.
    pub fn tag(&self, item: &Item, list: &[TagOption]) -> Result<Answer> {
        self.run(Job::Tags, item, list)
    }

    /// Both in one request: the summary, list tags, and new names when proposing.
    pub fn summarize_and_tag(&self, item: &Item, list: &[TagOption]) -> Result<Answer> {
        if list.is_empty() && !self.proposes() {
            return Ok(Answer {
                summary: Some(self.summarize(item)?),
                ..Answer::default()
            });
        }
        self.run(Job::Both, item, list)
    }

    /// Sends one job; a reply that isn't valid is retried once, with what was wrong.
    fn run(&self, job: Job, item: &Item, list: &[TagOption]) -> Result<Answer> {
        let at = || format!("provider {} ({})", self.name, job.task().as_str());
        if job != Job::Summary && list.is_empty() && !self.proposes() {
            bail!(
                "provider {} (tag): the canonical tag list is empty; declare tags in \
                 [tags.canonical] or mark some with `shlv tag canonical add`",
                self.name
            );
        }
        let mut req = job.request(item, list, &self.proposing, &self.style);
        for attempt in 0..2 {
            let reply = self.llm.complete(&req).with_context(at)?;
            match job.check(&reply, list, self.proposing.max, self.style.tag_style()) {
                Ok(a) => return Ok(a),
                Err(why) if attempt == 1 => {
                    return Err(anyhow!("invalid reply twice: {why}")).with_context(at);
                }
                Err(why) => req.retry = Some((reply, why)),
            }
        }
        unreachable!()
    }

    /// Candidate tags for each of `interests` (task 034), up to `per` each. Sends only the
    /// interests and the tag style (the style sentence and `[tags.style] guidelines`): no
    /// bookmarks, no list. Names are fixed by the style (refused ones are dropped with the
    /// reason), deduplicated across interests, and grouped by interest in the order sent.
    pub fn seed(&self, interests: &[String], per: usize) -> Result<Seeds> {
        let at = || format!("provider {} (seed)", self.name);
        let mut req = seed_request(interests, per, &self.style);
        for attempt in 0..2 {
            let reply = self.llm.complete(&req).with_context(at)?;
            match check_seeds(&reply, interests, per, self.style.tag_style()) {
                Ok(s) => return Ok(s),
                Err(why) if attempt == 1 => {
                    return Err(anyhow!("invalid reply twice: {why}")).with_context(at);
                }
                Err(why) => req.retry = Some((reply, why)),
            }
        }
        unreachable!()
    }

    /// A draft of the tag list from a sample of bookmarks (task 035): new tags for what recurs in
    /// `items` and the list doesn't cover, and, with a list, listed tags that look redundant.
    /// Sends the items (site, title, and a summary or the start of the page text), the list with
    /// descriptions and aliases, the interests, the names drafted so far, and the tag style.
    pub fn draft(&self, items: &[DraftItem], guide: &DraftGuide) -> Result<Drafted> {
        let at = || format!("provider {} (draft)", self.name);
        let mut req = draft_request(items, guide, &self.style);
        for attempt in 0..2 {
            let reply = self.llm.complete(&req).with_context(at)?;
            match check_draft(&reply, guide, self.style.tag_style()) {
                Ok(d) => return Ok(d),
                Err(why) if attempt == 1 => {
                    return Err(anyhow!("invalid reply twice: {why}")).with_context(at);
                }
                Err(why) => req.retry = Some((reply, why)),
            }
        }
        unreachable!()
    }

    /// Whether this provider can run jobs as a Message Batch (kind `anthropic`).
    pub fn supports_batches(&self) -> bool {
        self.llm.batches().is_some()
    }

    fn batches(&self) -> Result<&anthropic::Anthropic> {
        self.llm.batches().ok_or_else(|| {
            anyhow!(
                "provider {}: batches need kind \"anthropic\" (the Message Batches API)",
                self.name
            )
        })
    }

    /// Submits jobs as one Message Batch; returns its id. Each job has a `custom_id` that comes
    /// back with its result.
    pub fn submit_batch(&self, jobs: &[(String, Job, Item)], list: &[TagOption]) -> Result<String> {
        let b = self.batches()?;
        let requests = jobs
            .iter()
            .map(|(id, job, item)| {
                (
                    id.clone(),
                    b.body(&job.request(item, list, &self.proposing, &self.style)),
                )
            })
            .collect();
        b.submit_batch(requests)
            .with_context(|| format!("provider {} (batch)", self.name))
    }

    /// A batch's processing status and counts.
    pub fn batch_status(&self, id: &str) -> Result<anthropic::BatchStatus> {
        self.batches()?
            .batch_status(id)
            .with_context(|| format!("provider {} (batch {id})", self.name))
    }

    /// An ended batch's results, checked like live replies: `(custom_id, answer or reason)`.
    pub fn batch_results(
        &self,
        status: &anthropic::BatchStatus,
        job_of: impl Fn(&str) -> Option<Job>,
        list: &[TagOption],
    ) -> Result<Vec<(String, Result<Answer, String>)>> {
        let raw = self
            .batches()?
            .batch_results(status)
            .with_context(|| format!("provider {} (batch {})", self.name, status.id))?;
        Ok(raw
            .into_iter()
            .map(|(id, r)| {
                let a = match (r, job_of(&id)) {
                    (Ok(text), Some(job)) => {
                        job.check(&text, list, self.proposing.max, self.style.tag_style())
                    }
                    (Ok(_), None) => Err(format!("unknown custom_id {id:?}")),
                    (Err(e), _) => Err(e),
                };
                (id, a)
            })
            .collect())
    }
}

/// What one request asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    Summary,
    Tags,
    Both,
}

/// A checked reply.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Answer {
    pub summary: Option<String>,
    pub tags: Vec<Choice>,
    /// New names for the list, when proposing: normalized, none of them on the list (a list
    /// name or alias given as new is moved to `tags`), at most [`Proposing::max`].
    pub proposed: Vec<NewTag>,
    /// New names the tag style refused, with the reason.
    pub refused: Vec<String>,
}

impl Job {
    pub fn task(self) -> Task {
        match self {
            Job::Tags => Task::Tag,
            Job::Summary | Job::Both => Task::Summarize,
        }
    }

    /// The structured-output request for this job. With an empty `list`, a tag job asks only
    /// for new names (when `proposing`).
    pub fn request(
        self,
        item: &Item,
        list: &[TagOption],
        proposing: &Proposing,
        style: &StyleConfig,
    ) -> Request {
        let mut props = serde_json::Map::new();
        let mut system = Vec::new();
        if self != Job::Tags {
            system.push(SUMMARY_PROMPT.to_string());
            props.insert("summary".into(), json!({ "type": "string" }));
        }
        if self != Job::Summary {
            system.push(tag_prompt(list, proposing, style));
            if !list.is_empty() {
                props.insert("tags".into(), tags_schema(list));
            }
            if proposing.max > 0 {
                props.insert("new_tags".into(), new_tags_schema());
            }
        }
        let name = match self {
            Job::Summary => "summary",
            Job::Tags => "tags",
            Job::Both => "summary_and_tags",
        };
        let required: Vec<&String> = props.keys().collect();
        Request {
            system: system.join("\n\n"),
            user: describe(item),
            schema_name: name,
            schema: json!({
                "type": "object",
                "properties": props,
                "required": required,
                "additionalProperties": false,
            }),
            retry: None,
            max_tokens: None,
        }
    }

    /// Checks a reply: valid JSON with the job's fields; tags resolved against `list`; with
    /// `proposals` above 0, up to that many new names, fixed by `style` (or refused).
    pub fn check(
        self,
        reply: &str,
        list: &[TagOption],
        proposals: usize,
        style: TagStyle,
    ) -> Result<Answer, String> {
        let v: Value =
            serde_json::from_str(strip_fences(reply)).map_err(|e| format!("not JSON ({e})"))?;
        let canonical = Canonical::new(list).styled(style);
        let mut a = Answer::default();
        if self != Job::Tags {
            a.summary = Some(summary_of(&v)?);
        }
        if self != Job::Summary {
            // A reply to a request sent with an empty list (a batch collected after names were
            // accepted) has only new names.
            let only_new = proposals > 0 && v.get("tags").is_none() && v.get("new_tags").is_some();
            if !list.is_empty() && !only_new {
                a.tags = tags_of(&v, &canonical)?;
            }
            if proposals > 0 {
                let new = new_tags_of(&v)?;
                let raw = new_tags_into(new, &canonical, proposals, style, &mut a);
                let mut tags = std::mem::take(&mut a.tags);
                tags.extend(raw);
                a.tags = canonical.filter(tags).0;
            }
        }
        Ok(a)
    }
}

/// Splits checked new names: those on the list (or aliases of list tags) become choices, which
/// are returned; the rest go into `a.proposed`, fixed by `style`, deduplicated, at most `max`.
/// Names the style refuses go into `a.refused`.
fn new_tags_into(
    new: Vec<NewTag>,
    canonical: &Canonical,
    max: usize,
    style: TagStyle,
    a: &mut Answer,
) -> Vec<Choice> {
    let mut listed = Vec::new();
    let out = &mut a.proposed;
    for t in new {
        let name = style.fix(&t.name);
        if name.is_empty() {
            continue;
        }
        if canonical.resolve(&name).is_some() {
            listed.push(Choice {
                tag: name,
                confidence: None,
            });
        } else if let Err(why) = style.check(&name) {
            a.refused.push(why);
        } else if out.len() < max && !out.iter().any(|o| o.name == name) {
            let description = t.description.and_then(|d| {
                crate::tag::check_description(&crate::html::collapse_ws(&d))
                    .ok()
                    .flatten()
            });
            out.push(NewTag { name, description });
        }
    }
    listed
}

impl ChooseTags for Provider {
    fn choose_tags(&self, item: &Item, list: &[TagOption]) -> Result<Vec<Choice>> {
        Provider::choose_tags(self, item, list)
    }
}

/// The provider configured for `task`, if any.
pub fn for_task(
    ai: Option<&AiConfig>,
    providers: &BTreeMap<String, Spanned<ProviderConfig>>,
    task: Task,
) -> Result<Option<Provider>> {
    let name = ai.and_then(|a| match task {
        Task::Summarize => a.summarize.as_ref(),
        Task::Tag => a.tag.as_ref(),
    });
    let Some(name) = name else { return Ok(None) };
    let name = name.get_ref();
    let config = providers
        .get(name)
        .ok_or_else(|| anyhow!("[ai] {} names unknown provider {name:?}", task.as_str()))?;
    Provider::new(name, config.get_ref()).map(Some)
}

const SUMMARY_PROMPT: &str = "You summarize bookmarked web pages. Write two or three plain \
    sentences, at most 60 words, saying what the page is and what a reader gets from it. Write \
    in the language of the page. No preamble, no markdown, no quotes around it. Reply with JSON \
    only: {\"summary\": \"...\"}.";

const TAG_PROMPT: &str = "You tag bookmarked web pages with tags from a fixed list. Choose the \
    tags that clearly fit the page: usually one to three, and none if nothing fits well. Use only \
    names from the list below, exactly as written. Reply with JSON only: {\"tags\": [...]}.";

/// What a proposed name should look like, in the configured style (hyphens by default).
fn new_tag_style(style: &TagStyle) -> String {
    use crate::style::Separator;
    let (joined, example) = match style.separator {
        None | Some(Separator::Hyphen) => ("words joined by hyphens", "home-lab"),
        Some(Separator::Underscore) => ("words joined by underscores", "home_lab"),
        Some(Separator::None) => ("words run together", "homelab"),
    };
    let length = style
        .max_length
        .map_or(String::new(), |n| format!(", at most {n} characters"));
    format!(
        "A new tag is a short, general topic name in lowercase, {joined}{length}, like GitHub \
         topics (rust, wifi, security, {example}): one concept each (pages get several tags \
         rather than one compound tag), the common name, a singular noun, never a synonym of a \
         listed tag, and never about format or quality (article, tutorial, useful)."
    )
}

/// The tagging instructions, the user's guidelines (`[tags.style] guidelines`), the list (if
/// any), and with proposing, the names already proposed.
fn tag_prompt(list: &[TagOption], p: &Proposing, style: &StyleConfig) -> String {
    let n = p.max;
    let new_tag_style = new_tag_style(&style.tag_style());
    let new_format = "\"new_tags\": [{\"name\": \"...\", \"description\": \"...\"}]";
    let mut s = if n == 0 {
        TAG_PROMPT.to_string()
    } else if list.is_empty() {
        format!(
            "You tag bookmarked web pages. There is no tag list yet: propose up to {n} tags that \
             fit the page in \"new_tags\", usually one to three, each with a one-line description \
             of what belongs under it. {new_tag_style} Reply with JSON only: {{{new_format}}}."
        )
    } else {
        format!(
            "You tag bookmarked web pages with tags from a fixed list. Choose the tags that \
             clearly fit the page: usually one to three, and none if nothing fits well. Use only \
             names from the list below, exactly as written, in \"tags\". If a main topic of the \
             page has no tag on the list, propose up to {n} new tags for the list in \
             \"new_tags\", each with a one-line description of what belongs under it; otherwise \
             leave \"new_tags\" empty. {new_tag_style} Reply with JSON only: \
             {{\"tags\": [...], {new_format}}}."
        )
    };
    if let Some(g) = style.guidelines() {
        s.push_str("\n\nThe user's guidelines for tags (they apply to choosing and proposing):\n");
        s.push_str(g);
    }
    if !list.is_empty() {
        s.push_str("\n\n");
        s.push_str(&tag_list(list));
    }
    if n > 0 && !p.known.is_empty() {
        s.push_str(
            "\n\nNames already proposed for other pages; propose one of these rather than a \
             synonym when it fits: ",
        );
        s.push_str(&p.known.join(", "));
    }
    s
}

/// A checked seed reply: candidates per interest, and names the style refused.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Seeds {
    /// `(interest, candidates)`, in the order the interests were sent; every interest is there,
    /// perhaps with no candidates.
    pub groups: Vec<(String, Vec<NewTag>)>,
    pub refused: Vec<String>,
}

/// The seed request: the instructions and the style in `system`, the interests in `user`.
pub fn seed_request(interests: &[String], per: usize, style: &StyleConfig) -> Request {
    let new_tag_style = new_tag_style(&style.tag_style());
    let mut system = format!(
        "You help someone start the tag list for their bookmarks. They name broad interests; for \
         each, propose up to {per} tags they are likely to need for pages they would save under \
         it: its main subtopics, tools, and kinds of things, each with a one-line description of \
         what belongs under it. Include the interest itself when it makes a good tag. Prefer \
         tags that are useful on their own over narrow ones, and don't propose the same tag \
         under two interests. {new_tag_style} Reply with JSON only: {{\"interests\": \
         [{{\"interest\": \"...\", \"tags\": [{{\"name\": \"...\", \"description\": \
         \"...\"}}]}}]}}, with one entry per interest, named exactly as given."
    );
    if let Some(g) = style.guidelines() {
        system.push_str("\n\nThe user's guidelines for tags:\n");
        system.push_str(g);
    }
    let mut user = String::from("Interests:");
    for i in interests {
        user.push_str("\n- ");
        user.push_str(i);
    }
    Request {
        system,
        user,
        schema_name: "seed",
        schema: json!({
            "type": "object",
            "properties": {
                "interests": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "interest": { "type": "string", "enum": interests },
                            "tags": new_tags_schema(),
                        },
                        "required": ["interest", "tags"],
                        "additionalProperties": false,
                    },
                },
            },
            "required": ["interests"],
            "additionalProperties": false,
        }),
        retry: None,
        max_tokens: None,
    }
}

/// Checks a seed reply: an `interests` array; entries for interests that weren't sent are
/// dropped, and a reply with none of them is invalid. Names are fixed by `style`, deduplicated
/// across interests, at most `per` each.
pub fn check_seeds(
    reply: &str,
    interests: &[String],
    per: usize,
    style: TagStyle,
) -> Result<Seeds, String> {
    let v: Value =
        serde_json::from_str(strip_fences(reply)).map_err(|e| format!("not JSON ({e})"))?;
    let arr = v
        .get("interests")
        .and_then(Value::as_array)
        .ok_or("no \"interests\" array")?;
    let mut s = Seeds {
        groups: interests.iter().map(|i| (i.clone(), Vec::new())).collect(),
        refused: Vec::new(),
    };
    let mut seen: Vec<String> = Vec::new();
    let mut matched = false;
    for entry in arr {
        let Some(name) = entry.get("interest").and_then(Value::as_str) else {
            continue;
        };
        let name = crate::store::normalize_tag(name);
        let Some(g) = s.groups.iter_mut().find(|(i, _)| *i == name) else {
            continue;
        };
        matched = true;
        let tags = entry.get("tags").and_then(new_tags_in).unwrap_or_default();
        for t in tags {
            let n = style.fix(&t.name);
            if n.is_empty() || seen.contains(&n) || g.1.len() >= per {
                continue;
            }
            if let Err(why) = style.check(&n) {
                s.refused.push(why);
                continue;
            }
            let description = t.description.and_then(|d| {
                crate::tag::check_description(&crate::html::collapse_ws(&d))
                    .ok()
                    .flatten()
            });
            seen.push(n.clone());
            g.1.push(NewTag {
                name: n,
                description,
            });
        }
    }
    if !matched && !interests.is_empty() {
        return Err(format!(
            "no entry for any of the interests sent ({})",
            interests.join(", ")
        ));
    }
    Ok(s)
}

/// Most characters of a summary or page text sent per bookmark in a draft.
pub const DRAFT_TEXT: usize = 300;

/// What a draft sends about one bookmark: no URL beyond the site, no note, no tags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DraftItem {
    pub site: String,
    pub title: Option<String>,
    /// The summary, or the start of the page text; cut to [`DRAFT_TEXT`] characters.
    pub text: Option<String>,
}

/// What guides a draft besides the sample.
#[derive(Debug, Clone, Copy)]
pub struct DraftGuide<'a> {
    /// The canonical list; empty to draft one from scratch.
    pub list: &'a [TagOption],
    /// Stored interests.
    pub interests: &'a [String],
    /// Names drafted by earlier requests, to reuse rather than coin synonyms of.
    pub known: &'a [String],
    /// Most new tags per request.
    pub max: usize,
}

/// The draft request: instructions, style, guidelines, interests, list, and names drafted so far
/// in `system`; the sample in `user`.
pub fn draft_request(items: &[DraftItem], g: &DraftGuide, style: &StyleConfig) -> Request {
    let max = g.max;
    let new_tag_style = new_tag_style(&style.tag_style());
    let listed = !g.list.is_empty();
    let mut system = String::from(
        "You help someone build the tag list for their bookmarks, from a sample of them: the \
         site, the title, and sometimes a summary or the start of the page. ",
    );
    if listed {
        system.push_str(&format!(
            "They already have the tag list below. Propose only additions: up to {max} new tags \
             for topics that recur in the sample and that no listed tag covers, each with a \
             one-line description of what belongs under it. Never propose a listed tag, one of \
             its aliases, or a synonym of one. Also point out listed tags that look redundant \
             (a synonym of another tag, or so close to it that one would do) in \"redundant\", \
             with the listed or new tag to merge each into and a short reason; leave it empty \
             when none are. "
        ));
    } else {
        system.push_str(&format!(
            "Draft their tag list: up to {max} tags that together would tag most of these \
             bookmarks well, each with a one-line description of what belongs under it. "
        ));
    }
    system.push_str(
        "Prefer tags that would fit several bookmarks over ones that fit a single page, and \
         general topics over narrow ones; a page may get several tags, so tags should combine \
         rather than overlap. ",
    );
    if !g.interests.is_empty() {
        system.push_str(&format!(
            "Their interests are: {}. Give each tag the interest it falls under in \
             \"interest\", or \"\" for none; tags outside these interests are welcome where the \
             sample calls for them. ",
            g.interests.join(", ")
        ));
    }
    system.push_str(&new_tag_style);
    let tag_format = if g.interests.is_empty() {
        "{\"name\": \"...\", \"description\": \"...\"}"
    } else {
        "{\"name\": \"...\", \"description\": \"...\", \"interest\": \"...\"}"
    };
    let redundant_format = if listed {
        ", \"redundant\": [{\"tag\": \"...\", \"into\": \"...\", \"reason\": \"...\"}]"
    } else {
        ""
    };
    system.push_str(&format!(
        " Reply with JSON only: {{\"tags\": [{tag_format}]{redundant_format}}}."
    ));
    if let Some(gl) = style.guidelines() {
        system.push_str("\n\nThe user's guidelines for tags:\n");
        system.push_str(gl);
    }
    if listed {
        system.push_str("\n\n");
        system.push_str(&tag_list(g.list));
    }
    if !g.known.is_empty() {
        system.push_str(
            "\n\nTags already drafted from other bookmarks of the same library; propose one of \
             these again (exactly as written) when it fits this sample, rather than a synonym: ",
        );
        system.push_str(&g.known.join(", "));
    }
    let mut user = String::from("Bookmarks:");
    for it in items {
        user.push_str("\n- ");
        user.push_str(if it.site.is_empty() { "?" } else { &it.site });
        if let Some(t) = it.title.as_deref().filter(|t| !t.trim().is_empty()) {
            user.push_str(": ");
            user.push_str(&crate::html::collapse_ws(t));
        }
        if let Some(t) = it.text.as_deref().filter(|t| !t.trim().is_empty()) {
            user.push_str("\n  ");
            user.push_str(&cut(&crate::html::collapse_ws(t), DRAFT_TEXT));
        }
    }
    let mut tag = json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "description": { "type": "string" },
        },
        "required": ["name", "description"],
        "additionalProperties": false,
    });
    if !g.interests.is_empty() {
        let mut names: Vec<&str> = g.interests.iter().map(String::as_str).collect();
        names.push("");
        tag["properties"]["interest"] = json!({ "type": "string", "enum": names });
        tag["required"] = json!(["name", "description", "interest"]);
    }
    let mut schema = json!({
        "type": "object",
        "properties": { "tags": { "type": "array", "items": tag } },
        "required": ["tags"],
        "additionalProperties": false,
    });
    if listed {
        let names: Vec<&str> = g.list.iter().map(|t| t.name.as_str()).collect();
        schema["properties"]["redundant"] = json!({
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "tag": { "type": "string", "enum": names },
                    "into": { "type": "string" },
                    "reason": { "type": "string" },
                },
                "required": ["tag", "into", "reason"],
                "additionalProperties": false,
            },
        });
        schema["required"] = json!(["tags", "redundant"]);
    }
    Request {
        system,
        user,
        schema_name: "draft",
        schema,
        retry: None,
        // About 60 tokens per tag; enough for a full reply, and a looping model stops early.
        max_tokens: Some(400 + 60 * u32::try_from(max).unwrap_or(100).min(100)),
    }
}

/// `s` cut to `n` characters, with `…` if it was longer.
fn cut(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((i, _)) => format!("{}…", s[..i].trim_end()),
        None => s.to_string(),
    }
}

/// Checks a draft reply: a `tags` array (a missing `redundant` means none). New names are fixed
/// by `style` (refused ones are reported), deduplicated, and cut to `max`; names on the list (or
/// aliases of listed tags) are left out, since a draft only adds. Interests not among those sent
/// count as none. A redundancy needs a listed tag (an alias counts as its tag) and something to
/// merge it into: another listed tag, a tag in this reply, or one drafted before.
pub fn check_draft(reply: &str, g: &DraftGuide, style: TagStyle) -> Result<Drafted, String> {
    let v: Value =
        serde_json::from_str(strip_fences(reply)).map_err(|e| format!("not JSON ({e})"))?;
    let arr = v
        .get("tags")
        .and_then(Value::as_array)
        .ok_or("no \"tags\" array")?;
    let listed = |n: &str| -> Option<String> {
        g.list
            .iter()
            .find(|t| t.name == n || t.aliases.iter().any(|a| a == n))
            .map(|t| t.name.clone())
    };
    let mut d = Drafted::default();
    // Names already on the list: a small model sometimes echoes the list back.
    let mut echoed = 0;
    for t in arr {
        let (name, description) = match t {
            Value::String(s) => (s.as_str(), None),
            _ => match t.get("name").and_then(Value::as_str) {
                Some(n) => (n, t.get("description").and_then(Value::as_str)),
                None => continue,
            },
        };
        let n = style.fix(name);
        if listed(&n).is_some() {
            echoed += 1;
            continue;
        }
        if n.is_empty() || d.tags.iter().any(|o| o.name == n) {
            continue;
        }
        if d.tags.len() >= g.max {
            break;
        }
        if let Err(why) = style.check(&n) {
            d.refused.push(why);
            continue;
        }
        let interest = t
            .get("interest")
            .and_then(Value::as_str)
            .map(crate::store::normalize_tag)
            .filter(|i| g.interests.contains(i));
        d.tags.push(DraftTag {
            name: n,
            description: description.and_then(clean_description),
            interest,
        });
    }
    for r in v
        .get("redundant")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(tag) = r
            .get("tag")
            .and_then(Value::as_str)
            .and_then(|t| listed(&style.fix(t)))
        else {
            continue;
        };
        let Some(into) = r.get("into").and_then(Value::as_str).map(|i| style.fix(i)) else {
            continue;
        };
        let into = match listed(&into) {
            Some(l) => l,
            None if d.tags.iter().any(|t| t.name == into) || g.known.contains(&into) => into,
            None => continue,
        };
        if into == tag || d.redundant.iter().any(|o| o.tag == tag) {
            continue;
        }
        d.redundant.push(Redundant {
            tag,
            into,
            reason: r
                .get("reason")
                .and_then(Value::as_str)
                .and_then(clean_description),
        });
    }
    if echoed > 0 && d.tags.is_empty() && d.refused.is_empty() && d.redundant.is_empty() {
        return Err(format!(
            "every tag in \"tags\" ({echoed}) is already on the list; propose only tags that \
             aren't, or none"
        ));
    }
    Ok(d)
}

/// A description from a reply: whitespace collapsed; `None` if empty or not acceptable.
fn clean_description(d: &str) -> Option<String> {
    crate::tag::check_description(&crate::html::collapse_ws(d))
        .ok()
        .flatten()
}

/// The new_tags array: names with descriptions. Its length isn't constrained in the schema (not
/// every structured-output implementation supports `maxItems`); extra names are dropped.
fn new_tags_schema() -> Value {
    json!({
        "type": "array",
        "items": {
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "description": { "type": "string" },
            },
            "required": ["name", "description"],
            "additionalProperties": false,
        },
    })
}

/// New names from a reply. A missing `new_tags` means none (a model in JSON mode may leave it
/// out); entries without a name string are skipped.
fn new_tags_of(v: &Value) -> Result<Vec<NewTag>, String> {
    match v.get("new_tags") {
        Some(arr) => new_tags_in(arr).ok_or_else(|| "\"new_tags\" is not an array".into()),
        None => Ok(Vec::new()),
    }
}

/// The names in an array of `{"name", "description"}` objects (or bare strings); `None` if it
/// isn't an array.
fn new_tags_in(arr: &Value) -> Option<Vec<NewTag>> {
    Some(
        arr.as_array()?
            .iter()
            .filter_map(|t| {
                // A bare string is a name without a description.
                let (name, description) = match t {
                    Value::String(s) => (s.as_str(), None),
                    _ => (
                        t.get("name")?.as_str()?,
                        t.get("description").and_then(Value::as_str),
                    ),
                };
                Some(NewTag {
                    name: name.to_string(),
                    description: description.map(str::to_string),
                })
            })
            .collect(),
    )
}

/// The canonical list for the prompt: `- name: description (also: aliases)`.
fn tag_list(list: &[TagOption]) -> String {
    let mut s = String::from("Tags:");
    for t in list {
        s.push_str("\n- ");
        s.push_str(&t.name);
        if let Some(d) = &t.description {
            s.push_str(": ");
            s.push_str(d);
        }
        if !t.aliases.is_empty() {
            s.push_str(&format!(" (also: {})", t.aliases.join(", ")));
        }
    }
    s
}

/// The tags array: names constrained to the list.
fn tags_schema(list: &[TagOption]) -> Value {
    let names: Vec<&str> = list.iter().map(|t| t.name.as_str()).collect();
    json!({ "type": "array", "items": { "type": "string", "enum": names } })
}

/// What is sent about a bookmark: URL, title, note, summary, and page text, when present.
pub fn describe(item: &Item) -> String {
    let mut s = format!("URL: {}", item.url);
    for (label, v) in [
        ("Title", &item.title),
        ("Note", &item.note),
        ("Summary", &item.summary),
    ] {
        if let Some(v) = v.as_deref().filter(|v| !v.trim().is_empty()) {
            s.push_str(&format!("\n{label}: {}", v.trim()));
        }
    }
    if let Some(t) = item.text.as_deref().filter(|t| !t.trim().is_empty()) {
        let t = match t.char_indices().nth(MAX_TEXT) {
            Some((i, _)) => &t[..i],
            None => t,
        };
        s.push_str("\n\nPage text:\n");
        s.push_str(t);
    }
    s
}

/// Some models wrap JSON in a Markdown code fence even when told not to.
fn strip_fences(s: &str) -> &str {
    let t = s.trim();
    match t.strip_prefix("```") {
        Some(rest) => {
            let rest = rest.trim_start_matches(|c: char| c.is_ascii_alphabetic());
            rest.strip_suffix("```").unwrap_or(rest).trim()
        }
        None => t,
    }
}

fn summary_of(v: &Value) -> Result<String, String> {
    let s = v
        .get("summary")
        .and_then(Value::as_str)
        .ok_or("no \"summary\" string")?;
    let s = crate::html::collapse_ws(s);
    let n = s.chars().count();
    if n == 0 {
        return Err("the summary is empty".into());
    }
    if n > MAX_SUMMARY {
        return Err(format!(
            "the summary has {n} characters; at most {MAX_SUMMARY} are allowed"
        ));
    }
    Ok(s)
}

fn tags_of(v: &Value, canonical: &Canonical) -> Result<Vec<Choice>, String> {
    let arr = v
        .get("tags")
        .and_then(Value::as_array)
        .ok_or("no \"tags\" array")?;
    let raw = arr
        .iter()
        .filter_map(Value::as_str)
        .map(|t| Choice {
            tag: t.to_string(),
            confidence: None,
        })
        .collect();
    Ok(canonical.filter(raw).0)
}

/// The error message in an API's error body (`{"error": {"message": ...}}` and similar), else
/// the start of the body.
pub(crate) fn error_message(body: &str) -> String {
    let v: Option<Value> = serde_json::from_str(body).ok();
    let msg = v.as_ref().and_then(|v| {
        let e = v.get("error").unwrap_or(v);
        e.get("message")
            .and_then(Value::as_str)
            .or_else(|| e.as_str())
            .map(str::to_string)
    });
    let msg = msg.unwrap_or_else(|| crate::html::collapse_ws(body));
    match msg.char_indices().nth(300) {
        Some((i, _)) => format!("{}…", &msg[..i]),
        None => msg,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_items() {
        let item = Item {
            url: "https://a.example/".into(),
            title: Some("A".into()),
            note: Some("  ".into()),
            summary: None,
            text: Some("body".into()),
        };
        assert_eq!(
            describe(&item),
            "URL: https://a.example/\nTitle: A\n\nPage text:\nbody"
        );
    }

    #[test]
    fn strips_code_fences() {
        assert_eq!(strip_fences("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_fences(" {\"a\":1} "), "{\"a\":1}");
    }

    #[test]
    fn error_messages() {
        assert_eq!(
            error_message(r#"{"error":{"message":"bad key"}}"#),
            "bad key"
        );
        assert_eq!(error_message(r#"{"error":"nope"}"#), "nope");
        assert_eq!(
            error_message("<html>\n oops </html>"),
            "<html> oops </html>"
        );
    }

    #[test]
    fn checks_summaries() {
        assert_eq!(summary_of(&json!({"summary": " a\n b "})).unwrap(), "a b");
        assert!(summary_of(&json!({"summary": " "})).is_err());
        assert!(summary_of(&json!({"summary": "x".repeat(1001)})).is_err());
        assert!(summary_of(&json!({"tags": []})).is_err());
    }
    #[test]
    fn checks_seed_replies() {
        use crate::style::Separator;
        let sent = vec!["nix".to_string(), "audio".to_string()];
        let style = TagStyle {
            separator: Some(Separator::Hyphen),
            max_length: Some(12),
        };
        let reply = json!({"interests": [
            {"interest": "Nix", "tags": [
                {"name": "Nix Flakes", "description": " flakes\n and lock files "},
                {"name": "home_manager_modules", "description": "too long"},
                {"name": "nixpkgs", "description": "packages"},
                "nixos",
            ]},
            {"interest": "audio", "tags": [{"name": "nixpkgs", "description": "dup"}]},
            {"interest": "cooking", "tags": [{"name": "knives", "description": "x"}]},
        ]});
        let s = check_seeds(&reply.to_string(), &sent, 2, style).unwrap();
        let names = |g: &[NewTag]| g.iter().map(|t| t.name.clone()).collect::<Vec<_>>();
        assert_eq!(s.groups[0].0, "nix");
        assert_eq!(names(&s.groups[0].1), ["nix-flakes", "nixpkgs"]);
        assert_eq!(
            s.groups[0].1[0].description.as_deref(),
            Some("flakes and lock files")
        );
        // A name already under another interest isn't repeated; unsent interests are dropped.
        assert_eq!(s.groups[1], ("audio".to_string(), Vec::new()));
        assert_eq!(s.groups.len(), 2);
        assert_eq!(s.refused.len(), 1);
        assert!(s.refused[0].contains("max_length = 12"));

        assert!(check_seeds("{\"interests\": []}", &sent, 2, style).is_err());
        assert!(check_seeds("{\"tags\": []}", &sent, 2, style).is_err());
    }

    #[test]
    fn seed_requests_send_only_interests_and_style() {
        let style = StyleConfig {
            guidelines: Some("Prefer tool names.".into()),
            ..StyleConfig::default()
        };
        let r = seed_request(&["nix".into(), "audio".into()], 5, &style);
        assert_eq!(r.user, "Interests:\n- nix\n- audio");
        assert!(r.system.contains("up to 5 tags"));
        assert!(r.system.contains("words joined by hyphens"));
        assert!(
            r.system
                .ends_with("guidelines for tags:\nPrefer tool names.")
        );
        assert_eq!(
            r.schema["properties"]["interests"]["items"]["properties"]["interest"]["enum"],
            json!(["nix", "audio"])
        );
    }

    fn opt(name: &str, aliases: &[&str]) -> TagOption {
        TagOption {
            name: name.into(),
            aliases: aliases.iter().map(|a| a.to_string()).collect(),
            description: None,
        }
    }

    #[test]
    fn checks_draft_replies() {
        let list = [opt("javascript", &["js-lang"]), opt("css", &[])];
        let interests = ["web".to_string(), "nix".to_string()];
        let known = ["flake".to_string()];
        let g = DraftGuide {
            list: &list,
            interests: &interests,
            known: &known,
            max: 3,
        };
        let style = TagStyle {
            max_length: Some(12),
            ..TagStyle::default()
        };
        let reply = json!({
            "tags": [
                {"name": "JavaScript", "description": "listed"},
                {"name": "js-lang", "description": "an alias of a listed tag"},
                {"name": "Web Assembly", "description": "  WebAssembly\n modules ", "interest": "Web"},
                {"name": "a-very-long-tag-name", "description": "too long"},
                {"name": "wasm", "description": "again", "interest": "nope"},
                "web-assembly",
                {"name": "nix", "description": "", "interest": "nix"},
                {"name": "rust", "description": "over max"},
            ],
            "redundant": [
                {"tag": "css", "into": "Web Assembly", "reason": "drafted"},
                {"tag": "css", "into": "javascript", "reason": "a second time"},
                {"tag": "js-lang", "into": "flake", "reason": "an alias, into a known name"},
                {"tag": "javascript", "into": "javascript", "reason": "itself"},
                {"tag": "missing", "into": "css"},
                {"tag": "css", "into": "unknown"},
            ],
        })
        .to_string();
        let d = check_draft(&reply, &g, style).unwrap();
        assert_eq!(
            d.tags,
            [
                DraftTag {
                    name: "web-assembly".into(),
                    description: Some("WebAssembly modules".into()),
                    interest: Some("web".into()),
                },
                DraftTag {
                    name: "wasm".into(),
                    description: Some("again".into()),
                    interest: None,
                },
                DraftTag {
                    name: "nix".into(),
                    description: None,
                    interest: Some("nix".into()),
                },
            ]
        );
        assert_eq!(d.refused.len(), 1);
        assert!(d.refused[0].contains("max_length = 12"), "{:?}", d.refused);
        assert_eq!(
            d.redundant,
            [
                Redundant {
                    tag: "css".into(),
                    into: "web-assembly".into(),
                    reason: Some("drafted".into()),
                },
                Redundant {
                    tag: "javascript".into(),
                    into: "flake".into(),
                    reason: Some("an alias, into a known name".into()),
                },
            ]
        );

        // A reply that only repeats the list is sent back; an empty one is fine.
        let echo = json!({"tags": [{"name": "css", "description": "x"}]}).to_string();
        let why = check_draft(&echo, &g, style).unwrap_err();
        assert!(why.contains("already on the list"), "{why}");
        let none = check_draft("```json\n{\"tags\": []}\n```", &g, style).unwrap();
        assert_eq!(none, Drafted::default());
        assert!(check_draft("{\"interests\": []}", &g, style).is_err());
        assert!(check_draft("tags", &g, style).is_err());
    }

    #[test]
    fn draft_requests_send_sites_titles_and_text() {
        let long = "word ".repeat(200);
        let items = [
            DraftItem {
                site: "github.com".into(),
                title: Some("  nix\n flakes ".into()),
                text: Some(long.clone()),
            },
            DraftItem {
                site: String::new(),
                title: None,
                text: None,
            },
        ];
        let list = [opt("rust", &["rs"])];
        let g = DraftGuide {
            list: &list,
            interests: &[],
            known: &[],
            max: 10,
        };
        let r = draft_request(&items, &g, &StyleConfig::default());
        let text = format!(
            "{}…",
            long.trim_end()
                .chars()
                .take(DRAFT_TEXT)
                .collect::<String>()
                .trim_end()
        );
        assert_eq!(
            r.user,
            format!("Bookmarks:\n- github.com: nix flakes\n  {text}\n- ?")
        );
        assert!(r.system.contains("up to 10 new tags"));
        assert!(r.system.ends_with("Tags:\n- rust (also: rs)"));
        assert!(!r.system.contains("interest"));
        assert_eq!(r.max_tokens, Some(1000));
        assert_eq!(r.schema["required"], json!(["tags", "redundant"]));
        assert!(r.schema["properties"]["tags"]["items"]["properties"]["interest"].is_null());

        // From scratch: no list, nothing redundant asked for.
        let g = DraftGuide { list: &[], ..g };
        let r = draft_request(&items, &g, &StyleConfig::default());
        assert!(r.system.contains("Draft their tag list: up to 10 tags"));
        assert!(!r.system.contains("Tags:"));
        assert!(r.schema["properties"]["redundant"].is_null());
    }
}
