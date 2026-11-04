"""Shared build macros."""

def peppy_rust_library(name, deps = [], test_deps = [], visibility = [], **kwargs):
    """A Rust library and its unit tests.

    Defines `:{name}` (the library; `buck2 test` on it runs the doctests) and
    `:{name}-unittest` (the crate compiled with `cfg(test)`). The crate name is
    `name` with dashes replaced by underscores, and the crate root is
    `src/lib.rs`.
    """
    common = dict(
        crate = name.replace("-", "_"),
        crate_root = "src/lib.rs",
        srcs = native.glob(["src/**/*.rs"]),
        **kwargs
    )
    native.rust_library(name = name, deps = deps, visibility = visibility, **common)
    native.rust_test(name = name + "-unittest", deps = deps + test_deps, **common)
