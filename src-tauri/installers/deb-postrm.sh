#!/bin/sh
# An upgrade also calls postrm with upgrade; leave its timer in place.
if [ "${1:-}" = remove ] || [ "${1:-}" = purge ]; then
    systemctl disable --now asale-update.timer 2>/dev/null || true
    rm -f /etc/systemd/system/asale-update.timer /etc/systemd/system/asale-update.service
    systemctl daemon-reload 2>/dev/null || true
fi
