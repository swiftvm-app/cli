#!/bin/sh
# Runs clippy for the Linux targets from a Mac, with zig as the C compiler: zstd is C.
# Usage: scripts/check-linux.sh [more arguments for cargo clippy]
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
command -v zig >/dev/null || { echo "check-linux: zig isn't installed" >&2; exit 1; }
for pair in aarch64-unknown-linux-musl:aarch64-linux-musl x86_64-unknown-linux-gnu:x86_64-linux-gnu; do
    target=${pair%%:*}
    variable=CC_$(echo "$target" | tr - _)
    env ZIG_TARGET="${pair#*:}" "$variable=$root/scripts/zig-cc" \
        cargo clippy --workspace --all-targets --target "$target" "$@"
done
