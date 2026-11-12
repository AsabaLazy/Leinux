use crate::types::{IniValues, LEIGOD_DIR};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::Write;
use std::net::{SocketAddr, UdpSocket};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const BYPASS_DNS_KEYWORDS: [&str; 9] = [
    "google", "gstatic", "youtube", "1e100", "ggpht", "anthropic", "claude", "openai", "github",
];

const GAME_CGROUP_NAME: &str = "leinux-games";
const GAME_CGROUP_DIR: &str = "/sys/fs/cgroup/leinux-games";

static STOP_ENFORCED: AtomicBool = AtomicBool::new(false);
static SCOPED_MODE: AtomicBool = AtomicBool::new(false);
static FW_VERIFIED_CLEAN: AtomicBool = AtomicBool::new(false);
static CACHED_PLUGIN_PID: AtomicU32 = AtomicU32::new(0);

struct GatewayCache {
    ts: Option<Instant>,
    ip: String,
}

fn gw_cache() -> &'static Mutex<GatewayCache> {
    static GW: OnceLock<Mutex<GatewayCache>> = OnceLock::new();
    GW.get_or_init(|| Mutex::new(GatewayCache { ts: None, ip: String::new() }))
}

pub fn set_stop_enforced(enforced: bool) {
    STOP_ENFORCED.store(enforced, Ordering::SeqCst);
    FW_VERIFIED_CLEAN.store(false, Ordering::SeqCst);
}

pub fn is_stop_enforced() -> bool {
    STOP_ENFORCED.load(Ordering::SeqCst)
}

pub fn set_scoped_mode(scoped: bool) {
    SCOPED_MODE.store(scoped, Ordering::SeqCst);
    FW_VERIFIED_CLEAN.store(false, Ordering::SeqCst);
    if !scoped {
        if let Ok(mut roots) = scoped_roots().lock() {
            roots.clear();
        }
    }
}

pub fn is_scoped_mode() -> bool {
    SCOPED_MODE.load(Ordering::SeqCst)
}

fn scoped_roots() -> &'static Mutex<BTreeSet<u32>> {
    static ROOTS: OnceLock<Mutex<BTreeSet<u32>>> = OnceLock::new();
    ROOTS.get_or_init(|| Mutex::new(BTreeSet::new()))
}

pub fn unregister_scoped_root_pid(pid: u32) {
    if let Ok(mut roots) = scoped_roots().lock() {
        roots.remove(&pid);
    }
}

pub fn attach_pid_to_game_cgroup(pid: u32) -> Result<(), String> {
    if !Path::new(&format!("/proc/{pid}")).exists() {
        return Err(format!("目标进程 {pid} 不存在"));
    }
    fs::create_dir_all(GAME_CGROUP_DIR)
        .map_err(|e| format!("无法创建游戏 cgroup ({GAME_CGROUP_DIR}): {e}"))?;
    fs::write(Path::new(GAME_CGROUP_DIR).join("cgroup.procs"), format!("{pid}\n"))
        .map_err(|e| format!("无法将进程 {pid} 移入游戏 cgroup: {e}"))?;
    if let Ok(mut roots) = scoped_roots().lock() {
        roots.retain(|&p| Path::new(&format!("/proc/{p}")).exists());
        roots.insert(pid);
    }
    set_scoped_mode(true);
    Ok(())
}

fn sync_descendants_to_game_cgroup() {
    let cg_procs_path = Path::new(GAME_CGROUP_DIR).join("cgroup.procs");
    let in_cg: BTreeSet<u32> = fs::read_to_string(&cg_procs_path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .collect();

    let mut seeds = in_cg.clone();
    if let Ok(mut roots) = scoped_roots().lock() {
        roots.retain(|&p| Path::new(&format!("/proc/{p}")).exists());
        seeds.extend(roots.iter().copied());
    }
    if seeds.is_empty() {
        return;
    }

    let Ok(entries) = fs::read_dir("/proc") else { return };
    let mut children_map: std::collections::HashMap<u32, Vec<u32>> = std::collections::HashMap::new();
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else { continue };
        let Some((_, after_comm)) = stat.rsplit_once(')') else { continue };
        let Some(ppid) = after_comm
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if ppid > 1 {
            children_map.entry(ppid).or_default().push(pid);
        }
    }

    let mut visited = seeds.clone();
    let mut stack: Vec<u32> = seeds.into_iter().collect();
    while let Some(cur) = stack.pop() {
        if let Some(kids) = children_map.get(&cur) {
            for &k in kids {
                if visited.insert(k) {
                    stack.push(k);
                }
            }
        }
    }

    for pid in visited {
        if !in_cg.contains(&pid) {
            let _ = fs::write(&cg_procs_path, format!("{pid}\n"));
        }
    }
}

pub fn count_game_cgroup_procs(exclude_pid: Option<u32>) -> usize {
    if is_scoped_mode() {
        sync_descendants_to_game_cgroup();
    }
    let Ok(text) = fs::read_to_string(Path::new(GAME_CGROUP_DIR).join("cgroup.procs")) else {
        return 0;
    };
    text.lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .filter(|&p| Some(p) != exclude_pid && Path::new(&format!("/proc/{p}")).exists())
        .count()
}

pub fn state_dir() -> PathBuf {
    PathBuf::from(env::var("LEIGOD_STATE_DIR").unwrap_or_else(|_| "/var/lib/leigod".to_string()))
}

pub fn read_json_or_default<T: DeserializeOwned + Default>(path: &Path) -> T {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn write_json_atomic<T: Serialize>(path: &Path, val: &T, pretty: bool, mode: Option<u32>) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let serialized = if pretty {
        serde_json::to_string_pretty(val)
    } else {
        serde_json::to_string(val)
    };
    let Ok(body) = serialized else { return };
    let tmp = path.with_extension("tmp");
    if fs::write(&tmp, body).is_ok() {
        if let Some(m) = mode {
            let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(m));
        }
        let _ = fs::rename(&tmp, path);
    }
}

pub fn read_device_mac() -> String {
    [PathBuf::from("/sys/class/net/wlan0/address"), state_dir().join("device-mac")]
        .into_iter()
        .find_map(|p| {
            let s = fs::read_to_string(p).ok()?;
            let trimmed = s.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
        .unwrap_or_default()
}

pub fn list_tun_interfaces() -> Vec<String> {
    let Ok(entries) = fs::read_dir("/sys/class/net") else {
        return Vec::new();
    };
    let mut tuns: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| name.starts_with("tun_"))
        .collect();
    tuns.sort_by(|a, b| (a != "tun_PC").cmp(&(b != "tun_PC")).then_with(|| a.cmp(b)));
    tuns
}

pub fn has_active_tun() -> bool {
    Path::new("/sys/class/net/tun_PC").exists() || !list_tun_interfaces().is_empty()
}

pub fn read_iface_bytes(iface: &str) -> (u64, u64) {
    let read_stat = |name: &str| -> u64 {
        fs::read_to_string(format!("/sys/class/net/{iface}/statistics/{name}"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    };
    (read_stat("rx_bytes"), read_stat("tx_bytes"))
}

pub fn probe_udp_6066() -> &'static str {
    for path in ["/proc/net/udp", "/proc/net/udp6"] {
        let Ok(text) = fs::read_to_string(path) else { continue };
        let listening = text.lines().skip(1).any(|line| {
            line.split_whitespace()
                .nth(1)
                .and_then(|addr| addr.split(':').nth(1))
                .is_some_and(|port| port.eq_ignore_ascii_case("17b2"))
        });
        if listening {
            return "监听正常 (6066/udp)";
        }
    }
    "未检测到 6066/udp 监听"
}

fn is_plugin_pid(pid: u32) -> bool {
    pid > 0
        && fs::read(format!("/proc/{pid}/cmdline"))
            .is_ok_and(|cmd| cmd.windows(19).any(|w| w == b"acc-gw.router.amd64"))
}

pub fn find_plugin_pid() -> Option<u32> {
    let cached = CACHED_PLUGIN_PID.load(Ordering::Relaxed);
    if is_plugin_pid(cached) {
        return Some(cached);
    }
    let found = fs::read_dir("/proc").ok()?.flatten().find_map(|entry| {
        let pid = entry.file_name().into_string().ok()?.parse::<u32>().ok()?;
        is_plugin_pid(pid).then_some(pid)
    })?;
    CACHED_PLUGIN_PID.store(found, Ordering::Relaxed);
    Some(found)
}

pub fn check_plugin_service() -> (bool, String) {
    if find_plugin_pid().is_some() {
        return (true, "active".to_string());
    }
    match Command::new("systemctl")
        .args(["is-active", crate::CORE_SERVICE])
        .output()
    {
        Ok(out) => {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let ok = out.status.success() || s == "activating";
            (ok, if s.is_empty() { "inactive".to_string() } else { s })
        }
        Err(_) => (false, "unknown".to_string()),
    }
}

pub fn read_ini_values() -> IniValues {
    let mut ini = IniValues::default();
    let base = Path::new(LEIGOD_DIR);

    if let Ok(text) = fs::read_to_string(base.join("config/acc_version.ini")) {
        for (k, v) in text.lines().filter_map(|l| l.trim().split_once('=')) {
            if k.trim().eq_ignore_ascii_case("version") {
                ini.version = v.trim().to_string();
            }
        }
    }
    if let Ok(text) = fs::read_to_string(base.join("config/accelerator.ini")) {
        for (k, v) in text.lines().filter_map(|l| l.trim().split_once('=')) {
            let val = v.trim();
            if val.is_empty() {
                continue;
            }
            match k.trim().to_ascii_lowercase().as_str() {
                "token" | "bind_token" | "rtoken" => ini.rtoken = val.to_string(),
                "user_id" | "userid" => ini.user_id = val.to_string(),
                _ => {}
            }
        }
    }
    ini
}

pub fn flush_dns_cache() {
    let _ = Command::new("resolvectl").arg("flush-caches").output();
}

fn probe_dns_real_ip(server_ip: &str) -> bool {
    let Ok(addr) = format!("{server_ip}:53").parse::<SocketAddr>() else { return false };
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else { return false };
    let _ = sock.set_read_timeout(Some(Duration::from_millis(400)));
    let _ = sock.set_write_timeout(Some(Duration::from_millis(400)));
    let query: &[u8] = b"\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\
\x03api\x0csteampowered\x03com\x00\x00\x01\x00\x01";
    if sock.send_to(query, addr).is_err() {
        return false;
    }
    let mut buf = [0u8; 512];
    let Ok((n, _)) = sock.recv_from(&mut buf) else { return false };
    if n < 16 || (buf[3] & 0x0f) != 0 {
        return false;
    }
    let ip = &buf[n - 4..n];
    !(ip[0] == 198 && (ip[1] == 18 || ip[1] == 19)) && ip[0] != 0 && ip[0] != 127
}

fn lladdr_from_neigh_line<'a>(parts: &[&'a str]) -> Option<&'a str> {
    let idx = parts.iter().position(|&x| x == "lladdr")?;
    parts.get(idx + 1).copied()
}

fn find_ipv6_router_ipv4() -> Option<String> {
    let v6_out = Command::new("ip").args(["-6", "neigh", "show"]).output().ok()?;
    let router_macs: BTreeSet<String> = String::from_utf8_lossy(&v6_out.stdout)
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            parts.contains(&"router").then(|| lladdr_from_neigh_line(&parts))?.map(str::to_ascii_lowercase)
        })
        .collect();
    if router_macs.is_empty() {
        return None;
    }

    let v4_out = Command::new("ip").args(["-4", "neigh", "show"]).output().ok()?;
    String::from_utf8_lossy(&v4_out.stdout).lines().find_map(|line| {
        let parts: Vec<&str> = line.split_whitespace().collect();
        let mac = lladdr_from_neigh_line(&parts)?.to_ascii_lowercase();
        router_macs.contains(&mac).then_some((*parts.first()?).to_string())
    })
}

fn read_default_route_gw() -> String {
    let Ok(out) = Command::new("ip").args(["route", "show", "default"]).output() else {
        return String::new();
    };
    let s = String::from_utf8_lossy(&out.stdout);
    let parts: Vec<&str> = s.split_whitespace().collect();
    parts
        .iter()
        .position(|&x| x == "via")
        .and_then(|i| parts.get(i + 1))
        .map(|ip| (*ip).to_string())
        .unwrap_or_default()
}

pub fn detect_gateway_ip() -> String {
    if let Ok(guard) = gw_cache().lock() {
        if guard.ts.is_some_and(|t| t.elapsed() < Duration::from_secs(60)) && !guard.ip.is_empty() {
            return guard.ip.clone();
        }
    }

    let default_gw = read_default_route_gw();
    let mut chosen = default_gw.clone();

    if let Some(v4_router) = find_ipv6_router_ipv4() {
        if v4_router != default_gw && probe_dns_real_ip(&v4_router) {
            chosen = v4_router;
        }
    }
    if chosen == default_gw && !default_gw.is_empty() && !probe_dns_real_ip(&default_gw) {
        if let Some((prefix, _)) = default_gw.rsplit_once('.') {
            if let Some(cand) = ["1", "254"]
                .into_iter()
                .map(|sfx| format!("{prefix}.{sfx}"))
                .find(|cand| cand != &default_gw && probe_dns_real_ip(cand))
            {
                chosen = cand;
            }
        }
    }

    if let Ok(mut guard) = gw_cache().lock() {
        guard.ts = Some(Instant::now());
        if !chosen.is_empty() {
            guard.ip = chosen.clone();
        }
        return guard.ip.clone();
    }
    chosen
}

pub fn resolve_local_dns_list(custom_dns: &str) -> Vec<String> {
    let custom = custom_dns.trim();
    let mut dns_list: Vec<String> = if !custom.is_empty() {
        custom
            .split(|c: char| c == ',' || c == ';' || c.is_whitespace())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| if s.contains(':') { s.to_string() } else { format!("{s}:53") })
            .collect()
    } else {
        let gw = detect_gateway_ip();
        if gw.is_empty() { Vec::new() } else { vec![format!("{gw}:53")] }
    };
    if dns_list.is_empty() {
        dns_list.extend(["223.5.5.5:53".to_string(), "114.114.114.114:53".to_string()]);
    }
    dns_list
}

fn run_ipt(args: &[&str]) -> bool {
    Command::new("iptables")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub fn install_mangle_guard() {
    let mut batch = String::from(
        "*mangle\n\
         :LEIGOD_GUARD - [0:0]\n\
         -A LEIGOD_GUARD -o lo -j ACCEPT\n\
         -A LEIGOD_GUARD -m owner --uid-owner 0 -j ACCEPT\n\
         -A LEIGOD_GUARD -d 10.0.0.0/8 -p tcp -j ACCEPT\n\
         -A LEIGOD_GUARD -d 10.0.0.0/8 -p udp ! --dport 53 -j ACCEPT\n\
         -A LEIGOD_GUARD -d 172.16.0.0/12 -p tcp -j ACCEPT\n\
         -A LEIGOD_GUARD -d 172.16.0.0/12 -p udp ! --dport 53 -j ACCEPT\n\
         -A LEIGOD_GUARD -d 192.168.0.0/16 -p tcp -j ACCEPT\n\
         -A LEIGOD_GUARD -d 192.168.0.0/16 -p udp ! --dport 53 -j ACCEPT\n\
         -A LEIGOD_GUARD -d 198.18.0.0/15 -j ACCEPT\n",
    );
    for kw in BYPASS_DNS_KEYWORDS {
        batch.push_str(&format!(
            "-A LEIGOD_GUARD -p udp --dport 53 -m string --algo bm --icase --string {kw} -j ACCEPT\n"
        ));
    }
    if is_scoped_mode() && fs::create_dir_all(GAME_CGROUP_DIR).is_ok() {
        batch.push_str(&format!(
            "-A LEIGOD_GUARD -p udp --dport 53 -j RETURN\n\
             -A LEIGOD_GUARD -m cgroup ! --path {GAME_CGROUP_NAME} -j ACCEPT\n"
        ));
    }
    batch.push_str(
        "-A LEIGOD_GUARD -p tcp -m conntrack --ctstate ESTABLISHED,RELATED -m connmark ! --mark 0x102 -j ACCEPT\n\
         -A LEIGOD_GUARD -p tcp --syn -m conntrack --ctstate NEW -j CONNMARK --set-mark 0x102\n\
         COMMIT\n",
    );

    let restored = Command::new("iptables-restore")
        .arg("--noflush")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .and_then(|mut child| {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(batch.as_bytes());
            }
            child.wait()
        })
        .is_ok_and(|s| s.success());

    if !restored {
        let _ = run_ipt(&["-t", "mangle", "-N", "LEIGOD_GUARD"]);
        let _ = run_ipt(&["-t", "mangle", "-F", "LEIGOD_GUARD"]);
    }

    while run_ipt(&["-t", "mangle", "-D", "OUTPUT", "-j", "LEIGOD_GUARD"]) {}
    let _ = run_ipt(&["-t", "mangle", "-I", "OUTPUT", "1", "-j", "LEIGOD_GUARD"]);
}

pub fn remove_mangle_guard() {
    while run_ipt(&["-t", "mangle", "-D", "OUTPUT", "-j", "LEIGOD_GUARD"]) {}
    let _ = run_ipt(&["-t", "mangle", "-F", "LEIGOD_GUARD"]);
    let _ = run_ipt(&["-t", "mangle", "-X", "LEIGOD_GUARD"]);
}

pub fn run_cleanup_net() {
    let _ = Command::new("/bin/sh")
        .arg(Path::new(LEIGOD_DIR).join("leigod_cleanup_net.sh"))
        .status();
}

pub fn check_firewall_residuals() -> bool {
    let has_tun = has_active_tun();
    let Ok(out) = Command::new("iptables").args(["-t", "mangle", "-S", "OUTPUT"]).output() else {
        return false;
    };
    let s = String::from_utf8_lossy(&out.stdout);
    let has_rules = s.contains("0x10") || s.contains("GAMEACC");

    if is_stop_enforced() {
        if has_rules || has_tun {
            run_cleanup_net();
            flush_dns_cache();
            FW_VERIFIED_CLEAN.store(false, Ordering::SeqCst);
        } else {
            FW_VERIFIED_CLEAN.store(true, Ordering::SeqCst);
        }
        return false;
    }
    if has_rules {
        FW_VERIFIED_CLEAN.store(false, Ordering::SeqCst);
        if !s.lines().nth(1).is_some_and(|l| l.contains("LEIGOD_GUARD")) {
            install_mangle_guard();
        }
        return true;
    }
    if s.contains("-A OUTPUT -j LEIGOD_GUARD") {
        remove_mangle_guard();
    }
    FW_VERIFIED_CLEAN.store(!has_tun, Ordering::SeqCst);
    false
}

pub fn check_firewall_periodic(force: bool) {
    if force || has_active_tun() || !FW_VERIFIED_CLEAN.load(Ordering::SeqCst) {
        let _ = check_firewall_residuals();
    }
}
