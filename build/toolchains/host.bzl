"""Helpers for per-host toolchain settings."""

def by_host(values):
    """A select() over the supported hosts.

    `values` maps "darwin-arm64", "linux-amd64", and "linux-arm64" (the Go
    release names) to the value for that host.
    """
    return select({
        "config//os:linux": select({
            "config//cpu:arm64": values["linux-arm64"],
            "config//cpu:x86_64": values["linux-amd64"],
        }),
        "config//os:macos": select({
            "config//cpu:arm64": values["darwin-arm64"],
        }),
    })
