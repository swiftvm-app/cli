#!/bin/sh
# name: Cinnamon
# summary: The desktop of Linux Mint, from the distribution's own packages
# category: desktop
# systems: ubuntu debian fedora arch
# memory: 4
# disk: 24
#
# A setup script for @NAME@. It runs as root in a new virtual machine the first time the machine
# starts. A line it prints that starts with "==> " is the step @NAME@ shows.

set -eu
@DESKTOP@

case $ID in
    ubuntu) packages="cinnamon-core dconf-cli network-manager-config-connectivity-ubuntu" ;;
    debian) packages="cinnamon-core dconf-cli network-manager-config-connectivity-debian firefox-esr" ;;
    arch | archarm) packages="cinnamon gnome-terminal networkmanager firefox" ;;
    fedora) packages="@cinnamon-desktop firefox" ;;
    *) echo "This script is written for Ubuntu, Debian, Fedora and Arch Linux, not for $ID." >&2; exit 1 ;;
esac
desktop_install Cinnamon "$packages"

echo "==> Finishing"
# The screen doesn't blank or lock while nobody types: whoever drives the desktop from outside
# finds it as it was left. The password still unlocks a locked screen, sudo and SSH.
mkdir -p /etc/dconf/profile /etc/dconf/db/local.d
[ -e /etc/dconf/profile/user ] || printf 'user-db:user\n' > /etc/dconf/profile/user
grep -qx 'system-db:local' /etc/dconf/profile/user || echo 'system-db:local' >> /etc/dconf/profile/user
cat > /etc/dconf/db/local.d/90-@NAME@ << 'END'
[org/cinnamon/desktop/session]
idle-delay=uint32 0

[org/cinnamon/desktop/screensaver]
lock-enabled=false
END
dconf update
# Without graphics acceleration, the session that draws in software where there is one.
desktop_finish cinnamon2d cinnamon
