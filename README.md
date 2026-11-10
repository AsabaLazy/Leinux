# Leinux

雷神加速器 x86_64 Linux 原生客户端（Rust + GTK4 + libadwaita），集成了原生桌面 GUI、`lgd` 命令行工具与后台路由守护进程。

## 特性

- **Rust + GTK4 + libadwaita 三合一单二进制**：GNOME 风格 libadwaita 桌面窗口、`lgd` 终端命令行与后台守护进程（`--daemon`）合为单一原生二进制（~1.1 MB），同时保留 `http://127.0.0.1:8088` 本地接口。
- **一键弹窗登录与免手机自动绑定**：内置官方登录小窗口（`lgd --login` 或界面一键打开），登录成功后自动提取 `pToken`、关闭小窗口、在全新裸机上完成硬件绑定并永久自动续期官网凭证，全程无需安装手机雷神 App。
- **时长自动管理**：启动加速自动恢复雷神官网时长，停止加速或系统关机（`SIGTERM`）自动暂停时长。
- **七大洲全区服动态发现与并发探活**：直接从雷神云端动态拉取全部可用加速区服（按七大洲分类，大洲通用节点置顶并按节点数智能排序）；启动时 `250ms` 多线程并发 TCP 探活，自动剔除失效节点并按实测延迟优选。
- **软路由与局域网共存**：`LEIGOD_GUARD` 防火墙保护链自动放行局域网、软路由 Fake-IP（`198.18.0.0/15`）、非游戏 DNS 及已建立的存量连接，仅对白名单游戏流量走 `tun_PC` 加速。
- **沙盒隔离与遥测屏蔽**：基于 `systemd` 沙盒运行，自动屏蔽非必要的日志与遥测上报。

## 安装

### Arch Linux

```bash
paru -S leinux-bin
```

### 通用 Linux (Debian / Ubuntu / Fedora / Arch / openSUSE)

```bash
git clone https://github.com/AsabaLazy/Leinux.git
cd Leinux
sudo ./install.sh
```

## 快速使用

### 1. 启动原生 GTK4 控制面板

从桌面应用菜单点击 **Leinux**，或在终端执行：

```bash
lgd
```

首次安装后无需安装手机雷神 App，直接点击主界面的 **登录账号**（或在终端执行 `lgd --login`），在弹出的官方小窗口中完成手机号验证码或密码登录，客户端即可自动提取凭证、完成本机硬件绑定并从云端拉取全部区服节点。

### 2. `lgd` 命令行用法

```bash
lgd                  # 自动启动后台服务并打开原生 GTK4 控制面板窗口
lgd --login          # 弹出官方登录小窗口，登录成功后自动完成设备绑定与凭证保存
lgd --list           # 按七大洲列出云端动态同步的全部可用区服及节点数
lgd --start [区服]    # 免开界面直接恢复时长并加速 (不传参数则使用上次选择的区服)
lgd --pause          # 仅断开加速隧道并暂停官网账号时长 (保持后台服务运行)
lgd --stop           # 暂停官网账号时长并停止后台服务
lgd --status         # 查看服务运行状态、当前加速区服/延迟与账号剩余时长
lgd --port <端口>     # 修改本地守护服务端口 (写入 /etc/default/leigod-webui) 并重启服务
lgd --help           # 显示帮助信息
```

## 卸载

```bash
sudo ./uninstall.sh
```

## 许可证与声明

本项目原创代码采用 [MIT License](LICENSE)。雷神核心二进制（`acc-gw.router.amd64`）与 IP 库（`ipdatacloud_country.xdb`）在安装/构建时从雷神官方服务器下载并校验 SHA-256，其权利归雷神官方所有，详见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
