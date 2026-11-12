# [![](share/banner.svg)](https://github.com/AsabaLazy/Leinux)

[![Core Check](https://github.com/AsabaLazy/Leinux/actions/workflows/check-core.yml/badge.svg)](https://github.com/AsabaLazy/Leinux/actions/workflows/check-core.yml)
[![Version](https://img.shields.io/github/v/tag/AsabaLazy/Leinux?label=version)](https://github.com/AsabaLazy/Leinux/tags)
[![Arch Repo](https://img.shields.io/badge/Arch_Linux-aslz-1793d1?logo=arch-linux&logoColor=white)](https://github.com/AsabaLazy/aur-ASLZ)
[![License](https://img.shields.io/github/license/AsabaLazy/Leinux)](LICENSE)

[English](README.md) | [中文](README_zh-CN.md)

## 🚀 介绍

Leinux 是一款使用 Rust、GTK4 与 libadwaita 开发的轻量级、现代化雷神加速器 x86_64 Linux 原生客户端，将 GNOME 原生桌面界面、`lgd` 命令行工具与基于 `cgroup v2` 的策略路由守护进程融合为单一原生二进制（约 1.3 MB）。

除常规一键全局海外加速外，Leinux 原生支持接入 Steam 启动选项（`lgd %command%`），将加速流量严格隔离在目标游戏进程树内，并随游戏启动与退出全自动恢复、暂停雷神官网付费时长。

## ✨ 主要特性

- 🪶 **轻量原生体验** — 主界面采用 GTK4 与 libadwaita 1.7 原生渲染，非 Electron、非 WebView 套壳，后台静默待机 CPU 占用为 `0.0%`。
- 🎮 **Steam 进程级专享分流** — Steam 启动项填入 `lgd %command%` 即可通过内核 `cgroup v2` 仅加速该游戏（自动涵盖 Proton / Wine 子进程），对游戏内存零注入、不触发反作弊，日常浏览器与下载直连互不干扰。
- ⏱️ **全自动时长托管** — 启动游戏或开启加速时自动恢复官网时长，退出游戏、手动停止或系统关机（`SIGTERM`）时自动拆毁隧道并暂停时长，杜绝忘关加速白扣时长。
- 🌐 **双引擎一键弹窗登录** — 同时兼容 Chromium 全系与 Firefox 全系浏览器（含 Flatpak 版）小窗登录，无需安装手机雷神 App 即可完成裸机硬件绑定与凭证持久自动续期。
- 🌍 **七大洲云端发现与并发测速** — 从雷神云端动态同步七大洲全部可用区服，启动瞬间多线程并发 TCP 握手测速（`250 ms` 上限，自动过滤软路由透明代理假延迟），按实测第一跳延迟择优接入。
- 🛡️ **软路由与局域网无感共存** — `LEIGOD_GUARD` 防火墙保护链自动放行局域网段、软路由 Fake-IP（`198.18.0.0/15`）、非游戏常用域名 DNS 及存量连接。
- 🔒 **严格沙盒与遥测屏蔽** — 基于 `systemd` 只读文件系统沙盒（`ProtectSystem=strict`）运行，默认拦截非必要的日志与遥测上报。

## 🎮 Steam 与命令行使用

在 Steam 游戏属性的**启动选项**中填入以下命令，即可启用单游戏进程专享加速并随游戏退出自动暂停时长：

```bash
lgd %command%
```

如需为特定游戏单独固定区服（如日本 `jp`、中国台湾 `tw`、韩国 `kr`），添加 `-a` 参数即可（支持与 `mangohud` 等工具串联）：

```bash
lgd -a jp %command%
```

| 命令 | 功能说明 |
| --- | --- |
| `lgd` | 自动拉起后台服务并打开原生 GTK4 桌面控制面板。 |
| `lgd [-a 区服] <命令...>` | 在独立 `cgroup v2` 隧道内专享加速运行程序，进程树全部退出后自动暂停官网时长。 |
| `lgd --login` | 弹出官方登录小窗口，登录成功后自动完成设备绑定与凭证保存。 |
| `lgd --list` | 按七大洲分类列出云端动态同步的全部可用区服及节点数。 |
| `lgd --start [区服]` | 免开界面恢复时长并启动全局海外加速（不传参数默认沿用上次区服）。 |
| `lgd --pause` | 仅断开加速隧道并暂停官网账号时长（保持后台守护服务运行）。 |
| `lgd --stop` | 暂停官网账号时长并停止后台 `systemd` 服务。 |
| `lgd --status` | 查看服务运行状态、当前隧道模式、节点延迟与账号剩余时长。 |
| `lgd --port <端口>` | 修改本地守护服务端口（写入 `/etc/default/leinux`）并重启服务。 |

## ⬇️ 安装

### Arch Linux（`[aslz]` 软件源）

按 [AsabaLazy/aur-ASLZ](https://github.com/AsabaLazy/aur-ASLZ) 说明在 `/etc/pacman.conf` 中添加 `[aslz]` 源后执行：

```bash
sudo pacman -Sy leinux-bin
```

### 通用 Linux（Debian / Ubuntu / Fedora / Arch / openSUSE）

```bash
git clone https://github.com/AsabaLazy/Leinux.git
cd Leinux
sudo ./install.sh
```

卸载：

```bash
sudo ./uninstall.sh
```

## 📄 许可证与声明

本仓库原创代码采用 [MIT License](LICENSE) 开源。雷神闭源运行组件（`acc-gw.router.amd64`）与 IP 库（`ipdatacloud_country.xdb`）在安装或构建时从雷神官方服务器下载并校验 SHA-256，其权利归各自所有者所有，详见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
