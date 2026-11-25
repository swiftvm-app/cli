#!/bin/sh
# Builds virtiofsd, which serves shared folders to machines on Linux, from a release whose checksum
# is known, as one static program: <folder>/virtiofsd. Machines take it from next to the command.
# It needs libseccomp and libcap-ng as static libraries, which Alpine has: the build runs in an
# Alpine container, for this computer's processor, with Docker.
# Usage: scripts/build-virtiofsd.sh <folder>
set -eu
version=1.15.0
sha256=aeac41b813d490bb11941595b0ba09e49b8884dbe0d47f5df2fd891c8e0ecace
alpine=alpine:3.22
[ $# = 1 ] || { echo "usage: $0 <folder>" >&2; exit 2; }
[ "$(uname)" = Linux ] || { echo "build-virtiofsd: virtiofsd is built on Linux" >&2; exit 1; }
command -v docker >/dev/null || { echo "build-virtiofsd: docker isn't installed" >&2; exit 1; }
mkdir -p "$1"
out=$(cd "$1" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"
# From crates.io, whose files never change once they are published.
curl -fsSL -o virtiofsd.tar.gz "https://static.crates.io/crates/virtiofsd/virtiofsd-$version.crate"
echo "$sha256  virtiofsd.tar.gz" | sha256sum -c - >/dev/null || { echo "build-virtiofsd: the source's checksum is wrong" >&2; exit 1; }
tar xzf virtiofsd.tar.gz
# What the container writes belongs to whoever runs this, so that it can be removed.
docker run --rm -v "$work:/work" -w "/work/virtiofsd-$version" -e OWNER="$(id -u):$(id -g)" "$alpine" sh -euc '
    trap "chown -R \"\$OWNER\" /work" EXIT
    apk add --no-cache -q cargo musl-dev libseccomp-dev libseccomp-static libcap-ng-dev libcap-ng-static >/dev/null
    # With the target named, the flags are the program'"'"'s alone, not those of the macros its build runs.
    # The two libraries are linked in too: left to themselves, their crates link them as shared
    # libraries, which a static program can'"'"'t call.
    target=$(rustc -vV | sed -n "s/^host: //p")
    export LIBSECCOMP_LINK_TYPE=static LIBSECCOMP_LIB_PATH=/usr/lib LIBCAPNG_LINK_TYPE=static LIBCAPNG_LIB_PATH=/usr/lib
    RUSTFLAGS="-C target-feature=+crt-static" cargo build --release --locked --quiet --target "$target"
    cp "target/$target/release/virtiofsd" /work/virtiofsd
'
cp virtiofsd "$out/virtiofsd"
echo "$out/virtiofsd"
