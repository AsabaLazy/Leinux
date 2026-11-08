use crate::load_host_port;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LEIGOD_DIR: &str = "/opt/leigod";
const WEBAPI_BASE: &str = "https://webapi.leigod.com";
const FALLBACK_SIGN: &str = "6aedef9ce4b01aa456e120f2";

const PRESET_AREAS: [&str; 10] = ["auto", "hk", "jp", "kr", "tw", "sea", "us", "eu", "ru", "au"];

const BYPASS_DNS_KEYWORDS: [&str; 9] = [
    "google", "gstatic", "youtube", "1e100", "ggpht", "anthropic", "claude", "openai", "github",
];

const BLOCKED_ROUTER_CMDS: [&str; 15] = [
    "OPTROUTER",
    "OPT",
    "NEWREBOOTDEVICE",
    "NEWRESETDEVICE",
    "SHELLEXEC",
    "SHELL",
    "UNINSTALLENGINE",
    "NEWUPDATEFW",
    "NEWUPDATEFWVER",
    "NEWMODIFYUBUSSETTINGS",
    "NEWMODIFYNETWORKSETTINGS",
    "NEWMODIFYDHCPSETTINGS",
    "NEWMODIFYWIFISETTINGS",
    "MODIFYROUTER",
    "MODIFY",
];

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

struct ScanCache {
    gw_ts: Option<Instant>,
    gw_ip: String,
    last_sig: Vec<(u64, u64)>,
    profiles: Vec<Value>,
}

fn cache() -> &'static Mutex<ScanCache> {
    static CACHE: OnceLock<Mutex<ScanCache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(ScanCache {
            gw_ts: None,
            gw_ip: String::new(),
            last_sig: Vec::new(),
            profiles: Vec::new(),
        })
    })
}

fn state_dir() -> PathBuf {
    PathBuf::from(env::var("LEIGOD_STATE_DIR").unwrap_or_else(|_| "/var/lib/leigod".to_string()))
}

fn profiles_file() -> PathBuf {
    state_dir().join("saved_profiles.json")
}

fn account_file() -> PathBuf {
    state_dir().join("account.json")
}

pub fn format_region_title(area: &str) -> String {
    match area.to_lowercase().as_str() {
        "auto" | "global" => "全球智能分流".to_string(),
        "hk" => "中国香港".to_string(),
        "jp" => "日本东京".to_string(),
        "kr" => "韩国首尔".to_string(),
        "tw" => "中国台湾台北".to_string(),
        "sea" => "东南亚新加坡".to_string(),
        "us" => "美国洛杉矶".to_string(),
        "eu" | "de" => "德国法兰克福".to_string(),
        "ru" => "俄罗斯莫斯科".to_string(),
        "au" => "澳大利亚悉尼".to_string(),
        other => other.to_uppercase(),
    }
}

fn geo_sources(area: &str) -> &'static [&'static str] {
    match area {
        "hk" => &["hk"],
        "jp" => &["jp"],
        "kr" => &["kr"],
        "tw" => &["tw"],
        "sea" => &["sea", "MY", "VN"],
        "us" => &["us"],
        "eu" => &["eu", "de"],
        "ru" => &["ru"],
        "au" => &["au"],
        _ => &[],
    }
}

fn now_timestamp_str() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

pub fn ws_rpc_call(host: &str, port: u16, path: &str, payload: &Value, timeout_ms: u64) -> Result<Value, String> {
    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .map_err(|e| format!("Invalid address: {e}"))?;
    let timeout = Duration::from_millis(timeout_ms);
    let mut sock = TcpStream::connect_timeout(&addr, timeout).map_err(|e| e.to_string())?;
    let _ = sock.set_read_timeout(Some(timeout));
    let _ = sock.set_write_timeout(Some(timeout));

    let handshake = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host}:{port}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\r\n"
    );
    sock.write_all(handshake.as_bytes()).map_err(|e| e.to_string())?;

    let mut header_buf = Vec::new();
    let mut byte = [0u8; 1];
    while !header_buf.ends_with(b"\r\n\r\n") {
        let n = sock.read(&mut byte).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("WebSocket handshake closed unexpectedly".to_string());
        }
        header_buf.push(byte[0]);
    }

    let status_line = String::from_utf8_lossy(&header_buf);
    if !status_line.lines().next().unwrap_or("").contains("101") {
        return Err(format!("WebSocket handshake rejected: {status_line}"));
    }

    let body = payload.to_string().into_bytes();
    let mask_key: [u8; 4] = [0x37, 0xfa, 0x21, 0x3d];
    let mut frame = vec![0x81u8];
    let len = body.len();
    if len < 126 {
        frame.push(0x80 | (len as u8));
    } else if len < 65536 {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        frame.push(0x80 | 127);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }
    frame.extend_from_slice(&mask_key);
    for (i, b) in body.iter().enumerate() {
        frame.push(b ^ mask_key[i % 4]);
    }
    sock.write_all(&frame).map_err(|e| e.to_string())?;

    let mut first_two = [0u8; 2];
    sock.read_exact(&mut first_two).map_err(|e| e.to_string())?;
    let mut r_len = (first_two[1] & 0x7f) as usize;
    if r_len == 126 {
        let mut b = [0u8; 2];
        sock.read_exact(&mut b).map_err(|e| e.to_string())?;
        r_len = u16::from_be_bytes(b) as usize;
    } else if r_len == 127 {
        let mut b = [0u8; 8];
        sock.read_exact(&mut b).map_err(|e| e.to_string())?;
        r_len = u64::from_be_bytes(b) as usize;
    }
    let masked = (first_two[1] & 0x80) != 0;
    let mut srv_mask = [0u8; 4];
    if masked {
        sock.read_exact(&mut srv_mask).map_err(|e| e.to_string())?;
    }
    let mut data = vec![0u8; r_len];
    sock.read_exact(&mut data).map_err(|e| e.to_string())?;
    if masked {
        for (i, b) in data.iter_mut().enumerate() {
            *b ^= srv_mask[i % 4];
        }
    }
    serde_json::from_slice(&data).map_err(|e| e.to_string())
}

pub fn webapi_post(endpoint: &str, account_token: &str) -> Result<Value, String> {
    let url = format!("{WEBAPI_BASE}{endpoint}");
    let body = json!({
        "account_token": account_token,
        "lang": "zh_CN",
        "os_type": 4,
        "region_code": 1,
    })
    .to_string();

    let out = Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            "6",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-H",
            "Origin: https://www.leigod.com",
            "-H",
            "Referer: https://www.leigod.com/",
            "-A",
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
            "-d",
            &body,
            &url,
        ])
        .output()
        .map_err(|e| e.to_string())?;

    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())
}

fn load_saved_profiles() -> Vec<Value> {
    let p = profiles_file();
    if let Ok(text) = fs::read_to_string(p) {
        if let Ok(Value::Array(arr)) = serde_json::from_str(&text) {
            return arr;
        }
    }
    Vec::new()
}

fn save_profiles(profiles: &[Value]) {
    let dir = state_dir();
    let _ = fs::create_dir_all(&dir);
    let p = profiles_file();
    let tmp = p.with_extension("tmp");
    if let Ok(s) = serde_json::to_string_pretty(profiles) {
        if fs::write(&tmp, s).is_ok() {
            let _ = fs::rename(&tmp, &p);
        }
    }
    if let Ok(mut guard) = cache().lock() {
        guard.profiles = profiles.to_vec();
        guard.last_sig.clear();
    }
}

fn load_account_config() -> Value {
    let p = account_file();
    if let Ok(text) = fs::read_to_string(p) {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            if v.is_object() {
                return v;
            }
        }
    }
    json!({"account_token": "", "auto_pause_on_stop": true, "custom_dns": ""})
}

fn save_account_config(cfg: &Value) {
    let dir = state_dir();
    let _ = fs::create_dir_all(&dir);
    let p = account_file();
    let tmp = p.with_extension("tmp");
    if let Ok(s) = serde_json::to_string_pretty(cfg) {
        if fs::write(&tmp, s).is_ok() {
            let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
            let _ = fs::rename(&tmp, &p);
        }
    }
    if let Ok(mut guard) = cache().lock() {
        guard.last_sig.clear();
    }
}

fn flush_dns_cache() {
    let _ = Command::new("resolvectl").arg("flush-caches").output();
}

fn detect_gateway_ip() -> String {
    if let Ok(guard) = cache().lock() {
        if let Some(ts) = guard.gw_ts {
            if ts.elapsed() < Duration::from_secs(15) && !guard.gw_ip.is_empty() {
                return guard.gw_ip.clone();
            }
        }
    }
    let mut gw = String::new();
    if let Ok(out) = Command::new("ip").args(["route", "show", "default"]).output() {
        let s = String::from_utf8_lossy(&out.stdout);
        let parts: Vec<&str> = s.split_whitespace().collect();
        if let Some(idx) = parts.iter().position(|&x| x == "via") {
            if let Some(ip) = parts.get(idx + 1) {
                gw = (*ip).to_string();
            }
        }
    }
    if let Ok(mut guard) = cache().lock() {
        guard.gw_ts = Some(Instant::now());
        if !gw.is_empty() {
            guard.gw_ip = gw.clone();
        }
        return guard.gw_ip.clone();
    }
    gw
}

fn resolve_local_dns_list() -> Vec<String> {
    let cfg = load_account_config();
    let custom = cfg["custom_dns"].as_str().unwrap_or("").trim();
    let mut dns_list = Vec::new();
    if !custom.is_empty() {
        for part in custom.split(|c: char| c == ',' || c == ';' || c.is_whitespace()) {
            let item = part.trim();
            if item.is_empty() {
                continue;
            }
            if item.contains(':') {
                dns_list.push(item.to_string());
            } else {
                dns_list.push(format!("{item}:53"));
            }
        }
    } else {
        let gw = detect_gateway_ip();
        if !gw.is_empty() {
            dns_list.push(format!("{gw}:53"));
        }
    }
    for fb in ["223.5.5.5:53", "114.114.114.114:53"] {
        if !dns_list.iter().any(|x| x == fb) {
            dns_list.push(fb.to_string());
        }
    }
    dns_list
}

fn run_ipt(args: &[&str]) -> bool {
    Command::new("iptables")
        .args(args)
        .stdout( std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn install_mangle_guard() {
    let _ = run_ipt(&["-t", "mangle", "-N", "LEIGOD_GUARD"]);
    let _ = run_ipt(&["-t", "mangle", "-F", "LEIGOD_GUARD"]);

    let base_rules: &[&[&str]] = &[
        &["-t", "mangle", "-A", "LEIGOD_GUARD", "-o", "lo", "-j", "ACCEPT"],
        &["-t", "mangle", "-A", "LEIGOD_GUARD", "-m", "owner", "--uid-owner", "0", "-j", "ACCEPT"],
        &["-t", "mangle", "-A", "LEIGOD_GUARD", "-d", "10.0.0.0/8", "-p", "tcp", "-j", "ACCEPT"],
        &["-t", "mangle", "-A", "LEIGOD_GUARD", "-d", "10.0.0.0/8", "-p", "udp", "!", "--dport", "53", "-j", "ACCEPT"],
        &["-t", "mangle", "-A", "LEIGOD_GUARD", "-d", "172.16.0.0/12", "-j", "ACCEPT"],
        &["-t", "mangle", "-A", "LEIGOD_GUARD", "-d", "192.168.0.0/16", "-j", "ACCEPT"],
        &["-t", "mangle", "-A", "LEIGOD_GUARD", "-d", "198.18.0.0/15", "-j", "ACCEPT"],
    ];
    for r in base_rules {
        let _ = run_ipt(r);
    }
    for kw in BYPASS_DNS_KEYWORDS {
        let _ = run_ipt(&[
            "-t", "mangle", "-A", "LEIGOD_GUARD", "-p", "udp", "--dport", "53",
            "-m", "string", "--algo", "bm", "--icase", "--string", kw, "-j", "ACCEPT",
        ]);
    }
    let _ = run_ipt(&[
        "-t", "mangle", "-A", "LEIGOD_GUARD", "-p", "tcp", "-m", "conntrack",
        "--ctstate", "ESTABLISHED,RELATED", "-m", "connmark", "!", "--mark", "0x102", "-j", "ACCEPT",
    ]);
    let _ = run_ipt(&[
        "-t", "mangle", "-A", "LEIGOD_GUARD", "-p", "tcp", "--syn", "-m", "conntrack",
        "--ctstate", "NEW", "-j", "CONNMARK", "--set-mark", "0x102",
    ]);

    while run_ipt(&["-t", "mangle", "-D", "OUTPUT", "-j", "LEIGOD_GUARD"]) {}
    let _ = run_ipt(&["-t", "mangle", "-I", "OUTPUT", "1", "-j", "LEIGOD_GUARD"]);
}

pub fn remove_mangle_guard() {
    while run_ipt(&["-t", "mangle", "-D", "OUTPUT", "-j", "LEIGOD_GUARD"]) {}
    let _ = run_ipt(&["-t", "mangle", "-F", "LEIGOD_GUARD"]);
    let _ = run_ipt(&["-t", "mangle", "-X", "LEIGOD_GUARD"]);
}

fn check_firewall_residuals() -> bool {
    if let Ok(out) = Command::new("iptables").args(["-t", "mangle", "-S", "OUTPUT"]).output() {
        let s = String::from_utf8_lossy(&out.stdout);
        if s.contains("0x10") || s.contains("GAMEACC") {
            if !s.contains("-A OUTPUT -j LEIGOD_GUARD") {
                install_mangle_guard();
            }
            return true;
        }
        if s.contains("-A OUTPUT -j LEIGOD_GUARD") {
            remove_mangle_guard();
        }
    }
    false
}

fn pause_account_time(account_token: &str, router_token: &str) -> Value {
    if !router_token.is_empty() {
        install_mangle_guard();
        let _ = ws_rpc_call(
            "127.0.0.1",
            5588,
            "/ws",
            &json!({
                "cmd": "STOPACC",
                "tag": "pause-stop",
                "sn": "",
                "cli": "webui",
                "code": 0,
                "err": "",
                "data": {"token": router_token, "catalogs": [2]},
            }),
            3000,
        );
    }
    remove_mangle_guard();
    flush_dns_cache();
    if account_token.trim().is_empty() {
        return json!({"code": 0, "local_only": true, "msg": "已停止本地加速（未配置官网 Token，未暂停官网时长）"});
    }
    match webapi_post("/api/user/pause", account_token.trim()) {
        Ok(v) => v,
        Err(e) => json!({"code": -1, "msg": e}),
    }
}

fn make_profile(area: &str, conf_str: &str, node_count: usize, catalog: i64, sub_catalog: i64, game_id: &str, updated_at: &str) -> Value {
    let ts = if updated_at.is_empty() {
        now_timestamp_str()
    } else {
        updated_at.to_string()
    };
    json!({
        "id": format!("region-{area}"),
        "name": format_region_title(area),
        "catalog": catalog,
        "subCatalog": sub_catalog,
        "gameId": game_id,
        "area": area,
        "nodeCount": node_count,
        "conf": conf_str,
        "updatedAt": ts,
    })
}

fn probe_node_uri(uri: &str, timeout: Duration) -> Option<f64> {
    let hp = uri.rsplit('@').next()?.trim();
    let addr = hp.to_socket_addrs().ok()?.next()?;
    let t0 = Instant::now();
    TcpStream::connect_timeout(&addr, timeout).ok()?;
    Some(t0.elapsed().as_secs_f64() * 1000.0)
}

fn filter_live_nodes_in_conf(mut conf_obj: Value) -> Value {
    let area = conf_obj["game"]["area"].as_str().unwrap_or("").trim().to_lowercase();
    let Some(srv) = conf_obj.get_mut("server").and_then(|v| v.as_object_mut()) else {
        return conf_obj;
    };

    let all_nodes = srv.get("allNode").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let mut candidate_uris = BTreeSet::new();
    for n in &all_nodes {
        if let Some(u) = n["node"].as_str() {
            candidate_uris.insert(u.to_string());
        }
    }
    if let Some(geo_obj) = srv.get("geo").and_then(|v| v.as_object()) {
        if area == "auto" {
            for gv in geo_obj.values().filter_map(|v| v.as_array()) {
                for u in gv.iter().filter_map(|x| x.as_str()) {
                    if !u.is_empty() && u != "direct" {
                        candidate_uris.insert(u.to_string());
                    }
                }
            }
        } else if let Some(gv) = geo_obj.get(&area).and_then(|v| v.as_array()) {
            for u in gv.iter().filter_map(|x| x.as_str()) {
                if !u.is_empty() && u != "direct" {
                    candidate_uris.insert(u.to_string());
                }
            }
        }
    }

    if candidate_uris.is_empty() {
        return conf_obj;
    }

    let timeout = Duration::from_millis(250);
    let uris_vec: Vec<String> = candidate_uris.iter().cloned().collect();
    let mut rtt_map = HashMap::new();

    thread::scope(|s| {
        let handles: Vec<_> = uris_vec
            .iter()
            .map(|u| s.spawn(move || (u.as_str(), probe_node_uri(u, timeout))))
            .collect();
        for h in handles {
            if let Ok((u, Some(rtt))) = h.join() {
                rtt_map.insert(u.to_string(), rtt);
            }
        }
    });

    if !all_nodes.is_empty() {
        let live_nodes: Vec<Value> = all_nodes
            .into_iter()
            .filter(|n| n["node"].as_str().map(|u| rtt_map.contains_key(u)).unwrap_or(false))
            .collect();
        if !live_nodes.is_empty() {
            let non_tproxy: Vec<Value> = live_nodes
                .iter()
                .filter(|n| {
                    n["node"]
                        .as_str()
                        .and_then(|u| rtt_map.get(u))
                        .copied()
                        .unwrap_or(0.0)
                        >= 1.5
                })
                .cloned()
                .collect();
            let mut ranked = if non_tproxy.is_empty() { live_nodes } else { non_tproxy };
            ranked.sort_by(|a, b| {
                let ra = a["node"].as_str().and_then(|u| rtt_map.get(u)).copied().unwrap_or(999.0);
                let rb = b["node"].as_str().and_then(|u| rtt_map.get(u)).copied().unwrap_or(999.0);
                ra.partial_cmp(&rb).unwrap_or(std::cmp::Ordering::Equal)
            });
            for n in &mut ranked {
                if let Some(u) = n["node"].as_str().map(|s| s.to_string()) {
                    if let Some(&rtt) = rtt_map.get(&u) {
                        n["ping"] = json!((rtt.round() as i64).max(1));
                    }
                }
            }
            if let Some(first_uri) = ranked.first().and_then(|n| n["node"].as_str()).map(|s| s.to_string()) {
                srv.insert("tcp".to_string(), json!([first_uri]));
            }
            srv.insert("allNode".to_string(), Value::Array(ranked));
        }
    }

    if let Some(geo_obj) = srv.get_mut("geo").and_then(|v| v.as_object_mut()) {
        for (_, gv) in geo_obj.iter_mut() {
            let Some(arr) = gv.as_array() else { continue };
            if !arr.iter().any(|u| u.as_str().map(|s| candidate_uris.contains(s)).unwrap_or(false)) {
                continue;
            }
            let mut live_gv: Vec<Value> = arr
                .iter()
                .filter(|u| {
                    let s = u.as_str().unwrap_or("");
                    s == "direct" || rtt_map.contains_key(s)
                })
                .cloned()
                .collect();
            if !live_gv.is_empty() {
                live_gv.sort_by(|a, b| {
                    let sa = a.as_str().unwrap_or("");
                    let sb = b.as_str().unwrap_or("");
                    let ra = if sa == "direct" { 0.0 } else { rtt_map.get(sa).copied().unwrap_or(999.0) };
                    let rb = if sb == "direct" { 0.0 } else { rtt_map.get(sb).copied().unwrap_or(999.0) };
                    ra.partial_cmp(&rb).unwrap_or(std::cmp::Ordering::Equal)
                });
                *gv = Value::Array(live_gv);
            }
        }
    }

    conf_obj
}

fn sanitize_acc_conf(conf_input: &Value) -> (String, Value, bool) {
    let mut parsed: Value = match conf_input {
        Value::String(s) => match serde_json::from_str(s) {
            Ok(v) => v,
            Err(_) => return (s.clone(), json!({}), false),
        },
        other => other.clone(),
    };

    let mut modified = false;
    if let Some(report) = parsed.get_mut("report").and_then(|v| v.as_object_mut()) {
        for (k, v) in report.iter_mut() {
            if k == "heart" {
                continue;
            }
            if let Some(obj) = v.as_object_mut() {
                if obj.get("enable") != Some(&Value::Bool(false))
                    || obj.get("url").and_then(|u| u.as_str()).unwrap_or("") != ""
                {
                    obj.insert("enable".to_string(), Value::Bool(false));
                    obj.insert("interval".to_string(), json!(0));
                    obj.insert("url".to_string(), json!(""));
                    modified = true;
                }
            }
        }
    }

    if let Some(log_cfg) = parsed.get_mut("log").and_then(|v| v.as_object_mut()) {
        if log_cfg.get("level").and_then(|v| v.as_str()) != Some("none")
            || log_cfg.get("outFile") != Some(&Value::Bool(false))
        {
            log_cfg.insert("level".to_string(), json!("none"));
            log_cfg.insert("outFile".to_string(), Value::Bool(false));
            modified = true;
        }
    }

    if let Some(limit_cfg) = parsed.get_mut("limit").and_then(|v| v.as_object_mut()) {
        for k in ["play_kb", "down_kb", "up_kb"] {
            if limit_cfg.get(k).and_then(|v| v.as_i64()).unwrap_or(0) < 8192 {
                limit_cfg.insert(k.to_string(), json!(8192));
                modified = true;
            }
        }
        if limit_cfg.get("data_kb").and_then(|v| v.as_i64()).unwrap_or(0) < 16384 {
            limit_cfg.insert("data_kb".to_string(), json!(16384));
            modified = true;
        }
    }

    if let Some(ipdata_cfg) = parsed.get_mut("ipdata").and_then(|v| v.as_object_mut()) {
        let xdb_path = "/opt/leigod/config/ipdatacloud_country.xdb";
        if ipdata_cfg.get("path").and_then(|v| v.as_str()) != Some(xdb_path) {
            ipdata_cfg.insert("path".to_string(), json!(xdb_path));
            modified = true;
        }
    }

    if let Some(srv_cfg) = parsed.get_mut("server").and_then(|v| v.as_object_mut()) {
        let want_dns = json!(resolve_local_dns_list());
        if srv_cfg.get("dns") != Some(&want_dns) {
            srv_cfg.insert("dns".to_string(), want_dns);
            modified = true;
        }
    }

    if let Some(game_cfg) = parsed.get_mut("game").and_then(|v| v.as_object_mut()) {
        let is_auto = game_cfg
            .get("area")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .eq_ignore_ascii_case("auto");
        if game_cfg.get("dyn_line") != Some(&Value::Bool(is_auto)) {
            game_cfg.insert("dyn_line".to_string(), Value::Bool(is_auto));
            modified = true;
        }
    }

    let target_rules = json!([{"host": ["geo:cn"], "serv": ["direct"]}]);
    if parsed.get("rules") != Some(&target_rules) {
        if let Some(obj) = parsed.as_object_mut() {
            obj.insert("rules".to_string(), target_rules);
            modified = true;
        }
    }

    let clean_str = serde_json::to_string(&parsed).unwrap_or_else(|_| "{}".to_string());
    (clean_str, parsed, modified)
}

fn consolidate_regional_profiles(profiles: Vec<Value>) -> (Vec<Value>, bool) {
    let mut by_area: BTreeMap<String, Value> = BTreeMap::new();
    let mut parsed_by_area: BTreeMap<String, Value> = BTreeMap::new();
    let mut nodes_by_area: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
    let mut merged_geo: Map<String, Value> = Map::new();
    let mut changed = false;

    for p in profiles {
        let area = p["area"].as_str().unwrap_or("").trim().to_lowercase();
        if area.is_empty() || area == "-" {
            changed = true;
            continue;
        }
        let (clean_str, conf_obj, was_mod) = sanitize_acc_conf(&p["conf"]);
        if let Some(srv_geo) = conf_obj["server"]["geo"].as_object() {
            for (gk, gv) in srv_geo {
                let Some(arr) = gv.as_array() else { continue };
                if arr.is_empty() {
                    continue;
                }
                let entry = merged_geo
                    .entry(gk.clone())
                    .or_insert_with(|| Value::Array(Vec::new()));
                if let Some(cur) = entry.as_array_mut() {
                    for u in arr {
                        if !cur.contains(u) {
                            cur.push(u.clone());
                        }
                    }
                }
            }
        }

        if area != "auto" {
            let area_nodes = nodes_by_area.entry(area.clone()).or_default();
            if let Some(all_nodes) = conf_obj["server"]["allNode"].as_array() {
                for n in all_nodes {
                    let Some(uri) = n["node"].as_str() else { continue };
                    let sign = n["speed_sign"].as_str().unwrap_or(FALLBACK_SIGN);
                    let replace = match area_nodes.get(uri) {
                        None => true,
                        Some(old_n) => old_n["speed_sign"].as_str() == Some(FALLBACK_SIGN) && sign != FALLBACK_SIGN,
                    };
                    if replace {
                        area_nodes.insert(
                            uri.to_string(),
                            json!({
                                "node": uri,
                                "ping": n["ping"].as_i64().unwrap_or(20),
                                "speed_sign": sign,
                            }),
                        );
                    }
                }
            }
        }

        let node_count = conf_obj["server"]["allNode"].as_array().map(|a| a.len()).unwrap_or(0);
        let keep = match by_area.get(&area) {
            None => true,
            Some(ex) => node_count >= ex["nodeCount"].as_u64().unwrap_or(0) as usize,
        };
        if keep {
            let new_entry = make_profile(
                &area,
                &clean_str,
                node_count,
                p["catalog"].as_i64().unwrap_or(2),
                p["subCatalog"].as_i64().unwrap_or(0),
                p["gameId"].as_str().unwrap_or(""),
                p["updatedAt"].as_str().unwrap_or(""),
            );
            if p["id"] != new_entry["id"]
                || p["name"] != new_entry["name"]
                || was_mod
                || p["nodeCount"].as_u64() != Some(node_count as u64)
                || by_area.contains_key(&area)
            {
                changed = true;
            }
            by_area.insert(area.clone(), new_entry);
            parsed_by_area.insert(area, conf_obj);
        } else {
            changed = true;
        }
    }

    let base_area = if parsed_by_area.contains_key("hk") {
        Some("hk".to_string())
    } else if parsed_by_area.contains_key("jp") {
        Some("jp".to_string())
    } else {
        parsed_by_area.keys().next().cloned()
    };

    if let Some(b_area) = base_area {
        if !merged_geo.is_empty() {
            let base_entry = by_area[&b_area].clone();
            let base_conf = parsed_by_area[&b_area].clone();
            let default_sign = base_conf["user"]["speed_sign"]
                .as_str()
                .unwrap_or(FALLBACK_SIGN)
                .to_string();
            let now_ts = base_entry["updatedAt"].as_str().unwrap_or("").to_string();
            let merged_geo_val = Value::Object(merged_geo.clone());

            for &target_area in &["hk", "jp", "kr", "tw", "sea", "us", "eu", "ru", "au"] {
                let area_nodes = nodes_by_area.entry(target_area.to_string()).or_default();
                for &gk in geo_sources(target_area) {
                    if let Some(arr) = merged_geo.get(gk).and_then(|v| v.as_array()) {
                        for u in arr.iter().filter_map(|x| x.as_str()) {
                            if u != "direct" && !area_nodes.contains_key(u) {
                                area_nodes.insert(
                                    u.to_string(),
                                    json!({"node": u, "ping": 25, "speed_sign": default_sign}),
                                );
                            }
                        }
                    }
                }
                if area_nodes.is_empty() {
                    continue;
                }
                let merged_node_list: Vec<Value> = area_nodes.values().cloned().collect();
                let first_uri = merged_node_list[0]["node"].as_str().unwrap_or("").to_string();

                if !by_area.contains_key(target_area) {
                    let mut syn_conf = base_conf.clone();
                    syn_conf["game"]["area"] = json!(target_area);
                    syn_conf["game"]["dyn_line"] = Value::Bool(false);
                    syn_conf["server"]["tcp"] = json!([first_uri]);
                    syn_conf["server"]["allNode"] = Value::Array(merged_node_list.clone());
                    syn_conf["server"]["geo"] = merged_geo_val.clone();
                    let (syn_str, syn_parsed, _) = sanitize_acc_conf(&syn_conf);
                    parsed_by_area.insert(target_area.to_string(), syn_parsed);
                    by_area.insert(
                        target_area.to_string(),
                        make_profile(
                            target_area,
                            &syn_str,
                            merged_node_list.len(),
                            base_entry["catalog"].as_i64().unwrap_or(2),
                            base_entry["subCatalog"].as_i64().unwrap_or(0),
                            base_entry["gameId"].as_str().unwrap_or(""),
                            &now_ts,
                        ),
                    );
                    changed = true;
                } else if let Some(conf_obj) = parsed_by_area.get_mut(target_area) {
                    let want_nodes = Value::Array(merged_node_list.clone());
                    if conf_obj["server"]["allNode"] != want_nodes || conf_obj["server"]["geo"] != merged_geo_val {
                        conf_obj["server"]["allNode"] = want_nodes;
                        conf_obj["server"]["tcp"] = json!([first_uri]);
                        conf_obj["server"]["geo"] = merged_geo_val.clone();
                        let (clean_str, new_conf, _) = sanitize_acc_conf(conf_obj);
                        *conf_obj = new_conf;
                        if let Some(entry) = by_area.get_mut(target_area) {
                            entry["conf"] = json!(clean_str);
                            entry["nodeCount"] = json!(merged_node_list.len());
                        }
                        changed = true;
                    }
                }
            }

            let mut auto_conf = parsed_by_area.get("hk").cloned().unwrap_or(base_conf);
            auto_conf["game"]["area"] = json!("auto");
            auto_conf["game"]["dyn_line"] = Value::Bool(true);
            auto_conf["server"]["geo"] = merged_geo_val;
            let (auto_str, _, _) = sanitize_acc_conf(&auto_conf);
            let geo_node_total: usize = merged_geo
                .iter()
                .filter(|(k, v)| !k.is_empty() && **v != json!(["direct"]))
                .map(|(_, v)| v.as_array().map(|a| a.len()).unwrap_or(0))
                .sum();

            let need_auto = match by_area.get("auto") {
                None => true,
                Some(a) => {
                    a["conf"].as_str() != Some(&auto_str)
                        || a["nodeCount"].as_u64() != Some(geo_node_total as u64)
                }
            };
            if need_auto {
                by_area.insert(
                    "auto".to_string(),
                    make_profile(
                        "auto",
                        &auto_str,
                        geo_node_total,
                        base_entry["catalog"].as_i64().unwrap_or(2),
                        base_entry["subCatalog"].as_i64().unwrap_or(0),
                        base_entry["gameId"].as_str().unwrap_or(""),
                        &now_ts,
                    ),
                );
                changed = true;
            }
        }
    }

    let mut ordered = Vec::new();
    for k in PRESET_AREAS {
        if let Some(v) = by_area.remove(k) {
            ordered.push(v);
        }
    }
    for (_, v) in by_area {
        ordered.push(v);
    }
    (ordered, changed)
}

fn harvest_webapi_log_profiles(candidate_dirs: &[PathBuf]) -> Vec<Value> {
    let mut recovered = Vec::new();
    for d in candidate_dirs {
        for sub in [d.clone(), d.join("log")] {
            let Ok(entries) = fs::read_dir(&sub) else { continue };
            let mut log_files: Vec<PathBuf> = entries
                .filter_map(|e| e.ok().map(|x| x.path()))
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.starts_with("web_api") && n.ends_with(".log"))
                        .unwrap_or(false)
                })
                .collect();
            log_files.sort();

            for lf in log_files {
                let Ok(text) = fs::read_to_string(&lf) else { continue };
                for line in text.lines() {
                    if !line.contains("[api] request:")
                        || (!line.contains("startAcc") && !line.contains("STARTACC"))
                    {
                        continue;
                    }
                    let Some((_, tail)) = line.split_once("[api] request:") else { continue };
                    let raw = tail.split(", response:").next().unwrap_or("").trim();
                    let Ok(req) = serde_json::from_str::<Value>(raw) else { continue };
                    let conf_raw = &req["data"]["conf"];
                    if conf_raw.is_null() {
                        continue;
                    }
                    let (clean_str, parsed, _) = sanitize_acc_conf(conf_raw);
                    let area = parsed["game"]["area"].as_str().unwrap_or("").trim().to_lowercase();
                    if area.is_empty() || area == "-" {
                        continue;
                    }
                    let node_count = parsed["server"]["allNode"].as_array().map(|a| a.len()).unwrap_or(0);
                    recovered.push(make_profile(
                        &area,
                        &clean_str,
                        node_count,
                        req["data"]["catalog"].as_i64().unwrap_or(2),
                        req["data"]["subCatalog"].as_i64().unwrap_or(0),
                        parsed["game"]["id"].as_str().unwrap_or(""),
                        "",
                    ));
                }
            }
        }
    }
    recovered
}

fn file_sig(p: &Path) -> (u64, u64) {
    fs::metadata(p)
        .map(|m| (m.mtime_nsec() as u64, m.len()))
        .unwrap_or((0, 0))
}

pub fn scan_and_capture_active_profiles() -> Vec<Value> {
    let mut candidate_dirs = vec![PathBuf::from(LEIGOD_DIR).join("config"), PathBuf::from("/tmp/acc")];
    if let Ok(out) = Command::new("pgrep").args(["-f", "/opt/leigod/acc-gw.router.amd64"]).output() {
        if let Some(pid) = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .find(|s| s.chars().all(|c| c.is_ascii_digit()))
        {
            candidate_dirs.push(PathBuf::from(format!("/proc/{pid}/root/tmp/acc")));
            candidate_dirs.push(PathBuf::from(format!("/proc/{pid}/root/opt/leigod/config")));
        }
    }

    let mut sig = vec![file_sig(&profiles_file()), file_sig(&account_file())];
    for d in &candidate_dirs {
        sig.push(file_sig(&d.join("acc_core_conf.json")));
        if let Ok(entries) = fs::read_dir(d.join("log")) {
            let mut lfs: Vec<PathBuf> = entries
                .filter_map(|e| e.ok().map(|x| x.path()))
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.starts_with("web_api") && n.ends_with(".log"))
                        .unwrap_or(false)
                })
                .collect();
            lfs.sort();
            for lf in lfs {
                sig.push(file_sig(&lf));
            }
        }
    }

    if let Ok(guard) = cache().lock() {
        if guard.last_sig == sig && !guard.profiles.is_empty() {
            return guard.profiles.clone();
        }
    }

    let (mut profiles, mut changed) = consolidate_regional_profiles(load_saved_profiles());
    let mut existing_keys: BTreeSet<String> = profiles
        .iter()
        .filter_map(|p| p["id"].as_str().map(|s| s.to_string()))
        .collect();

    for d in &candidate_dirs {
        let core_file = d.join("acc_core_conf.json");
        if !core_file.is_file() {
            continue;
        }
        let Ok(text) = fs::read_to_string(&core_file) else { continue };
        let Ok(mut core_meta) = serde_json::from_str::<Value>(&text) else { continue };
        let mut core_mod = false;

        if let Some(acc_list) = core_meta.get_mut("acceleration").and_then(|v| v.as_array_mut()) {
            for item in acc_list {
                let Some(obj) = item.as_object_mut() else { continue };
                for (dev_key, dev_val) in obj.iter_mut() {
                    if dev_val.get("acc_conf").is_none() {
                        continue;
                    }
                    let (raw_text, parsed, was_mod) = sanitize_acc_conf(&dev_val["acc_conf"]);
                    if was_mod {
                        dev_val["acc_conf"] = json!(raw_text);
                        core_mod = true;
                    }
                    let game_area = parsed["game"]["area"]
                        .as_str()
                        .or_else(|| dev_val["game_area"].as_str())
                        .unwrap_or("hk")
                        .to_lowercase();
                    let entry = make_profile(
                        &game_area,
                        &raw_text,
                        parsed["server"]["allNode"].as_array().map(|a| a.len()).unwrap_or(0),
                        dev_val["device_type"]
                            .as_i64()
                            .or_else(|| dev_val["catalog_type"].as_i64())
                            .unwrap_or(2),
                        dev_val["sub_type"].as_i64().unwrap_or(0),
                        parsed["game"]["id"]
                            .as_str()
                            .or_else(|| dev_val["server"].as_str())
                            .unwrap_or(dev_key),
                        "",
                    );
                    let prof_id = entry["id"].as_str().unwrap_or("").to_string();
                    if existing_keys.contains(&prof_id) {
                        for old in &mut profiles {
                            if old["id"] == entry["id"] && old["conf"] != entry["conf"] {
                                *old = entry.clone();
                                changed = true;
                            }
                        }
                    } else {
                        existing_keys.insert(prof_id);
                        profiles.insert(0, entry);
                        changed = true;
                    }
                }
            }
        }
        if core_mod {
            if let Ok(s) = serde_json::to_string(&core_meta) {
                let _ = fs::write(&core_file, s);
            }
        }
        break;
    }

    let harvested = harvest_webapi_log_profiles(&candidate_dirs);
    if !harvested.is_empty() {
        profiles.extend(harvested);
        changed = true;
    }

    if changed {
        let (cons, _) = consolidate_regional_profiles(profiles);
        profiles = cons;
        save_profiles(&profiles);
    }

    if let Ok(mut guard) = cache().lock() {
        guard.profiles = profiles.clone();
        guard.last_sig = sig;
    }
    profiles
}

fn probe_udp_6066() -> &'static str {
    if let Ok(out) = Command::new("ss").args(["-ulnp"]).output() {
        if String::from_utf8_lossy(&out.stdout).contains(":6066 ") {
            return "监听正常 (6066/udp)";
        }
    }
    "未检测到 6066/udp 监听"
}

fn read_ini_values() -> (String, String, String) {
    let mut version = String::new();
    let mut token = String::new();
    let mut user_id = String::new();

    if let Ok(text) = fs::read_to_string(Path::new(LEIGOD_DIR).join("config/acc_version.ini")) {
        for line in text.lines() {
            if let Some((k, v)) = line.trim().split_once('=') {
                if k.trim().eq_ignore_ascii_case("version") {
                    version = v.trim().to_string();
                }
            }
        }
    }
    if let Ok(text) = fs::read_to_string(Path::new(LEIGOD_DIR).join("config/accelerator.ini")) {
        for line in text.lines() {
            if let Some((k, v)) = line.trim().split_once('=') {
                let kl = k.trim().to_lowercase();
                let val = v.trim();
                if val.is_empty() {
                    continue;
                }
                if matches!(kl.as_str(), "token" | "bind_token" | "rtoken") {
                    token = val.to_string();
                } else if matches!(kl.as_str(), "user_id" | "userid") {
                    user_id = val.to_string();
                }
            }
        }
    }
    (version, token, user_id)
}

pub fn read_host_status() -> Value {
    let (service_active, service_state) = match Command::new("systemctl")
        .args(["is-active", "leigod_plugin.service"])
        .output()
    {
        Ok(out) => {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (out.status.success(), if s.is_empty() { "inactive".to_string() } else { s })
        }
        Err(_) => (false, "unknown".to_string()),
    };

    let mut mac = String::new();
    for p in [PathBuf::from("/sys/class/net/wlan0/address"), state_dir().join("device-mac")] {
        if let Ok(s) = fs::read_to_string(p) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                mac = trimmed.to_string();
                break;
            }
        }
    }

    let mut tun_if = "未创建 (开启加速后生成)".to_string();
    let mut active_tun = String::new();
    if let Ok(out) = Command::new("ip").args(["-o", "link", "show", "type", "tun"]).output() {
        let s = String::from_utf8_lossy(&out.stdout);
        let tuns: Vec<String> = s
            .lines()
            .filter_map(|line| line.split(':').nth(1))
            .map(|part| part.trim().split('@').next().unwrap_or("").to_string())
            .filter(|name| name.starts_with("tun_"))
            .collect();
        if !tuns.is_empty() {
            active_tun = tuns[0].clone();
            tun_if = tuns.join(", ");
        }
    }

    let traffic_iface = if active_tun.is_empty() {
        "tun_PC".to_string()
    } else {
        active_tun.clone()
    };
    let mut rx_bytes: u64 = 0;
    let mut tx_bytes: u64 = 0;
    if !active_tun.is_empty() {
        rx_bytes = fs::read_to_string(format!("/sys/class/net/{active_tun}/statistics/rx_bytes"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        tx_bytes = fs::read_to_string(format!("/sys/class/net/{active_tun}/statistics/tx_bytes"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
    }

    let (version, token, user_id) = read_ini_values();
    let acct_cfg = load_account_config();

    json!({
        "service_active": service_active,
        "service_state": service_state,
        "mac": mac,
        "tun_interface": tun_if,
        "traffic_iface": traffic_iface,
        "rx_bytes": rx_bytes,
        "tx_bytes": tx_bytes,
        "has_gameacc_chain": check_firewall_residuals(),
        "udp_6066": probe_udp_6066(),
        "version": version,
        "token": token,
        "user_id": user_id,
        "profiles": scan_and_capture_active_profiles(),
        "has_account_token": !acct_cfg["account_token"].as_str().unwrap_or("").trim().is_empty(),
        "auto_pause_on_stop": acct_cfg["auto_pause_on_stop"].as_bool().unwrap_or(true),
    })
}

fn send_http(mut stream: TcpStream, status: u16, content_type: &str, body: &[u8]) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        _ => "OK",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
}

fn send_json(stream: TcpStream, status: u16, val: &Value) {
    let body = val.to_string().into_bytes();
    send_http(stream, status, "application/json; charset=utf-8", &body);
}

fn handle_client(stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() || request_line.is_empty() {
        return;
    }
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        return;
    }
    let method = parts[0];
    let path = parts[1];

    let mut content_length: usize = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line == "\r\n" || line == "\n" || line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
    }

    let mut body_bytes = vec![0u8; content_length];
    if content_length > 0 && reader.read_exact(&mut body_bytes).is_err() {
        return;
    }

    if method == "GET" {
        match path {
            "/" | "/index.html" => {
                let html_path = Path::new(LEIGOD_DIR).join("webui/index.html");
                let body = fs::read(html_path).unwrap_or_else(|_| b"open-Raijin daemon running".to_vec());
                send_http(stream, 200, "text/html; charset=utf-8", &body);
            }
            "/api/host-status" => {
                send_json(stream, 200, &read_host_status());
            }
            "/api/profiles" => {
                send_json(stream, 200, &json!({"profiles": scan_and_capture_active_profiles()}));
            }
            "/api/account/status" => {
                let cfg = load_account_config();
                let acct_tok = cfg["account_token"].as_str().unwrap_or("").trim().to_string();
                let gw = detect_gateway_ip();
                let gw_dns = if gw.is_empty() {
                    "223.5.5.5:53".to_string()
                } else {
                    format!("{gw}:53")
                };
                if acct_tok.is_empty() {
                    send_json(
                        stream,
                        200,
                        &json!({
                            "configured": false,
                            "auto_pause_on_stop": cfg["auto_pause_on_stop"].as_bool().unwrap_or(true),
                            "custom_dns": cfg["custom_dns"].as_str().unwrap_or(""),
                            "detected_gateway_dns": gw_dns,
                        }),
                    );
                } else {
                    match webapi_post("/api/user/info", &acct_tok) {
                        Ok(res) => send_json(
                            stream,
                            200,
                            &json!({
                                "configured": true,
                                "auto_pause_on_stop": cfg["auto_pause_on_stop"].as_bool().unwrap_or(true),
                                "custom_dns": cfg["custom_dns"].as_str().unwrap_or(""),
                                "detected_gateway_dns": gw_dns,
                                "response": res,
                            }),
                        ),
                        Err(e) => send_json(stream, 502, &json!({"configured": true, "error": e})),
                    }
                }
            }
            _ => send_json(stream, 404, &json!({"error": "Not found"})),
        }
        return;
    }

    if method == "POST" {
        let req_json: Value = if body_bytes.is_empty() {
            json!({})
        } else {
            serde_json::from_slice(&body_bytes).unwrap_or_else(|_| json!({}))
        };

        match path {
            "/api/ws-rpc" => {
                let mut req_obj = req_json;
                let cmd_upper = req_obj["cmd"].as_str().unwrap_or("").to_uppercase();
                if BLOCKED_ROUTER_CMDS.contains(&cmd_upper.as_str()) {
                    send_json(
                        stream,
                        403,
                        &json!({
                            "cmd": cmd_upper,
                            "code": -1,
                            "err": format!("已拦截高危路由器固件指令 {cmd_upper}"),
                            "data": {},
                        }),
                    );
                    return;
                }

                if matches!(cmd_upper.as_str(), "STARTACC" | "START") {
                    install_mangle_guard();
                    if !req_obj["data"]["conf"].is_null() {
                        let (_, parsed_conf, _) = sanitize_acc_conf(&req_obj["data"]["conf"]);
                        let filtered = filter_live_nodes_in_conf(parsed_conf);
                        let clean_conf = serde_json::to_string(&filtered).unwrap_or_default();
                        req_obj["data"]["conf"] = filtered.clone();

                        let area = filtered["game"]["area"].as_str().unwrap_or("").trim().to_lowercase();
                        if !area.is_empty() && area != "-" {
                            let mut profiles = load_saved_profiles();
                            let prof_id = format!("region-{area}");
                            if !profiles.iter().any(|p| p["id"].as_str() == Some(&prof_id)) {
                                let nc = filtered["server"]["allNode"].as_array().map(|a| a.len()).unwrap_or(0);
                                profiles.insert(
                                    0,
                                    make_profile(
                                        &area,
                                        &clean_conf,
                                        nc,
                                        req_obj["data"]["catalog"].as_i64().unwrap_or(2),
                                        req_obj["data"]["subCatalog"].as_i64().unwrap_or(0),
                                        filtered["game"]["id"].as_str().unwrap_or(""),
                                        "",
                                    ),
                                );
                                save_profiles(&profiles);
                            }
                        }
                    }
                }

                match ws_rpc_call("127.0.0.1", 5588, "/ws", &req_obj, 5000) {
                    Ok(resp) => {
                        if matches!(cmd_upper.as_str(), "STARTACC" | "START") {
                            install_mangle_guard();
                            flush_dns_cache();
                        } else if matches!(cmd_upper.as_str(), "STOPACC" | "STOP") {
                            remove_mangle_guard();
                            flush_dns_cache();
                        }
                        send_json(stream, 200, &resp);
                    }
                    Err(e) => send_json(
                        stream,
                        502,
                        &json!({"cmd": "ERROR", "code": -1, "err": format!("连接 127.0.0.1:5588/ws 失败: {e}"), "data": {}}),
                    ),
                }
            }
            "/api/cleanup-net" => {
                let _ = Command::new("/bin/sh")
                    .arg(Path::new(LEIGOD_DIR).join("leigod_cleanup_net.sh"))
                    .status();
                send_json(
                    stream,
                    200,
                    &json!({"ok": true, "msg": "已清理 GAMEACC 链、策略路由表 99、ipset 与残留 tun 接口"}),
                );
            }
            "/api/service-restart" => {
                let ok = Command::new("systemctl")
                    .args(["restart", "leigod_plugin.service"])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                send_json(
                    stream,
                    if ok { 200 } else { 500 },
                    &json!({"ok": ok, "msg": if ok { "已安全重启 leigod_plugin.service 守护服务" } else { "重启服务失败" }}),
                );
            }
            "/api/account/save-token" => {
                let mut cfg = load_account_config();
                if let Some(raw_tok) = req_json["account_token"].as_str() {
                    let trimmed = raw_tok.trim();
                    if !trimmed.is_empty() {
                        let tok = if trimmed.starts_with('{') {
                            serde_json::from_str::<Value>(trimmed)
                                .ok()
                                .and_then(|v| v["account_token"].as_str().map(|s| s.to_string()))
                                .unwrap_or_else(|| trimmed.to_string())
                        } else {
                            trimmed.to_string()
                        };
                        cfg["account_token"] = json!(tok);
                    }
                }
                if let Some(ap) = req_json["auto_pause_on_stop"].as_bool() {
                    cfg["auto_pause_on_stop"] = Value::Bool(ap);
                }
                if let Some(cdns) = req_json.get("custom_dns").and_then(|v| v.as_str()) {
                    cfg["custom_dns"] = json!(cdns.trim());
                }
                save_account_config(&cfg);
                if req_json.get("custom_dns").is_some() {
                    let _ = scan_and_capture_active_profiles();
                }
                let acct_tok = cfg["account_token"].as_str().unwrap_or("").trim();
                let info = if acct_tok.is_empty() {
                    json!({})
                } else {
                    webapi_post("/api/user/info", acct_tok).unwrap_or_else(|_| json!({}))
                };
                send_json(
                    stream,
                    200,
                    &json!({
                        "ok": true,
                        "auto_pause_on_stop": cfg["auto_pause_on_stop"].as_bool().unwrap_or(true),
                        "custom_dns": cfg["custom_dns"].as_str().unwrap_or(""),
                        "response": info,
                    }),
                );
            }
            "/api/account/pause" => {
                let cfg = load_account_config();
                let acct_tok = cfg["account_token"].as_str().unwrap_or("").trim().to_string();
                let (_, token, _) = read_ini_values();
                let res = pause_account_time(&acct_tok, &token);
                send_json(stream, 200, &res);
            }
            "/api/account/recover" => {
                let cfg = load_account_config();
                let acct_tok = cfg["account_token"].as_str().unwrap_or("").trim().to_string();
                if acct_tok.is_empty() {
                    send_json(stream, 400, &json!({"code": -1, "msg": "请先配置官网 account_token"}));
                } else {
                    match webapi_post("/api/user/recover", &acct_tok) {
                        Ok(res) => send_json(stream, 200, &res),
                        Err(e) => send_json(stream, 502, &json!({"code": -1, "msg": e})),
                    }
                }
            }
            _ => send_json(stream, 404, &json!({"error": "Not found"})),
        }
    }
}

extern "C" fn on_sigterm(_sig: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

pub fn run_daemon() {
    unsafe {
        libc::signal(libc::SIGTERM, on_sigterm as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_sigterm as *const () as libc::sighandler_t);
    }

    let (host, port) = load_host_port();
    let listener = TcpListener::bind(format!("{host}:{port}")).expect("Failed to bind daemon HTTP port");
    let _ = listener.set_nonblocking(true);

    thread::spawn(|| loop {
        let _ = scan_and_capture_active_profiles();
        thread::sleep(Duration::from_millis(1500));
    });

    println!("open-Raijin daemon running at http://{host}:{port}");
    while !SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                thread::spawn(move || handle_client(stream));
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(40));
            }
            Err(_) => {
                thread::sleep(Duration::from_millis(100));
            }
        }
    }

    let cfg = load_account_config();
    let acct_tok = cfg["account_token"].as_str().unwrap_or("").trim().to_string();
    if !acct_tok.is_empty() && cfg["auto_pause_on_stop"].as_bool().unwrap_or(true) {
        let (_, token, _) = read_ini_values();
        let _ = pause_account_time(&acct_tok, &token);
    }
}
