# Which tag-choosing layer works? (task 025)

Run 2026-11-16. Scripts: `src/main.rs` in this directory (`tag-models prep|run|eval`). The data and
predictions stay out of the repo.

## Data

[HetRec 2011 Delicious](https://grouplens.org/datasets/hetrec-2011/) (`hetrec2011-delicious-2k`):
69,226 URLs with titles, bookmarked and tagged by 1,867 Delicious users in 2010 and earlier, with
tag timestamps. License: "made available for non-commercial use"; cite Delicious and the HetRec 2011
workshop. Fine for this private experiment; nothing from it is committed or shipped.

The canonical list (`canonical.toml`) has 56 tags with one-line descriptions, written as a user
would write `[tags.canonical]`. Its aliases map 180 raw Delicious tags onto it (for example `web20`,
`twitter`, `facebook` → `socialmedia`); generic tags (`web`, `cool`, `free`) map to nothing. A
bookmark's labels are the canonical tags any user gave it. 50,422 bookmarks have at least one (2.14
on average); none of the user's own bookmarks were used.

Split by time (first tag assignment): the oldest 80% (40,337) train, the newest 20% (10,085) test.
Settings: **cold** (no training data), **few** (the first 5 training examples of each tag, 172
bookmarks), **full** (all 40,337). Slow candidates ran on 300 test bookmarks spread evenly over the
test split. Every candidate sees only the title and URL (the dataset has no page text, and most of
its 2010 pages are gone).

## Candidates

- `lexical`: bookmark words against tag names, aliases (weight 1) and description words (0.4).
- `nb`, `lr`: hashed-token (title, domain, host labels, path words) multinomial naive Bayes, and
  one-vs-rest logistic regression (SGD, 5 epochs, L2 1e-6), 2^18 buckets; `lr-b12..16` with fewer
  buckets; `-few` on the few-shot set; `lr+lexical-few` blends the lexical score in for tags with
  fewer than 50 examples.
- `laya-*`: Laya (Q8_0, 449 MB) on llama.cpp's `/v1/systemone` endpoint, zero-shot. `laya-choice`:
  one 56-option `choice`, its probabilities as scores. `laya-noul`: one yes/no question per tag.
  `laya-hybrid`: the choice, then yes/no for its 8 likeliest tags.
- `llm`: `Sharp-MiniCPM5-2B` through shlv's own openai-compatible provider (018), constrained to the
  list by a JSON-schema `enum`; it returns tags without probabilities.
- Fine-tuning Laya and embedding models weren't tried: the VM has no Python stack, and no embedding
  model was served.

## Results

On the 300-bookmark sample, all candidates on the same bookmarks:

| candidate        | setting | P@1   | R@3   | P@0.5 | R@0.5 | R@3 common | R@3 rare | ECE   | P(p≥0.9) | n(p≥0.9) | time/bookmark |
| ---------------- | ------- | ----- | ----- | ----- | ----- | ---------- | -------- | ----- | -------- | -------- | ------------- |
| `lr`             | full    | 0.603 | 0.537 | 0.745 | 0.260 | 0.525      | 0.346    | 0.005 | 0.880    | 0.28     | < 0.1 ms      |
| `nb`             | full    | 0.503 | 0.485 | 0.133 | 0.273 | 0.525      | 0.210    | 0.076 | 0.254    | 1.39     | 0.1 ms        |
| `lexical`        | cold    | 0.467 | 0.387 | 0.566 | 0.344 | 0.330      | 0.321    | 0.032 | 0.776    | 0.19     | < 0.1 ms      |
| `lr+lexical-few` | few     | 0.453 | 0.428 | 0.575 | 0.336 | 0.394      | 0.321    | 0.026 | -        | 0.00     | < 0.1 ms      |
| `laya-choice`    | cold    | 0.373 | 0.371 | 0.377 | 0.184 | 0.343      | 0.337    | 0.039 | 0.406    | 0.83     | 285 ms        |
| `llm` (2B)       | cold    | 0.350 | 0.266 | 0.363 | 0.248 | 0.228      | 0.271    | n/a   | n/a      | n/a      | 404 ms        |
| `laya-hybrid`    | cold    | 0.307 | 0.366 | 0.260 | 0.219 | 0.348      | 0.216    | 0.041 | 0.429    | 0.05     | 1,024 ms      |
| `laya-noul`      | cold    | 0.197 | 0.262 | 0.146 | 0.276 | 0.226      | 0.176    | 0.112 | 0.080    | 0.25     | 5,139 ms      |

On the whole test split (10,085 bookmarks), for the fast candidates:

| candidate        | setting | P@1   | R@3   | P@0.5 | R@0.5 | ECE   | P(p≥0.9) | n(p≥0.9) | nonzero weights |
| ---------------- | ------- | ----- | ----- | ----- | ----- | ----- | -------- | -------- | --------------- |
| `lr` (2^18)      | full    | 0.534 | 0.521 | 0.680 | 0.256 | 0.004 | 0.851    | 0.26     | 1.0 M (≥0.05)   |
| `lr-b16` (2^16)  | full    | 0.525 | 0.510 | 0.673 | 0.251 | 0.005 | 0.848    | 0.25     | 1.0 M           |
| `lr-b14`         | full    | 0.501 | 0.488 | 0.647 | 0.244 | 0.007 | 0.845    | 0.25     | 0.7 M           |
| `lr-b12`         | full    | 0.447 | 0.439 | 0.592 | 0.219 | 0.012 | 0.819    | 0.22     | 0.2 M           |
| `nb`             | full    | 0.481 | 0.500 | 0.124 | 0.297 | 0.086 | 0.197    | 1.67     |                 |
| `lr+lexical-few` | few     | 0.400 | 0.384 | 0.511 | 0.299 | 0.027 | 0.857    | 0.00     | 61 k            |
| `lexical`        | cold    | 0.382 | 0.345 | 0.504 | 0.305 | 0.034 | 0.677    | 0.19     |                 |
| `nb-few`         | few     | 0.128 | 0.160 | 0.036 | 0.789 | 0.693 | 0.037    | 25.1     |                 |
| `lr-few`         | few     | 0.100 | 0.132 | 0.261 | 0.003 | 0.022 | 0.467    | 0.00     | 81 k            |

Metrics: P@1, first choice is a true tag; R@3, share of true tags in the first three; P/R@0.5, of
choices with p ≥ 0.5; R@3 common/rare, macro recall over the most and least frequent third of the
tags; ECE, expected calibration error over every (bookmark, tag) pair, 10 bins; P(p≥0.9), precision
of choices at 0.9 or more (what auto mode would apply); n(p≥0.9), how many such choices per
bookmark. Training the full `lr` takes 2 s (40k bookmarks, 56 tags, 5 epochs).

The lexical score is uncalibrated as raw overlap. Measured precision by kind of match (whole test
split): one description word 0.06, two 0.16-0.19, one name or alias term 0.48, two or more 0.68.

### Observations

- **Trained logistic regression wins**, at every size and on both common and rare tags, and it is
  well calibrated (ECE 0.005): what it calls 0.9 is right 85-88% of the time. Naive Bayes ranks
  almost as well on common tags but its probabilities are useless (precision 0.25 at p ≥ 0.9, and
  over one such "certain" tag per bookmark).
- **2^16 buckets** cost about 0.01 P@1 against 2^18; 2^14 costs 0.03, 2^12 0.09.
- **Cold start: the lexical match is the best zero-shot layer here**, ahead of Laya and the 2B LLM,
  at no cost. Five examples per tag aren't enough to train on alone (`lr-few` 0.10), but blending
  them with the lexical score helps (`lr+lexical-few` R@3 0.38 against 0.35).
- **Laya zero-shot doesn't pay off.** One multi-option `choice` gives a primary tag about as well as
  the lexical match (P@1 0.37), concentrates probability on it (0.83 tags per bookmark "≥0.9", only
  41% right), and takes 285 ms. One yes/no question per tag is far worse (P@1 0.20) and badly
  calibrated (8% right at ≥0.9), at 5 s per bookmark; the hybrid sits between. The model card says
  the base checkpoints need fine-tuning and ship overconfident; this agrees, and the anecdote from
  the task notes (`rust` ranked 8th by yes/no questions) generalizes.
- **A small local LLM is not a good tagger from titles alone**: P@1 0.35, R@3 0.27 (it picks one tag
  most of the time), 404 ms per bookmark with a warm prompt cache. With page text (021's default) it
  does better on obvious tags but costs 10-25 s per bookmark on this host. A larger or hosted model
  would do better; this experiment didn't measure one (no API key).

## Recommendations

- **In the binary (022):** hashed-token one-vs-rest logistic regression, 2^16 buckets, title,
  domain, host labels, and path words (plus summary and note words when present), trained on tagged
  bookmarks; the lexical match against names, aliases, and descriptions as the cold start, with its
  scores mapped to the precision measured above (0.05 / 0.15 / 0.45 / 0.65), and blended in for tags
  with fewer than 50 examples. Not naive Bayes.
- **Thresholds (023):** offer classifier suggestions at p ≥ 0.5 (precision about 0.67-0.75) and
  apply in auto mode at p ≥ 0.9 (precision 0.85-0.88). Lexical-only scores never reach 0.9, so
  cold-start guesses always wait for review.
- **Behind providers:** LLMs stay an opt-in `[ai] tag` provider, used where the classifier is unsure
  (023's `fallback`). A decision-model provider (026) isn't worth building on this evidence:
  zero-shot Laya loses to the free lexical match and is 3-4 orders of magnitude slower. Revisit if a
  fine-tuned or calibrated decision model becomes available to test, or if users ask for Jev.
- **`shlv model eval`** lets users repeat this on their own bookmarks, locally.

## Reproducing

```sh
# HetRec files in DATA (e.g. unzip hetrec2011-delicious-2k.zip), output in OUT
cargo run --release -- prep DATA OUT
for c in lexical nb-few nb lr-few lr lr+lexical lr+lexical-few; do cargo run --release -- run $c OUT; done
TAG_BUCKETS_LOG2=16 cargo run --release -- run lr OUT   # bucket sweep: 12, 14, 16, 18
LLAMA_CPP_BASE_URL=http://host/v1 cargo run --release -- run laya-choice OUT  # also laya-noul, laya-hybrid
LLAMA_CPP_BASE_URL=http://host/v1 LLM_MODEL=Sharp-MiniCPM5-2B cargo run --release -- run llm OUT
cargo run --release -- eval OUT                 # every candidate on its own items
SAMPLE_ONLY=1 cargo run --release -- eval OUT   # every candidate on the 300-bookmark sample
```
