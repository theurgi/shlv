# Tag classifier

shlv learns tags from your own bookmarks: a small classifier (logistic regression over words from
the title, host, URL path, summary, and note) trained on bookmarks that have canonical tags, plus
suggestions you rejected. It runs offline in microseconds and is stored in the database. Tags with
few examples fall back on matching their name, aliases, and description (`tag describe`), so it
helps from the first day; the match counts less as a tag gathers examples and drops out at 100.

```console
$ shlv model train      # suggest retrains it by itself when your tags change
trained on 8039 examples for 30 canonical tags in 182 ms; the model is 325 KB
$ shlv model eval       # train on your oldest 80%, test on the newest 20%
```

`model eval` reports precision and recall of the first choices, calibration (whether a 0.9 means
90%), and with `--provider` the same for the `[ai] tag` provider on a few test bookmarks, so you can
choose layers by measurement. Nothing leaves your machine unless you pass `--provider`. Read it with
its sampling error: the `±` column is the 95% margin of P@1 from the test sample alone, about ±0.13
with 50 test bookmarks (a 250-bookmark library) and ±0.06 with 300, so smaller differences between
layers may well be chance. Calibration comes twice: `ECE` over every bookmark and tag, which the
many easy "no"s keep small, and `ECE≥0.5` over the choices it would offer (`threshold`), followed by
a table of those choices by probability against how often they were right.

With the classifier built, `suggest` runs the layers in order: domain rules, then the classifier
(suggestions with source `model` and its probability), then the `[ai] tag` provider where the
classifier is unsure. `add` runs the classifier too, with the stored model. Tune it in `[model]`:

```toml
[model]
threshold = 0.5 # offer classifier tags with at least this probability
auto_threshold = 0.9 # with suggestions = "auto", apply only from here up; the rest wait
provider = "fallback" # ask [ai] tag: "fallback" (when the classifier is unsure), "always", "never"
```

"Unsure" means nothing reached `threshold`, or a tag that did has no examples yet. One example is
enough: requiring more (up to the 100 at which the lexical guess stops counting) sent more bookmarks
to the provider without making the offers on the rest more precise
([measured](../experiments/tag-models/RESULTS.md#provider-fallback-when-is-the-classifier-sure-task-041)
on 200 to 5k bookmarks). Every accepted or rejected suggestion is a training example the next time
the model trains.

## Back-filling a new tag

A tag added to the list is missing from every bookmark you tagged before, and the classifier reads
each of those as "this tag doesn't apply". `suggest --tag` finds the bookmarks that should have it:

```console
$ shlv suggest --tag wifi --all
classifier checked 8412 bookmarks for #wifi: 23 new suggestions
23 candidates for #wifi, 9 of them reviewed (listed first):
...
$ shlv suggest --tag wifi --all --pick   # y accepts, n rejects, s skips, q stops
```

It offers only that tag (no rules, no other tags) and ranks the candidates: bookmarks with canonical
tags first, since they're the classifier's examples, then by probability. A new tag has no examples
yet, so the classifier offers it from a probability of 0.1 rather than `threshold`: one match of its
name or an alias scores 0.45, two words of its description 0.15. With an `[ai] tag` provider (and
`provider` not `"never"`), the provider is asked too, with only that tag to choose and no proposals:
about the IDs given, or with `--all` about the tagged bookmarks the classifier passed over. Its yes
ranks as 0.5. A rejected candidate isn't offered again. [`shlv review`](config.md#review) runs the
same back-fill, on reviewed bookmarks, for each tag added after a batch.
