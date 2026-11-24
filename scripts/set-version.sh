#!/bin/sh
# Sets the version, `version` under [workspace.package] in Cargo.toml, and Cargo.lock with it, and
# prints it. It takes a version, such as 1.2.0 or 1.2.0-beta.1, or what to step:
#
#   patch   1.2.3 gives 1.2.4, and 1.2.4-beta.1 gives 1.2.4
#   minor   1.2.3 gives 1.3.0
#   major   1.2.3 gives 2.0.0
#
# Usage: scripts/set-version.sh <version | patch | minor | major>
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
[ $# = 1 ] || { echo "usage: $0 <version | patch | minor | major>" >&2; exit 2; }
current=$(awk -F'"' '/^\[/ { section = $0 } section == "[workspace.package]" && /^version *=/ { print $2; exit }' "$root/Cargo.toml")
[ -n "$current" ] || { echo "set-version: Cargo.toml has no version under [workspace.package]" >&2; exit 1; }

base=${current%%-*}
major=${base%%.*}
rest=${base#*.}
minor=${rest%%.*}
patch=${rest#*.}
case $1 in
    patch) if [ "$base" != "$current" ]; then next=$base; else next=$major.$minor.$((patch + 1)); fi ;;
    minor) next=$major.$((minor + 1)).0 ;;
    major) next=$((major + 1)).0.0 ;;
    *) next=${1#v} ;;
esac
if ! echo "$next" | grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.]+)?$'; then
    echo "set-version: \"$1\" isn't a version such as 1.2.0 or 1.2.0-beta.1, nor patch, minor or major" >&2
    exit 2
fi

if [ "$next" != "$current" ]; then
    awk -v next_version="$next" '
        /^\[/ { section = $0 }
        section == "[workspace.package]" && /^version *=/ && !done { print "version = \"" next_version "\""; done = 1; next }
        { print }
    ' "$root/Cargo.toml" > "$root/.Cargo.toml.new"
    mv "$root/.Cargo.toml.new" "$root/Cargo.toml"
    # The workspace's own packages alone: the versions of the others stay as they are.
    (cd "$root" && cargo update --workspace --quiet)
fi
echo "$next"
