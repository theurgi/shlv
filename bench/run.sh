#!/usr/bin/env bash
# Benchmarks shlv against a synthetic database. Run from the repo root in the dev shell:
#
#   bench/run.sh [count]      # default 100000
#
# The database is generated once per count and reused (bench/data/, git-ignored). `add` runs
# against a copy so the base database stays fixed.
set -euo pipefail

count=${1:-100000}
cd "$(dirname "$0")/.."
data=bench/data
db=$data/$count.db
mkdir -p "$data"

cargo build --release --quiet --bin shlv --example gen
bin=$PWD/target/release/shlv

if [[ ! -f $db ]]; then
  target/release/examples/gen "$count" "$db"
fi

export SHLV_DB=$PWD/$db
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

# add writes, so give it a scratch copy and a fresh URL each run. It's the only benchmarked
# command that reads the config, so it runs without one and with a typical one.
scratch=$data/scratch.db
cp "$db" "$scratch"
export SHLV_DB=$PWD/$scratch
config=$data/config.toml
cat > "$config" <<'TOML'
strict_tags = true

[ai]
model = "example"

[provider.example]
url = "http://localhost:11434"
TOML
SHLV_CONFIG=/nonexistent "$bin" tag canonical add bench > /dev/null
add="$bin add \"https://bench.example/\$RANDOM\$RANDOM\$RANDOM\" -t bench"
hyperfine --warmup 5 --min-runs 30 --shell bash --export-markdown "$data/add-$count.md" \
  -n 'add new url' "SHLV_CONFIG=/nonexistent $add" \
  -n 'add duplicate' "SHLV_CONFIG=/nonexistent $bin add https://bench.example/same -t bench" \
  -n 'add new url, strict config' "SHLV_CONFIG=$PWD/$config $add" \
  -n 'add duplicate, strict config' \
  "SHLV_CONFIG=$PWD/$config $bin add https://bench.example/same -t bench"
rm -f "$scratch" "$scratch"-wal "$scratch"-shm "$config"

echo
echo "binary: $(stat -c %s "$bin") bytes"
echo "database: $(stat -c %s "$db") bytes, $(( $(stat -c %s "$db") / count )) bytes/bookmark"
