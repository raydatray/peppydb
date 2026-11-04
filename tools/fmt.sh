#!/bin/sh
# Formatting and layout checks. `--fix` rewrites files instead of checking.
set -eu
cd "$(dirname "$0")/.."
tools/check-tools

fix=false
[ "${1:-}" = "--fix" ] && fix=true
status=0

# Rust. rustfmt follows `mod` declarations from each crate root, so this
# covers every Rust file without needing Cargo.
if $fix; then
    rustfmt crates/*/src/lib.rs bin/*/src/main.rs
else
    rustfmt --check crates/*/src/lib.rs bin/*/src/main.rs || status=1
fi

# Go.
if [ -d go ]; then
    if $fix; then
        gofmt -w go
    else
        unformatted=$(gofmt -l go)
        if [ -n "$unformatted" ]; then
            echo "fmt: not gofmt-formatted (run tools/fmt.sh --fix):" >&2
            echo "$unformatted" >&2
            status=1
        fi
    fi

    # Buck2 and the go command must agree on import paths: a package_name in
    # go/<dir>/BUCK must be peppydb/go/<dir>.
    for buck in $(find go -name BUCK); do
        dir=$(dirname "$buck")
        for name in $(sed -n 's/^ *package_name = "\(.*\)",$/\1/p' "$buck"); do
            if [ "$name" != "peppydb/$dir" ]; then
                echo "fmt: $buck: package_name \"$name\" should be \"peppydb/$dir\"" >&2
                status=1
            fi
        done
    done
fi

exit "$status"
