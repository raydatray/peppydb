"""The Rust toolchain: pinned stable rustc, or a pinned nightly for Miri.

Both toolchains are downloaded by Buck2 and verified by sha256:
- Default: stable Rust from `rust_stable.bzl` (pinned by `tools/update-rust`),
  including `rustc`, `rustdoc`, and `clippy-driver`.
- `@build/mode/miri`: nightly Rust from `miri_nightly.bzl` (pinned by
  `tools/update-miri`), providing a Miri driver and sysroot so every `rust_test`
  gets a `[miri]` sub-target. The whole toolchain switches, not only the driver,
  because procedural macros must be built by the same compiler that loads them.
"""

load("@prelude//rust:rust_toolchain.bzl", "PanicRuntime", "RustToolchainInfo")

# The Rust target triple of each supported host (the hosts of GO_RELEASES).
_TRIPLE = select({
    "prelude//os:linux": select({
        "prelude//cpu:arm64": "aarch64-unknown-linux-gnu",
        "prelude//cpu:x86_64": "x86_64-unknown-linux-gnu",
    }),
    "prelude//os:macos": select({
        "prelude//cpu:arm64": "aarch64-apple-darwin",
    }),
})

def _impl(ctx):
    dist = ctx.attrs.dist[DefaultInfo].default_outputs[0]
    compiler = RunInfo(args = cmd_args(dist, format = "{}/bin/rustc"))
    rustdoc = RunInfo(args = cmd_args(dist, format = "{}/bin/rustdoc"))
    clippy_driver = RunInfo(args = cmd_args(dist, format = "{}/bin/clippy-driver"))
    miri_driver = None
    if ctx.attrs.miri_sysroot:
        sysroot = ctx.attrs.miri_sysroot[DefaultInfo].default_outputs[0]

        # The driver needs Miri's own sysroot (a standard library built with
        # MIR for every function) both when it compiles dependencies and when
        # it interprets a test.
        miri_driver = RunInfo(args = cmd_args(
            cmd_args(dist, format = "{}/bin/miri"),
            cmd_args(sysroot, format = "--sysroot={}"),
        ))

    return [
        DefaultInfo(),
        RustToolchainInfo(
            allow_lints = ctx.attrs.allow_lints,
            clippy_driver = clippy_driver,
            compiler = compiler,
            default_edition = ctx.attrs.default_edition,
            deny_lints = ctx.attrs.deny_lints,
            doctests = ctx.attrs.doctests,
            miri_driver = miri_driver,
            miri_flags = ctx.attrs.miri_flags,
            nightly_features = True,
            panic_runtime = PanicRuntime("unwind"),
            rustc_flags = ctx.attrs.rustc_flags,
            rustc_target_triple = ctx.attrs.rustc_target_triple,
            rustdoc = rustdoc,
            warn_lints = [],
        ),
    ]

peppy_rust_toolchain = rule(
    impl = _impl,
    attrs = {
        "allow_lints": attrs.list(attrs.string(), default = []),
        "default_edition": attrs.option(attrs.string(), default = None),
        "deny_lints": attrs.list(attrs.string(), default = []),
        # The merged Rust toolchain directory (bin/, lib/).
        "dist": attrs.exec_dep(providers = [DefaultInfo]),
        "doctests": attrs.bool(default = False),
        "miri_flags": attrs.list(attrs.arg(), default = []),
        # The Miri sysroot directory; set only under `@build/mode/miri`.
        "miri_sysroot": attrs.option(attrs.exec_dep(providers = [DefaultInfo]), default = None),
        "rustc_flags": attrs.list(attrs.arg(), default = []),
        "rustc_target_triple": attrs.string(default = _TRIPLE),
    },
    is_toolchain_rule = True,
)
