use std::io::{self, BufWriter, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand, ValueEnum};
use shlv::config::{self, Config};
use shlv::health::{Health, HealthOptions, StyleKind};
use shlv::store::normalize_tag;
use shlv::tag::{format_canonical, format_canonical_nix, format_canonical_toml, parse_canonical};
use shlv::{
    Bookmark, Diverted, Format, ImportOptions, ListQuery, NewBookmark, NonCanonicalRuleTags,
    NonCanonicalTags, Pinned, Proposal, ProposalState, RuleStats, Store, SuggestMode, Suggestion,
    TagInfo, TagSort,
};
use shlv::{netscape, transfer};

#[cfg(feature = "ai")]
mod ai_cmd;
mod backfill_cmd;
#[cfg(feature = "ai")]
mod draft_cmd;
mod edit_cmd;
#[cfg(feature = "model")]
mod model_cmd;
// The interactive screen: `edit --pick`, `suggest --tag --pick`, `tag seed`, `tag draft`, and
// `review`.
mod review_cmd;
mod screen;
#[cfg(feature = "ai")]
mod seed_cmd;
mod tui;

/// A fast bookmark manager.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Without one, `shlv` on a terminal opens the TUI.
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Add a bookmark (or update it, if the URL is already saved)
    Add {
        url: String,
        /// Tag (repeatable)
        #[arg(short, long = "tag", value_name = "TAG")]
        tags: Vec<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        note: Option<String>,
        /// Fetch the page and use its title (unless --title is given)
        #[arg(long)]
        fetch: bool,
    },
    /// List bookmarks, newest first
    Ls {
        /// Only bookmarks with this tag (repeatable; all must match)
        #[arg(short, long = "tag", value_name = "TAG")]
        tags: Vec<String>,
        /// Maximum results; 0 for all
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: usize,
        /// One JSON object per line
        #[arg(long)]
        json: bool,
    },
    /// Search bookmarks: words, "phrases", #tag, tag:x, site:host, AND/OR/NOT
    Search {
        #[arg(required = true)]
        query: Vec<String>,
        /// Maximum results; 0 for all
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: usize,
        /// One JSON object per line
        #[arg(long)]
        json: bool,
    },
    /// Show one bookmark in full
    Show {
        id: i64,
        /// Print as JSON
        #[arg(long)]
        json: bool,
    },
    /// Change bookmarks' tags, title, or note: by id, or ids from stdin (`search ... | shlv edit -`)
    Edit(edit_cmd::EditArgs),
    /// Delete bookmarks by id or URL
    Rm {
        #[arg(required = true, value_name = "ID|URL")]
        targets: Vec<String>,
    },
    /// Open a bookmark in the browser ($BROWSER, else the system opener)
    Open { id: i64 },
    /// Manage tags: list, rename, merge, aliases, canonical list
    Tag {
        #[command(subcommand)]
        cmd: TagCmd,
    },
    /// Domain rules: bookmarks on a host get tags suggested (list, add, remove)
    Rule {
        #[command(subcommand)]
        cmd: RuleCmd,
    },
    /// Review bookmarks in batches: accept, toggle, or add tags one key each, then decide the
    /// names proposed, back-fill new tags, and retrain
    Review(review_cmd::ReviewArgs),
    /// Suggest tags for bookmarks (domain rules from config), and review the suggestions
    #[command(
        args_conflicts_with_subcommands = true,
        subcommand_negates_reqs = true,
        after_help = "With [tags] suggestions = \"auto\" in the config, suggested tags are \
                      applied at once instead of waiting for `shlv suggest accept`."
    )]
    Suggest {
        #[command(subcommand)]
        cmd: Option<SuggestCmd>,
        /// Bookmarks to suggest tags for
        #[arg(
            value_name = "ID",
            required_unless_present = "all",
            conflicts_with = "all"
        )]
        ids: Vec<i64>,
        /// Suggest tags for every bookmark (rules: one transaction)
        #[arg(long)]
        all: bool,
        /// Back-fill one canonical tag: offer only TAG (classifier and [ai] tag provider, no
        /// rules) and list its candidates, reviewed bookmarks first
        #[arg(long, value_name = "TAG")]
        tag: Option<String>,
        /// With --tag, go through the candidates one key each: y accepts, n rejects
        #[arg(long, requires = "tag")]
        pick: bool,
        #[cfg(feature = "ai")]
        #[command(flatten)]
        llm: ai_cmd::LlmArgs,
    },
    /// Import bookmarks: a browser export (Netscape HTML), JSON lines, or a list of URLs ("-" for
    /// stdin)
    Import {
        file: PathBuf,
        /// Input format (default: detected from the content)
        #[arg(long, value_enum)]
        format: Option<ImportFormat>,
        /// Tag each bookmark with the names of its folders
        #[arg(long)]
        folders_as_tags: bool,
        /// Drop tags that aren't canonical instead of failing (strict_tags)
        #[arg(long)]
        drop_unknown_tags: bool,
        /// Keep tags that aren't canonical as proposals for the list instead of tags (always on
        /// with strict_tags = "propose"); review them with `shlv tag proposals`
        #[arg(long, conflicts_with = "drop_unknown_tags")]
        propose_unknown_tags: bool,
        /// Don't run the domain rules ([tags.rules]) on the imported bookmarks
        #[arg(long)]
        no_rules: bool,
        /// Keep only URLs, add dates, and titles: no tags, notes, or summaries (for moving from
        /// another app and tagging afresh)
        #[arg(long, conflicts_with_all = ["folders_as_tags", "drop_unknown_tags", "propose_unknown_tags"])]
        urls_only: bool,
        /// With --urls-only, drop titles too (`shlv fetch --all` gets them again)
        #[arg(long, requires = "urls_only")]
        no_titles: bool,
    },
    /// Print every bookmark: JSON lines (like `ls --json`) or Netscape HTML for browsers
    Export {
        #[arg(long, value_enum, default_value_t = FormatArg::Json)]
        format: FormatArg,
    },
    /// Fetch page titles: for the IDs given, or with --all for every bookmark without one
    #[cfg(feature = "fetch")]
    Fetch {
        #[arg(
            value_name = "ID",
            required_unless_present = "all",
            conflicts_with = "all"
        )]
        ids: Vec<i64>,
        /// Every bookmark without a title
        #[arg(long)]
        all: bool,
        /// With --all, refetch titles that are already set
        #[arg(long, requires = "all")]
        force: bool,
        /// Requests at a time
        #[arg(short, long, default_value_t = 4, value_parser = clap::value_parser!(u16).range(1..=32))]
        jobs: u16,
    },
    /// Summarize bookmarks with the [ai] summarize provider: fetch each page, send its text
    #[cfg(feature = "ai")]
    Summarize {
        #[arg(
            value_name = "ID",
            required_unless_present = "all",
            conflicts_with = "all"
        )]
        ids: Vec<i64>,
        /// Every bookmark without a summary
        #[arg(long)]
        all: bool,
        /// Redo bookmarks that already have a summary
        #[arg(long)]
        force: bool,
        /// Requests at a time
        #[arg(short, long, default_value_t = 2, value_parser = clap::value_parser!(u16).range(1..=16))]
        jobs: u16,
        /// Go ahead when more than 20 bookmarks would be sent
        #[arg(short, long)]
        yes: bool,
        /// Submit one Message Batch (half price, results within 24 hours) instead of live requests
        #[arg(long)]
        batch: bool,
    },
    /// Train, inspect, or evaluate the tag classifier
    #[cfg(feature = "model")]
    Model {
        #[command(subcommand)]
        cmd: model_cmd::ModelCmd,
    },
    /// Collect Message Batches submitted with --batch (kind "anthropic")
    #[cfg(feature = "ai")]
    Batch {
        #[command(subcommand)]
        cmd: ai_cmd::BatchCmd,
    },
    /// Show where the config file is and what it sets
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print the config file path ($SHLV_CONFIG, else $XDG_CONFIG_HOME/shlv/config.toml)
    Path,
    /// Print the effective config, defaults included
    Show,
}

#[derive(Subcommand)]
enum SuggestCmd {
    /// List pending suggestions, newest bookmarks first
    Ls {
        /// Only this bookmark's
        id: Option<i64>,
        /// One JSON object per suggestion and line
        #[arg(long)]
        json: bool,
    },
    /// Accept a bookmark's pending suggestions (all, or the TAGs given), or every one with --all
    Accept {
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        id: Option<i64>,
        #[arg(conflicts_with = "all")]
        tags: Vec<String>,
        /// Accept every pending suggestion (one transaction)
        #[arg(long)]
        all: bool,
    },
    /// Reject a bookmark's pending suggestions (all, or the TAGs given); they aren't offered again
    Reject { id: i64, tags: Vec<String> },
}

#[derive(Subcommand)]
enum TagCmd {
    /// List tags with their bookmark counts; * marks canonical tags, = lists aliases
    Ls {
        /// Sort order
        #[arg(long, value_enum, default_value_t = SortArg::Name)]
        sort: SortArg,
        /// One JSON object per line
        #[arg(long)]
        json: bool,
    },
    /// Rename a tag (use merge if NEW already exists)
    Rename { old: String, new: String },
    /// Move every bookmark from FROM to INTO and leave FROM as an alias of INTO
    Merge { from: String, into: String },
    /// Make ALIAS resolve to TAG on input, or remove an alias with --rm
    Alias {
        alias: String,
        #[arg(required_unless_present = "rm", conflicts_with = "rm")]
        tag: Option<String>,
        /// Remove ALIAS
        #[arg(long)]
        rm: bool,
    },
    /// Describe what belongs under a tag, for tag suggestions ("" clears it)
    Describe { tag: String, text: String },
    /// Mark, unmark, list, import, or export canonical tags
    Canonical {
        #[command(subcommand)]
        cmd: CanonicalCmd,
    },
    /// Measure the tag list: redundant, too broad, rare, and badly named tags, coverage, and
    /// bookmarks no tag fits. Advisory: it changes nothing
    Health {
        /// Redundant pairs: each tag on at least this share of the other's bookmarks
        #[arg(long, value_name = "SHARE", default_value_t = 0.8, value_parser = share)]
        redundant: f64,
        /// Redundant pairs: together on at least this many bookmarks
        #[arg(long, value_name = "N", default_value_t = 3)]
        min_together: usize,
        /// Too broad: on at least this share of all bookmarks
        #[arg(long, value_name = "SHARE", default_value_t = 0.3, value_parser = share)]
        broad: f64,
        /// Rare: canonical tags on at most this many bookmarks
        #[arg(long, value_name = "N", default_value_t = 2)]
        rare: usize,
        /// Gaps: smallest group of untagged bookmarks to show
        #[arg(long, value_name = "N", default_value_t = 3)]
        min_group: usize,
        /// Gaps: most groups to show, by site and by title word
        #[arg(long, value_name = "N", default_value_t = 10)]
        groups: usize,
        /// Ask the [ai] tag provider to name the N largest gap groups (default 5); sends up to 15
        /// titles and URLs per group
        #[cfg(feature = "ai")]
        #[arg(long, value_name = "N", num_args = 0..=1, default_missing_value = "5")]
        name_gaps: Option<usize>,
        /// Print the report as one JSON object
        #[arg(long)]
        json: bool,
    },
    /// Expand interests into candidate tags with the [ai] tag provider and go through them one
    /// key each (keep, rename, merge, drop). Sends only the interests and the tag style
    #[cfg(feature = "ai")]
    Seed(seed_cmd::SeedArgs),
    /// Draft the list from a sample of bookmarks with the [ai] tag provider, guided by the list
    /// and stored interests, and go through it as `tag seed` does. Sends each sampled bookmark's
    /// site, title, and summary
    #[cfg(feature = "ai")]
    Draft(draft_cmd::DraftArgs),
    /// List stored interests (saved with `tag seed --save` or pinned in [tags] interests), or
    /// add and forget saved ones
    Interests {
        /// Store these interests
        #[arg(long, value_name = "INTEREST", num_args = 1..)]
        add: Vec<String>,
        /// Forget these saved interests (their tags stay)
        #[arg(long, value_name = "INTEREST", num_args = 1..)]
        rm: Vec<String>,
    },
    /// Review names proposed for the canonical list: list (the default), accept, alias, reject
    Proposals {
        #[command(subcommand)]
        cmd: Option<ProposalCmd>,
    },
}

#[derive(Subcommand)]
enum ProposalCmd {
    /// List proposals, those on the most bookmarks first
    Ls {
        /// One JSON object per line
        #[arg(long)]
        json: bool,
        /// List rejected names instead
        #[arg(long)]
        rejected: bool,
    },
    /// Add proposed names to the list (with their descriptions) and tag their bookmarks
    Accept {
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Make NAME an alias of TAG and tag NAME's bookmarks with TAG
    Alias { name: String, tag: String },
    /// Reject names: they aren't proposed again, from any source
    Reject {
        #[arg(required = true)]
        names: Vec<String>,
    },
}

#[derive(Subcommand)]
enum CanonicalCmd {
    /// Mark tags canonical (creating them if needed)
    Add {
        #[arg(required = true)]
        tags: Vec<String>,
    },
    /// Unmark canonical tags (the tags stay)
    Rm {
        #[arg(required = true)]
        tags: Vec<String>,
    },
    /// List canonical tags
    Ls,
    /// Add tags and aliases from a file ("-" for stdin): one tag per line, aliases after it
    Import { file: PathBuf },
    /// Print the canonical list in the format import reads, or in the config's shape
    Export {
        /// As a [tags.canonical] TOML table; # pinned marks tags the config declares
        #[arg(long, conflicts_with = "nix")]
        toml: bool,
        /// As the home-manager module's settings.tags.canonical; # pinned marks declared tags
        #[arg(long)]
        nix: bool,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum FormatArg {
    Json,
    Html,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ImportFormat {
    Json,
    Html,
    /// One URL per line; blank lines and # comments skipped
    Urls,
}

#[derive(Clone, Copy, ValueEnum)]
enum SortArg {
    Name,
    Count,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.cmd {
        Some(cmd) => run(cmd),
        None if io::stdin().is_terminal() && io::stdout().is_terminal() => run_tui(),
        // Without a terminal, bare `shlv` is a usage error, as before the TUI.
        None => {
            use clap::CommandFactory;
            Cli::command()
                .subcommand_required(true)
                .arg_required_else_help(true)
                .get_matches();
            unreachable!("clap exits without a subcommand")
        }
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            if e.downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
            {
                // `shlv ls | head` closed the pipe early: not an error.
                return ExitCode::SUCCESS;
            }
            eprintln!("shlv: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// The database, opened and configured the way every command needs it.
pub(crate) struct Opened {
    pub store: Store,
    pub config: Config,
    pub config_path: PathBuf,
    /// What syncing the declared list did, for the user.
    pub notes: Vec<String>,
}

/// Opens the database and applies the config to it: every command that opens the database
/// reads the config, since a declared canonical list has to be synced and pinned before anything
/// reads or writes tags.
pub(crate) fn open_store() -> Result<Opened> {
    let mut store = Store::open_default()?;
    let config_path = config::default_path()?;
    let mut config = Config::load(&config_path)?;
    store.set_strict_tags(config.strict_tags);
    store.set_suggest_mode(config.tags.suggestions);
    store.set_rules(config.tags.rules.clone());
    store.set_style(config.tags.style.tag_style());
    store.set_pinned_interests(&config.tags.interests);
    let mut notes = Vec::new();
    // The store keeps the declared list; nothing else reads it from `config`.
    if let Some(tags) = config.tags.canonical.take() {
        let stats = store.pin(tags).with_context(|| {
            format!(
                "cannot apply [tags.canonical] from {}",
                config_path.display()
            )
        })?;
        for (from, into, n) in stats.iter().flat_map(|s| &s.merged) {
            let n = plural(*n, "bookmark");
            notes.push(format!(
                "merged tag {from} into {into} ({n}): {} declares it an alias",
                config_path.display()
            ));
        }
    }
    Ok(Opened {
        store,
        config,
        config_path,
        notes,
    })
}

/// Runs the classifier on a bookmark just added, as `add` does: offline and fast, with the
/// stored model (`suggest` retrains it; `add` doesn't wait for that). Returns whether it ran:
/// it needs the `model` feature and a canonical list.
pub(crate) fn classify_added(store: &mut Store, config: &Config, id: i64) -> Result<bool> {
    #[cfg(feature = "model")]
    if !shlv::model::canonical_list(store)?.is_empty() {
        let classifier = store.classifier(false)?;
        store.apply_model(Some(&[id]), &classifier, &config.model)?;
        return Ok(true);
    }
    let _ = (store, config, id);
    Ok(false)
}

/// Bare `shlv` on a terminal.
fn run_tui() -> Result<ExitCode> {
    tui::run(open_store()?)?;
    Ok(ExitCode::SUCCESS)
}

fn run(cmd: Cmd) -> Result<ExitCode> {
    let stdout = io::stdout();
    let style = Style::detect(stdout.is_terminal());
    let mut out = BufWriter::new(stdout.lock());
    if let Cmd::Config { cmd } = cmd {
        run_config(cmd, &mut out)?;
        out.flush()?;
        return Ok(ExitCode::SUCCESS);
    }
    let Opened {
        mut store,
        config,
        config_path,
        notes,
    } = open_store()?;
    for note in notes {
        eprintln!("shlv: {note}");
    }
    let limit = |n: usize| if n == 0 { usize::MAX } else { n };
    let all = ListQuery {
        tags: Vec::new(),
        limit: usize::MAX,
    };

    match cmd {
        Cmd::Add {
            url,
            tags,
            title,
            note,
            fetch,
        } => {
            #[cfg(not(feature = "fetch"))]
            if fetch {
                bail!("--fetch isn't available: this build has no fetch support (feature `fetch`)");
            }
            // The request runs before the write transaction, so no lock is held while waiting.
            #[cfg(feature = "fetch")]
            let title = match title {
                None if fetch => fetch_title(&url),
                t => t,
            };
            let added = store
                .add(&NewBookmark {
                    url,
                    title,
                    note,
                    tags,
                })
                .map_err(|e| rules_hint(e, &config_path))?;
            let b = store.get(added.id)?.context("bookmark vanished")?;
            let verb = if added.created { "added" } else { "updated" };
            write!(out, "{verb} ")?;
            print_line(&mut out, &b, 0, &style)?;
            print_diverted(&mut out, &added.diverted, &style)?;
            let model_ran = classify_added(&mut store, &config, added.id)?;
            if !store.rules().is_empty() || model_ran {
                let pending = store.pending(Some(added.id))?;
                if !pending.is_empty() {
                    write!(out, "suggested")?;
                    print_suggested(&mut out, &pending, &style)?;
                    writeln!(out, "  (shlv suggest accept {})", added.id)?;
                }
            }
        }
        Cmd::Ls {
            tags,
            limit: n,
            json,
        } => {
            let q = ListQuery {
                tags,
                limit: limit(n),
            };
            let width = id_width(&store)?;
            store.list_each(&q, |b| print_item(&mut out, &b, json, width, &style))?;
        }
        Cmd::Search {
            query,
            limit: n,
            json,
        } => {
            let width = id_width(&store)?;
            store.search_each(&query.join(" "), limit(n), |b| {
                print_item(&mut out, &b, json, width, &style)
            })?;
        }
        Cmd::Show { id, json } => {
            let b = get(&store, id)?;
            if json {
                serde_json::to_writer(&mut out, &b).map_err(io::Error::from)?;
                writeln!(out)?;
            } else {
                print_full(&mut out, &b)?;
            }
        }
        Cmd::Edit(args) => {
            let code = edit_cmd::run_edit(&mut store, args, &mut out, &style)?;
            out.flush()?;
            return Ok(code);
        }
        Cmd::Rm { targets } => {
            let mut failed = false;
            for t in &targets {
                let id = match t.parse::<i64>() {
                    Ok(id) => Some(id),
                    Err(_) => store.find_by_url(t)?.map(|b| b.id),
                };
                match id {
                    Some(id) if store.delete(id)? => writeln!(out, "deleted {id}")?,
                    _ => {
                        out.flush()?;
                        eprintln!("shlv: no bookmark {t}");
                        failed = true;
                    }
                }
            }
            if failed {
                out.flush()?;
                return Ok(ExitCode::FAILURE);
            }
        }
        Cmd::Open { id } => open(&get(&store, id)?.url)?,
        Cmd::Import {
            file,
            format,
            folders_as_tags,
            drop_unknown_tags,
            propose_unknown_tags,
            no_rules,
            urls_only,
            no_titles,
        } => {
            let text = read_input(&file)?;
            let format = match format {
                Some(ImportFormat::Json) => Format::Json,
                Some(ImportFormat::Html) => Format::Html,
                Some(ImportFormat::Urls) => Format::Urls,
                None => Format::detect(&text),
            };
            let name = file.display();
            let mut items = transfer::parse(&text, format, folders_as_tags)
                .with_context(|| format!("cannot read {name}"))?;
            if urls_only {
                for item in &mut items {
                    item.urls_only(!no_titles);
                }
            }
            let opts = ImportOptions {
                drop_non_canonical: drop_unknown_tags,
                skip_rules: no_rules,
                propose_unknown: propose_unknown_tags,
            };
            let s = store.import(&items, opts).map_err(|e| {
                match e.downcast_ref::<NonCanonicalTags>() {
                    Some(bad) => anyhow!(
                        "nothing imported: {name} has tags that aren't canonical \
                         (strict_tags is on): {}\n\
                         hint: pass --drop-unknown-tags to import without them, \
                         --propose-unknown-tags to keep them as proposals for the list, or make \
                         them canonical (`shlv tag canonical add`)",
                        bad.0.join(" ")
                    ),
                    _ if let Some(bad) = e.downcast_ref::<NonCanonicalRuleTags>() => anyhow!(
                        "nothing imported: {bad}\n\
                         hint: make the tags canonical, change the rules ([tags.rules] in {}, or \
                         `shlv rule rm`), or pass --no-rules",
                        config_path.display()
                    ),
                    _ => e.context(format!("cannot import {name}")),
                }
            })?;
            for (url, why) in &s.skipped {
                eprintln!("shlv: skipped {why}: {}", truncate(url, 100));
            }
            if !s.dropped_tags.is_empty() {
                eprintln!("shlv: dropped tags: {}", s.dropped_tags.join(" "));
            }
            for why in &s.refused_tags {
                eprintln!("shlv: dropped tag: {why}");
            }
            if !s.proposed.is_empty() {
                eprintln!(
                    "shlv: kept {} as proposals (not canonical): {}\n\
                     hint: review them with `shlv tag proposals`",
                    plural(s.proposed.len(), "tag"),
                    s.proposed.join(" ")
                );
            }
            writeln!(
                out,
                "imported {name}: {} added, {} updated, {} skipped",
                s.added,
                s.updated,
                s.skipped.len()
            )?;
            if !no_rules && !store.rules().is_empty() {
                print_rule_stats(&mut out, &s.rules, store.suggest_mode())?;
            }
        }
        Cmd::Export { format } => match format {
            FormatArg::Json => {
                store.list_each(&all, |b| print_item(&mut out, &b, true, 0, &style))?;
            }
            FormatArg::Html => {
                netscape::write_header(&mut out)?;
                store.list_each(&all, |b| Ok(netscape::write_bookmark(&mut out, &b)?))?;
                netscape::write_footer(&mut out)?;
            }
        },
        Cmd::Tag {
            cmd:
                TagCmd::Health {
                    redundant,
                    min_together,
                    broad,
                    rare,
                    min_group,
                    groups,
                    #[cfg(feature = "ai")]
                    name_gaps,
                    json,
                },
        } => {
            let opts = HealthOptions {
                redundant,
                min_together,
                broad,
                rare,
                min_group,
                groups,
            };
            // With the classifier built, untagged bookmarks it can place aren't gaps in the list:
            // `suggest` would tag them. It uses the stored model as is (no retraining here).
            #[cfg(feature = "model")]
            let classifier = if shlv::model::canonical_list(&store)?.is_empty() {
                None
            } else {
                Some(store.classifier(false)?)
            };
            #[cfg(feature = "model")]
            let place = classifier.as_ref().map(|c| {
                let t = config.model.threshold;
                move |item: &shlv::choose::Item| c.probabilities(item).iter().any(|(_, p)| *p >= t)
            });
            #[cfg(feature = "model")]
            let place = place
                .as_ref()
                .map(|f| f as &dyn Fn(&shlv::choose::Item) -> bool);
            #[cfg(not(feature = "model"))]
            let place = None;
            #[allow(unused_mut)]
            let mut h = store.health(&opts, place)?;
            #[cfg(feature = "ai")]
            if let Some(n) = name_gaps {
                ai_cmd::name_gaps(&config, &store, &mut h.gaps, n)?;
            }
            if json {
                serde_json::to_writer_pretty(&mut out, &h).map_err(io::Error::from)?;
                writeln!(out)?;
            } else {
                print_health(&mut out, &h, &opts, store.pinned().is_some(), &style)?;
            }
        }
        #[cfg(feature = "ai")]
        Cmd::Tag {
            cmd: TagCmd::Draft(args),
        } => {
            out.flush()?;
            let code = draft_cmd::run_draft(&mut store, &config, args, &mut out, &style)?;
            out.flush()?;
            return Ok(code);
        }
        #[cfg(feature = "ai")]
        Cmd::Tag {
            cmd: TagCmd::Seed(args),
        } => {
            out.flush()?;
            let code = seed_cmd::run_seed(&mut store, &config, args, &mut out, &style)?;
            out.flush()?;
            return Ok(code);
        }
        Cmd::Tag { cmd } => {
            run_tag(&mut store, cmd, &mut out, &style).map_err(|e| pinned_hint(e, &config_path))?
        }
        Cmd::Rule { cmd } => {
            run_rule(&mut store, cmd, &mut out, &style).map_err(|e| pinned_hint(e, &config_path))?
        }
        Cmd::Suggest {
            cmd: None,
            ids,
            all,
            tag: Some(tag),
            pick,
            #[cfg(feature = "ai")]
            llm,
        } => {
            let b = backfill_cmd::Backfill {
                tag: &tag,
                ids: &ids,
                all,
                pick,
            };
            let code = backfill_cmd::run(
                &mut store,
                &config,
                b,
                #[cfg(feature = "ai")]
                &llm,
                &mut out,
                &style,
            )?;
            out.flush()?;
            return Ok(code);
        }
        Cmd::Suggest {
            cmd: None,
            ids,
            all,
            tag: None,
            #[cfg(feature = "ai")]
            llm,
            ..
        } => {
            // The classifier runs whenever it's built and there's a canonical list to choose from.
            #[cfg(feature = "model")]
            let model_on = !shlv::model::canonical_list(&store)?.is_empty();
            #[cfg(not(feature = "model"))]
            let model_on = false;
            #[cfg(feature = "ai")]
            let provider = match config.model.provider {
                config::ProviderUse::Never if model_on => None,
                _ => ai_cmd::tag_provider(&config, &store)?,
            };
            #[cfg(not(feature = "ai"))]
            let provider: Option<()> = None;
            if store.rules().is_empty() && provider.is_none() && !model_on {
                let llm = if cfg!(feature = "ai") {
                    ", and no [ai] tag provider"
                } else {
                    ""
                };
                bail!(
                    "nothing to suggest with: no domain rules in {} ([tags.rules]){llm}",
                    config_path.display()
                );
            }
            if !store.rules().is_empty() {
                let s = store
                    .apply_rules((!all).then_some(ids.as_slice()))
                    .map_err(|e| rules_hint(e, &config_path))?;
                print_rule_stats(&mut out, &s, store.suggest_mode())?;
            }
            // Bookmarks the classifier is sure about; with `provider = "fallback"` the provider
            // skips them.
            #[cfg(feature = "model")]
            let sure = if model_on {
                let classifier = store.classifier(true)?;
                let s = store.apply_model(
                    (!all).then_some(ids.as_slice()),
                    &classifier,
                    &config.model,
                )?;
                print_model_stats(&mut out, &s, store.suggest_mode())?;
                (config.model.provider == config::ProviderUse::Fallback).then_some(s.sure)
            } else {
                None
            };
            #[cfg(not(feature = "model"))]
            let sure: Option<std::collections::HashSet<i64>> = None;
            #[cfg(feature = "ai")]
            let code = match provider {
                Some(p) => {
                    out.flush()?;
                    ai_cmd::run_suggest_llm(
                        &mut store,
                        &config,
                        &p,
                        &ids,
                        all,
                        &llm,
                        sure.as_ref(),
                        None,
                        &mut out,
                    )?
                }
                None => ExitCode::SUCCESS,
            };
            #[cfg(not(feature = "ai"))]
            let (code, _) = (ExitCode::SUCCESS, sure);
            if !all {
                let width = id_width(&store)?;
                for id in ids {
                    if store.suggest_mode() == SuggestMode::Confirm {
                        print_pending(&mut out, &store.pending(Some(id))?, width, &style)?;
                    }
                    let proposed = store.proposals_for(id)?;
                    if !proposed.is_empty() {
                        write!(out, "{}{id:>width$}{}  proposed", style.id, style.reset)?;
                        for p in &proposed {
                            write!(out, "  {}#{p}{}", style.tag, style.reset)?;
                        }
                        writeln!(out, "  (shlv tag proposals)")?;
                    }
                }
            }
            out.flush()?;
            return Ok(code);
        }
        Cmd::Review(args) => {
            out.flush()?;
            let code = review_cmd::run(&mut store, &config, args, &mut out, &style)?;
            out.flush()?;
            return Ok(code);
        }
        Cmd::Suggest { cmd: Some(cmd), .. } => run_suggest(&mut store, cmd, &mut out, &style)?,
        #[cfg(feature = "fetch")]
        Cmd::Fetch {
            ids,
            all,
            force,
            jobs,
        } => return run_fetch(&mut store, &ids, all, force, jobs.into(), &mut out, &style),
        #[cfg(feature = "ai")]
        Cmd::Summarize {
            ids,
            all,
            force,
            jobs,
            yes,
            batch,
        } => {
            return ai_cmd::run_summarize(
                &mut store,
                &config,
                &ids,
                all,
                force,
                jobs.into(),
                yes,
                batch,
                &mut out,
                &style,
            );
        }
        #[cfg(feature = "model")]
        Cmd::Model { cmd } => {
            let code = model_cmd::run(&mut store, &config, cmd, &mut out)?;
            out.flush()?;
            return Ok(code);
        }
        #[cfg(feature = "ai")]
        Cmd::Batch { cmd } => {
            out.flush()?;
            let code = ai_cmd::run_batch(&mut store, &config, cmd, &mut out)?;
            out.flush()?;
            return Ok(code);
        }
        Cmd::Config { .. } => unreachable!("handled before opening the database"),
    }
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}

/// `add --fetch`: the page title, or `None` with a warning. A failed fetch doesn't fail `add`;
/// see task 016.
#[cfg(feature = "fetch")]
fn fetch_title(url: &str) -> Option<String> {
    let full = shlv::store::check_url(url).ok()?.0;
    match shlv::fetch::Fetcher::default().get(&full) {
        Ok(page) => {
            let title = shlv::fetch::title(&page.html);
            if title.is_none() {
                eprintln!("shlv: warning: {full} has no title");
            }
            title
        }
        Err(e) => {
            eprintln!("shlv: warning: cannot fetch {full}: {e}; adding it without a title");
            None
        }
    }
}

/// `shlv fetch`: sets titles from the pages, a few requests at a time, one commit per bookmark.
#[cfg(feature = "fetch")]
fn run_fetch(
    store: &mut Store,
    ids: &[i64],
    all: bool,
    force: bool,
    jobs: usize,
    out: &mut impl Write,
    style: &Style,
) -> Result<ExitCode> {
    use shlv::bulk::{self, Progress};
    use shlv::fetch::{self, Fetcher};

    let targets: Vec<(i64, String)> = if all {
        let mut v = Vec::new();
        store.list_each(
            &ListQuery {
                tags: Vec::new(),
                limit: usize::MAX,
            },
            |b| {
                if force || b.title.is_none() {
                    v.push((b.id, b.url));
                }
                Ok(())
            },
        )?;
        v
    } else {
        ids.iter()
            .map(|&id| Ok((id, get(store, id)?.url)))
            .collect::<Result<_>>()?
    };
    let fetcher = Fetcher::default();
    let mut progress = Progress::new("fetching", targets.len());
    let (mut set, mut untitled, mut failed) = (0, 0, 0);
    let width = id_width(store)?;
    bulk::run(
        &targets,
        jobs,
        |(_, url)| fetcher.get(url).map(|p| fetch::title(&p.html)),
        |i, r| -> Result<()> {
            let (id, url) = &targets[i];
            progress.clear();
            match r {
                Ok(Some(title)) => {
                    let edit = shlv::Edit {
                        title: Some(title),
                        ..Default::default()
                    };
                    // The bookmark may have been deleted meanwhile; then there's nothing to set.
                    if store.edit(*id, &edit)? {
                        set += 1;
                        print_line(out, &get(store, *id)?, width, style)?;
                        out.flush()?;
                    }
                }
                Ok(None) => {
                    untitled += 1;
                    eprintln!("shlv: {id}: {url} has no title");
                }
                Err(e) => {
                    failed += 1;
                    eprintln!("shlv: {id}: cannot fetch {url}: {e}");
                }
            }
            progress.tick();
            Ok(())
        },
    )?;
    drop(progress);
    if targets.len() > 1 || all {
        eprintln!(
            "shlv: fetched {} of {}: {untitled} without a title, {failed} failed",
            plural(set, "title"),
            plural(targets.len(), "bookmark"),
        );
    }
    Ok(if failed > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn run_config(cmd: ConfigCmd, out: &mut impl Write) -> Result<()> {
    let path = config::default_path()?;
    match cmd {
        ConfigCmd::Path => writeln!(out, "{}", path.display())?,
        ConfigCmd::Show => {
            let config = Config::load(&path)?;
            let status = if path.exists() {
                ""
            } else {
                " (not found; defaults)"
            };
            writeln!(out, "# {}{status}", path.display())?;
            for s in config.ignored_sections() {
                writeln!(out, "# [{s}] ignored: not in this build")?;
            }
            out.write_all(config.to_toml().as_bytes())?;
        }
    }
    Ok(())
}

/// Names the config file when an edit would undo the declared list.
fn pinned_hint(e: anyhow::Error, config: &Path) -> anyhow::Error {
    if e.downcast_ref::<Pinned>().is_none() {
        return e;
    }
    anyhow!("{e:#}\nhint: change it in {}", config.display())
}

/// One line on what the domain rules did, for `suggest` and `import`.
fn print_rule_stats(out: &mut impl Write, s: &RuleStats, mode: SuggestMode) -> io::Result<()> {
    let matched = format!(
        "rules matched {} of {}",
        s.matched,
        plural(s.checked, "bookmark")
    );
    match mode {
        SuggestMode::Confirm => writeln!(out, "{matched}: {}", plural(s.pending, "new suggestion")),
        SuggestMode::Auto => writeln!(
            out,
            "{matched}: {} applied to {}",
            plural(s.applied, "tag"),
            plural(s.tagged, "bookmark")
        ),
    }
}

/// One line on what the classifier did, for `suggest`.
#[cfg(feature = "model")]
fn print_model_stats(
    out: &mut impl Write,
    s: &shlv::model::ModelStats,
    mode: SuggestMode,
) -> io::Result<()> {
    let checked = format!("model checked {}", plural(s.checked, "bookmark"));
    match mode {
        SuggestMode::Confirm => writeln!(out, "{checked}: {}", plural(s.pending, "new suggestion")),
        SuggestMode::Auto => writeln!(
            out,
            "{checked}: {} applied, {} to review",
            plural(s.applied, "tag"),
            s.pending
        ),
    }
}
/// Names the config file when domain rules break strict mode.
fn rules_hint(e: anyhow::Error, config: &Path) -> anyhow::Error {
    match e.downcast_ref::<NonCanonicalRuleTags>() {
        Some(bad) => anyhow!(
            "{bad}\nhint: make the tags canonical, or change the rules ([tags.rules] in {}, or \
             `shlv rule rm`)",
            config.display()
        ),
        None => e,
    }
}

fn run_suggest(
    store: &mut Store,
    cmd: SuggestCmd,
    out: &mut impl Write,
    style: &Style,
) -> Result<()> {
    match cmd {
        SuggestCmd::Ls { id, json } => {
            if let Some(id) = id {
                get(store, id)?;
            }
            if json {
                store.pending_each(id, |s| {
                    serde_json::to_writer(&mut *out, &s).map_err(io::Error::from)?;
                    Ok(writeln!(out)?)
                })?;
            } else {
                // Rows come grouped by bookmark; print one line per bookmark.
                let width = id_width(store)?;
                let mut group: Vec<Suggestion> = Vec::new();
                store.pending_each(id, |s| {
                    if group.first().is_some_and(|g| g.id != s.id) {
                        print_pending(out, &group, width, style)?;
                        group.clear();
                    }
                    group.push(s);
                    Ok(())
                })?;
                print_pending(out, &group, width, style)?;
            }
        }
        SuggestCmd::Accept { all: true, .. } => {
            let (bookmarks, tags) = store.accept_all()?;
            writeln!(
                out,
                "accepted {} on {}",
                plural(tags, "tag"),
                plural(bookmarks, "bookmark")
            )?;
        }
        SuggestCmd::Accept {
            id: Some(id), tags, ..
        } => {
            get(store, id)?;
            let accepted = store.accept(id, &tags)?;
            write!(out, "accepted {}; now ", accepted.join(" "))?;
            print_line(out, &get(store, id)?, 0, style)?;
        }
        SuggestCmd::Accept { id: None, .. } => unreachable!("clap requires ID unless --all"),
        SuggestCmd::Reject { id, tags } => {
            get(store, id)?;
            let rejected = store.reject(id, &tags)?;
            writeln!(out, "rejected for {id}: {}", rejected.join(" "))?;
        }
    }
    Ok(())
}

/// One bookmark's pending suggestions on one line: id, title, URL, then the suggested tags.
fn print_pending(
    out: &mut impl Write,
    group: &[Suggestion],
    width: usize,
    s: &Style,
) -> io::Result<()> {
    let Some(first) = group.first() else {
        return Ok(());
    };
    write!(out, "{}{:>width$}{}  ", s.id, first.id, s.reset)?;
    if let Some(title) = &first.title {
        write!(out, "{}  ", one_line(title))?;
    }
    write!(out, "{}{}{}", s.dim, first.url, s.reset)?;
    print_suggested(out, group, s)?;
    writeln!(out)
}

/// Suggested tags with their source, and their confidence when it's below 1:
/// `  #code (rule)  #rust (model 0.82)`.
fn print_suggested(out: &mut impl Write, group: &[Suggestion], s: &Style) -> io::Result<()> {
    for g in group {
        write!(
            out,
            "  {}#{}{} {}({}",
            s.tag,
            g.tag,
            s.reset,
            s.dim,
            g.source.as_str()
        )?;
        if let Some(c) = g.confidence.filter(|c| *c < 1.0) {
            write!(out, " {c:.2}")?;
        }
        write!(out, "){}", s.reset)?;
    }
    Ok(())
}

/// `shlv rule`: domain rules saved in the database, with those pinned in config.
#[derive(Subcommand)]
enum RuleCmd {
    /// List the rules: host, tag, what each tagged (pending in parentheses); # pinned marks
    /// rules from [tags.rules] in the config
    Ls {
        /// One JSON object per line
        #[arg(long)]
        json: bool,
    },
    /// Suggest TAGs for bookmarks on HOST or its subdomains, and run the rules on the ones
    /// already saved
    Add {
        /// A host (github.com) or a URL on it
        host: String,
        #[arg(required = true)]
        tags: Vec<String>,
    },
    /// Remove HOST's rule for TAGs, or all its saved rules; drops the pending suggestions
    /// they made
    Rm { host: String, tags: Vec<String> },
}

fn run_rule(store: &mut Store, cmd: RuleCmd, out: &mut impl Write, style: &Style) -> Result<()> {
    match cmd {
        RuleCmd::Ls { json } => {
            let list = store.rule_list()?;
            if json {
                for r in &list {
                    serde_json::to_writer(&mut *out, r).map_err(io::Error::from)?;
                    writeln!(out)?;
                }
                return Ok(());
            }
            let width = list.iter().map(|r| r.host.len()).max().unwrap_or(0);
            for r in &list {
                let pending = if r.pending > 0 {
                    format!(" ({} pending)", r.pending)
                } else {
                    String::new()
                };
                let pinned = if r.pinned { "  # pinned" } else { "" };
                let pinned = format!("{}{pinned}{}", style.dim, style.reset);
                writeln!(
                    out,
                    "{:width$}  {}#{}{}  {}{pending}{pinned}",
                    r.host,
                    style.tag,
                    r.tag,
                    style.reset,
                    plural(r.tagged, "bookmark"),
                )?;
            }
        }
        RuleCmd::Add { host, tags } => {
            let added = store.add_rule(&host, &tags)?;
            let host = &added.host;
            if !added.existing.is_empty() {
                let t: Vec<String> = added.existing.iter().map(|t| format!("#{t}")).collect();
                writeln!(out, "{host} already gets {}", t.join(" "))?;
            }
            if !added.added.is_empty() {
                let t: Vec<String> = added.added.iter().map(|t| format!("#{t}")).collect();
                writeln!(out, "added rule {host} → {}", t.join(" "))?;
                let s = store.apply_rules_on(host)?;
                print_rule_stats(out, &s, store.suggest_mode())?;
            }
        }
        RuleCmd::Rm { host, tags } => {
            let r = store.remove_rule(&host, &tags)?;
            let t: Vec<String> = r.removed.iter().map(|t| format!("#{t}")).collect();
            write!(out, "removed rule {} → {}", r.host, t.join(" "))?;
            if r.dropped > 0 {
                write!(out, "; dropped {}", plural(r.dropped, "pending suggestion"))?;
            }
            writeln!(out)?;
        }
    }
    Ok(())
}

fn run_tag(store: &mut Store, cmd: TagCmd, out: &mut impl Write, style: &Style) -> Result<()> {
    match cmd {
        TagCmd::Ls { sort, json } => {
            let sort = match sort {
                SortArg::Name => TagSort::Name,
                SortArg::Count => TagSort::Count,
            };
            let tags = store.tags(sort)?;
            let width = tags.iter().map(|t| t.count).max().unwrap_or(0);
            let width = width.to_string().len();
            for t in &tags {
                if json {
                    serde_json::to_writer(&mut *out, t).map_err(io::Error::from)?;
                    writeln!(out)?;
                } else {
                    print_tag(out, t, width, style)?;
                }
            }
        }
        TagCmd::Rename { old, new } => {
            let r = store.rename_tag(&old, &new)?;
            let n = plural(r.bookmarks, "bookmark");
            writeln!(out, "renamed {} to {} ({n})", normalize_tag(&old), r.tag)?;
        }
        TagCmd::Merge { from, into } => {
            let r = store.merge_tags(&from, &into)?;
            let (from, n) = (normalize_tag(&from), plural(r.bookmarks, "bookmark"));
            writeln!(
                out,
                "merged {from} into {} ({n}); {from} is now an alias",
                r.tag
            )?;
        }
        TagCmd::Alias {
            alias,
            tag: None,
            rm: true,
        } => {
            let alias = normalize_tag(&alias);
            match store.remove_alias(&alias)? {
                Some(tag) => writeln!(out, "removed alias {alias} (of {tag})")?,
                None => bail!("no alias {alias:?}"),
            }
        }
        TagCmd::Alias {
            alias,
            tag: Some(tag),
            rm: false,
        } => {
            let added = store.add_alias(&alias, &tag)?;
            let (alias, tag) = (normalize_tag(&alias), normalize_tag(&tag));
            if added {
                writeln!(out, "added alias {alias} for {tag}")?;
            } else {
                writeln!(out, "{alias} is already an alias of {tag}")?;
            }
        }
        TagCmd::Alias { .. } => unreachable!("clap requires TAG unless --rm, and not both"),
        TagCmd::Describe { tag, text } => {
            let (name, changed) = store.describe_tag(&tag, &text)?;
            match (changed, text.trim().is_empty()) {
                (false, _) => writeln!(out, "{name}: unchanged")?,
                (true, true) => writeln!(out, "cleared the description of {name}")?,
                (true, false) => writeln!(out, "described {name}")?,
            }
        }
        TagCmd::Canonical { cmd } => match cmd {
            CanonicalCmd::Add { tags } => {
                let n = store.add_canonical(&tags)?;
                writeln!(out, "marked {} canonical", plural(n, "tag"))?;
            }
            CanonicalCmd::Rm { tags } => {
                let n = store.remove_canonical(&tags)?;
                writeln!(out, "unmarked {}", plural(n, "tag"))?;
            }
            CanonicalCmd::Ls => {
                for t in store.canonical_tags()? {
                    writeln!(out, "{}", t.name)?;
                }
            }
            CanonicalCmd::Import { file } => {
                let text = read_input(&file)?;
                let s = store
                    .import_canonical(&parse_canonical(&text))
                    .with_context(|| format!("cannot import {}", file.display()))?;
                write!(
                    out,
                    "imported {}: {} newly canonical, {} added",
                    plural(s.tags, "tag"),
                    s.marked,
                    plural(s.aliases, "alias"),
                )?;
                if s.described > 0 {
                    write!(out, ", {} set", plural(s.described, "description"))?;
                }
                writeln!(out)?;
            }
            CanonicalCmd::Export { toml, nix } => {
                let tags = store.canonical_tags()?;
                let text = if toml {
                    format_canonical_toml(&tags, store.pinned())
                } else if nix {
                    format_canonical_nix(&tags, store.pinned())
                } else {
                    format_canonical(&tags)
                };
                out.write_all(text.as_bytes())?
            }
        },
        TagCmd::Health { .. } => unreachable!("handled in run: it needs the config"),
        #[cfg(feature = "ai")]
        TagCmd::Seed(_) => unreachable!("handled in run: it needs the config"),
        #[cfg(feature = "ai")]
        TagCmd::Draft(_) => unreachable!("handled in run: it needs the config"),
        TagCmd::Interests { add, rm } => {
            if !add.is_empty() {
                let n = store.save_interests(&add)?;
                writeln!(out, "saved {}", plural(n, "new interest"))?;
            }
            if !rm.is_empty() {
                let n = store.forget_interests(&rm)?;
                writeln!(out, "forgot {}", plural(n, "interest"))?;
            }
            if add.is_empty() && rm.is_empty() {
                for i in store.interests()? {
                    let from = match (i.saved, i.pinned) {
                        (true, true) => "saved, pinned in config",
                        (false, true) => "pinned in config",
                        _ => "saved",
                    };
                    writeln!(out, "{}  {}({from}){}", i.name, style.dim, style.reset)?;
                }
            }
        }
        TagCmd::Proposals { cmd } => run_proposals(
            store,
            cmd.unwrap_or(ProposalCmd::Ls {
                json: false,
                rejected: false,
            }),
            out,
            style,
        )?,
    }
    Ok(())
}

fn run_proposals(
    store: &mut Store,
    cmd: ProposalCmd,
    out: &mut impl Write,
    style: &Style,
) -> Result<()> {
    match cmd {
        ProposalCmd::Ls { json, rejected } => {
            let state = if rejected {
                ProposalState::Rejected
            } else {
                ProposalState::Pending
            };
            let list = store.proposals(state)?;
            let width = list.iter().map(|p| p.bookmarks.len()).max().unwrap_or(0);
            let width = width.to_string().len();
            for p in &list {
                if json {
                    serde_json::to_writer(&mut *out, p).map_err(io::Error::from)?;
                    writeln!(out)?;
                } else {
                    print_proposal(out, p, width, style)?;
                }
            }
        }
        ProposalCmd::Accept { names } => {
            for name in &names {
                let d = store.accept_proposal(name)?;
                let described = if d.described { ", described" } else { "" };
                writeln!(
                    out,
                    "accepted {}: canonical{described}, tagged {}",
                    d.tag,
                    plural(d.tagged, "bookmark")
                )?;
            }
        }
        ProposalCmd::Alias { name, tag } => {
            let d = store.alias_proposal(&name, &tag)?;
            writeln!(
                out,
                "aliased {} to {}: tagged {}",
                normalize_tag(&name),
                d.tag,
                plural(d.tagged, "bookmark")
            )?;
        }
        ProposalCmd::Reject { names } => {
            for name in &names {
                let n = store.reject_proposal(name)?;
                let was = if n > 0 {
                    format!(" (it was proposed for {})", plural(n, "bookmark"))
                } else {
                    String::new()
                };
                writeln!(
                    out,
                    "rejected {}{was}; it won't be proposed again",
                    normalize_tag(name)
                )?;
            }
        }
    }
    Ok(())
}

/// One proposal: bookmark count, name, description, source.
fn print_proposal(out: &mut impl Write, p: &Proposal, width: usize, s: &Style) -> io::Result<()> {
    write!(
        out,
        "{}{:>width$}{}  {}{}{}",
        s.dim,
        p.bookmarks.len(),
        s.reset,
        s.tag,
        p.name,
        s.reset
    )?;
    if let Some(d) = &p.description {
        write!(out, "  {}# {d}{}", s.dim, s.reset)?;
    }
    writeln!(out, "  {}({}){}", s.dim, p.source.as_str(), s.reset)
}

/// Says which typed tags became proposals (`strict_tags = "propose"`) or were dropped as
/// rejected names.
fn print_diverted(out: &mut impl Write, d: &Diverted, s: &Style) -> io::Result<()> {
    if !d.proposed.is_empty() {
        write!(out, "proposed")?;
        for t in &d.proposed {
            write!(out, "  {}#{t}{}", s.tag, s.reset)?;
        }
        writeln!(
            out,
            "  (not canonical; `shlv tag proposals accept {}` adds them)",
            d.proposed.join(" ")
        )?;
    }
    if !d.rejected.is_empty() {
        out.flush()?;
        eprintln!(
            "shlv: dropped {}: rejected as proposals (`shlv tag proposals ls --rejected`)",
            d.rejected.join(" ")
        );
    }
    Ok(())
}

/// One tag: count, canonical marker, name, aliases.
fn print_tag(out: &mut impl Write, t: &TagInfo, width: usize, s: &Style) -> io::Result<()> {
    let mark = if t.canonical { '*' } else { ' ' };
    write!(
        out,
        "{}{:>width$}{} {mark} {}{}{}",
        s.dim, t.count, s.reset, s.tag, t.name, s.reset
    )?;
    if !t.aliases.is_empty() {
        write!(out, "  {}= {}{}", s.dim, t.aliases.join(" "), s.reset)?;
    }
    if let Some(d) = &t.description {
        write!(out, "  {}# {d}{}", s.dim, s.reset)?;
    }
    writeln!(out)
}

/// A share between 0 and 1, for `tag health` thresholds.
fn share(s: &str) -> Result<f64, String> {
    match s.parse::<f64>() {
        Ok(v) if (0.0..=1.0).contains(&v) => Ok(v),
        _ => Err(format!("{s:?} is not a share between 0 and 1")),
    }
}

fn percent(part: usize, whole: usize) -> String {
    if whole == 0 {
        return "0%".into();
    }
    format!("{:.0}%", 100.0 * part as f64 / whole as f64)
}

/// A rough age: `5 days`, `3 months`, `2 years`.
fn age(ms: i64) -> String {
    let days = (ms / 86_400_000).max(0) as usize;
    match days {
        0..=60 => plural(days, "day"),
        61..=730 => plural(days / 30, "month"),
        _ => plural(days / 365, "year"),
    }
}

/// `tag health` as text: one section per measurement, with the command that acts on each line.
fn print_health(
    out: &mut impl Write,
    h: &Health,
    opts: &HealthOptions,
    declared: bool,
    s: &Style,
) -> io::Result<()> {
    let c = &h.coverage;
    let head = |out: &mut dyn Write, title: &str, hint: &str| {
        writeln!(out, "\n{title} {}{hint}{}", s.dim, s.reset)
    };
    let cmd = |c: String| format!("  {}({c}){}", s.dim, s.reset);
    let tag = |t: &str| format!("{}{t}{}", s.tag, s.reset);

    writeln!(
        out,
        "coverage: {} bookmarks, {} tagged ({}), {} with a canonical tag ({}); \
         {:.1} tags per tagged bookmark",
        c.bookmarks,
        c.tagged,
        percent(c.tagged, c.bookmarks),
        c.canonical_tagged,
        percent(c.canonical_tagged, c.bookmarks),
        c.tags_per_tagged,
    )?;
    let hist = c.histogram;
    writeln!(
        out,
        "  {} {}, {} canonical; bookmarks by tag count: 0: {}  1: {}  2: {}  3: {}  4+: {}",
        c.tags,
        if c.tags == 1 { "tag" } else { "tags" },
        c.canonical,
        hist[0],
        hist[1],
        hist[2],
        hist[3],
        hist[4]
    )?;

    head(
        out,
        "redundant",
        &format!(
            "(each on {} or more of the other's bookmarks: merge?)",
            percent((opts.redundant * 100.0).round() as usize, 100)
        ),
    )?;
    for r in &h.redundant {
        writeln!(
            out,
            "  {} + {}  together on {} of {} and {} ({:.2} Jaccard){}",
            tag(&r.a),
            tag(&r.b),
            r.together,
            r.a_count,
            r.b_count,
            r.jaccard,
            cmd(format!("shlv tag merge {} {}", r.a, r.b))
        )?;
    }
    if h.redundant.is_empty() {
        writeln!(out, "  none")?;
    }

    head(
        out,
        "too broad",
        &format!(
            "(on {} or more of all bookmarks: they don't narrow a search)",
            percent((opts.broad * 100.0).round() as usize, 100)
        ),
    )?;
    for b in &h.broad {
        writeln!(
            out,
            "  {}  {} ({})",
            tag(&b.tag),
            b.count,
            percent(b.count, c.bookmarks)
        )?;
    }
    if h.broad.is_empty() {
        writeln!(out, "  none")?;
    }

    head(
        out,
        "rare",
        &format!("(canonical, on {} or fewer)", plural(opts.rare, "bookmark")),
    )?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    for r in &h.rare {
        let old = r.created_at.map_or(String::new(), |t| match age(now - t) {
            a if a == "0 days" => ", added today".to_string(),
            a => format!(", added {a} ago"),
        });
        let act = if r.pinned {
            cmd("declared in the config".into())
        } else {
            cmd(format!("shlv tag canonical rm {}", r.tag))
        };
        writeln!(
            out,
            "  {}  {}{old}{act}",
            tag(&r.tag),
            plural(r.count, "bookmark")
        )?;
    }
    if h.rare.is_empty() {
        writeln!(out, "  none")?;
    }

    let g = &h.gaps;
    let what = if c.canonical > 0 {
        "without a canonical tag"
    } else {
        "without tags"
    };
    write!(out, "\ngaps: {} {what}", plural(g.bookmarks, "bookmark"))?;
    match g.placeable {
        Some(0) => write!(out, "; the classifier can't place any of them")?,
        Some(p) => write!(
            out,
            "; the classifier could tag {p} of them{}",
            cmd("shlv suggest --all".into())
        )?,
        None => {}
    }
    writeln!(out)?;
    for (label, groups) in [("by site", &g.by_host), ("by title word", &g.by_word)] {
        if groups.is_empty() {
            continue;
        }
        writeln!(out, "  {label}:")?;
        let width = groups[0].ids.len().to_string().len();
        for gr in groups {
            write!(
                out,
                "    {}{:>width$}{}  {}",
                s.dim,
                gr.ids.len(),
                s.reset,
                gr.key
            )?;
            if let Some(t) = &gr.existing {
                write!(out, "  is tag {}", tag(t))?;
            }
            if let Some(n) = &gr.named {
                let listed = if n.listed { " (on the list)" } else { "" };
                write!(out, "  → {}{listed}", tag(&n.name))?;
                if let Some(d) = &n.description {
                    write!(out, " {}# {d}{}", s.dim, s.reset)?;
                }
            }
            if let Some(e) = gr.examples.first() {
                write!(
                    out,
                    "  {}e.g. {}{}",
                    s.dim,
                    truncate(&one_line(e), 60),
                    s.reset
                )?;
            }
            writeln!(out)?;
        }
    }

    head(out, "style", "([tags.style] and naming guidelines)")?;
    for i in &h.style {
        let act = match i.kind {
            StyleKind::Style => String::new(),
            StyleKind::Plural => cmd(format!("shlv tag merge {} {}", i.tag, i.related[0])),
            StyleKind::Compound => String::new(),
        };
        writeln!(out, "  {}{act}", i.message)?;
    }
    if h.style.is_empty() {
        writeln!(out, "  none")?;
    }
    if !h.interests.is_empty() {
        head(
            out,
            "interests",
            "(stored; tags kept under each when seeding, or named like it)",
        )?;
        let width = h
            .interests
            .iter()
            .map(|i| i.interest.chars().count())
            .max()
            .unwrap_or(0);
        for i in &h.interests {
            write!(out, "  {:<width$}  ", i.interest)?;
            if i.tags.is_empty() {
                writeln!(
                    out,
                    "no tags{}",
                    cmd(format!("shlv tag seed {}", i.interest))
                )?;
                continue;
            }
            let tags: Vec<String> = i.tags.iter().map(|t| tag(t)).collect();
            writeln!(
                out,
                "{}, {}: {}",
                plural(i.tags.len(), "tag"),
                plural(i.bookmarks, "bookmark"),
                tags.join(" ")
            )?;
        }
    }
    if declared {
        writeln!(
            out,
            "\n{}tags declared in the config change there, not with these commands{}",
            s.dim, s.reset
        )?;
    }
    Ok(())
}

/// Reads a file, or stdin for "-".
fn read_input(file: &Path) -> Result<String> {
    if file.as_os_str() == "-" {
        let mut text = String::new();
        io::stdin()
            .read_to_string(&mut text)
            .context("cannot read stdin")?;
        return Ok(text);
    }
    std::fs::read_to_string(file).with_context(|| format!("cannot read {}", file.display()))
}

/// At most `max` characters of `s`, with `…` if cut.
fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

fn plural(n: usize, word: &str) -> String {
    let es = ['s', 'x'].iter().any(|c| word.ends_with(*c))
        || word.ends_with("ch")
        || word.ends_with("sh");
    match (n, es) {
        (1, _) => format!("1 {word}"),
        (_, true) => format!("{n} {word}es"),
        _ => format!("{n} {word}s"),
    }
}

/// Width of the id column: the widest id the database has handed out, so rows align without
/// collecting them first.
fn id_width(store: &Store) -> Result<usize> {
    Ok(store.max_id()?.to_string().len())
}

fn get(store: &Store, id: i64) -> Result<Bookmark> {
    store
        .get(id)?
        .ok_or_else(|| anyhow!("no bookmark with id {id}"))
}

/// ANSI styles; all empty when color is off.
struct Style {
    dim: &'static str,
    id: &'static str,
    tag: &'static str,
    reset: &'static str,
}

impl Style {
    fn detect(tty: bool) -> Self {
        let color = tty && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
        if color {
            Style {
                dim: "\x1b[2m",
                id: "\x1b[33m",
                tag: "\x1b[36m",
                reset: "\x1b[0m",
            }
        } else {
            Style {
                dim: "",
                id: "",
                tag: "",
                reset: "",
            }
        }
    }
}

/// Prints one listed bookmark: a JSON line, or a human line with the id padded to `width`.
fn print_item(
    out: &mut impl Write,
    b: &Bookmark,
    json: bool,
    width: usize,
    style: &Style,
) -> Result<()> {
    if json {
        serde_json::to_writer(&mut *out, b).map_err(io::Error::from)?;
        writeln!(out)?;
    } else {
        print_line(out, b, width, style)?;
    }
    Ok(())
}

/// One line: id, title, URL, tags. Title and note are flattened onto the line.
fn print_line(out: &mut impl Write, b: &Bookmark, width: usize, s: &Style) -> io::Result<()> {
    write!(out, "{}{:>width$}{}  ", s.id, b.id, s.reset)?;
    if let Some(title) = &b.title {
        write!(out, "{}  ", one_line(title))?;
    }
    write!(out, "{}{}{}", s.dim, b.url, s.reset)?;
    for t in &b.tags {
        write!(out, "  {}#{t}{}", s.tag, s.reset)?;
    }
    writeln!(out)
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn print_full(out: &mut impl Write, b: &Bookmark) -> io::Result<()> {
    writeln!(out, "id:       {}", b.id)?;
    writeln!(out, "url:      {}", b.url)?;
    if let Some(t) = &b.title {
        writeln!(out, "title:    {t}")?;
    }
    if !b.tags.is_empty() {
        writeln!(out, "tags:     {}", b.tags.join(" "))?;
    }
    writeln!(out, "created:  {}", format_time(b.created_at))?;
    writeln!(out, "updated:  {}", format_time(b.updated_at))?;
    writeln!(out, "uid:      {}", b.uid)?;
    if let Some(n) = &b.note {
        writeln!(out, "\n{n}")?;
    }
    if let Some(s) = &b.summary {
        writeln!(out, "\nSummary:\n{s}")?;
    }
    Ok(())
}

/// Formats Unix milliseconds as `YYYY-MM-DD HH:MM UTC`.
fn format_time(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days, from Howard Hinnant's date algorithms.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60
    )
}

/// Opens `url` with `$BROWSER` (first entry of a `:`-separated list; `%s` is replaced by the
/// URL), else `xdg-open`, or `open` on macOS.
fn open(url: &str) -> Result<()> {
    let browser = std::env::var("BROWSER").unwrap_or_default();
    let browser = browser.split(':').next().unwrap_or("").trim();
    let (prog, args): (String, Vec<String>) = if browser.is_empty() {
        let prog = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        (prog.into(), vec![url.into()])
    } else {
        let mut words = browser.split_whitespace().map(str::to_string);
        let prog = words.next().unwrap_or_default();
        let mut args: Vec<String> = words.collect();
        if args.iter().any(|a| a.contains("%s")) {
            args = args.iter().map(|a| a.replace("%s", url)).collect();
        } else {
            args.push(url.into());
        }
        (prog, args)
    };
    // Wait, so terminal browsers keep the terminal and failures are reported.
    let status = Command::new(&prog)
        .args(&args)
        .status()
        .with_context(|| format!("cannot run {prog}"))?;
    if !status.success() {
        bail!("{prog} failed ({status})");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_times() {
        assert_eq!(format_time(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_time(951_782_400_000), "2000-02-29 00:00 UTC");
        assert_eq!(format_time(1_794_717_000_000), "2026-11-15 04:30 UTC");
        assert_eq!(format_time(-60_000), "1969-12-31 23:59 UTC");
    }
}
