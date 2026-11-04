#!/bin/sh
# Formatting checks. `--fix` rewrites files instead of checking.
# rustfmt follows `mod` declarations from each crate root, so this covers
# every Rust file without needing Cargo.
set -eu
cd "$(dirname "$0")/.."
tools/check-tools

check=--check
[ "${1:-}" = "--fix" ] && check=

# shellcheck disable=SC2086
rustfmt $check crates/*/src/lib.rs bin/*/src/main.rs
