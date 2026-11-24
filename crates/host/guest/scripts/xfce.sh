#!/bin/sh
# name: Xfce
# summary: A light desktop that is quick without graphics acceleration
# category: desktop
# systems: ubuntu debian fedora arch
# memory: 2
# disk: 16
#
# A setup script for @NAME@. It runs as root in a new virtual machine the first time the machine
# starts. A line it prints that starts with "==> " is the step @NAME@ shows.

set -eu
@DESKTOP@

case $ID in
    ubuntu) packages="xubuntu-desktop-minimal network-manager-config-connectivity-ubuntu" ;;
    # Without the screen locker of a login screen that there isn't.
    debian) packages="xfce4 xfce4-terminal network-manager-gnome network-manager-config-connectivity-debian firefox-esr light-locker-" ;;
    arch | archarm) packages="xfce4 xfce4-terminal network-manager-applet networkmanager firefox" ;;
    fedora) packages="@xfce-desktop firefox" ;;
    *) echo "This script is written for Ubuntu, Debian, Fedora and Arch Linux, not for $ID." >&2; exit 1 ;;
esac
desktop_install Xfce "$packages"

echo "==> Finishing"
# The screen doesn't blank or lock while nobody types: whoever drives the desktop from outside
# finds it as it was left. The password still unlocks a locked screen, sudo and SSH.
settings=/etc/xdg/xfce4/xfconf/xfce-perchannel-xml
mkdir -p "$settings"
cat > "$settings/xfce4-power-manager.xml" << 'END'
<?xml version="1.0" encoding="UTF-8"?>
<channel name="xfce4-power-manager" version="1.0">
  <property name="xfce4-power-manager" type="empty">
    <property name="dpms-enabled" type="bool" value="false"/>
    <property name="blank-on-ac" type="int" value="0"/>
    <property name="lock-screen-suspend-hibernate" type="bool" value="false"/>
  </property>
</channel>
END
cat > "$settings/xfce4-screensaver.xml" << 'END'
<?xml version="1.0" encoding="UTF-8"?>
<channel name="xfce4-screensaver" version="1.0">
  <property name="saver" type="empty">
    <property name="enabled" type="bool" value="false"/>
  </property>
  <property name="lock" type="empty">
    <property name="enabled" type="bool" value="false"/>
  </property>
</channel>
END
desktop_finish xubuntu xfce
