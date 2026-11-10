#!/bin/sh
# flashcp-gui prerm: stop every user's drive daemon first (each flushes and unmounts its drives).
set -e
if [ "$1" = remove ] || [ "$1" = deconfigure ]; then
  systemctl --global disable flashcp-drived.service 2>/dev/null || true
  for u in $(loginctl list-users --no-legend 2>/dev/null | awk '$1 >= 1000 {print $2}'); do
    systemctl --user -M "$u@" stop flashcp-drived.service 2>/dev/null || true
  done
fi
exit 0
