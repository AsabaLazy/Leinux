#!/bin/sh
set -eu

umask 077

STATE_DIR="${LEIGOD_STATE_DIR:-/var/lib/leigod}"
MAC_FILE="${STATE_DIR}/device-mac"

mkdir -p "$STATE_DIR" /tmp/acc/log
chmod 0700 "$STATE_DIR" /tmp/acc /tmp/acc/log

if [ -s "$MAC_FILE" ]; then
    DEVICE_MAC=$(tr 'A-F' 'a-f' < "$MAC_FILE" | tr -d ' \t\r\n')
else
    mid=$(tr -d ' \t\r\n' < /etc/machine-id)
    hex=$(printf 'leigod-device:%s\n' "$mid" | sha256sum | cut -c1-10)
    DEVICE_MAC=$(printf '02:%s:%s:%s:%s:%s' \
        "$(printf '%s' "$hex" | cut -c1-2)" \
        "$(printf '%s' "$hex" | cut -c3-4)" \
        "$(printf '%s' "$hex" | cut -c5-6)" \
        "$(printf '%s' "$hex" | cut -c7-8)" \
        "$(printf '%s' "$hex" | cut -c9-10)")
    printf '%s\n' "$DEVICE_MAC" > "${MAC_FILE}.tmp"
    chmod 0600 "${MAC_FILE}.tmp"
    mv -f "${MAC_FILE}.tmp" "$MAC_FILE"
fi

if ! ip link show wlan0 >/dev/null 2>&1; then
    ip link add wlan0 type dummy
fi

if ip -d link show wlan0 2>/dev/null | grep -qw dummy; then
    cur_mac=$(cat /sys/class/net/wlan0/address 2>/dev/null || true)
    if [ "$cur_mac" != "$DEVICE_MAC" ]; then
        ip link set wlan0 down
        ip link set wlan0 address "$DEVICE_MAC"
    fi
    ip link set wlan0 up
fi

exec /opt/leigod/acc-gw.router.amd64 -r daemon -m tun -p 5588 >/dev/null 2>&1 </dev/null
