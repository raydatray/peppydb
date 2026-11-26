"""Hermetic Linux C/C++ tools backed by pinned LLVM and a Debian sysroot.

Returns `CxxToolsInfo` for the prelude's `cxx_tools_info_toolchain`. Uses
Buck2's built-in `cmd_script` helper to bundle each LLVM tool with its
`--sysroot`, `-fuse-ld=lld`, and `-B` flags into a single callable path (as
required by `rustc`'s `-Clinker=<path>`) while tracking both the LLVM and
sysroot directories as hidden action inputs.
"""

load("@prelude//cxx:cxx_toolchain_types.bzl", "LinkerType")
load("@prelude//toolchains:cxx.bzl", "CxxToolsInfo")
load("@prelude//utils:cmd_script.bzl", "cmd_script")

def _llvm_cxx_tools_impl(ctx: AnalysisContext) -> list[Provider]:
    llvm = ctx.attrs.llvm[DefaultInfo].default_outputs[0]
    sysroot = ctx.attrs.sysroot[DefaultInfo].default_outputs[0]
    common = [
        cmd_args(sysroot, format = "--sysroot={}"),
        cmd_args(llvm, format = "-B{}/bin"),
    ]

    clang = cmd_script(
        actions = ctx.actions,
        name = "clang",
        cmd = cmd_args(cmd_args(llvm, format = "{}/bin/clang"), common),
    )
    clang_xx = cmd_script(
        actions = ctx.actions,
        name = "clang++",
        cmd = cmd_args(cmd_args(llvm, format = "{}/bin/clang++"), common),
    )
    linker = cmd_script(
        actions = ctx.actions,
        name = "linker",
        cmd = cmd_args(
            cmd_args(llvm, format = "{}/bin/clang++"),
            common,
            "-fuse-ld=lld",
        ),
    )
    ar = cmd_script(
        actions = ctx.actions,
        name = "llvm-ar",
        cmd = cmd_args(cmd_args(llvm, format = "{}/bin/llvm-ar")),
    )

    return [
        DefaultInfo(),
        CxxToolsInfo(
            archiver = ar,
            archiver_type = "gnu",
            asm_compiler = clang,
            asm_compiler_type = "clang",
            compiler = clang,
            compiler_type = "clang",
            cvtres_compiler = None,
            cxx_compiler = clang_xx,
            linker = linker,
            linker_type = LinkerType("gnu"),
            rc_compiler = None,
        ),
    ]

llvm_cxx_tools = rule(
    impl = _llvm_cxx_tools_impl,
    attrs = {
        "llvm": attrs.dep(providers = [DefaultInfo]),
        "sysroot": attrs.dep(providers = [DefaultInfo]),
    },
)
