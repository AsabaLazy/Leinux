# [![](share/banner.svg)](https://github.com/AsabaLazy/Leinux)

[![Core Check](https://github.com/AsabaLazy/Leinux/actions/workflows/check-core.yml/badge.svg)](https://github.com/AsabaLazy/Leinux/actions/workflows/check-core.yml)
[![Version](https://img.shields.io/github/v/tag/AsabaLazy/Leinux?label=version)](https://github.com/AsabaLazy/Leinux/tags)
[![Arch Repo](https://img.shields.io/badge/Arch_Linux-aslz-1793d1?logo=arch-linux&logoColor=white)](https://github.com/AsabaLazy/aur-ASLZ)
[![License](https://img.shields.io/github/license/AsabaLazy/Leinux)](LICENSE)

[English](README.md) | [中文](README_zh-CN.md)

## 🚀 Introduction

Leinux is a lightweight, modern, native x86_64 Linux client for Leigod Accelerator built with Rust, GTK4, and libadwaita. It combines a native GNOME desktop interface, the `lgd` command-line tool, and a `cgroup v2` policy-routing daemon into a single ~1.3 MB binary.

Beyond one-click global acceleration, Leinux integrates directly with Steam Launch Options (`lgd %command%`) to isolate acceleration strictly to target game process trees, automatically resuming and pausing paid account time alongside game lifecycles.

## ✨ Features

- 🪶 **Lightweight native experience** — rendered natively with GTK4 and libadwaita 1.7. No Electron. No WebView shell. `0.0%` idle background CPU usage.
- 🎮 **Per-process Steam integration** — add `lgd %command%` to Steam Launch Options to route only the target game (including Proton and Wine child processes) via kernel `cgroup v2`, with zero anti-cheat injection risk.
- ⏱️ **Automatic time management** — resumes official Leigod account hours on game launch and automatically tears down the tunnel and pauses hours on game exit or system shutdown (`SIGTERM`).
- 🌐 **Dual-engine popup login** — one-click login popup compatible with both Chromium-based and Firefox-based browsers (including Flatpak editions), performing bare-metal device binding and automatic token renewal without the mobile app.
- 🌍 **Global region discovery & TCP probing** — dynamically syncs all available regions across seven continents from the Leigod cloud and concurrently probes nodes (`250 ms` TCP handshake, filtering TProxy false positives) for lowest first-hop latency.
- 🛡️ **Soft-router & LAN coexistence** — `LEIGOD_GUARD` netfilter rules automatically bypass LAN subnets, Fake-IP ranges (`198.18.0.0/15`), non-game DNS queries, and pre-existing connections.
- 🔒 **Sandboxed & telemetry-free** — runs under `systemd` strict filesystem protection (`ProtectSystem=strict`) with non-essential telemetry endpoints disabled.

## 🎮 Steam & CLI Usage

Add Leinux to any game's **Steam Launch Options** for automatic per-process acceleration and time pausing:

```bash
lgd %command%
```

To pin a specific game to a dedicated region (e.g., Japan `jp`, Taiwan `tw`, Korea `kr`), pass `-a`:

```bash
lgd -a jp %command%
```

| Command | Description |
| --- | --- |
| `lgd` | Start background services if needed and open the native GTK4 control window. |
| `lgd [-a AREA] <CMD...>` | Run a command inside an isolated `cgroup v2` tunnel and pause account hours on exit. |
| `lgd --login` | Open the official browser popup to sign in, bind hardware, and sync credentials. |
| `lgd --list` | List all cloud-synced regions and node counts grouped by continent. |
| `lgd --start [AREA]` | Resume account hours and start global overseas acceleration (defaults to last region). |
| `lgd --pause` | Disconnect the tunnel and pause official Leigod account hours. |
| `lgd --stop` | Pause account hours and stop background `systemd` services. |
| `lgd --status` | Show daemon status, active tunnel mode, node latency, and remaining account hours. |
| `lgd --port <PORT>` | Update the local daemon port in `/etc/default/leinux` and restart the service. |

## ⬇️ Installation

### Arch Linux (`[aslz]` repository)

Add the `[aslz]` repository from [AsabaLazy/aur-ASLZ](https://github.com/AsabaLazy/aur-ASLZ) to `/etc/pacman.conf`, then install:

```bash
sudo pacman -Sy leinux-bin
```

### Generic Linux (Debian / Ubuntu / Fedora / Arch / openSUSE)

```bash
git clone https://github.com/AsabaLazy/Leinux.git
cd Leinux
sudo ./install.sh
```

To uninstall:

```bash
sudo ./uninstall.sh
```

## 📄 License

Original source code in this repository is licensed under the [MIT License](LICENSE). Proprietary Leigod runtime binaries (`acc-gw.router.amd64`) and IP databases (`ipdatacloud_country.xdb`) are fetched from official servers during installation/packaging and verified via SHA-256; see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) for details.
