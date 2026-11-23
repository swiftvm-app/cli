#!/bin/sh
# Cargo's runner on macOS: signs a built program with the virtualization entitlement, then runs it.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
codesign --sign - --force --entitlements "$root/config/virtualization.entitlements" "$1" 2> /dev/null
exec "$@"
