#!/bin/sh
# Builds the release program as dist/<name>, where <name> is `name` under [workspace.metadata] in
# Cargo.toml.
# On a Mac it is signed with the virtualization entitlement, without which it can't run machines.
#
# With a target, such as aarch64-unknown-linux-musl, it builds for that instead, into
# dist/<target>/<name>. For Linux that is a static program, and passt goes next to it when this
# runs on Linux (scripts/build-passt.sh). From a Mac, zig is the C compiler: zstd is C.
# Usage: scripts/build.sh [<target>]
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
name=$(awk -F'"' '/^\[/ { section = $0 } section == "[workspace.metadata]" && /^name *=/ { print $2; exit }' "$root/Cargo.toml")
[ -n "$name" ] || { echo "Cargo.toml has no name under [workspace.metadata]" >&2; exit 1; }
cd "$root"

if [ $# -gt 0 ]; then
    target=$1
    variable=$(echo "$target" | tr a-z- A-Z_)
    if [ "$(uname)" = Darwin ]; then
        case $target in
            *-linux-*)
                command -v zig >/dev/null || { echo "build: zig isn't installed" >&2; exit 1; }
                zig_target=$(echo "$target" | sed 's/-unknown-linux-/-linux-/')
                export ZIG_TARGET="$zig_target"
                export "CC_$(echo "$target" | tr - _)=$root/scripts/zig-cc"
                export "AR_$(echo "$target" | tr - _)=$root/scripts/zig-ar"
                export "CARGO_TARGET_${variable}_LINKER=rust-lld"
                ;;
        esac
    fi
    cargo build --release -p cli --target "$target"
    mkdir -p "dist/$target"
    cp "target/$target/release/cli" "dist/$target/.$name.new"
    mv -f "dist/$target/.$name.new" "dist/$target/$name"
    case $target in
        *-linux-*)
            if [ "$(uname)" = Linux ] && [ "$(uname -m)" = "${target%%-*}" ]; then
                scripts/build-passt.sh "dist/$target" >/dev/null
            else
                echo "build: passt isn't built here: that takes Linux on the same processor. Machines use the system's passt." >&2
            fi
            ;;
    esac
    echo "dist/$target/$name"
    exit 0
fi

cargo build --release -p cli
mkdir -p dist
# Put in place under a new name and then renamed: a machine that is running is a process of the
# program that was there before, and macOS ends a process whose program's file is written over.
cp target/release/cli "dist/.$name.new"
if [ "$(uname)" = Darwin ]; then
    codesign --sign - --force --identifier "$name" --entitlements config/virtualization.entitlements "dist/.$name.new"
fi
mv -f "dist/.$name.new" "dist/$name"
echo "dist/$name"
