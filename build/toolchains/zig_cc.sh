#!/bin/sh
# Runs the pinned Zig with writable caches (plan 0035).
#
# Usage: zig_cc.sh SEED ZIG_DIR ARG...
#   SEED     `-`, or a tar archive of a primed Zig global cache
#            (toolchains//:zig-runtime-cache-*). It is unpacked into the
#            global cache before Zig runs.
#   ZIG_DIR  the unpacked Zig distribution.
#   ARG...   the Zig command, for example `cc -target x86_64-linux-gnu.2.17`.
#
# Zig needs writable cache directories, and an action must not write to its
# inputs, so both caches live in the action's scratch directory. A caller can
# set ZIG_GLOBAL_CACHE_DIR and ZIG_LOCAL_CACHE_DIR itself; the Miri sysroot
# genrule does, so that one Cargo build shares one cache.
set -eu

seed=$1
zig=$2/zig
shift 2

root=${BUCK_SCRATCH_PATH:-${TMPDIR:-/tmp}/peppydb-zig-$$}
case $root in
/*) ;;
*) root=$PWD/$root ;;
esac
: "${ZIG_GLOBAL_CACHE_DIR:=$root/zig-global-cache}"
: "${ZIG_LOCAL_CACHE_DIR:=$root/zig-local-cache}"
export ZIG_GLOBAL_CACHE_DIR ZIG_LOCAL_CACHE_DIR

if [ "$seed" != - ] && [ ! -d "$ZIG_GLOBAL_CACHE_DIR" ]; then
    mkdir -p "$ZIG_GLOBAL_CACHE_DIR"
    tar -xf "$seed" -C "$ZIG_GLOBAL_CACHE_DIR"
fi

# The prelude's C compiles pass an `@argsfile` whose lines are more
# `@argsfile`s. Clang reads nested response files; `zig cc` does not
# ("NestedResponseFile"). Replace each response file with a flattened copy:
# the prelude writes one argument per line, so flattening replaces every
# `@file` line with that file's lines.
flatten() {
    while IFS= read -r line || [ -n "$line" ]; do
        case $line in
        @*) flatten "${line#@}" ;;
        *) printf '%s\n' "$line" ;;
        esac
    done <"$1"
}

mkdir -p "$root"
count=0
for arg; do
    shift
    case $arg in
    @*)
        count=$((count + 1))
        flat=$root/zig-args-$count
        flatten "${arg#@}" >"$flat"
        set -- "$@" "@$flat"
        ;;
    *) set -- "$@" "$arg" ;;
    esac
done

exec "$zig" "$@"
