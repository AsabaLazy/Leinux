#!/bin/sh
set -eu

[ "$(id -u)" -eq 0 ] || { echo "[ERROR] 请使用 root 权限运行: sudo ./uninstall.sh" >&2; exit 1; }

systemctl stop leigod_webui.service leigod_plugin.service 2>/dev/null || true
systemctl disable leigod_webui.service leigod_plugin.service 2>/dev/null || true

if [ -x /opt/leigod/leigod_cleanup_net.sh ]; then
    /opt/leigod/leigod_cleanup_net.sh 2>/dev/null || true
fi

if [ -r /var/lib/leigod/device-mac ] && command -v ip >/dev/null 2>&1 \
    && ip -d link show wlan0 2>/dev/null | grep -qw dummy; then
    expected=$(tr 'A-F' 'a-f' < /var/lib/leigod/device-mac | tr -d ' \t\r\n')
    actual=$(cat /sys/class/net/wlan0/address 2>/dev/null || true)
    if [ -n "$expected" ] && [ "$actual" = "$expected" ]; then
        ip link delete wlan0 2>/dev/null || true
    fi
fi

rm -f /etc/systemd/system/leigod_plugin.service \
      /etc/systemd/system/leigod_webui.service \
      /usr/bin/leigod-webui \
      /usr/bin/lgd \
      /usr/share/applications/leigod-webui.desktop \
      /usr/share/icons/hicolor/scalable/apps/io.github.asabalazy.open-raijin.svg \
      /usr/share/icons/hicolor/symbolic/apps/io.github.asabalazy.open-raijin-symbolic.svg \
      /usr/share/polkit-1/rules.d/50-leigod-plugin.rules
systemctl daemon-reload 2>/dev/null || true
rm -rf /opt/leigod /run/leigod

if [ -L /home/leigod ] && [ "$(readlink /home/leigod)" = "/opt/leigod" ]; then
    rm -f /home/leigod
fi

if [ "${LEIGOD_PURGE_STATE:-0}" = "1" ]; then
    rm -rf /var/lib/leigod
fi

echo "[INFO] open-Raijin 已卸载。"
