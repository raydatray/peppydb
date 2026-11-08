# A pinned Linux environment for contended benchmarks and Linux test runs.
# Build and run it with tools/linux-run, which supplies the build context:
# rust-toolchain.toml and tools/ci/install-dotslash, so the toolchain and
# DotSlash pins stay in one place each.
FROM ubuntu:24.04@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852

ENV DEBIAN_FRONTEND=noninteractive
# clang: the system C++ toolchain links with clang++ (Buck2 prelude);
# build-essential: cc for Cargo. python3: the Buck2 prelude's helper scripts.
# unzip: http_archive (plan 0009). git: tools that list files.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential ca-certificates clang curl git lld python3 unzip zstd \
    && rm -rf /var/lib/apt/lists/*

# Rust in rustup's default locations, with the components preinstalled:
# Buck2 runs actions with a minimal environment (plan 0009).
COPY rust-toolchain.toml /tmp/rust-toolchain.toml
RUN channel=$(sed -n 's/^channel = "\(.*\)"/\1/p' /tmp/rust-toolchain.toml) \
    && curl -fsSL --retry 5 https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain "$channel" \
            --component clippy,rustfmt,rust-src
ENV PATH=/root/.cargo/bin:/usr/local/bin:$PATH

# DotSlash, which runs the pinned tools/buck2.
COPY install-dotslash /tmp/install-dotslash
RUN TMPDIR=/tmp sh /tmp/install-dotslash \
    && cp /tmp/dotslash-*/dotslash /usr/local/bin/dotslash

WORKDIR /work/peppydb
