#!/bin/sh
# Builds the release program as dist/<name>, where <name> is what the NAME file says.
# On a Mac it is signed with the virtualization entitlement, without which it can't run machines.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
name=$(tr -d '[:space:]' < "$root/NAME")
cd "$root"
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
