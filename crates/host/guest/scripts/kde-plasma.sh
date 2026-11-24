#!/bin/sh
# name: KDE Plasma
# summary: The KDE Plasma desktop
# category: desktop
# systems: ubuntu debian fedora arch
# memory: 4
# disk: 24
#
# A setup script for @NAME@. It runs as root in a new virtual machine the first time the machine
# starts. A line it prints that starts with "==> " is the step @NAME@ shows.
#
# Plasma runs on X11 here: systems whose Plasma has no X11 session any more can't run it.

set -eu
@DESKTOP@

case $ID in
    ubuntu) packages="kde-plasma-desktop network-manager-config-connectivity-ubuntu" ;;
    debian) packages="kde-plasma-desktop network-manager-config-connectivity-debian firefox-esr" ;;
    arch | archarm) packages="plasma-desktop plasma-x11-session konsole dolphin plasma-nm networkmanager firefox" ;;
    fedora) packages="@kde-desktop plasma-workspace-x11 konsole dolphin plasma-nm firefox" ;;
    *) echo "This script is written for Ubuntu, Debian, Fedora and Arch Linux, not for $ID." >&2; exit 1 ;;
esac
# Left out on Fedora: the assistant for a first start, which would ask for an account that is there
# already, and the welcome window that would cover the desktop at every first login.
excluded=--exclude=plasma-setup,plasma-welcome
desktop_install "KDE Plasma" "$packages"

echo "==> Finishing"
# The screen doesn't lock while nobody types: whoever drives the desktop from outside finds it as it
# was left. The password still unlocks a locked screen, sudo and SSH.
mkdir -p /etc/xdg
printf '[Daemon]\nAutolock=false\nLockOnResume=false\n' > /etc/xdg/kscreenlockerrc
# Nor does the Welcome Center cover it at the first login: it shows itself once for each version of
# itself, which is taken as seen. Where it isn't installed there is nothing to keep away.
plasma=''
if command -v dpkg-query > /dev/null; then
    plasma=$(dpkg-query -W -f '${Version}' plasma-welcome 2> /dev/null || true)
elif command -v rpm > /dev/null; then
    plasma=$(rpm -q --qf '%{VERSION}' plasma-welcome 2> /dev/null || true)
else
    plasma=$(pacman -Q plasma-welcome 2> /dev/null | awk '{ print $2 }')
fi
# Without the epoch and the packaging's revision: 4:6.3.4-1 is 6.3.4.
plasma=$(printf '%s' "$plasma" | sed 's/^[0-9]*://; s/-.*//')
[ -z "$plasma" ] || printf '[General]\nLastSeenVersion=%s\n' "$plasma" > /etc/xdg/plasma-welcomerc
# Plasma's parts run in the session rather than as services of the account's systemd, which would
# outlive a session that is started again, and keep the next one from starting.
printf '[General]\nsystemdBoot=false\n' > /etc/xdg/startkderc
# Plasma 6 calls its X11 session plasmax11, and Plasma 5 plasma.
desktop_finish plasmax11 plasma
