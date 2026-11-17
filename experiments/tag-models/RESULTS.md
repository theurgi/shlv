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

| candidate        | setting | P@1   | ±     | R@3   | P@0.5 | R@0.5 | R@3 common | R@3 rare | ECE   | P(p≥0.9) | n(p≥0.9) | time/bookmark |
| ---------------- | ------- | ----- | ----- | ----- | ----- | ----- | ---------- | -------- | ----- | -------- | -------- | ------------- |
| `lr`             | full    | 0.603 | 0.055 | 0.537 | 0.745 | 0.260 | 0.525      | 0.346    | 0.005 | 0.880    | 0.28     | < 0.1 ms      |
| `nb`             | full    | 0.503 | 0.056 | 0.485 | 0.133 | 0.273 | 0.525      | 0.210    | 0.076 | 0.254    | 1.39     | 0.1 ms        |
| `lexical`        | cold    | 0.467 | 0.056 | 0.387 | 0.566 | 0.344 | 0.330      | 0.321    | 0.032 | 0.776    | 0.19     | < 0.1 ms      |
| `lr+lexical-few` | few     | 0.453 | 0.056 | 0.428 | 0.575 | 0.336 | 0.394      | 0.321    | 0.026 | -        | 0.00     | < 0.1 ms      |
| `laya-choice`    | cold    | 0.373 | 0.054 | 0.371 | 0.377 | 0.184 | 0.343      | 0.337    | 0.039 | 0.406    | 0.83     | 285 ms        |
| `llm` (2B)       | cold    | 0.350 | 0.054 | 0.266 | 0.363 | 0.248 | 0.228      | 0.271    | n/a   | n/a      | n/a      | 404 ms        |
| `laya-hybrid`    | cold    | 0.307 | 0.052 | 0.366 | 0.260 | 0.219 | 0.348      | 0.216    | 0.041 | 0.429    | 0.05     | 1,024 ms      |
| `laya-noul`      | cold    | 0.197 | 0.045 | 0.262 | 0.146 | 0.276 | 0.226      | 0.176    | 0.112 | 0.080    | 0.25     | 5,139 ms      |

On the whole test split (10,085 bookmarks), for the fast candidates:

| candidate        | setting | P@1   | ±     | R@3   | P@0.5 | R@0.5 | ECE   | P(p≥0.9) | n(p≥0.9) | nonzero weights |
| ---------------- | ------- | ----- | ----- | ----- | ----- | ----- | ----- | -------- | -------- | --------------- |
| `lr` (2^18)      | full    | 0.534 | 0.010 | 0.521 | 0.680 | 0.256 | 0.004 | 0.851    | 0.26     | 1.0 M (≥0.05)   |
| `lr-b16` (2^16)  | full    | 0.525 | 0.010 | 0.510 | 0.673 | 0.251 | 0.005 | 0.848    | 0.25     | 1.0 M           |
| `lr-b14`         | full    | 0.501 | 0.010 | 0.488 | 0.647 | 0.244 | 0.007 | 0.845    | 0.25     | 0.7 M           |
| `lr-b12`         | full    | 0.447 | 0.010 | 0.439 | 0.592 | 0.219 | 0.012 | 0.819    | 0.22     | 0.2 M           |
| `nb`             | full    | 0.481 | 0.010 | 0.500 | 0.124 | 0.297 | 0.086 | 0.197    | 1.67     |                 |
| `lr+lexical-few` | few     | 0.400 | 0.010 | 0.384 | 0.511 | 0.299 | 0.027 | 0.857    | 0.00     | 61 k            |
| `lexical`        | cold    | 0.382 | 0.009 | 0.345 | 0.504 | 0.305 | 0.034 | 0.677    | 0.19     |                 |
| `nb-few`         | few     | 0.128 | 0.007 | 0.160 | 0.036 | 0.789 | 0.693 | 0.037    | 25.1     |                 |
| `lr-few`         | few     | 0.100 | 0.006 | 0.132 | 0.261 | 0.003 | 0.022 | 0.467    | 0.00     | 81 k            |

**Sampling error (added in 038).** `±` is the half-width of P@1's 95% interval (Wilson) from the
test sample alone. On 300 bookmarks it is ±0.05-0.06 (one standard error is about ±0.03), so on the
sample only differences above about 0.08 are clear: `lr` over `nb` and over `lexical`, and `lexical`
over the Laya and LLM candidates, but not `laya-choice` (0.37) against the 2B LLM (0.35), or `nb`
(0.50) against `lexical` (0.47). Paired comparisons (same bookmarks) are a little tighter than this
suggests. On the whole test split it is ±0.01, so the bucket sweep's steps are real.

Metrics: P@1, first choice is a true tag; R@3, share of true tags in the first three; P/R@0.5, of
choices with p ≥ 0.5; R@3 common/rare, macro recall over the most and least frequent third of the
tags; ECE, expected calibration error over every (bookmark, tag) pair, 10 bins; P(p≥0.9), precision
of choices at 0.9 or more (what auto mode would apply); n(p≥0.9), how many such choices per
bookmark. Training the full `lr` takes 2 s (40k bookmarks, 56 tags, 5 epochs).

The lexical score is uncalibrated as raw overlap. Measured precision by kind of match (whole test
split): one description word 0.06, two 0.16-0.19, one name or alias term 0.48, two or more 0.68.

### Observations

- **Trained logistic regression wins**, at every size and on both common and rare tags, and it is
  well calibrated over all pairs (ECE 0.005): what it calls 0.9 is right 85-88% of the time. (Over
  offered choices only it is about 0.1 overconfident; see task 038 below.) Naive Bayes ranks almost
  as well on common tags but its probabilities are useless (precision 0.25 at p ≥ 0.9, and over one
  such "certain" tag per bookmark).
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
  with fewer than 50 examples (100 since task 038). Not naive Bayes.
- **Thresholds (023):** offer classifier suggestions at p ≥ 0.5 (precision about 0.67-0.75) and
  apply in auto mode at p ≥ 0.9 (precision 0.85-0.88). Lexical-only scores never reach 0.9, so
  cold-start guesses always wait for review.
- **Behind providers:** LLMs stay an opt-in `[ai] tag` provider, used where the classifier is unsure
  (023's `fallback`). A decision-model provider (026) isn't worth building on this evidence:
  zero-shot Laya loses to the free lexical match and is 3-4 orders of magnitude slower. Revisit if a
  fine-tuned or calibrated decision model becomes available to test, or if users ask for Jev.
- **`shlv model eval`** lets users repeat this on their own bookmarks, locally.

## Small libraries (task 038)

Run 2026-11-17 with `tag-models small`. Everything above trains on 40k bookmarks; a personal library
has hundreds to a few thousand, and starts with none tagged. This trains **the shipped classifier**
(`shlv::model`: 2^16 buckets, quantized and pruned weights, the lexical blend) on random subsets of
the same train split, 5 subsets per size, and tests on the whole test split (the same 10,085
bookmarks). The run checks that its blend reproduces `Classifier::probabilities`.

### By training size (`FULL_AT = 100`, the new default)

| train  | tags ≥100 ex. | P@1 (5 subsets) | R@3   | P@0.5 | offered/bookmark | P@0.9 (5 subsets) | choices ≥0.9 | ECE all | ECE offered |
| ------ | ------------- | --------------- | ----- | ----- | ---------------- | ----------------- | ------------ | ------- | ----------- |
| 0      | 0             | 0.378           | 0.345 | 0.776 | 0.11             | -                 | 0            | 0.025   | 0.126       |
| 200    | 0             | 0.404 (0.40-41) | 0.345 | 0.776 | 0.11             | -                 | 0            | 0.021   | 0.177       |
| 500    | 0             | 0.441 (0.44-44) | 0.390 | 0.786 | 0.12             | -                 | 0            | 0.017   | 0.190       |
| 1,000  | 2             | 0.450 (0.44-45) | 0.420 | 0.758 | 0.17             | 0.902 (0.85-0.93) | 128          | 0.011   | 0.109       |
| 2,000  | 16            | 0.436 (0.43-44) | 0.413 | 0.728 | 0.28             | 0.867 (0.83-0.91) | 436          | 0.005   | 0.061       |
| 5,000  | 41            | 0.452 (0.45-46) | 0.433 | 0.696 | 0.46             | 0.863 (0.85-0.88) | 1,151        | 0.002   | 0.062       |
| 40,337 | 56            | 0.522           | 0.507 | 0.662 | 0.76             | 0.844             | 2,651        | 0.004   | 0.127       |

Sampling error: P@1 ±0.010 (95% bootstrap over the test split; ±0.055 on the 300-bookmark sample);
P@0.9 ±0.06 at 1k (128 choices), ±0.03 at 2k, ±0.02 at 5k, ±0.013 at full size. The spread between
subsets (in parentheses) is about as large as the test-set error, so one library of a given size can
land 0.01-0.02 either way on P@1 and 0.05 on P@0.9.

- **Thresholds hold.** Precision at p ≥ 0.9 is 0.84-0.90 at every size where anything reaches 0.9,
  so `auto_threshold = 0.9` keeps meaning about 85%. Below about 1k bookmarks nothing reaches it, so
  auto mode applies nothing and every suggestion waits for review, which is the safe failure.
  Precision at the 0.5 offer threshold is higher on small libraries (0.76-0.79) than on large ones
  (0.66): fewer, mostly lexical, offers.
- **Lexical probabilities hold.** On the whole test split: two or more name or alias terms 0.776
  (1,087 choices, ±0.025), one 0.478 (10,691, ±0.009), two description words 0.22 (54, ±0.11), one
  0.056 (4,299, ±0.007). The shipped 0.65 / 0.45 / 0.15 / 0.05 are at or below these, so they stay:
  a cold-start guess is, if anything, slightly better than it says.
- **The cold-start blend changed: `FULL_AT` 50 → 100.** With 50, P@1 dipped at 1k-2k bookmarks
  (0.414 and 0.407, against 0.433 at 500 and 0.378 with no training): a tag passing 50 examples lost
  the lexical score before its trained model was better than the lexical score. With 100: 0.450 and
  0.436, the same from 5k up, and P@0.9 the same or better. The cost is at the small end: P@1 0.404
  against 0.418 at 200 (just outside the noise), R@3 0.345 against 0.364 at 200 and 0.390 against
  0.411 at 500, and nothing reaches 0.9 at 500 (45 choices at 0.90 with 50). 1k-5k is where most
  personal libraries are, so 100 wins.

The blend sweep (mean of 5 subsets; "cap c": the trained model never gets more than weight c, so the
lexical score keeps 1 - c for good; "stack": the lexical match kinds as extra LR features):

| train  | blend        | P@1   | R@3   | P@0.5 | P@0.9 | choices ≥0.9 | ECE offered |
| ------ | ------------ | ----- | ----- | ----- | ----- | ------------ | ----------- |
| 200    | 50           | 0.418 | 0.364 | 0.794 | -     | 0            | 0.223       |
| 200    | **100**      | 0.404 | 0.345 | 0.776 | -     | 0            | 0.177       |
| 200    | 200          | 0.401 | 0.343 | 0.776 | -     | 0            | 0.151       |
| 500    | 50           | 0.433 | 0.411 | 0.773 | 0.903 | 45           | 0.142       |
| 500    | **100**      | 0.441 | 0.390 | 0.786 | -     | 0            | 0.190       |
| 500    | 50 cap 0.85  | 0.440 | 0.414 | 0.778 | 0.911 | 22           | 0.151       |
| 1,000  | 50           | 0.414 | 0.394 | 0.736 | 0.860 | 230          | 0.076       |
| 1,000  | **100**      | 0.450 | 0.420 | 0.758 | 0.902 | 128          | 0.109       |
| 1,000  | 200          | 0.449 | 0.389 | 0.775 | -     | 0            | 0.163       |
| 1,000  | 50 cap 0.85  | 0.442 | 0.418 | 0.757 | 0.893 | 81           | 0.095       |
| 1,000  | stack 50     | 0.442 | 0.415 | 0.693 | 0.816 | 356          | 0.067       |
| 2,000  | 50           | 0.407 | 0.390 | 0.725 | 0.864 | 493          | 0.047       |
| 2,000  | **100**      | 0.436 | 0.413 | 0.728 | 0.867 | 436          | 0.061       |
| 2,000  | 200          | 0.462 | 0.430 | 0.758 | 0.885 | 277          | 0.096       |
| 2,000  | 50 cap 0.85  | 0.442 | 0.425 | 0.751 | 0.890 | 202          | 0.058       |
| 5,000  | 50           | 0.449 | 0.430 | 0.696 | 0.865 | 1,212        | 0.065       |
| 5,000  | **100**      | 0.452 | 0.433 | 0.696 | 0.863 | 1,151        | 0.062       |
| 5,000  | 50 cap 0.85  | 0.470 | 0.453 | 0.722 | 0.896 | 550          | 0.024       |
| 5,000  | stack 50     | 0.480 | 0.452 | 0.681 | 0.843 | 1,599        | 0.084       |
| 40,337 | 50 / **100** | 0.522 | 0.507 | 0.662 | 0.844 | 2,651        | 0.127       |
| 40,337 | 50 cap 0.85  | 0.530 | 0.515 | 0.698 | 0.901 | 1,365        | 0.053       |
| 40,337 | stack 50     | 0.532 | 0.509 | 0.658 | 0.844 | 2,776        | 0.130       |

No blend at all (`FULL_AT` 1) is far worse on small libraries (P@1 0.20 at 200, 0.33 at 1k): a few
examples per tag aren't a model. **Capping** the trained weight (0.85) matches or beats 100 from 2k
up (P@1 +0.01-0.02) and is better calibrated, but below a cap of 0.9 a tag can only reach 0.9 when
its name or alias is in the bookmark. Delicious favors that (its tags are often title words); a
user's `reading-list` or `work` would never be auto-applied, so it wasn't adopted. **Stacking**
helps P@1 at 5k and up, but it is less calibrated at 0.9 (0.82-0.84), and the features would need
the list. Neither is worth it on this evidence.

### Calibration of what is offered

The ECE above (025, 022) is over every (bookmark, tag) pair. With 56 tags and two true ones per
bookmark, almost all of them are easy negatives near p = 0. That keeps it at 0.004 even when the
offered choices are off. Over offered choices only (p ≥ 0.5), the full model's ECE is 0.127:

| p       | 200: n / mean p / right | 1k: n / mean p / right | 5k: n / mean p / right | 40k: n / mean p / right |
| ------- | ----------------------- | ---------------------- | ---------------------- | ----------------------- |
| 0.5-0.6 | 584 / 0.58 / 0.72       | 736 / 0.55 / 0.71      | 1,021 / 0.55 / 0.54    | 1,407 / 0.55 / 0.45     |
| 0.6-0.7 | 487 / 0.62 / 0.84       | 354 / 0.65 / 0.75      | 861 / 0.65 / 0.60      | 1,184 / 0.65 / 0.52     |
| 0.7-0.8 | 21 / 0.72 / 0.93        | 279 / 0.75 / 0.79      | 781 / 0.75 / 0.68      | 1,174 / 0.75 / 0.61     |
| 0.8-0.9 | 0                       | 188 / 0.86 / 0.83      | 866 / 0.85 / 0.76      | 1,230 / 0.85 / 0.69     |
| 0.9-1.0 | 0                       | 128 / 0.95 / 0.90      | 1,151 / 0.96 / 0.86    | 2,651 / 0.97 / 0.84     |

(n is per subset.) Small libraries are **underconfident**, since the offers are mostly lexical and
the lexical probabilities are conservative. Large ones are **overconfident** by about 0.1 in every
bin. The 0.9-1.0 bin averages p = 0.97 and is right 84% of the time. The 0.9 threshold still works
as "about 85%", but a 0.97 doesn't mean 97%. Two likely causes, neither tested here: time drift
(training is older than the test bookmarks), and missing labels (a true tag no user gave counts as
wrong). Real libraries have both. `shlv model eval` now prints this ECE and table, so a user sees it
on their own data.

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
cargo run --release -- small OUT                # task 038: the shipped classifier at 200..5k bookmarks
```
