"""Hermetic Linux C tools backed by the pinned Zig distribution (plan 0035).

`zig_cxx_tools` returns `CxxToolsInfo` for the prelude's
`cxx_tools_info_toolchain`. Its compiler, archiver, and linker are scripts
around `zig cc`, `zig c++`, and `zig ar`. The prelude's Rust rules take their
linker from this toolchain, so every Rust link on Linux, every cgo link, and
every cgo C compile runs through Zig. The linker, lld, compiler-rt,
libunwind, and the glibc version all come from the sha256-pinned Zig archive,
not from the host.

`zig_runtime_cache` builds Zig's runtime support once and archives the cache,
so a link does not have to build it again. The design follows the rue project
(`toolchains/cxx_tools.bzl` and `toolchains/zig/defs.bzl` in
rue-language/rue).
"""

load("@prelude//cxx:cxx_toolchain_types.bzl", "LinkerType")
load("@prelude//toolchains:cxx.bzl", "CxxToolsInfo")
load("@prelude//utils:cmd_script.bzl", "cmd_script")

def _zig_command(wrapper: Artifact, seed, zig: Artifact) -> cmd_args:
    # See zig_cc.sh: SEED is `-` or a primed cache archive.
    return cmd_args("/bin/sh", wrapper, seed, zig)

def _zig_cxx_tools_impl(ctx: AnalysisContext) -> list[Provider]:
    zig = ctx.attrs.zig[DefaultInfo].default_outputs[0]
    cache = ctx.attrs.runtime_cache[DefaultInfo].default_outputs[0]
    target = ["-target", ctx.attrs.target]

    def tool(name: str, cmd: cmd_args) -> cmd_args:
        # rustc takes its linker as one `-Clinker=<path>`, so bundle each
        # multi-word Zig command into a script.
        return cmd_script(actions = ctx.actions, name = name, cmd = cmd)

    cc = tool("zig-cc", cmd_args(_zig_command(ctx.attrs.wrapper, "-", zig), "cc", target))
    cxx = tool("zig-c++", cmd_args(_zig_command(ctx.attrs.wrapper, "-", zig), "c++", target))
    ar = tool("zig-ar", cmd_args(_zig_command(ctx.attrs.wrapper, "-", zig), "ar"))

    # The linker is `zig cc`, not `zig c++`: `zig c++` would build libc++
    # into the cache on first use, and Rust links need no C++ runtime. It
    # starts from the primed runtime cache (about 15 s of work per link
    # otherwise), and discards the debug sections of Zig's runtime objects so
    # that a link does not depend on the checkout path.
    linker = tool("zig-link", cmd_args(
        _zig_command(ctx.attrs.wrapper, cache, zig),
        "cc",
        target,
        cmd_args("-Wl,-T,", ctx.attrs.linker_script, delimiter = ""),
    ))

    return [
        DefaultInfo(),
        CxxToolsInfo(
            archiver = ar,
            archiver_type = "gnu",
            asm_compiler = cc,
            asm_compiler_type = "clang",
            compiler = cc,
            compiler_type = "clang",
            cvtres_compiler = None,
            cxx_compiler = cxx,
            linker = linker,
            linker_type = LinkerType("gnu"),
            rc_compiler = None,
        ),
    ]

zig_cxx_tools = rule(
    impl = _zig_cxx_tools_impl,
    attrs = {
        "linker_script": attrs.source(),
        "runtime_cache": attrs.dep(providers = [DefaultInfo]),
        # Zig target, including the glibc version, e.g. `x86_64-linux-gnu.2.17`.
        "target": attrs.string(),
        "wrapper": attrs.source(),
        "zig": attrs.dep(providers = [DefaultInfo]),
    },
)

# The link shapes of a Linux Rust build. Together they make Zig build every
# piece of runtime support that such a build can ask for: the start files and
# glibc stubs (plain), the position-independent start files (`-pie`), and
# libunwind (`-lgcc_s`, which every Rust link passes). The rue project found
# that shared objects need nothing more.
_RUNTIME_CACHE_LINK_SHAPES = [
    [],
    ["-pie", "-fPIE"],
    ["-lgcc_s"],
]

def _zig_runtime_cache_impl(ctx: AnalysisContext) -> list[Provider]:
    zig = ctx.attrs.zig[DefaultInfo].default_outputs[0]

    # One archive rather than the cache directory: the directory is about a
    # thousand small files, and the archive is an input of every link.
    cache = ctx.actions.declare_output("zig-runtime-cache.tar")
    lines = [
        "#!/bin/sh",
        "set -eu",
        "archive=$1",
        "zig=$2/zig",
        "target=$3",
        "source=$4",
        "scratch=$PWD/${BUCK_SCRATCH_PATH:?}",
        "export ZIG_GLOBAL_CACHE_DIR=\"$scratch/zig-global-cache\"",
        "export ZIG_LOCAL_CACHE_DIR=\"$scratch/zig-local-cache\"",
        "mkdir -p \"$ZIG_GLOBAL_CACHE_DIR\" \"$scratch/links\"",
    ] + [
        "\"$zig\" cc -target \"$target\" \"$source\" -o \"$scratch/links/{}\" {}".format(index, " ".join(flags))
        for index, flags in enumerate(_RUNTIME_CACHE_LINK_SHAPES)
    ] + [
        # Sorted names and zeroed metadata, so the archive depends only on
        # the cache contents.
        "tar --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner -cf \"$archive\" -C \"$ZIG_GLOBAL_CACHE_DIR\" .",
    ]
    script = ctx.actions.write("prime-zig-runtime-cache.sh", lines, is_executable = True)
    ctx.actions.run(
        cmd_args("/bin/sh", script, cache.as_output(), zig, ctx.attrs.target, ctx.attrs.src),
        category = "zig_runtime_cache",
        identifier = ctx.label.name,
    )
    return [DefaultInfo(default_output = cache)]

zig_runtime_cache = rule(
    impl = _zig_runtime_cache_impl,
    attrs = {
        # A trivial C program to link; only the cache it leaves is kept.
        "src": attrs.source(),
        "target": attrs.string(),
        "zig": attrs.dep(providers = [DefaultInfo]),
    },
)
