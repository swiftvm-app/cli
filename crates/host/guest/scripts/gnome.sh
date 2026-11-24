#!/bin/sh
# name: GNOME
# summary: The GNOME desktop, as Ubuntu, Debian, Fedora and Arch Linux set it up
# category: desktop
# systems: ubuntu debian fedora arch
# memory: 4
# disk: 24
#
# A setup script for @NAME@. It runs as root in a new virtual machine the first time the machine
# starts. A line it prints that starts with "==> " is the step @NAME@ shows.
#
# GNOME runs on X11 here, with software rendering: systems whose GNOME has no X11 session any more
# (GNOME 49 and later, as on Fedora 43) can't run it.

set -eu
@DESKTOP@

# Said before anything is installed: GNOME 49 dropped its X11 session.
version=${VERSION_ID:-0}
case $ID in
    fedora) [ "${version%%.*}" -lt 43 ] || no_x11=yes ;;
    ubuntu) [ "$(printf '%s\n' "$version" 25.10 | sort -V | head -n 1)" != 25.10 ] || no_x11=yes ;;
    arch | archarm) no_x11=yes ;;
esac
if [ "${no_x11:-}" = yes ]; then
    echo "GNOME on $PRETTY_NAME has no X11 session, and the desktop runs on a VNC server, which is an X server. Choose another desktop, such as xfce." >&2
    exit 1
fi

case $ID in
    # A minus takes a package out: the assistant that would ask for an account that is there
    # already, and Firefox, which has a step of its own.
    ubuntu) packages="ubuntu-desktop-minimal network-manager-config-connectivity-ubuntu gnome-initial-setup- firefox-" ;;
    debian) packages="gnome-core network-manager-config-connectivity-debian firefox-esr gnome-initial-setup-" ;;
    arch | archarm) packages="gnome-shell gnome-session gnome-control-center gnome-console nautilus gnome-text-editor gnome-keyring xdg-user-dirs-gtk networkmanager firefox" ;;
    fedora) packages="@gnome-desktop ptyxis firefox" ;;
    *) echo "This script is written for Ubuntu, Debian, Fedora and Arch Linux, not for $ID." >&2; exit 1 ;;
esac
# Left out on Fedora too: the assistant, and the tour that would cover the desktop at every first
# login.
excluded=--exclude=gnome-initial-setup,gnome-tour
desktop_install GNOME "$packages"

echo "==> Finishing"
# The screen doesn't blank or lock while nobody types, and no welcome covers it at the first login:
# whoever drives the desktop from outside finds it as it was left. The password still unlocks a
# locked screen, sudo and SSH.
mkdir -p /etc/dconf/profile /etc/dconf/db/local.d
[ -e /etc/dconf/profile/user ] || printf 'user-db:user\n' > /etc/dconf/profile/user
grep -qx 'system-db:local' /etc/dconf/profile/user || echo 'system-db:local' >> /etc/dconf/profile/user
cat > /etc/dconf/db/local.d/90-@NAME@ << 'END'
[org/gnome/desktop/session]
idle-delay=uint32 0

[org/gnome/desktop/screensaver]
lock-enabled=false

[org/gnome/shell]
welcome-dialog-last-shown-version='999'
END
dconf update
desktop_finish ubuntu-xorg gnome-xorg
