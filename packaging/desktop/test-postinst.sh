#!/bin/sh
# Checks when deb-postinst.sh turns drive mode on (the systemd user preset): on a fresh install and
# on an upgrade from a version without drive mode (before 0.3.0), and never on a later upgrade.
# Runs the real script with stub systemctl/udevadm/loginctl first in PATH. Needs dpkg.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
t=$(mktemp -d)
trap 'rm -rf "$t"' EXIT
for c in udevadm loginctl; do printf '#!/bin/sh\nexit 0\n' > "$t/$c"; done
printf '#!/bin/sh\necho "$*" >> "%s/calls"\nexit 0\n' "$t" > "$t/systemctl"
chmod +x "$t/udevadm" "$t/loginctl" "$t/systemctl"
fail=0
check() { # old version ("" = fresh install), expected preset calls
  : > "$t/calls"
  PATH="$t:$PATH" sh "$here/deb-postinst.sh" configure "$1"
  n=$(grep -c -- '--global preset flashcp-drived.service' "$t/calls" || true)
  if [ "$n" != "$2" ]; then
    echo "FAIL: configure '$1': $n preset calls, expected $2"
    fail=1
  fi
}
check "" 1
check 0.1.0 1
check 0.2.0 1
check 0.3.0 0
check 0.3.1 0
check 1.0.0 0
exit $fail
