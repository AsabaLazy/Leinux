# Agent Instructions

Native Rust + GTK4 + libadwaita client, `lgd` CLI, and routing daemon (`--daemon`) for Leigod Accelerator on x86_64 Linux.

## Rules & Invariants

1. **Tracked Prebuilt Binary**: `bin/leinux` is tracked in Git and packaged directly by downstream `aur-ASLZ` (`leinux-bin`). After any `src/` or `Cargo.toml` change, always run `cargo clippy --all-targets -- -D warnings && cargo test && cargo build --release` and copy `target/release/leinux` to `bin/leinux`.
2. **Decoupled Versioning**: Client changes increment `Cargo.toml` `version` (`+0.0.01` base-10 while preserving SemVer/`pacman` integer order: e.g. `0.2.29 -> 0.2.30`, never `0.2.3` or leading-zero `0.3.01`). Never modify `opt/leigod/config/acc_version.ini` (`1.3.0.02`) or `BIN_SHA256`/`XDB_SHA256` in `install.sh` unless explicitly verifying an upstream Leigod core update.
3. **Core, Cgroup & Teardown**: Never implement client-side plain-HTTP self-updates for `acc-gw.router.amd64` (monitored via `.github/workflows/check-core.yml`). On `STOPACC` (`src/rpc.rs`), always let `acc-gw.router.amd64` tear down `tun_PC` before `leigod_cleanup_net.sh` to prevent `SIGSEGV`. Keep `CAP_SYS_NICE` + `/sys/fs/cgroup` writable in `systemd/leinux.service` for cross-UID `cgroup.procs` migration, and never `rmdir /sys/fs/cgroup/leinux-games` (preserves kernel `xt_cgroup` inode).
4. **Pure libadwaita HIG**: Keep `src/gui.rs` strictly on native GTK4 + libadwaita 1.7 widgets and semantic classes (`AdwToastOverlay`, `AdwToggleGroup`, `AdwPreferencesDialog`, `AdwAboutDialog`, `AdwSpinner`). Do not introduce custom `CssProvider` rules or toast notifications on routine start/stop actions.
5. **Account Time Safety**: Whenever live-testing acceleration (`--start`) on a machine with a bound Leigod account, immediately execute `leinux --pause` afterward and verify `已暂停` via `leinux --status` so paid hours are never left draining.
