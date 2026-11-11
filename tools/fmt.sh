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
    rustfmt lib/*/src/lib.rs proto/*/*/rust/src/lib.rs bin/*/src/main.rs tools/*/src/main.rs
else
    rustfmt --check lib/*/src/lib.rs proto/*/*/rust/src/lib.rs bin/*/src/main.rs tools/*/src/main.rs || status=1
fi

# Go, with gofmt from the pinned Go distribution (not from PATH). Go files
# can be in any first-party directory (plan 0027); the go command's own
# ignore list in go.mod does not apply to gofmt, so list them here.
gofiles=$(find . \( -path ./buck-out -o -path ./target -o -path ./third-party -o -name '.*' ! -name . \) -prune -o -name '*.go' -print)
if [ -n "$gofiles" ]; then
    goroot=$(tools/buck2 build toolchains//:go-root --show-full-simple-output 2>/dev/null)
    gofmt="$goroot/bin/gofmt"
    if $fix; then
        "$gofmt" -w $gofiles
    else
        unformatted=$("$gofmt" -l $gofiles)
        if [ -n "$unformatted" ]; then
            echo "fmt: not gofmt-formatted (run tools/fmt.sh --fix):" >&2
            echo "$unformatted" >&2
            status=1
        fi
    fi

    # Buck2 and the go command must agree on import paths: a package_name in
    # <dir>/BUCK must be peppydb/<dir>.
    for buck in $(echo "$gofiles" | xargs -n1 dirname | sort -u | sed 's|^\./||; s|$|/BUCK|'); do
        [ -f "$buck" ] || continue
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
