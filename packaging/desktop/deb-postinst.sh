#!/bin/sh
# flashcp-gui postinst: drive mode on for every user (systemd user preset), udev rule active.
set -e
if [ "$1" = configure ]; then
  udevadm control --reload 2>/dev/null || true
  systemctl --global preset flashcp-drived.service 2>/dev/null || true
  # Users logged in right now: start their daemon (others get it at their next login).
  for u in $(loginctl list-users --no-legend 2>/dev/null | awk '$1 >= 1000 {print $2}'); do
    systemctl --user -M "$u@" daemon-reload 2>/dev/null || true
    systemctl --user -M "$u@" start flashcp-drived.service 2>/dev/null || true
  done
fi
exit 0
