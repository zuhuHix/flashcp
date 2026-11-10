#!/bin/sh
# flashcp-gui postinst: drive mode on for every user (systemd user preset), udev rule active.
set -e
if [ "$1" = configure ]; then
  udevadm control --reload 2>/dev/null || true
  # Fresh install only: an upgrade keeps an admin's `--global disable` and users' opt-outs.
  if [ -z "$2" ]; then
    systemctl --global preset flashcp-drived.service 2>/dev/null || true
  fi
  # Users logged in right now: (re)start their daemon if it is enabled for them (others get it
  # at their next login).
  for u in $(loginctl list-users --no-legend 2>/dev/null | awk '$1 >= 1000 {print $2}'); do
    systemctl --user -M "$u@" daemon-reload 2>/dev/null || true
    if systemctl --user -M "$u@" is-enabled --quiet flashcp-drived.service 2>/dev/null; then
      systemctl --user -M "$u@" restart flashcp-drived.service 2>/dev/null || true
    fi
  done
fi
exit 0
