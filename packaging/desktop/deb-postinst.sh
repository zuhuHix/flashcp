#!/bin/sh
# flashcp-gui postinst: drive mode on for every user (systemd user preset), udev rule active.
set -e
if [ "$1" = configure ]; then
  udevadm control --reload 2>/dev/null || true
  # A fresh install, or an upgrade from a version that had no drive mode (before 0.3.0): turn it
  # on. A later upgrade keeps an admin's `--global disable` and users' opt-outs.
  if [ -z "$2" ] || dpkg --compare-versions "$2" lt 0.3.0; then
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
