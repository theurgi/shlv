//! `shlv model`: train, inspect, and evaluate the tag classifier.

use std::collections::HashSet;
use std::io::Write;
use std::process::ExitCode;

use anyhow::{Result, bail};
use shlv::Store;
use shlv::choose::{Choice, ChooseTags};
use shlv::config::Config;
use shlv::model::{self, Classifier, Metrics, train};

use crate::{format_time, plural};

#[derive(clap::Subcommand)]
pub enum ModelCmd {
    /// Train the classifier on your tags now (suggest retrains it when the tags change)
    Train,
    /// Show the stored model: when it was trained, on how many examples, its size
    Status,
    /// Measure the classifier on your own bookmarks: train on the oldest 80%, test the newest 20%
    Eval {
        /// Also measure the [ai] tag provider on up to --limit test bookmarks (sends them)
        #[arg(long)]
        provider: bool,
        /// How many test bookmarks to send to the provider
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Go ahead when more than 20 bookmarks would be sent
        #[arg(short, long)]
        yes: bool,
    },
}

pub fn run(
    store: &mut Store,
    config: &Config,
    cmd: ModelCmd,
    out: &mut impl Write,
) -> Result<ExitCode> {
    match cmd {
        ModelCmd::Train => {
            let t = store.train_model()?;
            writeln!(
                out,
                "trained on {} for {} in {} ms; the model is {}",
                plural(t.examples, "example"),
                plural(t.tags, "canonical tag"),
                t.millis,
                size(t.bytes)
            )?;
        }
        ModelCmd::Status => match store.stored_model()? {
            None => writeln!(
                out,
                "no model yet: `shlv model train`, or `shlv suggest` trains one"
            )?,
            Some(s) => {
                let cold: Vec<&str> = s
                    .model
                    .tags
                    .iter()
                    .filter(|t| (t.positives as usize) < model::FULL_AT)
                    .map(|t| t.name.as_str())
                    .collect();
                writeln!(
                    out,
                    "trained {} on {} for {}; {}{}",
                    format_time(s.trained_at),
                    plural(s.model.examples as usize, "example"),
                    plural(s.model.tags.len(), "tag"),
                    size(s.bytes),
                    if s.stale {
                        "; stale: the tags changed since"
                    } else {
                        ""
                    }
                )?;
                if !cold.is_empty() {
                    writeln!(
                        out,
                        "fewer than {} examples (the name and description match still counts): {}",
                        model::FULL_AT,
                        cold.join(" ")
                    )?;
                }
            }
        },
        ModelCmd::Eval {
            provider,
            limit,
            yes,
        } => {
            let test = eval(store, config, out)?;
            if provider {
                return eval_provider(store, config, &test, limit, yes, out);
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn size(bytes: usize) -> String {
    if bytes >= 1 << 20 {
        format!("{:.1} MB", bytes as f64 / (1 << 20) as f64)
    } else {
        format!("{:.0} KB", (bytes as f64 / 1024.0).ceil())
    }
}

type TestSet = Vec<(shlv::choose::Item, HashSet<String>)>;

/// The time-split evaluation of the classifier; returns the test set for a provider run.
fn eval(store: &Store, config: &Config, out: &mut impl Write) -> Result<TestSet> {
    let offer_at = config.model.threshold;
    let list = model::canonical_list(store)?;
    if list.is_empty() {
        bail!("no canonical tags: the classifier chooses from the canonical list");
    }
    let examples = store.training_examples()?;
    let tagged = examples
        .iter()
        .filter(|(_, _, e)| !e.positive.is_empty())
        .count();
    if tagged < 20 {
        bail!(
            "too few tagged bookmarks to evaluate ({tagged}; at least 20 with canonical tags \
             are needed)"
        );
    }
    // Oldest 80% to train (rejections included), newest 20% of the tagged ones to test.
    let cut = examples.len() * 4 / 5;
    let (train_set, test_set) = examples.split_at(cut);
    let model = train(
        &train_set
            .iter()
            .map(|(_, _, e)| e.clone())
            .collect::<Vec<_>>(),
        &list,
    );
    let test: TestSet = test_set
        .iter()
        .filter(|(_, _, e)| !e.positive.is_empty())
        .map(|(_, i, e)| (i.clone(), e.positive.clone()))
        .collect();
    let lexical = Classifier::new(&list, None);
    let trained = Classifier::new(&list, Some(&model));
    writeln!(
        out,
        "{} canonical tags; trained on the oldest {}, tested on the newest {}",
        list.len(),
        plural(train_set.len(), "example"),
        plural(test.len(), "tagged bookmark")
    )?;
    header(out, offer_at)?;
    let t = std::time::Instant::now();
    let r = run_layer(&trained, &test, &list)?;
    let micros = t.elapsed().as_secs_f64() * 1e6 / test.len() as f64;
    let classifier = model::evaluate(&r, list.len(), offer_at);
    row(out, "classifier", &classifier, &format!("{micros:.0} µs"))?;
    let r = run_layer(&lexical, &test, &list)?;
    row(
        out,
        "names and descriptions only",
        &model::evaluate(&r, list.len(), offer_at),
        "",
    )?;
    writeln!(
        out,
        "\n±: 95% sampling error of P@1 on {}; smaller differences between layers are \
         noise.\nECE: over every bookmark and tag, mostly easy negatives; \
         ECE≥{offer_at}: over offered choices only.",
        plural(test.len(), "test bookmark")
    )?;
    reliability(out, &classifier, offer_at)?;
    Ok(test)
}

/// What the classifier offered, by probability, against how often it was right.
fn reliability(out: &mut impl Write, m: &Metrics, offer_at: f64) -> std::io::Result<()> {
    if m.reliability.is_empty() {
        return writeln!(out, "\nThe classifier offered nothing at p ≥ {offer_at}.");
    }
    writeln!(
        out,
        "\nClassifier choices offered (p ≥ {offer_at}):\n{:<11} {:>7} {:>7} {:>6}",
        "p", "choices", "mean p", "right"
    )?;
    for b in &m.reliability {
        writeln!(
            out,
            "{:<11} {:>7} {:>7.2} {:>6.2}",
            format!("{}-{:.1}", b.from, b.to),
            b.choices,
            b.mean_p,
            b.precision
        )?;
    }
    Ok(())
}

fn run_layer(
    layer: &dyn ChooseTags,
    test: &TestSet,
    list: &[shlv::choose::TagOption],
) -> Result<Vec<(Vec<Choice>, HashSet<String>)>> {
    test.iter()
        .map(|(item, truth)| Ok((layer.choose_tags(item, list)?, truth.clone())))
        .collect()
}

fn header(out: &mut impl Write, offer_at: f64) -> std::io::Result<()> {
    writeln!(
        out,
        "\n{:<28} {:>5} {:>5} {:>5} {:>6} {:>6} {:>6} {:>8} {:>6} {:>6}  time/bookmark",
        "layer",
        "P@1",
        "±",
        "R@3",
        "P@0.5",
        "R@0.5",
        "ECE",
        format!("ECE≥{offer_at}"),
        "P@0.9",
        "n@0.9"
    )
}

fn row(out: &mut impl Write, name: &str, m: &Metrics, time: &str) -> std::io::Result<()> {
    let opt = |x: Option<f64>| x.map_or("-".to_string(), |x| format!("{x:.2}"));
    writeln!(
        out,
        "{:<28} {:>5.2} {:>5.2} {:>5.2} {:>6.2} {:>6.2} {:>6} {:>8} {:>6} {:>6.2}  {time}",
        name,
        m.precision_at_1,
        m.precision_at_1_margin,
        m.recall_at_3,
        m.precision_at_half,
        m.recall_at_half,
        opt(m.ece),
        opt(m.ece_offered),
        opt(m.precision_at_09),
        m.per_item_at_09
    )
}

/// The `[ai] tag` provider on the newest test bookmarks.
fn eval_provider(
    store: &Store,
    config: &Config,
    test: &TestSet,
    limit: usize,
    yes: bool,
    out: &mut impl Write,
) -> Result<ExitCode> {
    let provider = crate::ai_cmd::provider(config, shlv::ai::Task::Tag)?;
    let list = model::canonical_list(store)?;
    let sample: Vec<_> = test.iter().rev().take(limit).collect();
    out.flush()?;
    crate::ai_cmd::confirm(
        sample.len(),
        "evaluating with",
        &crate::ai_cmd::describe_provider(config, &provider),
        yes,
    )?;
    let t = std::time::Instant::now();
    let mut results = Vec::new();
    let mut failed = 0;
    for (item, truth) in &sample {
        match provider.choose_tags(item, &list) {
            Ok(c) => results.push((c, truth.clone())),
            Err(e) => {
                failed += 1;
                eprintln!("shlv: {}: {e:#}", item.url);
            }
        }
    }
    let secs = t.elapsed().as_secs_f64() / sample.len().max(1) as f64;
    row(
        out,
        &format!("provider {}", provider.name()),
        &model::evaluate(&results, list.len(), config.model.threshold),
        &format!("{secs:.1} s"),
    )?;
    writeln!(
        out,
        "\nThe provider ran on the newest {} only{}; it returns no probabilities, so ECE and \
         P@0.9 don't apply.",
        plural(sample.len(), "test bookmark"),
        if failed > 0 {
            format!(" ({failed} failed)")
        } else {
            String::new()
        }
    )?;
    crate::ai_cmd::print_usage(&provider);
    Ok(ExitCode::SUCCESS)
}
