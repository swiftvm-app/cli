#!/bin/sh
# Builds the release program as dist/<name>, where <name> is `name` under [workspace.metadata] in
# Cargo.toml.
#
# With a target, such as aarch64-unknown-linux-musl or x86_64-apple-darwin, it builds for that
# instead, into dist/<target>/<name>. For Linux that is a static program, and passt goes next to it
# when this runs on Linux on the same processor (scripts/build-passt.sh), and virtiofsd too when
# Docker is there (scripts/build-virtiofsd.sh). From a Mac, zig is the C
# compiler for Linux: zstd is C.
#
# A program for macOS is signed with the virtualization entitlement, without which it can't run
# machines, and with the hardened runtime, which notarization requires. SIGN_IDENTITY names the
# certificate, such as "Developer ID Application: …"; without it the signature is ad hoc.
# Usage: scripts/build.sh [<target>]
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
name=$(awk -F'"' '/^\[/ { section = $0 } section == "[workspace.metadata]" && /^name *=/ { print $2; exit }' "$root/Cargo.toml")
[ -n "$name" ] || { echo "Cargo.toml has no name under [workspace.metadata]" >&2; exit 1; }
[ $# -le 1 ] || { echo "usage: $0 [<target>]" >&2; exit 2; }
cd "$root"

sign() {
    identity=${SIGN_IDENTITY:--}
    set -- --sign "$identity" --force --identifier "$name" --options runtime \
        --entitlements config/virtualization.entitlements "$1"
    # An ad hoc signature can't carry Apple's timestamp.
    [ "$identity" = - ] || set -- --timestamp "$@"
    codesign "$@"
}

if [ $# = 1 ]; then
    target=$1
    built=target/$target/release/cli
    out=dist/$target
    if [ "$(uname)" = Darwin ]; then
        case $target in
            *-linux-*)
                command -v zig >/dev/null || { echo "build: zig isn't installed" >&2; exit 1; }
                variable=$(echo "$target" | tr a-z- A-Z_)
                export ZIG_TARGET="$(echo "$target" | sed 's/-unknown-linux-/-linux-/')"
                export "CC_$(echo "$target" | tr - _)=$root/scripts/zig-cc"
                export "AR_$(echo "$target" | tr - _)=$root/scripts/zig-ar"
                export "CARGO_TARGET_${variable}_LINKER=rust-lld"
                ;;
        esac
    fi
    cargo build --release -p cli --target "$target"
else
    target=
    built=target/release/cli
    out=dist
    cargo build --release -p cli
fi

mkdir -p "$out"
# Put in place under a new name and then renamed: a machine that is running is a process of the
# program that was there before, and macOS ends a process whose program's file is written over.
cp "$built" "$out/.$name.new"
case ${target:-$(uname)} in
    *-apple-darwin | Darwin) sign "$out/.$name.new" ;;
esac
mv -f "$out/.$name.new" "$out/$name"

case $target in
    *-linux-*)
        if [ "$(uname)" = Linux ] && [ "$(uname -m)" = "${target%%-*}" ]; then
            scripts/build-passt.sh "$out" >/dev/null
            if docker info >/dev/null 2>&1; then
                scripts/build-virtiofsd.sh "$out" >/dev/null
            else
                echo "build: virtiofsd isn't built here: that takes Docker that this user can use. Shared folders use the system's virtiofsd." >&2
            fi
        else
            echo "build: passt and virtiofsd aren't built here: that takes Linux on the same processor. Machines use the system's." >&2
        fi
        ;;
esac
echo "$out/$name"
