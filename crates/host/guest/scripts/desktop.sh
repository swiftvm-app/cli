# What every desktop script shares, put into each by @NAME@: the packages, and the desktop on a VNC
# server of its own. The machine has no display: the desktop draws into the VNC server, an X server
# that listens on localhost:@SCREEN_PORT@ only, which @NAME@ reaches through its guest agent. The
# account is logged in to it by @NAME@-desktop.service, with no login screen, after every start.

. /etc/os-release
export DEBIAN_FRONTEND=noninteractive
account=${@USER_VARIABLE@:-}
[ -n "$account" ] || { echo "The machine has no account to log in to its desktop." >&2; exit 1; }

# apt, with an answer to everything it could ask. It waits for the package lock, which the
# system's own updates may hold on a first start.
apt_get() {
    apt-get -y -o DPkg::Lock::Timeout=900 -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold "$@"
}

# desktop_install <title> <packages>: installs the desktop's packages with the VNC server, and
# what @NAME@ opens things and reads the clipboard with.
desktop_install() {
    title=$1
    packages=$2
    case $ID in
        arch | archarm) packages="$packages tigervnc xorg-xauth xclip xdg-utils" ;;
        fedora) packages="$packages tigervnc-server xorg-x11-xauth xclip xdg-utils" ;;
        *) packages="$packages tigervnc-standalone-server xauth dbus-x11 xclip xdg-utils" ;;
    esac

    if command -v pacman > /dev/null; then
        echo "==> Getting the list of packages"
        # The keys that sign today's packages first, which may be newer than the system's.
        pacman -Sy --noconfirm --needed $(pacman -Qqs '^arch.*-keyring$')

        echo "==> Downloading $title"
        # The whole system is brought up to date with it: Arch Linux doesn't mix old packages with new.
        pacman -Su --noconfirm --needed --downloadonly $packages

        echo "==> Installing $title"
        pacman -Su --noconfirm --needed $packages
        systemctl enable NetworkManager
    elif command -v dnf > /dev/null; then
        echo "==> Getting the list of packages"
        dnf -y makecache

        echo "==> Downloading $title"
        dnf -y install --downloadonly ${excluded:-} $packages

        echo "==> Installing $title"
        dnf -y install ${excluded:-} $packages
    else
        echo "==> Getting the list of packages"
        # A setup that was cut short carries on here. Unlike apt, dpkg doesn't wait for its lock.
        tries=0
        until dpkg --configure -a; do
            tries=$((tries + 1))
            [ "$tries" -lt 18 ] || exit 1
            sleep 10
        done
        apt_get update

        echo "==> Downloading $title"
        apt_get install --download-only $packages

        echo "==> Installing $title"
        apt_get install $packages

        if [ "$ID" = ubuntu ]; then
            echo "==> Installing Firefox"
            # Ubuntu's Firefox comes from its store, which may not answer, or crawl so that the setup
            # would take hours. A desktop without a browser is still a desktop: after ten minutes it
            # goes without, and the store's download is called off.
            if ! timeout 600 snap install firefox; then
                snap abort --last=install > /dev/null 2>&1 || true
                echo "Firefox couldn't be installed. Install it later with: sudo snap install firefox"
            fi
        fi
    fi
}

# desktop_finish <session>...: logs the account in to the first of these X11 sessions, by the
# names of their files in /usr/share/xsessions, and starts it.
desktop_finish() {
    echo "==> Starting the desktop"
    session=''
    for candidate in "$@"; do
        if [ -e "/usr/share/xsessions/$candidate.desktop" ]; then
            session=$candidate
            break
        fi
    done
    if [ -z "$session" ]; then
        echo "This system has no X11 session of the desktop ($*), and the desktop runs on a VNC server, which is an X server." >&2
        exit 1
    fi

    home=/usr/local/lib/@NAME@/desktop
    mkdir -p "$home"
    cat > "$home/session" << 'END'
#!/bin/sh
# The account's desktop session, on a VNC server: started by @NAME@-desktop.service as the account,
# in a login session of its own. $1 is the session's file in /usr/share/xsessions, without .desktop.
set -u
file=/usr/share/xsessions/$1.desktop
command=$(sed -n 's/^Exec=//p' "$file" | head -n 1)
names=$(sed -n 's/^DesktopNames=//p' "$file" | head -n 1 | tr ';' ':' | sed 's/:$//')
[ -z "$names" ] || export XDG_CURRENT_DESKTOP="$names"
# The key to the X server, in the account's usual file too: a program started some other way, with
# only DISPLAY=:0, finds it there.
cookie=$(mcookie)
export XAUTHORITY="$XDG_RUNTIME_DIR/Xauthority"
rm -f "$XAUTHORITY"
xauth -q -f "$XAUTHORITY" add :0 . "$cookie"
xauth -q -f "$HOME/.Xauthority" add :0 . "$cookie"
# An X server that ended without cleaning up leaves its lock and socket, which would be taken for
# the new one's. One whose process lives, or can't be told of, is left alone.
if [ -e /tmp/.X0-lock ]; then
    holder=$(tr -dc 0-9 < /tmp/.X0-lock)
    if [ -n "$holder" ] && ! kill -0 "$holder" 2> /dev/null && [ ! -d "/proc/$holder" ]; then
        rm -f /tmp/.X0-lock /tmp/.X11-unix/X0
    fi
fi
# Which login session this is, for the service's stop to end: what the session starts lives in it,
# outside the service.
printf '%s\n' "${XDG_SESSION_ID:-}" > "$RUNTIME_DIRECTORY/session"
# No password: only localhost reaches it, and only through the agent from outside. Shared, so that
# a viewer doesn't put @NAME@'s own connection off. No screen saver, which would blank the screen.
xvnc=$(command -v Xvnc || command -v Xtigervnc)
"$xvnc" :0 -auth "$XAUTHORITY" -rfbport @SCREEN_PORT@ -localhost -SecurityTypes None -AlwaysShared \
    -AcceptSetDesktopSize -geometry 1280x800 -depth 24 -s 0 -desktop "$(hostname)" &
waited=0
until [ -S /tmp/.X11-unix/X0 ]; do
    kill -0 $! 2> /dev/null || exit 1
    [ "$waited" -lt 300 ] || exit 1
    sleep 0.1
    waited=$((waited + 1))
done
export DISPLAY=:0
if [ -S "$XDG_RUNTIME_DIR/bus" ]; then
    # The account's own bus, which systemd keeps: what it starts finds the screen too.
    export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"
    dbus-update-activation-environment --systemd DISPLAY XAUTHORITY XDG_CURRENT_DESKTOP \
        XDG_SESSION_TYPE XDG_SESSION_DESKTOP XDG_SESSION_CLASS 2> /dev/null || true
    exec sh -c "$command"
fi
exec dbus-run-session -- sh -c "$command"
END
    chmod 755 "$home/session"

    cat > "$home/stop" << 'END'
#!/bin/sh
# Ends the desktop's login session once @NAME@-desktop.service has stopped, run as root with the
# account in $1. What the session started lives in the session's scope, and the desktop's services
# in the account's own systemd: both would outlive the service, and the next session would find
# its display and its desktop taken.
session=$(cat /run/@NAME@-desktop/session 2> /dev/null)
if [ -n "$session" ]; then
    systemctl stop "session-$session.scope" 2> /dev/null
fi
systemctl --user --machine="$1@" stop graphical-session.target 2> /dev/null
# The X server has gone with the session once its lock has.
waited=0
while [ -e /tmp/.X0-lock ] && [ "$waited" -lt 50 ]; do
    sleep 0.1
    waited=$((waited + 1))
done
exit 0
END
    chmod 755 "$home/stop"

    cat > /etc/systemd/system/@NAME@-desktop.service << END
# The desktop of the machine's account, on a VNC server: what @NAME@ shows of the machine's screen.
[Unit]
Description=@NAME@ desktop
After=systemd-user-sessions.service network.target

[Service]
User=$account
# A login session, as a login screen would open: the account's runtime folder, bus and services.
PAMName=login
Environment=XDG_SESSION_TYPE=x11 XDG_SESSION_CLASS=user XDG_SESSION_DESKTOP=$session
WorkingDirectory=~
ExecStart=$home/session $session
RuntimeDirectory=@NAME@-desktop
ExecStopPost=+$home/stop $account
Restart=always
RestartSec=2

[Install]
WantedBy=multi-user.target
END

    # The desktop's session has no seat, so polkit doesn't count it as one at the machine, and the
    # desktop would ask for a password for what such a session does without asking: colour
    # profiles, the network, shutting down. The account may do anything with sudo, without a
    # password, already.
    mkdir -p /etc/polkit-1/rules.d
    cat > /etc/polkit-1/rules.d/50-@NAME@-desktop.rules << END
// Written by @NAME@'s desktop setup: the account of the desktop isn't asked for its password.
polkit.addRule(function (action, subject) {
    if (subject.user == "$account") {
        return polkit.Result.YES;
    }
});
END

    # No login screen: there is no display for one, and the account is logged in already.
    for manager in gdm gdm3 sddm lightdm plasmalogin; do
        systemctl disable --now "$manager.service" > /dev/null 2>&1 || true
    done
    rm -f /etc/systemd/system/display-manager.service

    # The network card stays with the service that has it, which asks for an address by the card's
    # own. The desktop's NetworkManager only watches the card, so that the desktop knows it is
    # online. Two services that both configure one card would take turns at undoing each other's work.
    mkdir -p /etc/NetworkManager/conf.d
    cat > /etc/NetworkManager/conf.d/zz-@NAME@.conf << 'END'
[main]
no-auto-default=*

[keyfile]
unmanaged-devices=

[device-@NAME@]
match-device=type:ethernet
managed=1
END
    # Ubuntu's desktop would hand every card to NetworkManager.
    if [ -e /usr/lib/netplan/00-network-manager-all.yaml ]; then
        printf 'network:\n  version: 2\n  renderer: networkd\n' > /etc/netplan/00-network-manager-all.yaml
        chmod 600 /etc/netplan/00-network-manager-all.yaml
    fi
    # The host may sleep with the machine in it. A system that goes to sleep by itself may not wake up.
    systemctl mask sleep.target suspend.target hibernate.target hybrid-sleep.target

    systemctl daemon-reload
    systemctl enable @NAME@-desktop.service
    systemctl restart @NAME@-desktop.service
    # Ready when the setup is: whoever waits for it can look at the screen at once.
    waited=0
    until ss -ltn 2> /dev/null | grep -q ":@SCREEN_PORT@ "; do
        if [ "$waited" -ge 60 ]; then
            echo "The desktop didn't start. journalctl -u @NAME@-desktop says why." >&2
            exit 1
        fi
        sleep 1
        waited=$((waited + 1))
    done
}
