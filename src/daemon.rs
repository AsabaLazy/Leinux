use crate::load_host_port;
use crate::net::{
    check_firewall_residuals, check_plugin_service, detect_gateway_ip, flush_dns_cache,
    has_active_tun, install_mangle_guard, list_tun_interfaces, probe_udp_6066, read_device_mac,
    read_iface_bytes, read_ini_values, remove_mangle_guard, resolve_local_dns_list,
    run_cleanup_net, set_stop_enforced,
};
use crate::profiles::{
    invalidate_profile_cache, load_saved_profiles, prepare_live_conf, refresh_cloud_profiles,
    sanitize_acc_conf, scan_profiles,
};
use crate::rpc::{
    ensure_router_bound, exchange_ptoken_for_web_token, fetch_account_snapshot, is_valid_ptoken,
    load_account_config, pause_account_time, query_active_acc_state, resolve_p_token,
    save_account_config, webapi_post_with_auto_renew, ws_rpc_call,
};
use crate::types::{DaemonSnapshot, ProfileSummary, LEIGOD_DIR};
use serde::Serialize;
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

const MAX_BODY_BYTES: usize = 512 * 1024;
const HTTP_WORKERS: usize = 6;

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

fn send_http(mut stream: TcpStream, status: u16, content_type: &str, body: &[u8]) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Payload Too Large",
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

fn send_json<T: Serialize>(stream: TcpStream, status: u16, val: &T) {
    let body = serde_json::to_vec(val).unwrap_or_else(|_| b"{}".to_vec());
    send_http(stream, status, "application/json; charset=utf-8", &body);
}

fn parse_query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        if k == key { Some(v) } else { None }
    })
}

fn build_daemon_snapshot(known_rev: u64, include_account: bool) -> DaemonSnapshot {
    let (service_active, service_state) = check_plugin_service();
    let tuns = list_tun_interfaces();
    let (tun_interface, traffic_iface, rx_bytes, tx_bytes) = if let Some(first) = tuns.first() {
        let (rx, tx) = read_iface_bytes(first);
        (tuns.join(", "), first.clone(), rx, tx)
    } else {
        ("未创建 (开启加速后生成)".to_string(), "tun_PC".to_string(), 0, 0)
    };

    let (version, token, user_id) = read_ini_values();
    let has_gameacc_chain = check_firewall_residuals();
    let (stored_profiles, rev) = scan_profiles();
    let (is_accelerating, active_area, net_delay, loss_rate) = query_active_acc_state(&token);

    let profiles = if known_rev == rev && known_rev != 0 {
        None
    } else {
        Some(stored_profiles.into_iter().map(|p| p.summary).collect::<Vec<ProfileSummary>>())
    };
    let account = if include_account { Some(fetch_account_snapshot()) } else { None };

    DaemonSnapshot {
        service_active,
        service_state,
        mac: read_device_mac(),
        tun_interface,
        traffic_iface,
        rx_bytes,
        tx_bytes,
        has_gameacc_chain,
        udp_6066: probe_udp_6066().to_string(),
        version,
        token,
        user_id,
        is_accelerating,
        active_area,
        net_delay,
        loss_rate,
        profiles_rev: rev,
        profiles,
        account,
    }
}

fn build_legacy_host_status() -> Value {
    let snap = build_daemon_snapshot(0, false);
    let (stored_profiles, _) = scan_profiles();
    let acct_cfg = load_account_config();
    let has_account_token =
        !acct_cfg.account_token.trim().is_empty() || !resolve_p_token(&acct_cfg).is_empty();

    json!({
        "service_active": snap.service_active,
        "service_state": snap.service_state,
        "mac": snap.mac,
        "tun_interface": snap.tun_interface,
        "traffic_iface": snap.traffic_iface,
        "rx_bytes": snap.rx_bytes,
        "tx_bytes": snap.tx_bytes,
        "has_gameacc_chain": snap.has_gameacc_chain,
        "udp_6066": snap.udp_6066,
        "version": snap.version,
        "token": snap.token,
        "user_id": snap.user_id,
        "profiles": stored_profiles,
        "has_account_token": has_account_token,
        "auto_pause_on_stop": acct_cfg.auto_pause_on_stop,
    })
}

fn wait_for_tun_ready() {
    for _ in 0..25 {
        if has_active_tun() {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn execute_start_for_conf(
    mut rtoken: String,
    catalog: i64,
    sub_catalog: i64,
    raw_conf: &Value,
) -> Result<Value, String> {
    set_stop_enforced(false);
    let acct_cfg = load_account_config();
    let p_tok = resolve_p_token(&acct_cfg);
    if !p_tok.is_empty() && ensure_router_bound(&p_tok, &acct_cfg.user_id, false) {
        let (_, new_rtok, _) = read_ini_values();
        if !new_rtok.is_empty() {
            rtoken = new_rtok;
        }
    }
    let (_, ini_rtok, ini_uid) = read_ini_values();
    if rtoken.is_empty() {
        rtoken = ini_rtok;
    }
    let uid = if !acct_cfg.user_id.trim().is_empty() { &acct_cfg.user_id } else { &ini_uid };

    let dns_list = resolve_local_dns_list(&acct_cfg.custom_dns);
    let (sanitized, _) = sanitize_acc_conf(raw_conf, &dns_list);
    let live_conf = prepare_live_conf(sanitized, &p_tok, uid);

    install_mangle_guard();
    let req = json!({
        "cmd": "STARTACC",
        "tag": "daemon-start",
        "sn": "",
        "cli": "webui",
        "code": 0,
        "err": "",
        "data": {
            "token": rtoken,
            "catalog": catalog,
            "subCatalog": sub_catalog,
            "conf": live_conf,
        }
    });

    let resp = ws_rpc_call("127.0.0.1", 5588, "/ws", &req, 5000)?;
    if resp["code"].as_i64() == Some(0) {
        wait_for_tun_ready();
    }
    install_mangle_guard();
    flush_dns_cache();
    Ok(resp)
}

fn handle_acc_start(req_json: &Value) -> (u16, Value) {
    let area = req_json["area"].as_str().unwrap_or("hk").trim().to_lowercase();
    let (profiles, _) = scan_profiles();
    let Some(prof) = profiles.iter().find(|p| p.summary.area.eq_ignore_ascii_case(&area)) else {
        let avail: Vec<&str> = profiles.iter().map(|p| p.summary.area.as_str()).collect();
        return (
            400,
            json!({"ok": false, "msg": format!("未知区服 '{area}'，当前可用区服: {}", avail.join(", "))}),
        );
    };

    if req_json["recover"].as_bool().unwrap_or(true) {
        let _ = webapi_post_with_auto_renew("/api/user/recover");
    }

    let (_, rtoken, _) = read_ini_values();
    match execute_start_for_conf(rtoken, prof.summary.catalog, prof.summary.sub_catalog, &prof.conf) {
        Ok(resp) if resp["code"].as_i64() == Some(0) => (
            200,
            json!({
                "ok": true,
                "code": 0,
                "area": prof.summary.area,
                "name": prof.summary.name,
            }),
        ),
        Ok(resp) => (502, json!({"ok": false, "code": resp["code"], "msg": "启动加速失败", "rpc": resp})),
        Err(e) => (502, json!({"ok": false, "code": -1, "msg": e})),
    }
}

fn handle_ws_rpc(mut req_obj: Value) -> (u16, Value) {
    let cmd_upper = req_obj["cmd"].as_str().unwrap_or("").to_uppercase();
    if BLOCKED_ROUTER_CMDS.contains(&cmd_upper.as_str()) {
        return (
            403,
            json!({
                "cmd": cmd_upper,
                "code": -1,
                "err": format!("已拦截高危路由器固件指令 {cmd_upper}"),
                "data": {},
            }),
        );
    }

    if cmd_upper == "AUTOREPAIRBOUND" {
        let cfg = load_account_config();
        let p_tok = resolve_p_token(&cfg);
        if !p_tok.is_empty() {
            let _ = ensure_router_bound(&p_tok, &cfg.user_id, true);
            let (_, new_rtok, new_uid) = read_ini_values();
            if !new_rtok.is_empty() {
                req_obj["data"]["rToken"] = json!(new_rtok);
            }
            if !new_uid.is_empty() {
                req_obj["data"]["userId"] = json!(new_uid);
            }
        }
    }

    if matches!(cmd_upper.as_str(), "STARTACC" | "START") {
        let rtoken = req_obj["data"]["token"].as_str().unwrap_or("").to_string();
        let catalog = req_obj["data"]["catalog"].as_i64().unwrap_or(2);
        let sub_cat = req_obj["data"]["subCatalog"].as_i64().unwrap_or(0);
        return match execute_start_for_conf(rtoken, catalog, sub_cat, &req_obj["data"]["conf"]) {
            Ok(resp) => (200, resp),
            Err(e) => (
                502,
                json!({"cmd": "ERROR", "code": -1, "err": format!("连接 127.0.0.1:5588/ws 失败: {e}"), "data": {}}),
            ),
        };
    }

    match ws_rpc_call("127.0.0.1", 5588, "/ws", &req_obj, 5000) {
        Ok(resp) => {
            if matches!(cmd_upper.as_str(), "STOPACC" | "STOP") {
                set_stop_enforced(true);
                run_cleanup_net();
                remove_mangle_guard();
                flush_dns_cache();
            }
            (200, resp)
        }
        Err(e) => (
            502,
            json!({"cmd": "ERROR", "code": -1, "err": format!("连接 127.0.0.1:5588/ws 失败: {e}"), "data": {}}),
        ),
    }
}

fn extract_raw_token(input: &str) -> String {
    let trimmed = input.trim();
    if !trimmed.starts_with('{') {
        return trimmed.to_string();
    }
    serde_json::from_str::<Value>(trimmed)
        .ok()
        .and_then(|v| {
            v["account_token"]
                .as_str()
                .or_else(|| v["p_token"].as_str())
                .or_else(|| v["token"].as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| trimmed.to_string())
}

fn handle_save_token(req_json: &Value) -> Value {
    let mut cfg = load_account_config();
    if let Some(raw_tok) = req_json["account_token"].as_str() {
        let tok = extract_raw_token(raw_tok);
        if !tok.is_empty() {
            if is_valid_ptoken(&tok) {
                cfg.p_token = tok.clone();
                if let Some((web_tok, uid)) = exchange_ptoken_for_web_token(&tok) {
                    cfg.account_token = web_tok;
                    if !uid.is_empty() {
                        cfg.user_id = uid.clone();
                        let _ = ensure_router_bound(&tok, &uid, false);
                    }
                }
            } else {
                cfg.account_token = tok;
                let p_tok = resolve_p_token(&cfg);
                if !p_tok.is_empty() {
                    cfg.p_token = p_tok;
                }
            }
        }
    }
    if let Some(ap) = req_json["auto_pause_on_stop"].as_bool() {
        cfg.auto_pause_on_stop = ap;
    }
    let dns_changed = if let Some(cdns) = req_json.get("custom_dns").and_then(Value::as_str) {
        cfg.custom_dns = cdns.trim().to_string();
        true
    } else {
        false
    };

    save_account_config(&cfg);
    invalidate_profile_cache();
    if dns_changed {
        let _ = scan_profiles();
    }

    let info = webapi_post_with_auto_renew("/api/user/info").unwrap_or_else(|_| json!({}));
    json!({
        "ok": true,
        "auto_pause_on_stop": cfg.auto_pause_on_stop,
        "custom_dns": cfg.custom_dns,
        "response": info,
    })
}

fn handle_account_status() -> (u16, Value) {
    let cfg = load_account_config();
    let p_tok = resolve_p_token(&cfg);
    let gw = detect_gateway_ip();
    let gw_dns = if gw.is_empty() { "223.5.5.5:53".to_string() } else { format!("{gw}:53") };

    if cfg.account_token.trim().is_empty() && p_tok.is_empty() {
        return (
            200,
            json!({
                "configured": false,
                "auto_pause_on_stop": cfg.auto_pause_on_stop,
                "custom_dns": cfg.custom_dns,
                "detected_gateway_dns": gw_dns,
            }),
        );
    }
    match webapi_post_with_auto_renew("/api/user/info") {
        Ok(res) => (
            200,
            json!({
                "configured": true,
                "auto_pause_on_stop": cfg.auto_pause_on_stop,
                "custom_dns": cfg.custom_dns,
                "detected_gateway_dns": gw_dns,
                "response": res,
            }),
        ),
        Err(e) => (502, json!({"configured": true, "error": e})),
    }
}

fn handle_client(stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

    let Ok(cloned) = stream.try_clone() else { return };
    let mut reader = BufReader::new(cloned);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() || request_line.is_empty() {
        return;
    }
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(raw_target)) = (parts.next(), parts.next()) else { return };
    let (path, query) = raw_target.split_once('?').unwrap_or((raw_target, ""));

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

    if content_length > MAX_BODY_BYTES {
        send_json(stream, 413, &json!({"error": "Payload too large"}));
        return;
    }
    let mut body_bytes = vec![0u8; content_length];
    if content_length > 0 && reader.read_exact(&mut body_bytes).is_err() {
        return;
    }

    if method == "GET" {
        match path {
            "/" | "/index.html" => {
                let html = fs::read(Path::new(LEIGOD_DIR).join("webui/index.html"))
                    .unwrap_or_else(|_| b"Leinux daemon running".to_vec());
                send_http(stream, 200, "text/html; charset=utf-8", &html);
            }
            "/api/status" => {
                let known_rev = parse_query_param(query, "known_rev")
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                let include_acct = parse_query_param(query, "include_account") == Some("1");
                send_json(stream, 200, &build_daemon_snapshot(known_rev, include_acct));
            }
            "/api/host-status" => send_json(stream, 200, &build_legacy_host_status()),
            "/api/profiles" => {
                let (profiles, _) = scan_profiles();
                send_json(stream, 200, &json!({"profiles": profiles}));
            }
            "/api/account/status" => {
                let (code, val) = handle_account_status();
                send_json(stream, code, &val);
            }
            _ => send_json(stream, 404, &json!({"error": "Not found"})),
        }
        return;
    }

    if method != "POST" {
        send_json(stream, 404, &json!({"error": "Method not allowed"}));
        return;
    }

    let req_json: Value = if body_bytes.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&body_bytes).unwrap_or_else(|_| json!({}))
    };

    match path {
        "/api/acc/start" => {
            let (code, val) = handle_acc_start(&req_json);
            send_json(stream, code, &val);
        }
        "/api/ws-rpc" => {
            let (code, val) = handle_ws_rpc(req_json);
            send_json(stream, code, &val);
        }
        "/api/cleanup-net" => {
            run_cleanup_net();
            send_json(
                stream,
                200,
                &json!({"ok": true, "msg": "已清理 GAMEACC 链、策略路由表 99、ipset 与残留 tun 接口"}),
            );
        }
        "/api/profiles/refresh" => match refresh_cloud_profiles(8) {
            Ok(n) => send_json(
                stream,
                200,
                &json!({"ok": true, "count": n, "msg": format!("已从雷神云端同步 {n} 个区服最新节点池")}),
            ),
            Err(msg) => send_json(stream, 502, &json!({"ok": false, "msg": msg})),
        },
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
        "/api/account/save-token" => send_json(stream, 200, &handle_save_token(&req_json)),
        "/api/account/pause" => send_json(stream, 200, &pause_account_time()),
        "/api/account/recover" => match webapi_post_with_auto_renew("/api/user/recover") {
            Ok(res) => send_json(stream, 200, &res),
            Err(e) => send_json(stream, 502, &json!({"code": -1, "msg": e})),
        },
        _ => send_json(stream, 404, &json!({"error": "Not found"})),
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
    let bind_addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&bind_addr).expect("Failed to bind daemon HTTP port");

    let (tx, rx) = mpsc::sync_channel::<TcpStream>(32);
    let shared_rx = Arc::new(Mutex::new(rx));
    for _ in 0..HTTP_WORKERS {
        let worker_rx = Arc::clone(&shared_rx);
        thread::spawn(move || loop {
            let Some(stream) = worker_rx.lock().ok().and_then(|g| g.recv().ok()) else { break };
            handle_client(stream);
        });
    }

    let wake_addr = bind_addr.clone();
    thread::spawn(move || {
        let mut tick = 0u32;
        loop {
            if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
                let _ = TcpStream::connect(&wake_addr);
                break;
            }
            if tick.is_multiple_of(10) {
                let _ = scan_profiles();
                let _ = check_firewall_residuals();
            }
            tick = tick.wrapping_add(1);
            thread::sleep(Duration::from_millis(200));
        }
    });

    println!("Leinux daemon running at http://{bind_addr}");
    for incoming in listener.incoming() {
        if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
            break;
        }
        if let Ok(stream) = incoming {
            if tx.try_send(stream).is_err() {
                // Queue full: drop connection cleanly rather than spawning unbounded threads.
            }
        }
    }

    let _ = load_saved_profiles();
    let cfg = load_account_config();
    if (!cfg.account_token.trim().is_empty() || !resolve_p_token(&cfg).is_empty())
        && cfg.auto_pause_on_stop
    {
        let _ = pause_account_time();
    }
}
