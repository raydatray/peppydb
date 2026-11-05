"""The Rust toolchain: stable rustc from PATH, or a pinned nightly for Miri.

By default this is the prelude's `system_rust_toolchain`: `rustc`,
`rustdoc`, and `clippy-driver` come from PATH, where rustup pins them with
rust-toolchain.toml.

With `nightly` set (only under `@build/mode/miri`), the compiler and rustdoc
come from a nightly toolchain that Buck2 downloads (miri_nightly.bzl), and
the toolchain provides a Miri driver. Every `rust_test` then has a `[miri]`
sub-target that interprets it. The whole toolchain switches, not only the
driver, because procedural macros must be built by the same compiler that
loads them.
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
    compiler = RunInfo(args = ["rustc"])
    rustdoc = RunInfo(args = ["rustdoc"])
    miri_driver = None
    if ctx.attrs.nightly:
        dist = ctx.attrs.nightly[DefaultInfo].default_outputs[0]
        sysroot = ctx.attrs.miri_sysroot[DefaultInfo].default_outputs[0]
        compiler = RunInfo(args = cmd_args(dist, format = "{}/bin/rustc"))
        rustdoc = RunInfo(args = cmd_args(dist, format = "{}/bin/rustdoc"))

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
            clippy_driver = RunInfo(args = ["clippy-driver"]),
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
        "doctests": attrs.bool(default = False),
        "miri_flags": attrs.list(attrs.arg(), default = []),
        # The Miri sysroot directory; required with `nightly`.
        "miri_sysroot": attrs.option(attrs.exec_dep(providers = [DefaultInfo]), default = None),
        # The merged nightly toolchain directory (bin/, lib/).
        "nightly": attrs.option(attrs.exec_dep(providers = [DefaultInfo]), default = None),
        "rustc_flags": attrs.list(attrs.arg(), default = []),
        "rustc_target_triple": attrs.string(default = _TRIPLE),
    },
    is_toolchain_rule = True,
)
