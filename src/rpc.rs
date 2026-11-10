use crate::net::{
    detect_gateway_ip, flush_dns_cache, install_mangle_guard, read_device_mac, read_ini_values,
    remove_mangle_guard, run_cleanup_net, set_stop_enforced, state_dir,
};
use crate::profiles::cached_profile_p_token;
use crate::types::{AccountConfig, AccountSnapshot, APP_ID, BUSI_TYPE, WEBAPI_BASE};
use serde_json::{json, Map, Value};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

fn account_file() -> PathBuf {
    state_dir().join("account.json")
}

pub fn load_account_config() -> AccountConfig {
    let Ok(text) = fs::read_to_string(account_file()) else {
        return AccountConfig::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

pub fn save_account_config(cfg: &AccountConfig) {
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
}

pub fn is_valid_ptoken(tok: &str) -> bool {
    let t = tok.trim();
    t.len() == 32 && t.chars().all(|c| c.is_ascii_hexdigit())
}

pub fn resolve_p_token(cfg: &AccountConfig) -> String {
    if is_valid_ptoken(&cfg.p_token) {
        return cfg.p_token.trim().to_string();
    }
    cached_profile_p_token()
}

fn escape_curl_cfg(val: &str) -> String {
    let mut out = String::with_capacity(val.len() + 8);
    for ch in val.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(ch),
        }
    }
    out
}

/// Executes curl reading URL, headers, and body from stdin (`--config -`)
/// so tokens never appear in `/proc/<pid>/cmdline`.
pub fn curl_json(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
    timeout_secs: u64,
) -> Result<Value, String> {
    let mut cfg = format!("url = \"{}\"\nrequest = \"{}\"\n", escape_curl_cfg(url), method);
    for (k, v) in headers {
        cfg.push_str(&format!("header = \"{}: {}\"\n", escape_curl_cfg(k), escape_curl_cfg(v)));
    }
    if let Some(b) = body {
        cfg.push_str(&format!("data-raw = \"{}\"\n", escape_curl_cfg(b)));
    }

    let mut child = Command::new("curl")
        .args(["-fsSL", "--max-time", &timeout_secs.to_string(), "--config", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(cfg.as_bytes());
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())
}

fn write_ws_frame(sock: &mut TcpStream, opcode: u8, payload: &[u8]) -> Result<(), String> {
    let mask_key: [u8; 4] = [0x37, 0xfa, 0x21, 0x3d];
    let mut frame = vec![0x80 | (opcode & 0x0f)];
    let len = payload.len();
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
    for (i, &b) in payload.iter().enumerate() {
        frame.push(b ^ mask_key[i % 4]);
    }
    sock.write_all(&frame).map_err(|e| e.to_string())
}

pub fn ws_rpc_call(host: &str, port: u16, path: &str, payload: &Value, timeout_ms: u64) -> Result<Value, String> {
    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .map_err(|e| format!("Invalid address: {e}"))?;
    let timeout = Duration::from_millis(timeout_ms);
    let mut sock = TcpStream::connect_timeout(&addr, timeout).map_err(|e| e.to_string())?;
    let _ = sock.set_nodelay(true);
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

    let mut reader = BufReader::with_capacity(4096, sock.try_clone().map_err(|e| e.to_string())?);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).map_err(|e| e.to_string())?;
    if !status_line.contains("101") {
        return Err(format!("WebSocket handshake rejected: {}", status_line.trim()));
    }
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).map_err(|e| e.to_string())?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }

    let body = payload.to_string().into_bytes();
    write_ws_frame(&mut sock, 0x1, &body)?;

    loop {
        let mut hdr = [0u8; 2];
        reader.read_exact(&mut hdr).map_err(|e| e.to_string())?;
        let opcode = hdr[0] & 0x0f;
        let masked = (hdr[1] & 0x80) != 0;
        let mut r_len = (hdr[1] & 0x7f) as usize;
        if r_len == 126 {
            let mut b = [0u8; 2];
            reader.read_exact(&mut b).map_err(|e| e.to_string())?;
            r_len = u16::from_be_bytes(b) as usize;
        } else if r_len == 127 {
            let mut b = [0u8; 8];
            reader.read_exact(&mut b).map_err(|e| e.to_string())?;
            r_len = u64::from_be_bytes(b) as usize;
        }
        let mut srv_mask = [0u8; 4];
        if masked {
            reader.read_exact(&mut srv_mask).map_err(|e| e.to_string())?;
        }
        let mut data = vec![0u8; r_len.min(4 * 1024 * 1024)];
        reader.read_exact(&mut data).map_err(|e| e.to_string())?;
        if masked {
            for (i, b) in data.iter_mut().enumerate() {
                *b ^= srv_mask[i % 4];
            }
        }
        match opcode {
            0x8 => return Err("WebSocket closed by server".to_string()),
            0x9 => {
                let _ = write_ws_frame(&mut sock, 0xA, &data);
            }
            0xA => continue,
            _ => return serde_json::from_slice(&data).map_err(|e| e.to_string()),
        }
    }
}

fn unwrap_ws_data(resp: &Value) -> Value {
    if let Some(s) = resp["data"].as_str() {
        if let Ok(parsed) = serde_json::from_str::<Value>(s) {
            return parsed;
        }
    }
    resp["data"].clone()
}

pub fn query_active_acc_state(token: &str) -> (bool, String, i64, String) {
    if token.trim().is_empty() {
        return (false, String::new(), 0, "0".to_string());
    }
    let dev_req = json!({
        "cmd": "GETDEVICELIST", "tag": "1", "sn": "", "cli": "leinux",
        "code": 0, "err": "", "data": {"token": token}
    });
    let Ok(dev_resp) = ws_rpc_call("127.0.0.1", 5588, "/ws", &dev_req, 1500) else {
        return (false, String::new(), 0, "0".to_string());
    };
    let d = unwrap_ws_data(&dev_resp);
    let list = d
        .as_array()
        .or_else(|| d["listGameDevices"]["list"].as_array())
        .or_else(|| d["list"].as_array());

    let Some(active) = list.and_then(|arr| {
        arr.iter().find(|x| {
            x["accState"].as_i64() == Some(1)
                || x["accState"].as_str() == Some("1")
                || x["accState"].as_str() == Some("started")
                || x["state"].as_i64() == Some(1)
        })
    }) else {
        return (false, String::new(), 0, "0".to_string());
    };

    let active_area = active["area"].as_str().unwrap_or("hk").to_lowercase();
    let info_req = json!({
        "cmd": "GETACCINFO", "tag": "2", "sn": "", "cli": "leinux",
        "code": 0, "err": "", "data": {"token": token}
    });
    let mut net_delay = 0i64;
    let mut loss_rate = "0".to_string();
    if let Ok(ainfo_resp) = ws_rpc_call("127.0.0.1", 5588, "/ws", &info_req, 1500) {
        let ainfo = unwrap_ws_data(&ainfo_resp);
        let obj = if ainfo["accInfo"].is_object() { &ainfo["accInfo"] } else { &ainfo };
        net_delay = obj["netDelay"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .or_else(|| obj["netDelay"].as_i64())
            .unwrap_or(0);
        loss_rate = obj["lossRate"]
            .as_str()
            .map(str::to_string)
            .or_else(|| obj["lossRate"].as_i64().map(|n| n.to_string()))
            .unwrap_or_else(|| "0".to_string());
    }

    (true, active_area, net_delay, loss_rate)
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

    let headers = [
        ("Content-Type", "application/json"),
        ("Origin", "https://www.leigod.com"),
        ("Referer", "https://www.leigod.com/"),
        (
            "User-Agent",
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
        ),
    ];
    curl_json("POST", &url, &headers, Some(&body), 6)
}

pub fn exchange_ptoken_for_web_token(p_token: &str) -> Option<(String, String)> {
    let p_token = p_token.trim();
    if p_token.is_empty() {
        return None;
    }
    let headers = [
        ("Content-Type", "application/json"),
        ("token", p_token),
        ("appId", APP_ID),
        ("version", "600"),
        ("reqChannel", "2"),
        ("platform", "2"),
        ("busiType", BUSI_TYPE),
    ];
    let d1 = curl_json(
        "POST",
        "https://opapi.xxghh.biz/u-mobile/getSpeedWebTokenSign",
        &headers,
        Some("{}"),
        5,
    )
    .ok()?;
    let ret_obj = d1["retData"].as_object()?;
    let uid = ret_obj
        .get("user_id")
        .and_then(|v| v.as_i64().map(|n| n.to_string()).or_else(|| v.as_str().map(str::to_string)))
        .unwrap_or_default();

    let clean_body: Map<String, Value> = ret_obj
        .iter()
        .filter(|(_, v)| !v.is_null())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let body_str = Value::Object(clean_body).to_string();

    let d2 = curl_json(
        "POST",
        "https://webapi.xxghh.biz/passport/web/token",
        &[("Content-Type", "application/json")],
        Some(&body_str),
        5,
    )
    .ok()?;
    let acct_tok = d2["data"]["account_token"].as_str()?.trim().to_string();
    if acct_tok.is_empty() {
        return None;
    }
    Some((acct_tok, uid))
}

pub fn ensure_router_bound(p_token: &str, user_id: &str, force: bool) -> bool {
    let p_token = p_token.trim();
    if p_token.is_empty() {
        return false;
    }
    let (_, cur_rtoken, cur_uid) = read_ini_values();
    let same_user = user_id.trim().is_empty() || cur_uid == user_id.trim();
    if !force && same_user && !cur_rtoken.is_empty() && !cur_rtoken.starts_with('<') {
        return true;
    }
    let uid = if !user_id.trim().is_empty() {
        user_id.trim().to_string()
    } else if !cur_uid.is_empty() {
        cur_uid
    } else if let Some((_, u)) = exchange_ptoken_for_web_token(p_token) {
        u
    } else {
        return false;
    };
    let req = json!({
        "cmd": "newBindRouter",
        "tag": "leinux-auto-bind",
        "sn": read_device_mac(),
        "data": {
            "captcha": "",
            "appId": APP_ID,
            "reqChannel": "2",
            "pToken": p_token,
            "checkUrl": "https://opapi1.xxghh.biz/speed",
            "userId": uid,
            "bindCode": "",
        }
    });
    ws_rpc_call("127.0.0.1", 5588, "/ws", &req, 6000)
        .map(|res| res["code"].as_i64() == Some(0))
        .unwrap_or(false)
}

pub fn webapi_post_with_auto_renew(endpoint: &str) -> Result<Value, String> {
    let mut cfg = load_account_config();
    let p_tok = resolve_p_token(&cfg);

    if !p_tok.is_empty() {
        let _ = ensure_router_bound(&p_tok, &cfg.user_id, false);
    }

    if cfg.account_token.trim().is_empty() && !p_tok.is_empty() {
        if let Some((new_tok, uid)) = exchange_ptoken_for_web_token(&p_tok) {
            cfg.account_token = new_tok;
            cfg.p_token = p_tok.clone();
            if !uid.is_empty() {
                cfg.user_id = uid;
            }
            save_account_config(&cfg);
        }
    }

    let acct_tok = cfg.account_token.trim().to_string();
    if acct_tok.is_empty() {
        return Err("请先配置官网 account_token 或移动端 pToken".to_string());
    }

    let first = webapi_post(endpoint, &acct_tok);
    if matches!(first, Ok(ref v) if v["code"].as_i64() == Some(0)) {
        return first;
    }

    if !p_tok.is_empty() {
        if let Some((new_tok, uid)) = exchange_ptoken_for_web_token(&p_tok) {
            cfg.account_token = new_tok.clone();
            cfg.p_token = p_tok;
            if !uid.is_empty() {
                cfg.user_id = uid;
            }
            save_account_config(&cfg);
            return webapi_post(endpoint, &new_tok);
        }
    }
    first
}

pub fn fetch_account_snapshot() -> AccountSnapshot {
    let cfg = load_account_config();
    let p_tok = resolve_p_token(&cfg);
    let gw = detect_gateway_ip();
    let detected_gateway_dns = if gw.is_empty() {
        "223.5.5.5:53".to_string()
    } else {
        format!("{gw}:53")
    };
    let configured = !cfg.account_token.trim().is_empty() || !p_tok.is_empty();
    if !configured {
        return AccountSnapshot {
            configured: false,
            paused: true,
            hours: 0,
            mins: 0,
            sub_text: "未配置 Token".to_string(),
            auto_pause_on_stop: cfg.auto_pause_on_stop,
            custom_dns: cfg.custom_dns,
            detected_gateway_dns,
        };
    }

    match webapi_post_with_auto_renew("/api/user/info") {
        Ok(res) if res["code"].as_i64() == Some(0) && res["data"].is_object() => {
            let d = &res["data"];
            let paused = d["pause_status_id"].as_i64().unwrap_or(1) == 1
                || d["pause_status_id"].as_str() == Some("1");
            let sec = d["expiry_time_samp"]
                .as_i64()
                .or_else(|| d["expiry_time_samp"].as_str().and_then(|s| s.parse().ok()))
                .or_else(|| d["user_pause_time"].as_i64())
                .unwrap_or(0);
            let nick = d["nickname"]
                .as_str()
                .or_else(|| d["mobile"].as_str())
                .unwrap_or("已认证")
                .to_string();
            AccountSnapshot {
                configured: true,
                paused,
                hours: sec / 3600,
                mins: (sec % 3600) / 60,
                sub_text: nick,
                auto_pause_on_stop: cfg.auto_pause_on_stop,
                custom_dns: cfg.custom_dns,
                detected_gateway_dns,
            }
        }
        _ => AccountSnapshot {
            configured: true,
            paused: true,
            hours: 0,
            mins: 0,
            sub_text: "Token 已过期".to_string(),
            auto_pause_on_stop: cfg.auto_pause_on_stop,
            custom_dns: cfg.custom_dns,
            detected_gateway_dns,
        },
    }
}

pub fn pause_account_time() -> Value {
    set_stop_enforced(true);
    let (_, router_token, _) = read_ini_values();
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
                "cli": "leinux",
                "code": 0,
                "err": "",
                "data": {"token": router_token, "catalogs": [2]},
            }),
            3000,
        );
    }
    run_cleanup_net();
    remove_mangle_guard();
    flush_dns_cache();

    let cfg = load_account_config();
    if cfg.account_token.trim().is_empty() && resolve_p_token(&cfg).is_empty() {
        return json!({
            "code": 0,
            "local_only": true,
            "msg": "已停止本地加速（未配置官网 Token，未暂停官网时长）"
        });
    }
    match webapi_post_with_auto_renew("/api/user/pause") {
        Ok(v) => v,
        Err(e) => json!({"code": -1, "msg": e}),
    }
}
