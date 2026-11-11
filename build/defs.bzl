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
