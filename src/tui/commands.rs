//! The `:` command line: views by name, and the views' actions as commands (`:tag rename a b`,
//! `:rule add host tag`, `:fetch`, …), with Tab completing command names, then tag names.

use super::app::{App, View};
use super::worker::Job;

/// Commands and what they take, for `:help` and completion.
pub const COMMANDS: &[(&str, &str)] = &[
    ("search", "QUERY: show it in Bookmarks"),
    ("review", "start a review batch"),
    (
        "tag",
        "rename A B | merge A B | alias ALIAS TAG | unalias ALIAS | canonical TAG… | uncanonical TAG… | describe TAG TEXT",
    ),
    ("rule", "add HOST TAG… | rm HOST [TAG…]"),
    ("interest", "add NAME… | rm NAME…"),
    (
        "seed",
        "[INTEREST…]: expand interests into candidate tags (ai)",
    ),
    ("draft", "[N]: draft tags from N bookmarks (ai)"),
    ("fetch", "fetch titles for bookmarks without one (fetch)"),
    ("summarize", "summarize bookmarks without a summary (ai)"),
    ("train", "retrain the classifier (model)"),
    ("eval", "measure the classifier (model)"),
    ("jobs", "the Status view"),
    ("cancel", "[N]: cancel job N, or every job"),
    ("help", "the keys"),
    ("quit", "quit"),
];

const TAG_SUBCOMMANDS: &[&str] = &[
    "rename",
    "merge",
    "alias",
    "unalias",
    "canonical",
    "uncanonical",
    "describe",
];

impl App {
    /// Runs a `:` command.
    pub fn run_command(&mut self, text: &str) {
        let words: Vec<&str> = text.split_whitespace().collect();
        let Some(&cmd) = words.first() else {
            return;
        };
        let args = &words[1..];
        let rest = text
            .trim_start()
            .split_once(char::is_whitespace)
            .map_or("", |(_, r)| r.trim());
        match cmd {
            "q" | "q!" | "quit" | "qa" | "wq" | "x" => self.quit = true,
            "help" | "h" => self.help = true,
            "search" | "s" => self.show_bookmarks(rest),
            "review" => {
                self.switch(View::Review);
                self.run_review();
            }
            "tag" => self.tag_command(args, text),
            "rule" => self.rule_command(args),
            "interest" | "interests" => self.interest_command(args),
            "seed" => {
                let topics: Vec<String> = args.iter().map(|s| s.to_string()).collect();
                let interests = if topics.is_empty() {
                    match self.store.interests() {
                        Ok(i) => i.into_iter().map(|i| i.name).collect(),
                        Err(e) => return self.warn(format!("{e:#}")),
                    }
                } else {
                    topics
                };
                if interests.is_empty() {
                    return self
                        .warn("no interests: :seed nix home-lab, or add some with :interest add");
                }
                self.ask_seed(interests);
            }
            "draft" => {
                let n = match args.first().map(|a| a.parse::<usize>()) {
                    None => 200,
                    Some(Ok(n)) if n > 0 => n,
                    _ => return self.warn("draft N: a number of bookmarks above 0"),
                };
                if let Err(e) = self.ask_draft(n, 30) {
                    self.warn(format!("{e:#}"));
                }
            }
            "fetch" => self.send_job(Job::FetchAll),
            "summarize" => self.ask_summarize(),
            "train" => self.send_job(Job::Train),
            "eval" => {
                self.switch(View::Status);
                self.send_job(Job::Eval);
            }
            "jobs" => self.switch(View::Status),
            "cancel" => match args.first().map(|a| a.parse::<u64>()) {
                None => self.cancel_all(),
                Some(Ok(n)) => self.cancel_job(n),
                Some(Err(_)) => self.warn("cancel N: a job number (the Status view lists them)"),
            },
            name => match View::by_name(name) {
                Some(v) => self.switch(v),
                None => self.warn(format!("unknown command :{text} (? lists the keys)")),
            },
        }
    }

    fn tag_command(&mut self, args: &[&str], text: &str) {
        let r = match args {
            ["rename", old, new] => self
                .store
                .rename_tag(old, new)
                .map(|r| format!("renamed #{old} to #{} ({} bookmarks)", r.tag, r.bookmarks)),
            ["merge", from, into] => self.store.merge_tags(from, into).map(|r| {
                format!(
                    "merged #{from} into #{} ({} bookmarks); {from} is an alias now",
                    r.tag, r.bookmarks
                )
            }),
            ["alias", alias, tag] => self.store.add_alias(alias, tag).map(|added| {
                if added {
                    format!("{alias} is an alias of #{tag}")
                } else {
                    format!("{alias} already is an alias of #{tag}")
                }
            }),
            ["unalias", alias] => match self.store.remove_alias(alias) {
                Ok(Some(of)) => Ok(format!("removed alias {alias} (of #{of})")),
                Ok(None) => Err(anyhow::anyhow!("no alias {alias:?}")),
                Err(e) => Err(e),
            },
            ["canonical", tags @ ..] if !tags.is_empty() => {
                let tags: Vec<String> = tags.iter().map(|t| t.to_string()).collect();
                self.store
                    .add_canonical(&tags)
                    .map(|n| format!("marked {} canonical", crate::plural(n, "tag")))
            }
            ["uncanonical", tags @ ..] if !tags.is_empty() => {
                let tags: Vec<String> = tags.iter().map(|t| t.to_string()).collect();
                self.store
                    .remove_canonical(&tags)
                    .map(|n| format!("unmarked {}", crate::plural(n, "tag")))
            }
            ["describe", tag, ..] => {
                // The text is everything after the tag.
                let desc = text
                    .split_once(tag)
                    .map_or("", |(_, d)| d.trim())
                    .trim_matches('"');
                self.store.describe_tag(tag, desc).map(|(name, changed)| {
                    if changed {
                        format!("described #{name}")
                    } else {
                        format!("#{name}: unchanged")
                    }
                })
            }
            _ => Err(anyhow::anyhow!(
                "tag {}",
                COMMANDS.iter().find(|c| c.0 == "tag").map_or("", |c| c.1)
            )),
        };
        match r {
            Ok(msg) => {
                self.say(msg);
                self.tags_changed();
            }
            Err(e) => self.warn(format!("{e:#}")),
        }
    }

    fn rule_command(&mut self, args: &[&str]) {
        match args {
            ["add", host, tags @ ..] if !tags.is_empty() => {
                if let Err(e) = self.rules_add(host, &tags.join(" ")) {
                    self.warn(format!("{e:#}"));
                }
            }
            ["rm", host, tags @ ..] => {
                let tags: Vec<String> = tags.iter().map(|t| t.to_string()).collect();
                match self.store.remove_rule(host, &tags) {
                    Ok(r) => {
                        let mut msg =
                            format!("removed rule {} → #{}", r.host, r.removed.join(" #"));
                        if r.dropped > 0 {
                            msg.push_str(&format!(
                                "; dropped {}",
                                crate::plural(r.dropped, "pending suggestion")
                            ));
                        }
                        self.say(msg);
                        self.bookmarks_changed();
                        self.rules_changed();
                    }
                    Err(e) => self.warn(format!("{e:#}")),
                }
            }
            _ => self.warn("rule add HOST TAG… | rule rm HOST [TAG…]"),
        }
    }

    fn interest_command(&mut self, args: &[&str]) {
        let names = |a: &[&str]| -> Vec<String> {
            a.iter()
                .map(|n| shlv::seed::normalize_interest(n))
                .filter(|n| !n.is_empty())
                .collect()
        };
        let r = match args {
            ["add", rest @ ..] if !rest.is_empty() => self
                .store
                .save_interests(&names(rest))
                .map(|n| format!("saved {}", crate::plural(n, "new interest"))),
            ["rm", rest @ ..] if !rest.is_empty() => self
                .store
                .forget_interests(&names(rest))
                .map(|n| format!("forgot {}", crate::plural(n, "interest"))),
            _ => Err(anyhow::anyhow!("interest add NAME… | interest rm NAME…")),
        };
        match r {
            Ok(msg) => {
                self.say(msg);
                self.interests_changed();
            }
            Err(e) => self.warn(format!("{e:#}")),
        }
    }

    /// Asks before summarizing every bookmark without a summary.
    fn ask_summarize(&mut self) {
        {
            let p = match crate::ai_cmd::provider(&self.config, shlv::ai::Task::Summarize) {
                Ok(p) => p,
                Err(e) => return self.warn(format!("{e:#}")),
            };
            let mut n = 0;
            let r = self.store.list_each(
                &shlv::ListQuery {
                    tags: Vec::new(),
                    limit: usize::MAX,
                },
                |b| {
                    n += usize::from(b.summary.is_none());
                    Ok(())
                },
            );
            if let Err(e) = r {
                return self.warn(format!("{e:#}"));
            }
            if n == 0 {
                return self.say("every bookmark has a summary");
            }
            let to = crate::ai_cmd::describe_provider(&self.config, &p);
            self.confirm(
                vec![
                    format!("summarize sends to {to}:"),
                    format!(
                        "  {}, one request each: the title, URL, and the start of the page \
                         text (fetched now)",
                        crate::plural(n, "bookmark without a summary")
                    ),
                    "  Status (6) shows the progress; x cancels.".into(),
                ],
                Job::SummarizeAll,
            );
        }
    }

    /// Candidates for Tab after `head` (the text before the cursor): command names, then
    /// subcommands, then tag names or interests.
    pub(super) fn command_completions(&mut self, head: &str) -> Vec<String> {
        let words: Vec<&str> = head.split_whitespace().collect();
        // The word being typed is the last one, unless the text ends in a space.
        let done = if head.ends_with(char::is_whitespace) {
            words.len()
        } else {
            words.len().saturating_sub(1)
        };
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        match (done, words.first().copied()) {
            (0, _) => {
                let mut v: Vec<String> = COMMANDS.iter().map(|c| c.0.to_string()).collect();
                v.extend(View::ALL.iter().map(|v| v.name().to_string()));
                v
            }
            (1, Some("tag")) => names(TAG_SUBCOMMANDS),
            (1, Some("rule")) => names(&["add", "rm"]),
            (1, Some("interest" | "interests")) => names(&["add", "rm"]),
            (_, Some("seed")) | (2.., Some("interest" | "interests")) => self
                .store
                .interests()
                .map(|i| i.into_iter().map(|i| i.name).collect())
                .unwrap_or_default(),
            (2, Some("rule")) => self
                .store
                .rules()
                .iter()
                .map(|(h, _)| h.to_string())
                .collect(),
            (_, Some("tag" | "search" | "s" | "rule")) => self.tag_names().to_vec(),
            _ => Vec::new(),
        }
    }
}
