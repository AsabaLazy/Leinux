#!/bin/sh
set -eu

umask 022

BASE="/opt/leigod"
SCRIPT_DIR=$(CDPATH= cd -P "$(dirname "$0")" && pwd)
BASE_URL="${LEIGOD_BASE_URL:-http://119.3.40.126}"
BIN_SHA256="3640d47abaee4a6b4312e3489137311ab309d27dcb92ab640c0fe3d953328c1b"
XDB_SHA256="3536000ee9eb3997538cb37a14f90d571979734c48b23b8d548eceac16666273"

[ "$(id -u)" -eq 0 ] || { echo "[ERROR] 请使用 root 权限运行: sudo ./install.sh" >&2; exit 1; }
[ "$(uname -m)" = "x86_64" ] || { echo "[ERROR] 仅支持 x86_64 架构" >&2; exit 1; }

if command -v apt-get >/dev/null 2>&1; then
    apt-get update && apt-get install -y --no-install-recommends ca-certificates curl iproute2 ipset iptables procps libgtk-4-1 libgtk-4-dev libadwaita-1-dev cargo rustc pkg-config
elif command -v dnf >/dev/null 2>&1; then
    dnf install -y ca-certificates curl iproute ipset iptables procps-ng gtk4-devel libadwaita-devel cargo rust pkgconf-pkg-config
elif command -v pacman >/dev/null 2>&1; then
    pacman -S --noconfirm --needed ca-certificates curl iproute2 ipset iptables procps-ng gtk4 libadwaita rust pkgconf
elif command -v zypper >/dev/null 2>&1; then
    zypper --non-interactive install ca-certificates curl iproute2 ipset iptables procps gtk4-devel libadwaita-devel cargo rust pkg-config
fi

if [ ! -x "$SCRIPT_DIR/target/release/leinux" ]; then
    echo "[INFO] 正在编译 Leinux (Rust + GTK4)..."
    (cd "$SCRIPT_DIR" && cargo build --release)
fi

systemctl stop leinux.service leinux-core.service leigod_webui.service leigod_plugin.service 2>/dev/null || true
systemctl disable leigod_webui.service leigod_plugin.service 2>/dev/null || true
rm -f /etc/systemd/system/leigod_webui.service \
      /etc/systemd/system/leigod_plugin.service \
      /usr/bin/leigod-webui \
      /usr/share/applications/leigod-webui.desktop \
      /usr/share/polkit-1/rules.d/50-leigod-plugin.rules

tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' 0 HUP INT TERM

echo "[INFO] 正在下载并校验雷神官方组件..."
curl -fL --retry 3 -o "$tmp_dir/acc-gw.router.amd64" "$BASE_URL/acc-gw.router.amd64"
curl -fL --retry 3 -o "$tmp_dir/ipdatacloud_country.xdb" "$BASE_URL/ipdatacloud_country.xdb"

printf '%s  %s\n%s  %s\n' \
    "$BIN_SHA256" "$tmp_dir/acc-gw.router.amd64" \
    "$XDB_SHA256" "$tmp_dir/ipdatacloud_country.xdb" | sha256sum -c -

install -dm755 "$BASE/config"
install -Dm755 "$tmp_dir/acc-gw.router.amd64" "$BASE/acc-gw.router.amd64"
install -Dm755 "$tmp_dir/acc-gw.router.amd64" "$BASE/acc_upgrade_monitor"
install -Dm644 "$tmp_dir/ipdatacloud_country.xdb" "$BASE/config/ipdatacloud_country.xdb"

install -Dm755 "$SCRIPT_DIR/opt/leigod/steamdeck_acc_monitor.sh" "$BASE/steamdeck_acc_monitor.sh"
install -Dm755 "$SCRIPT_DIR/opt/leigod/fake_ps" "$BASE/fake_ps"
install -Dm755 "$SCRIPT_DIR/opt/leigod/leigod_cleanup_net.sh" "$BASE/leigod_cleanup_net.sh"
install -Dm755 "$SCRIPT_DIR/target/release/leinux" /usr/bin/leinux
ln -sfn leinux /usr/bin/lgd
install -Dm644 "$SCRIPT_DIR/share/icons/io.github.asabalazy.leinux.svg" /usr/share/icons/hicolor/scalable/apps/io.github.asabalazy.leinux.svg
install -Dm644 "$SCRIPT_DIR/share/icons/io.github.asabalazy.leinux-symbolic.svg" /usr/share/icons/hicolor/symbolic/apps/io.github.asabalazy.leinux-symbolic.svg
install -Dm644 "$SCRIPT_DIR/share/io.github.asabalazy.leinux.desktop" /usr/share/applications/io.github.asabalazy.leinux.desktop
[ -f /etc/default/leinux ] || install -Dm644 "$SCRIPT_DIR/etc/default/leinux" /etc/default/leinux
install -Dm644 "$SCRIPT_DIR/polkit/50-leinux.rules" /usr/share/polkit-1/rules.d/50-leinux.rules

install -Dm644 "$SCRIPT_DIR/opt/leigod/fake_os-release" "$BASE/fake_os-release"
install -Dm644 "$SCRIPT_DIR/opt/leigod/fake_product_name" "$BASE/fake_product_name"
install -Dm644 "$SCRIPT_DIR/opt/leigod/fake_hosts" "$BASE/fake_hosts"
install -Dm644 "$SCRIPT_DIR/opt/leigod/config/acc_version.ini" "$BASE/config/acc_version.ini"
install -Dm644 "$SCRIPT_DIR/opt/leigod/config/accelerator.ini" "$BASE/config/accelerator.ini"
install -Dm644 "$SCRIPT_DIR/opt/leigod/config/new_upgrade_conf.json" "$BASE/config/new_upgrade_conf.json"
: > "$BASE/config/accelerator"
chmod 0644 "$BASE/config/accelerator"

ln -sfn "$BASE" /home/leigod
install -Dm644 "$SCRIPT_DIR/systemd/leinux-core.service" /etc/systemd/system/leinux-core.service
install -Dm644 "$SCRIPT_DIR/systemd/leinux.service" /etc/systemd/system/leinux.service
systemctl daemon-reload
systemctl enable --now leinux-core.service leinux.service

echo "[INFO] Leinux 安装完成！可在终端输入 'lgd' (或 'leinux') 打开原生 GTK4 控制面板或查看 'lgd --help'。"
