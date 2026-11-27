"""Shared build macros."""

def peppy_rust_library(name, deps = [], test_deps = [], visibility = [], feature_variants = {}, **kwargs):
    """A Rust library and its unit tests.

    Defines `:{name}` (the library; `buck2 test` on it runs the doctests) and
    `:{name}-unittest` (the crate compiled with `cfg(test)`). The crate name is
    `name` with dashes replaced by underscores, and the crate root is
    `src/lib.rs`.

    `feature_variants` maps a Cargo feature name to a dict with `deps` and
    `visibility`. Each entry defines `:{name}-{feature}`, the same crate
    built with that feature, depending on `deps` instead of `deps`. Use it
    for test-only switches that must reach dependent crates, which
    `cfg(test)` cannot (docs/design/build-system.md). Keep the visibility
    to the test targets that need the variant. tools/autocargo turns each
    variant into a `[features]` entry.

    A first-party `test_deps` entry `<label>-<feature>` replaces `<label>` in the unit
    tests' dependencies, so a crate's tests can use a dependency's feature
    variant.
    """
    common = dict(
        crate = name.replace("-", "_"),
        crate_root = "src/lib.rs",
        srcs = native.glob(["src/**/*.rs"]),
        **kwargs
    )
    native.rust_library(name = name, deps = deps, visibility = visibility, **common)
    test_only = [
        d
        for d in deps
        if not (d.startswith(("//lib/", "//proto/")) and [t for t in test_deps if t.startswith(d + "-")])
    ]
    native.rust_test(name = name + "-unittest", deps = test_only + test_deps, **common)
    for feature, variant in feature_variants.items():
        native.rust_library(
            name = name + "-" + feature,
            deps = variant["deps"],
            features = [feature],
            visibility = variant["visibility"],
            **common
        )

def _tla_test_impl(ctx: AnalysisContext) -> list[Provider]:
    java = cmd_args(ctx.attrs._jre[DefaultInfo].default_outputs[0], format = "{}/bin/java")
    jar = ctx.attrs._tla2tools[DefaultInfo].default_outputs[0]

    # Run TLC in a fresh temporary `-metadir` so parallel Buck2 tests never
    # collide on `states/` or leave untracked files in the working tree.
    # Avoid `exec` so the EXIT trap removes the temporary directory, and use
    # POSIX `shift` instead of bash array slicing for dash compatibility.
    cmd = cmd_args(
        "/bin/sh",
        "-c",
        'set -eu; java=$1; jar=$2; cfg=$3; spec=$4; shift 4; ' +
        'd=$(mktemp -d); trap \'rm -rf "$d"\' EXIT; ' +
        '"$java" -XX:+UseParallelGC -XX:ParallelGCThreads=1 ' +
        '-Djava.rmi.server.hostname=127.0.0.1 -cp "$jar" tlc2.TLC ' +
        '-cleanup -metadir "$d" -config "$cfg" "$@" "$spec"',
        "peppy_tla_test",
        java,
        jar,
        ctx.attrs.cfg,
        ctx.attrs.spec,
        ctx.attrs.tlc_flags,
        hidden = ctx.attrs.deps,
    )
    return [
        DefaultInfo(),
        ExternalRunnerTestInfo(
            type = "tla",
            command = [cmd],
        ),
    ]

peppy_tla_test = rule(
    impl = _tla_test_impl,
    doc = """Run the TLC model checker on a TLA+ specification and configuration.

    Uses the hermetic Temurin JRE (`toolchains//:jre`) and pinned `tla2tools.jar`
    (`toolchains//:tla2tools`). Defaults to `-workers 1` and serial GC so Buck2
    owns test parallelism across cores. Pass `deps` for any sibling `.tla`
    modules imported with `EXTENDS` or `INSTANCE`.
    """,
    attrs = {
        "cfg": attrs.source(),
        "deps": attrs.list(attrs.source(), default = []),
        "spec": attrs.source(),
        "tlc_flags": attrs.list(attrs.string(), default = ["-workers", "1"]),
        "_jre": attrs.default_only(attrs.dep(default = "toolchains//:jre")),
        "_tla2tools": attrs.default_only(attrs.dep(default = "toolchains//:tla2tools")),
    },
)
