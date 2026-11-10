#!/bin/sh
# flashcp-gui postrm: the udev rule is gone; re-run the rules for exFAT block devices so drives
# that are plugged in become visible to udisks again (normal automount from now on).
set -e
if [ "$1" = remove ] || [ "$1" = purge ]; then
  udevadm control --reload 2>/dev/null || true
  udevadm trigger --subsystem-match=block --action=change --property-match=ID_FS_TYPE=exfat 2>/dev/null || true
fi
if [ "$1" = purge ]; then
  rm -f /etc/flashcp/drive-mode-disabled
  rmdir /etc/flashcp 2>/dev/null || true
fi
exit 0
