#!/usr/bin/env bash
# Benchmarks shlv against a synthetic database. Run from the repo root in the dev shell:
#
#   bench/run.sh [count]      # default 100000
#
# The database is generated once per count and kept in bench/data/ (git-ignored), along with the
# result tables. Each run copies the binary and the database to a temporary directory under
# $TMPDIR (default /tmp) and works there: in the igloo VM, /workspace is virtiofs, which made `ls`
# 50% slower (13.3 vs 8.9 ms) and `ls -n 0` 2.4x slower than the same files on tmpfs, even with a
# warm cache. Writes run against scratch copies, so the base database stays fixed.
set -euo pipefail

count=${1:-100000}
cd "$(dirname "$0")/.."
data=$PWD/bench/data
mkdir -p "$data"

cargo build --release --quiet --bin shlv --example gen

if [[ ! -f $data/$count.db ]]; then
  target/release/examples/gen "$count" "$data/$count.db"
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/shlv-bench.XXXXXX")
trap 'rm -rf "$work"' EXIT
bin=$work/shlv
db=$work/$count.db
scratch=$work/scratch.db
cp target/release/shlv "$bin"
cp "$data/$count.db" "$db"
echo "working in $work"

export SHLV_DB=$db
# Every command reads the config now; keep the host's out of the read numbers.
export SHLV_CONFIG=/nonexistent
"$bin" ls -n 1 >/dev/null # apply migrations to an older generated database outside the timings
opts=(--warmup 5 --min-runs 30 -N --export-markdown "$data/read-$count.md")
hyperfine "${opts[@]}" \
  -n 'baseline: --version' "$bin --version" \
  -n 'ls' "$bin ls" \
  -n 'ls -t rust' "$bin ls -t rust" \
  -n 'ls -n 0 (all)' "$bin ls -n 0" \
  -n 'search rust' "$bin search rust" \
  -n 'search prefix pe' "$bin search pe" \
  -n 'search two words' "$bin search async guide" \
  -n 'search rare word' "$bin search kalomi" \
  -n 'search phrase' "$bin search '\"deep dive\"'" \
  -n 'search word #tag' "$bin search python '#web'" \
  -n 'search #tag only' "$bin search '#rust'" \
  -n 'search site: (500 hits)' "$bin search site:blog.omulri7.com" \
  -n 'search site: (5.8k hits)' "$bin search site:esmarulka0.co.uk"

# Every timing runs without a shell (-N). `--shell bash` subtracts an estimate of the shell's
# startup, which came out 1.4 ms low for `ls` (3.9 against 5.3 ms), and wrapping a command in `env`
# adds about 4 ms, so settings go in the environment of hyperfine itself.
#
# bench_add URL LABEL FILE: `add` of a new URL and of a duplicate, under $SHLV_CONFIG. The prepare
# step deletes the bookmark, and its tombstone frees the URL, so each run inserts a new row.
bench_add() {
  "$bin" add "$1" -t bench >/dev/null
  hyperfine -N --warmup 5 --min-runs 30 --export-markdown "$3" \
    --prepare "$bin rm $1" -n "add new url$2" "$bin add $1 -t bench" \
    --prepare "$bin --version" -n "add duplicate$2" "$bin add $1 -t bench"
}

# add writes, so give it a scratch copy. It runs without a config and with a typical one.
cp "$db" "$scratch"
export SHLV_DB=$scratch
config=$work/config.toml
cat > "$config" <<'TOML'
strict_tags = true

[ai]
model = "example"

[provider.example]
url = "http://localhost:11434"
TOML
"$bin" tag canonical add bench >/dev/null
bench_add https://bench.example/a "" "$data/add-$count.md"
SHLV_CONFIG=$config bench_add https://bench.example/b ", strict config" "$data/add-strict-$count.md"

# A declared canonical list: the database's tags plus synthetic ones, two aliases each (200 tags,
# 400 aliases). The first run mirrors it into the database; later runs only compare its hash. A
# manual tag edit clears the stored hash, which forces a resync that has little to do. Compare with
# `ls` in the read benchmarks.
declared=$work/declared.toml
{
  echo "[tags.canonical]"
  { "$bin" tag ls --json | sed 's/.*"name":"\([^"]*\)".*/\1/'; seq -f "topic%g" 1 170; } |
    sort -u | while read -r t; do echo "\"$t\" = [\"$t-alias\", \"x-$t\"]"; done
} >"$declared"
echo "declared: $(($(wc -l <"$declared") - 1)) tags"
TIMEFORMAT="first sync: %3R s"
time SHLV_CONFIG=$declared "$bin" tag canonical ls >/dev/null
SHLV_CONFIG=$declared hyperfine -N --warmup 5 --min-runs 30 \
  --export-markdown "$data/declared-$count.md" \
  --prepare "$bin --version" -n 'ls, declared list (unchanged)' "$bin ls" \
  --prepare "env SHLV_CONFIG=/nonexistent $bin tag canonical add scratch" \
  -n 'ls, declared list (resync)' "$bin ls"
rm -f "$scratch" "$scratch"-wal "$scratch"-shm "$config" "$declared"

# Export everything, import it into an empty database, then import it again (all updates). One run
# each: these take seconds, not milliseconds.
export SHLV_DB=$db
for format in json html; do
  TIMEFORMAT="export $format: %3R s"
  time "$bin" export --format "$format" >"$work/export.$format"
  export SHLV_DB=$scratch
  rm -f "$scratch" "$scratch"-wal "$scratch"-shm
  TIMEFORMAT="import $format (new): %3R s"
  time "$bin" import "$work/export.$format" >/dev/null
  TIMEFORMAT="import $format (again): %3R s"
  time "$bin" import "$work/export.$format" >/dev/null
  export SHLV_DB=$db
done
rm -f "$scratch" "$scratch"-wal "$scratch"-shm "$work/export.json" "$work/export.html"

# Domain rules: the 40 most common hosts (a fifth of the bookmarks) with two tags each, plus 20
# rules that match nothing. `add` with rules, then the rules on every bookmark, in confirm mode
# (record suggestions, run again with nothing new, accept them all) and in auto mode.
rules=$work/rules.toml
{
  echo "[tags.rules]"
  sqlite3 "$db" "SELECT substr(url_norm, 9, instr(substr(url_norm, 9), '/') - 1) h FROM bookmark \
    GROUP BY h ORDER BY count(*) DESC LIMIT 40" |
    awk '{ printf "\"%s\" = [\"site%d\", \"topic%d\"]\n", $0, NR, NR % 7 }'
  for i in $(seq 20); do echo "\"nomatch$i.example\" = [\"nm$i\"]"; done
} >"$rules"
auto=$work/rules-auto.toml
printf '[tags]\nsuggestions = "auto"\n\n' | cat - "$rules" >"$auto"
fresh() {
  rm -f "$scratch" "$scratch"-wal "$scratch"-shm
  cp "$db" "$scratch"
  SHLV_CONFIG=/nonexistent "$bin" ls -n 1 >/dev/null # migrate outside the timings
}
export SHLV_DB=$scratch
fresh
host=$(sed -n 2p "$rules" | cut -d'"' -f2)
SHLV_CONFIG=$rules bench_add https://bench.example/c ", 60 rules, no match" \
  "$data/add-rules-$count.md"
SHLV_CONFIG=$rules bench_add "https://$host/new" ", 60 rules, match" \
  "$data/add-rules-match-$count.md"
SHLV_CONFIG=$rules hyperfine -N --warmup 5 --min-runs 30 --export-markdown "$data/rules-$count.md" \
  -n 'ls, 60 rules' "$bin ls"
fresh
export SHLV_CONFIG=$rules
TIMEFORMAT="suggest --all (confirm): %3R s"
time "$bin" suggest --all
TIMEFORMAT="suggest --all (again, nothing new): %3R s"
time "$bin" suggest --all >/dev/null
TIMEFORMAT="suggest accept --all: %3R s"
time "$bin" suggest accept --all
fresh
export SHLV_CONFIG=$auto
TIMEFORMAT="suggest --all (auto): %3R s"
time "$bin" suggest --all
# Import with the rules, into an empty database, against the plain imports above.
SHLV_DB=$db SHLV_CONFIG=/nonexistent "$bin" export >"$work/export.json"
for config in "$rules" "$auto"; do
  export SHLV_CONFIG=$config
  rm -f "$scratch" "$scratch"-wal "$scratch"-shm
  TIMEFORMAT="import json (new), rules $(basename "$config" .toml): %3R s"
  time "$bin" import "$work/export.json"
  TIMEFORMAT="import json (again), rules $(basename "$config" .toml): %3R s"
  time "$bin" import "$work/export.json"
done
export SHLV_CONFIG=/nonexistent
rm -f "$scratch" "$scratch"-wal "$scratch"-shm "$rules" "$auto" "$work/export.json"

echo
echo "binary: $(stat -c %s "$bin") bytes"
echo "database: $(stat -c %s "$data/$count.db") bytes, $(( $(stat -c %s "$data/$count.db") / count )) bytes/bookmark"
