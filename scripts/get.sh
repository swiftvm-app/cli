#!/bin/sh
# Installs @NAME@ from a release on GitHub, for macOS 14 or later and Linux, on arm64 and x86-64:
#
#   curl -fsSL https://github.com/@REPO@/releases/latest/download/install.sh | sh
#
# @VARIABLE@_VERSION picks a release, such as 0.2.0, rather than the latest.
# @VARIABLE@_INSTALL_DIR is the folder it goes into, ~/.local/bin unless set.
#
# Each release has this file as install.sh, with the names filled in by the release workflow. It
# checks what it downloads against the release's SHA256SUMS. The program is put in place under a
# new name and then renamed: a machine that is running is a process of the program that was there
# before, and keeps it.
set -eu

name=@NAME@
repo=@REPO@

fail() {
    echo "install: $*" >&2
    exit 1
}

download() {
    if command -v curl >/dev/null; then
        curl -fsSL --retry 3 -o "$2" "$1"
    elif command -v wget >/dev/null; then
        wget -q -O "$2" "$1"
    else
        fail "installing takes curl or wget"
    fi
}

sha256() {
    if command -v sha256sum >/dev/null; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

# All of it is in a function, so that nothing runs if the download of this file is cut short.
main() {
    case $(uname -s) in
        Darwin)
            os=macos
            major=$(sw_vers -productVersion | cut -d. -f1)
            [ "$major" -ge 14 ] || fail "$name needs macOS 14 or later, and this is $(sw_vers -productVersion)"
            ;;
        Linux) os=linux ;;
        *) fail "$name runs on macOS and Linux, not $(uname -s)" ;;
    esac
    case $(uname -m) in
        arm64 | aarch64) arch=arm64 ;;
        x86_64 | amd64) arch=x86_64 ;;
        *) fail "$name runs on arm64 and x86-64, not $(uname -m)" ;;
    esac
    # A shell under Rosetta says x86_64 on a Mac with Apple's processor.
    if [ "$os-$arch" = macos-x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null)" = 1 ]; then
        arch=arm64
    fi

    version=${@VARIABLE@_VERSION:-}
    if [ -n "$version" ]; then
        base=https://github.com/$repo/releases/download/v${version#v}
    else
        base=https://github.com/$repo/releases/latest/download
    fi
    folder=${@VARIABLE@_INSTALL_DIR:-$HOME/.local/bin}
    asset=$name-$os-$arch.tar.gz

    work=$(mktemp -d)
    trap 'rm -rf "$work"' EXIT
    download "$base/$asset" "$work/$asset" || fail "couldn't download $base/$asset"
    download "$base/SHA256SUMS" "$work/SHA256SUMS" || fail "couldn't download $base/SHA256SUMS"
    wanted=$(awk -v asset="$asset" '$2 == asset { print $1 }' "$work/SHA256SUMS")
    [ -n "$wanted" ] || fail "SHA256SUMS has no line for $asset"
    [ "$(sha256 "$work/$asset")" = "$wanted" ] || fail "$asset isn't what SHA256SUMS says: the download is damaged"
    mkdir "$work/files"
    tar -xzf "$work/$asset" -C "$work/files"

    mkdir -p "$folder"
    folder=$(cd "$folder" && pwd)
    # passt, the network of machines on Linux, goes next to the program, where it looks for it.
    for file in "$name" passt; do
        [ -f "$work/files/$file" ] || continue
        cp "$work/files/$file" "$folder/.$file.new"
        chmod 755 "$folder/.$file.new"
        mv -f "$folder/.$file.new" "$folder/$file"
    done

    echo "Installed $("$folder/$name" version) in $folder/$name"
    case ":$PATH:" in
        *":$folder:"*) ;;
        *) echo "$folder isn't on PATH. Add it, or run $folder/$name." ;;
    esac
    if [ "$os" = linux ] && ! { [ -r /dev/kvm ] && [ -w /dev/kvm ]; }; then
        echo "Machines need /dev/kvm, which this user can't use. Add the user to the kvm group: sudo usermod -aG kvm \$USER, then log in again."
    fi
}

main
