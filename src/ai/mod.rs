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
pub mod openai;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use toml::Spanned;

use crate::choose::{Canonical, Choice, ChooseTags, Item, NewTag, TagOption};

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
    /// provider is used. Exclusive with `api_key_env`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_file: Option<String>,
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
        if self.api_key_env.is_some() && self.api_key_file.is_some() {
            return Err("set `api_key_env` or `api_key_file`, not both".into());
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

    /// The API key, read now. `None` when no key is configured (local servers).
    pub fn api_key(&self) -> Result<Option<String>> {
        if let Some(path) = &self.api_key_file {
            return read_key_file(Path::new(path)).map(Some);
        }
        let Some(var) = &self.api_key_env else {
            return Ok(None);
        };
        match std::env::var(var) {
            Ok(k) if !k.trim().is_empty() => Ok(Some(k.trim().to_string())),
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
}

impl Provider {
    /// Builds provider `name` from its config. Reads its API key now.
    pub fn new(name: &str, config: &ProviderConfig) -> Result<Provider> {
        let at = || format!("provider {name}");
        config.check().map_err(|e| anyhow!(e)).with_context(at)?;
        let key = config.api_key().with_context(at)?;
        let llm: Box<dyn Llm> = match config.kind {
            Kind::OpenaiCompatible => Box::new(openai::OpenAi::new(config, key)),
            Kind::Anthropic => match key {
                Some(key) => Box::new(anthropic::Anthropic::new(config, key)),
                None => bail!(
                    "provider {name}: kind \"anthropic\" needs an API key: set api_key_env or \
                     api_key_file"
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
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Asks for new tag names in tag requests from now on; see [`Proposing`].
    pub fn set_proposing(&mut self, p: Proposing) {
        self.proposing = p;
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
        let mut req = job.request(item, list, &self.proposing);
        for attempt in 0..2 {
            let reply = self.llm.complete(&req).with_context(at)?;
            match job.check(&reply, list, self.proposing.max) {
                Ok(a) => return Ok(a),
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
                    b.body(&job.request(item, list, &self.proposing)),
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
                    (Ok(text), Some(job)) => job.check(&text, list, self.proposing.max),
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
    pub fn request(self, item: &Item, list: &[TagOption], proposing: &Proposing) -> Request {
        let mut props = serde_json::Map::new();
        let mut system = Vec::new();
        if self != Job::Tags {
            system.push(SUMMARY_PROMPT.to_string());
            props.insert("summary".into(), json!({ "type": "string" }));
        }
        if self != Job::Summary {
            system.push(tag_prompt(list, proposing));
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
        }
    }

    /// Checks a reply: valid JSON with the job's fields; tags resolved against `list`; with
    /// `proposals` above 0, up to that many new names.
    pub fn check(
        self,
        reply: &str,
        list: &[TagOption],
        proposals: usize,
    ) -> Result<Answer, String> {
        let v: Value =
            serde_json::from_str(strip_fences(reply)).map_err(|e| format!("not JSON ({e})"))?;
        let canonical = Canonical::new(list);
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
                let raw = new_tags_into(new, &canonical, proposals, &mut a.proposed);
                let mut tags = std::mem::take(&mut a.tags);
                tags.extend(raw);
                a.tags = canonical.filter(tags).0;
            }
        }
        Ok(a)
    }
}

/// Splits checked new names: those on the list (or aliases of list tags) become choices, which
/// are returned; the rest go into `out`, deduplicated, at most `max`.
fn new_tags_into(
    new: Vec<NewTag>,
    canonical: &Canonical,
    max: usize,
    out: &mut Vec<NewTag>,
) -> Vec<Choice> {
    let mut listed = Vec::new();
    for t in new {
        let name = crate::store::normalize_tag(&t.name);
        if name.is_empty() {
            continue;
        }
        if canonical.resolve(&name).is_some() {
            listed.push(Choice {
                tag: name,
                confidence: None,
            });
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

/// What a proposed name should look like.
const NEW_TAG_STYLE: &str = "A new tag is a short, general topic name in lowercase, words joined \
    by hyphens, like GitHub topics (rust, wifi, security, home-lab): one concept each (pages get \
    several tags rather than one compound tag), the common name, a singular noun, never a synonym \
    of a listed tag, and never about format or quality (article, tutorial, useful).";

/// The tagging instructions, the list (if any), and with proposing, the names already proposed.
fn tag_prompt(list: &[TagOption], p: &Proposing) -> String {
    let n = p.max;
    let new_format = "\"new_tags\": [{\"name\": \"...\", \"description\": \"...\"}]";
    let mut s = if n == 0 {
        TAG_PROMPT.to_string()
    } else if list.is_empty() {
        format!(
            "You tag bookmarked web pages. There is no tag list yet: propose up to {n} tags that \
             fit the page in \"new_tags\", usually one to three, each with a one-line description \
             of what belongs under it. {NEW_TAG_STYLE} Reply with JSON only: {{{new_format}}}."
        )
    } else {
        format!(
            "You tag bookmarked web pages with tags from a fixed list. Choose the tags that \
             clearly fit the page: usually one to three, and none if nothing fits well. Use only \
             names from the list below, exactly as written, in \"tags\". If a main topic of the \
             page has no tag on the list, propose up to {n} new tags for the list in \
             \"new_tags\", each with a one-line description of what belongs under it; otherwise \
             leave \"new_tags\" empty. {NEW_TAG_STYLE} Reply with JSON only: \
             {{\"tags\": [...], {new_format}}}."
        )
    };
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
    let Some(arr) = v.get("new_tags") else {
        return Ok(Vec::new());
    };
    let arr = arr.as_array().ok_or("\"new_tags\" is not an array")?;
    Ok(arr
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
        .collect())
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
}
