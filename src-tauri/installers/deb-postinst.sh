#!/bin/sh
set -e
# dpkg has already authorized this installation. Future updates use a protected
# worker and systemd timer, never a password dialog after the seller has stopped.
if [ "${1:-}" = configure ] && command -v systemctl >/dev/null 2>&1; then
    owner="${SUDO_USER:-root}"
    home_dir="$(getent passwd "$owner" | cut -d: -f6)"
    [ -n "$home_dir" ] || home_dir=/root
    data_dir="${ASALE_DATA_DIR:-$home_dir/.asale}"
    /usr/lib/asale-updater/asale autoupdate enable --system-package --data-dir "$data_dir" \
      || echo 'Asale: could not enable the system updater; current selling services are unchanged.' >&2
fi
