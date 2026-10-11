#!/bin/sh
# Builds the release binary for packaging and prints its path.
#
# With cargo-zigbuild installed (cargo install cargo-zigbuild; pip install ziglang) it is
# linked against an old glibc and runs on distributions from 2020 on. Without it the result
# only runs on systems at least as new as this one.
set -eu

cd "$(dirname "$0")/.."
GLIBC=2.31

if cargo zigbuild --help >/dev/null 2>&1; then
    cargo zigbuild --release --target "x86_64-unknown-linux-gnu.$GLIBC" >&2
    echo target/x86_64-unknown-linux-gnu/release/inkswitch
else
    echo "cargo-zigbuild not found: building against this system's glibc" >&2
    cargo build --release >&2
    echo target/release/inkswitch
fi
