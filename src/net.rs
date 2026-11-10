use crate::types::LEIGOD_DIR;
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::Write;
use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const BYPASS_DNS_KEYWORDS: [&str; 9] = [
    "google", "gstatic", "youtube", "1e100", "ggpht", "anthropic", "claude", "openai", "github",
];

static STOP_ENFORCED: AtomicBool = AtomicBool::new(false);

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
}

pub fn is_stop_enforced() -> bool {
    STOP_ENFORCED.load(Ordering::SeqCst)
}

pub fn state_dir() -> PathBuf {
    PathBuf::from(env::var("LEIGOD_STATE_DIR").unwrap_or_else(|_| "/var/lib/leigod".to_string()))
}

pub fn read_device_mac() -> String {
    for p in [PathBuf::from("/sys/class/net/wlan0/address"), state_dir().join("device-mac")] {
        if let Ok(s) = fs::read_to_string(p) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    String::new()
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
        for line in text.lines().skip(1) {
            let Some(local_addr) = line.split_whitespace().nth(1) else { continue };
            if local_addr
                .split(':')
                .nth(1)
                .map(|port| port.eq_ignore_ascii_case("17b2"))
                .unwrap_or(false)
            {
                return "监听正常 (6066/udp)";
            }
        }
    }
    "未检测到 6066/udp 监听"
}

pub fn find_plugin_pid() -> Option<u32> {
    let entries = fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else { continue };
        let Ok(pid) = name.parse::<u32>() else { continue };
        let Ok(cmd) = fs::read(entry.path().join("cmdline")) else { continue };
        if cmd.windows(19).any(|w| w == b"acc-gw.router.amd64") {
            return Some(pid);
        }
    }
    None
}

pub fn check_plugin_service() -> (bool, String) {
    if find_plugin_pid().is_some() {
        return (true, "active".to_string());
    }
    match Command::new("systemctl")
        .args(["is-active", "leigod_plugin.service"])
        .output()
    {
        Ok(out) => {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (out.status.success(), if s.is_empty() { "inactive".to_string() } else { s })
        }
        Err(_) => (false, "unknown".to_string()),
    }
}

pub fn read_ini_values() -> (String, String, String) {
    let mut version = String::new();
    let mut token = String::new();
    let mut user_id = String::new();

    if let Ok(text) = fs::read_to_string(Path::new(LEIGOD_DIR).join("config/acc_version.ini")) {
        for (k, v) in text.lines().filter_map(|l| l.trim().split_once('=')) {
            if k.trim().eq_ignore_ascii_case("version") {
                version = v.trim().to_string();
            }
        }
    }
    if let Ok(text) = fs::read_to_string(Path::new(LEIGOD_DIR).join("config/accelerator.ini")) {
        for (k, v) in text.lines().filter_map(|l| l.trim().split_once('=')) {
            let val = v.trim();
            if val.is_empty() {
                continue;
            }
            match k.trim().to_ascii_lowercase().as_str() {
                "token" | "bind_token" | "rtoken" => token = val.to_string(),
                "user_id" | "userid" => user_id = val.to_string(),
                _ => {}
            }
        }
    }
    (version, token, user_id)
}

pub fn flush_dns_cache() {
    let _ = Command::new("resolvectl").arg("flush-caches").output();
}

pub fn probe_dns_real_ip(server_ip: &str) -> bool {
    let Ok(addr) = format!("{server_ip}:53").parse::<SocketAddr>() else {
        return false;
    };
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else {
        return false;
    };
    let _ = sock.set_read_timeout(Some(Duration::from_millis(400)));
    let _ = sock.set_write_timeout(Some(Duration::from_millis(400)));
    let query: &[u8] = b"\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\
\x03api\x0csteampowered\x03com\x00\x00\x01\x00\x01";
    if sock.send_to(query, addr).is_err() {
        return false;
    }
    let mut buf = [0u8; 512];
    let Ok((n, _)) = sock.recv_from(&mut buf) else {
        return false;
    };
    if n < 16 || (buf[3] & 0x0f) != 0 {
        return false;
    }
    let ip = &buf[n - 4..n];
    !(ip[0] == 198 && (ip[1] == 18 || ip[1] == 19)) && ip[0] != 0 && ip[0] != 127
}

fn find_ipv6_router_ipv4() -> Option<String> {
    let v6_out = Command::new("ip").args(["-6", "neigh", "show"]).output().ok()?;
    let mut router_macs = BTreeSet::new();
    for line in String::from_utf8_lossy(&v6_out.stdout).lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if !parts.contains(&"router") {
            continue;
        }
        if let Some(mac) = parts.iter().position(|&x| x == "lladdr").and_then(|i| parts.get(i + 1)) {
            router_macs.insert(mac.to_ascii_lowercase());
        }
    }
    if router_macs.is_empty() {
        return None;
    }
    let v4_out = Command::new("ip").args(["-4", "neigh", "show"]).output().ok()?;
    for line in String::from_utf8_lossy(&v4_out.stdout).lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        let Some(mac) = parts.iter().position(|&x| x == "lladdr").and_then(|i| parts.get(i + 1)) else {
            continue;
        };
        if router_macs.contains(&mac.to_ascii_lowercase()) {
            return parts.first().map(|ip| (*ip).to_string());
        }
    }
    None
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
        if guard.ts.map(|t| t.elapsed() < Duration::from_secs(60)).unwrap_or(false) && !guard.ip.is_empty() {
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
            for suffix in ["1", "254"] {
                let cand = format!("{prefix}.{suffix}");
                if cand != default_gw && probe_dns_real_ip(&cand) {
                    chosen = cand;
                    break;
                }
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
    let mut dns_list = Vec::new();
    if !custom.is_empty() {
        for item in custom
            .split(|c: char| c == ',' || c == ';' || c.is_whitespace())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            dns_list.push(if item.contains(':') { item.to_string() } else { format!("{item}:53") });
        }
    } else {
        let gw = detect_gateway_ip();
        if !gw.is_empty() {
            dns_list.push(format!("{gw}:53"));
        }
    }
    if dns_list.is_empty() {
        dns_list.push("223.5.5.5:53".to_string());
        dns_list.push("114.114.114.114:53".to_string());
    }
    dns_list
}

fn run_ipt(args: &[&str]) -> bool {
    Command::new("iptables")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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
        .map(|s| s.success())
        .unwrap_or(false);

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
        }
        return false;
    }
    if has_rules {
        if !s.lines().nth(1).map(|l| l.contains("LEIGOD_GUARD")).unwrap_or(false) {
            install_mangle_guard();
        }
        return true;
    }
    if s.contains("-A OUTPUT -j LEIGOD_GUARD") {
        remove_mangle_guard();
    }
    false
}
