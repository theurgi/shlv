# AI providers

With the `ai` feature, an LLM can write summaries and suggest tags. AI is off until you configure
it: there is no default provider or model. Each task names a provider, so tagging can stay local
while summaries go elsewhere:

```toml
[ai]
summarize = "local" # a [provider.*] name; leave a task out to turn it off
tag = "local"

[provider.local]
kind = "openai-compatible" # llama.cpp, Ollama, vLLM, LM Studio, and hosted APIs that speak it
base_url = "http://localhost:8080/v1"
model = "qwen2.5-3b-instruct"
# api_key_env = "OPENAI_API_KEY" # the variable holding the key; never the key itself
# timeout = 120                  # seconds per reply
```

The Claude API is `kind = "anthropic"`. The key comes from an environment variable or from a file
that shlv reads only when it sends a request (an agenix or sops-nix secret, say; shlv trims the
trailing newline and warns if every user can read the file). Set one of the two:

```toml
[provider.claude]
kind = "anthropic"
model = "claude-opus-5-5"
effort = "low" # low, medium, high, xhigh, max: less thinking, fewer tokens
api_key_env = "ANTHROPIC_API_KEY"
# api_key_file = "/run/agenix/anthropic" # instead of api_key_env
```

Requests to Anthropic cache the instructions and the canonical tag list, so a bulk run pays full
price for that prefix once. 429 and 529 responses are retried (honoring `retry-after`).

**What is sent.** For each bookmark, a request to the provider's `base_url` with the bookmark's URL,
title, note, and summary (when it has them), the page's extracted main text (up to 3000 words) when
the task fetches the page, and for tagging the canonical tag list with aliases and descriptions.
Nothing else from the database, and nothing at all unless `[ai]` names a provider. The API key is
read from the named environment variable only when a request is made.

`shlv summarize ID...` (or `--all` for every bookmark without a summary; `--force` redoes them)
fetches each page, extracts its main text, and stores the provider's two- or three-sentence summary,
which `search` then finds. Pages that can't be fetched count as failures; pages with no text (an
empty JavaScript app) are skipped. It runs two requests at a time (`-j`), commits each summary as it
arrives (an interrupted run keeps them; rerun to continue), prints token use, and exits nonzero if
any failed. It says how many bookmarks it will send where before it starts, and above 20 it needs
`--yes`.

With `[ai] tag` set, `shlv suggest ID...` also asks that provider to choose tags from the canonical
list (with aliases and descriptions; tags the bookmark has or rejected are left out), and its
choices join the rules' as suggestions with source `llm` and no confidence score, since a model's
own confidence isn't calibrated. The bookmark's summary is sent if it has one, else the page text.
`suggest --all` sends bookmarks with fewer than `--max-tags` tags (default 1: untagged ones) that
the provider hasn't tagged before (`--force` sends them again), with the same `-j`, `--yes`, and
failure reporting as `summarize`. When `[ai] summarize` and `[ai] tag` name the same provider, a
bookmark without a summary gets both from one request.

With an `anthropic` provider, `summarize --batch` and `suggest --batch` submit one Message Batch
instead of live requests: half the price, with results within 24 hours. Pages are fetched when
submitting. `shlv batch status` shows submitted batches, and `shlv batch collect` applies the
results of ended ones (from any later run, since pending batches are recorded in the database).
Failed or expired items are reported; rerunning the command without `--batch` retries them live.

Replies are checked: malformed JSON is retried once, tags not in the canonical list are dropped, and
empty or overlong summaries are rejected. If a server doesn't support JSON-schema output, shlv falls
back to JSON mode with the schema in the prompt.
