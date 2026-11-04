#!/bin/sh
# `cargo clippy --workspace --all-targets -- -D warnings` for the Buck2 build.
# Buck2 writes clippy output to a [clippy.txt] subtarget of each Rust target
# instead of failing the build, so build them all and fail if any is non-empty.
set -eu
cd "$(dirname "$0")/.."
tools/check-tools

targets=$(tools/buck2 uquery 'kind("rust_(library|binary|test)", //crates/... + //bin/...)' 2>/dev/null \
    | sed 's/$/[clippy.txt]/')
outputs=$(tools/buck2 build $targets --show-full-output 2>/dev/null | awk '{print $2}')

status=0
seen=""
for f in $outputs; do
    [ -s "$f" ] || continue
    status=1
    # A library's diagnostics also appear in its unit-test target; print each once.
    sum=$(cksum < "$f")
    case "$seen" in *"|$sum|"*) continue ;; esac
    seen="$seen|$sum|"
    cat "$f"
done
if [ "$status" -eq 0 ]; then
    echo "clippy: $(echo "$targets" | wc -l | tr -d ' ') targets clean"
fi
exit "$status"
