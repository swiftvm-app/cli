#!/bin/sh
# Builds the release program and installs it as <folder>/<name>, where <name> is `name` under
# [workspace.metadata] in Cargo.toml. <folder> is ~/.local/bin unless given.
#
# On a Mac the program is signed with the virtualization entitlement (scripts/build.sh). On Linux
# it is the static program for this processor, with passt next to it: that takes the processor's
# musl target (`rustup target add`), musl-tools for zstd, and what scripts/build-passt.sh needs.
# virtiofsd goes next to it too where Docker is there (scripts/build-virtiofsd.sh).
#
# It is put in place under a new name and then renamed: a machine that is running is a process of
# the program that was there before, and keeps it.
# Usage: scripts/install.sh [<folder>]
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
name=$(awk -F'"' '/^\[/ { section = $0 } section == "[workspace.metadata]" && /^name *=/ { print $2; exit }' "$root/Cargo.toml")
[ -n "$name" ] || { echo "Cargo.toml has no name under [workspace.metadata]" >&2; exit 1; }
[ $# -le 1 ] || { echo "usage: $0 [<folder>]" >&2; exit 2; }
folder=${1:-$HOME/.local/bin}

case $(uname) in
    Darwin)
        "$root/scripts/build.sh" >/dev/null
        built="$root/dist"
        ;;
    Linux)
        target=$(uname -m)-unknown-linux-musl
        "$root/scripts/build.sh" "$target" >/dev/null
        built="$root/dist/$target"
        ;;
    *)
        echo "install: $(uname) isn't supported" >&2
        exit 1
        ;;
esac

mkdir -p "$folder"
folder=$(cd "$folder" && pwd)
for file in "$name" passt virtiofsd; do
    [ -f "$built/$file" ] || continue
    # cp keeps a Mac program's signature, which is part of the file.
    cp "$built/$file" "$folder/.$file.new"
    mv -f "$folder/.$file.new" "$folder/$file"
done

echo "$folder/$name"
case ":$PATH:" in
    *":$folder:"*) ;;
    *) echo "install: $folder isn't on PATH. Add it, or run $folder/$name." >&2 ;;
esac
