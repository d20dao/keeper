#!/usr/bin/env bash
# Records the golden traces from the tree of keeper 0.4.1, the behaviour they stand for, and copies them here.
#
# The traces are what 0.4.1 asks the chain. They are never recorded from the tree that is under test: that would only
# say that the code does what it does. This script extracts the 0.4.1 commit, adds nothing to it but the test modules
# (keeper/src/golden*.rs, rig.rs and scripted*.rs, declared in lib.rs), runs the recorders there, and brings the traces
# back. The tests then check that this tree asks the same, call by call.
#
#   bash keeper/tests/golden/record.sh
#
# The trace of the explorer's indexer (arc-0.4.1.indexer.trace) runs the real indexer against a disposable PostgreSQL on
# 127.0.0.1:55439, user postgres, password public-local-test, database explorer_test: the one CI starts for the other
# explorer tests. Without it that trace is not recorded and the one in the repository stays.
#
# Settings (environment):
#   D20_GOLDEN_BASELINE  the commit to record from; default f03c688 (keeper 0.4.1)
#   D20_GOLDEN_WORK      where the tree is extracted and built; default keeper/target/golden, which git ignores. Its
#                        build directory stays, so the dependencies compile once.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../../.." && pwd)"
baseline="${D20_GOLDEN_BASELINE:-f03c688}"
source_commit="$(git -C "$repo" rev-parse --short=7 "$baseline^{commit}")"
work="${D20_GOLDEN_WORK:-$repo/keeper/target/golden}"
tree="$work/tree"

# The test modules of the golden traces. They use nothing of the keeper that 0.4.1 lacks.
modules="golden golden_production golden_failover golden_follower golden_websocket golden_telemetry golden_indexer rig scripted scripted_ws scripted_sink"

rm -rf "$tree"
mkdir -p "$tree"
# The Rust tests read test/fixtures, and nothing else outside keeper/. The TypeScript of test/ stays out: the tree is
# under keeper/target, and the repository's `tsc` would take it for its own and fail on it.
git -C "$repo" archive "$baseline" keeper test/fixtures | tar -x -C "$tree"
for module in $modules; do
  cp "$repo/keeper/src/$module.rs" "$tree/keeper/src/$module.rs"
  printf '#[cfg(test)]\nmod %s;\n' "$module" >> "$tree/keeper/src/lib.rs"
done

echo "recording from $source_commit in $tree"
# Record mode ends each test with a failure on purpose, so the exit status says nothing: the traces do.
D20_GOLDEN_RECORD=1 D20_GOLDEN_SOURCE="$source_commit" CARGO_TARGET_DIR="$work/target" \
  cargo +1.96.1 test --manifest-path "$tree/keeper/Cargo.toml" --locked golden -- --include-ignored || true

recorded=("$tree"/keeper/tests/golden/*.trace)
if [ ! -e "${recorded[0]}" ]; then
  echo "nothing was recorded" >&2
  exit 1
fi
cp "${recorded[@]}" "$here/"
echo "recorded from $source_commit:"
for trace in "${recorded[@]}"; do echo "  keeper/tests/golden/$(basename "$trace")"; done
if [ ! -e "$tree/keeper/tests/golden/arc-0.4.1.indexer.trace" ]; then
  echo "arc-0.4.1.indexer.trace was not recorded: it needs the PostgreSQL described above" >&2
fi
