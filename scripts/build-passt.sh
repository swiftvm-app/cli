#!/bin/sh
# Builds passt, the network of machines on Linux, from a release whose checksum is known, as one
# static program: <folder>/passt. Machines take it from next to the command.
# passt's build needs Linux: it uses GNU sed. CC names the C compiler, `cc` unless set.
# Usage: scripts/build-passt.sh <folder>
set -eu
version=2026_10_02.cba3570
sha256=7b81892b1070521ed217de672cdce43d7a37436e6f850cd04f2b63919ac484b6
[ $# = 1 ] || { echo "usage: $0 <folder>" >&2; exit 2; }
[ "$(uname)" = Linux ] || { echo "build-passt: passt is built on Linux, which its build needs" >&2; exit 1; }
mkdir -p "$1"
out=$(cd "$1" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"
curl -fsSL -o passt.tar.gz "https://passt.top/passt/snapshot/passt-$version.tar.gz"
echo "$sha256  passt.tar.gz" | sha256sum -c - >/dev/null || { echo "build-passt: the source's checksum is wrong" >&2; exit 1; }
tar xzf passt.tar.gz
cd "passt-$version"
# Static, and for 4 KiB pages, which x86-64 always has and most arm64 kernels do (not Asahi's
# 16 KiB), rather than for the build's own.
make passt CC="${CC:-cc}" VERSION="$version" CPPFLAGS="-UPAGE_SIZE -DPAGE_SIZE=4096 -DGLIBC_NO_STATIC_NSS" LDFLAGS=-static >/dev/null
cp passt "$out/passt"
echo "$out/passt"
