"""Pinned Zig distributions: the C compiler, archiver, and linker on Linux.

Source: the official releases on ziglang.org (`download/index.json`). Each
host has one sha256-pinned archive of about 55 MB, and `zig cc` provides
clang, lld, glibc stubs for a chosen glibc version, the C runtime start
files, compiler-rt, and libunwind. No separate sysroot is needed. See plan
0035 and docs/design/build-system.md.
"""

ZIG_VERSION = "0.17.1"

# host: archive
ZIG_RELEASES = {
    "linux-amd64": {
        "sha256": "3581863e3cbcfd0fce0efc682ce7e9e93da96668a977791d356a26d2a7e9b17a",
        "strip_prefix": "zig-x86_64-linux-0.17.1",
        "url": "https://ziglang.org/download/0.17.1/zig-x86_64-linux-0.17.1.tar.xz",
    },
    "linux-arm64": {
        "sha256": "a1546d5646eb184437918d6635ca6b762a0e889eac09cef720ee7a1ed3fe7795",
        "strip_prefix": "zig-aarch64-linux-0.17.1",
        "url": "https://ziglang.org/download/0.17.1/zig-aarch64-linux-0.17.1.tar.xz",
    },
}

# host: the Zig target of every compile and link. The glibc floor is 2.17, the
# oldest glibc that Rust's `*-unknown-linux-gnu` targets support, so a binary
# built here runs wherever Rust programs run. The C tools and the primed
# runtime cache must use the same target, or the cache would hold objects that
# no link asks for.
ZIG_TARGETS = {
    "linux-amd64": "x86_64-linux-gnu.2.17",
    "linux-arm64": "aarch64-linux-gnu.2.17",
}
