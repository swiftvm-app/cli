#!/bin/sh
# Installs @NAME@'s guest agent, and the setup scripts that were picked for this machine, from the
# disc that the machine was created with. The first-start setup runs it once, with the folder the
# disc is mounted at.
set -eu
from=$1
home=/usr/local/lib/@NAME@
units=/etc/systemd/system

mkdir -p "$home"
# Put in place under another name and then renamed, as an update of the agent does.
cp "$from/agent" "$home/agent.new"
chmod 755 "$home/agent.new"
mv -f "$home/agent.new" "$home/agent"
cp "$from/agent.service" "$units/@NAME@-agent.service"

if [ -f "$from/script-1" ]; then
    mkdir -p "$home/setup/scripts"
    cp "$from/setup-run.sh" "$home/setup/run.sh"
    cp "$from/settings" "$home/setup/settings"
    number=1
    while [ -f "$from/script-$number" ]; do
        # For the administrator alone: a script may hold what isn't everyone's to read.
        cp "$from/script-$number" "$home/setup/scripts/$number"
        chmod 700 "$home/setup/scripts/$number"
        cp "$from/script-$number.name" "$home/setup/scripts/$number.name"
        number=$((number + 1))
    done
    cp "$from/setup.service" "$units/@NAME@-setup.service"
fi

systemctl daemon-reload
systemctl enable @NAME@-agent.service
# Without waiting: this runs inside the first-start setup.
systemctl start --no-block @NAME@-agent.service
if [ -f "$home/setup/run.sh" ]; then
    sh "$home/setup/run.sh" --schedule
fi
