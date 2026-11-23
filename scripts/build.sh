#!/bin/sh
# Builds the release program as dist/<name>, where <name> is what the NAME file says.
# On a Mac it is signed with the virtualization entitlement, without which it can't run machines.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
name=$(tr -d '[:space:]' < "$root/NAME")
cd "$root"
cargo build --release -p cli
mkdir -p dist
cp target/release/cli "dist/$name"
if [ "$(uname)" = Darwin ]; then
    codesign --sign - --force --identifier "$name" --entitlements config/virtualization.entitlements "dist/$name"
fi
echo "dist/$name"
