"""Toolchain distributions, fetched from their origin and never from a cache.

`toolchain_distribution` replaces `http_archive` for the pinned toolchains:
the Rust components, Go, and Zig. It follows the rule of the same name in the
rue project (`toolchains/distribution.bzl` in rue-language/rue, Apache-2.0).

Why not `http_archive`: its extraction is an ordinary action. With
`[buck2] default_allow_cache_upload = true` (plan 0036), the extracted tree
is uploaded to the remote cache, and every machine with an empty `buck-out`
then downloads the whole tree from the cache instead of the much smaller
archive from its origin. These trees are the largest outputs in the build and
contain thousands of small files, so they dominated the remote-cache traffic
(plan 0036 measured terabytes).

This rule:

- downloads the archive with `download_file`, which Buck2 always serves from
  the origin URL, never from the remote cache;
- extracts it with `allow_cache_upload = False`, so no cache entry is ever
  written for the tree, and no machine is ever served one.

The archives are sha256-pinned, so local extraction produces the same bytes a
cache would have returned. The cost: Buck2 learns the tree's digest only by
running the extraction, so every machine with an empty `buck-out` downloads
and extracts each toolchain it needs.
"""

# `tar` decompression flag per archive format.
_TAR_DECOMPRESS = {
    "gz": "-z",
    "xz": "-J",
}

def _toolchain_distribution_impl(ctx: AnalysisContext) -> list[Provider]:
    archive = ctx.actions.declare_output("archive.tar." + ctx.attrs.compression)
    ctx.actions.download_file(
        archive.as_output(),
        ctx.attrs.url,
        sha256 = ctx.attrs.sha256,
    )

    output = ctx.actions.declare_output(ctx.label.name, dir = True)

    # `sh -c SCRIPT NAME ARG...` binds $0 to NAME and $1... to the arguments,
    # so no path needs quoting inside the script. The strip prefix is also the
    # only member extracted, as with `http_archive`.
    strip_components = len([part for part in ctx.attrs.strip_prefix.split("/") if part])
    ctx.actions.run(
        cmd_args(
            "/bin/sh",
            "-c",
            'set -eu; mkdir -p "$1"; exec tar "$5" -x -f "$2" -C "$1" --strip-components="$3" "$4"',
            "toolchain_distribution",
            output.as_output(),
            archive,
            str(strip_components),
            ctx.attrs.strip_prefix,
            _TAR_DECOMPRESS[ctx.attrs.compression],
        ),
        category = "toolchain_distribution",
        identifier = ctx.label.name,
        # The reason for this rule; see the module docstring.
        allow_cache_upload = False,
    )

    return [DefaultInfo(default_output = output)]

toolchain_distribution = rule(
    impl = _toolchain_distribution_impl,
    attrs = {
        "compression": attrs.enum(["gz", "xz"], default = "xz"),
        "sha256": attrs.string(),
        "strip_prefix": attrs.string(),
        "url": attrs.string(),
    },
)
