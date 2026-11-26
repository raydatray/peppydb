"""Pinned Linux LLVM and Debian Bullseye sysroot archives for hermetic linking.

Sources:
- LLVM: official llvm/llvm-project GitHub release `llvmorg-23.1.6`.
- Sysroot: Chromium's official Debian Bullseye sysroots (`sysroots.json`,
  glibc 2.31), compatible with Ubuntu 22.04+, Ubuntu 24.04 (WSL2, GitHub
  Actions, GCP VMs), and newer Linux distributions.
"""

LLVM_VERSION = "23.1.6"

# The paths the hermetic C/C++ toolchain uses from an LLVM release: the
# compiler driver, the linker, the archiver, and clang's resource directory
# (built-in headers and compiler-rt). The release is a 12 GB SDK, and every
# C/C++ compile and every Rust or cgo link declares the LLVM directory as an
# input, so extract only these (about 630 MB, 431 files).
LLVM_KEEP = [
    "bin/clang",
    "bin/clang++",
    "bin/clang-" + LLVM_VERSION.split(".")[0],
    "bin/ld.lld",
    "bin/lld",
    "bin/llvm-ar",
    "lib/clang",
]

# host: archive
LLVM_RELEASES = {
    "linux-amd64": {
        "sha256": "bd4d1c434ff69ef06d19e7934c04708da677e5f2eb51f5bcf052ab91818f0094",
        "strip_prefix": "LLVM-23.1.6-Linux-X64",
        "url": "https://github.com/llvm/llvm-project/releases/download/llvmorg-23.1.6/LLVM-23.1.6-Linux-X64.tar.xz",
    },
    "linux-arm64": {
        "sha256": "e9daa167da28b9e31d30046f05f98e3e7da3647cdbe1c1863e8b223c07ccf291",
        "strip_prefix": "LLVM-23.1.6-Linux-ARM64",
        "url": "https://github.com/llvm/llvm-project/releases/download/llvmorg-23.1.6/LLVM-23.1.6-Linux-ARM64.tar.xz",
    },
}

# Chromium Debian Bullseye sysroots (extract directly to ./bin, ./lib, ./usr).
LINUX_SYSROOTS = {
    "linux-amd64": {
        "sha256": "f851add76bdae472fb6c94b4f920bc4a132d85ac607951391297f636af7e934e",
        "url": "https://commondatastorage.googleapis.com/chrome-linux-sysroot/f851add76bdae472fb6c94b4f920bc4a132d85ac607951391297f636af7e934e",
    },
    "linux-arm64": {
        "sha256": "278c36fe3a785d7f89fbe3f46956899a3bd49fe665f68dc667d6f626a951fcd6",
        "url": "https://commondatastorage.googleapis.com/chrome-linux-sysroot/278c36fe3a785d7f89fbe3f46956899a3bd49fe665f68dc667d6f626a951fcd6",
    },
}
