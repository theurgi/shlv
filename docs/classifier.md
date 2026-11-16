# Tag classifier

With the `model` feature, shlv learns tags from your own bookmarks: a small classifier (logistic
regression over words from the title, host, URL path, summary, and note) trained on bookmarks that
have canonical tags, plus suggestions you rejected. It runs offline in microseconds and is stored in
the database. Tags with few examples fall back on matching their name, aliases, and description
(`tag describe`), so it helps from the first day.

```console
$ shlv model train      # suggest retrains it by itself when your tags change
trained on 8039 examples for 30 canonical tags in 182 ms; the model is 325 KB
$ shlv model eval       # train on your oldest 80%, test on the newest 20%
```

`model eval` reports precision and recall of the first choices, calibration (whether a 0.9 means
90%), and with `--provider` the same for the `[ai] tag` provider on a few test bookmarks, so you can
choose layers by measurement. Nothing leaves your machine unless you pass `--provider`.

With the classifier built, `suggest` runs the layers in order: domain rules, then the classifier
(suggestions with source `model` and its probability), then the `[ai] tag` provider where the
classifier is unsure. `add` runs the classifier too, with the stored model. Tune it in `[model]`:

```toml
[model]
threshold = 0.5 # offer classifier tags with at least this probability
auto_threshold = 0.9 # with suggestions = "auto", apply only from here up; the rest wait
provider = "fallback" # ask [ai] tag: "fallback" (when the classifier is unsure), "always", "never"
```

"Unsure" means nothing reached `threshold`, or a tag that did has fewer than 50 examples. Every
accepted or rejected suggestion is a training example the next time the model trains.
