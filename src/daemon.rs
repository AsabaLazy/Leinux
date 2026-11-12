use crate::net::{
    attach_pid_to_game_cgroup, check_firewall_periodic, check_plugin_service,
    count_game_cgroup_procs, flush_dns_cache, has_active_tun, install_mangle_guard, is_scoped_mode,
    list_tun_interfaces, probe_udp_6066, read_iface_bytes, read_ini_values,
    resolve_local_dns_list, run_cleanup_net, set_scoped_mode, set_stop_enforced,
    unregister_scoped_root_pid,
};
use crate::profiles::{
    invalidate_profile_cache, prepare_live_conf, refresh_cloud_profiles, sanitize_acc_conf,
    scan_profiles,
};
use crate::rpc::{
    core_rpc, ensure_router_bound, fetch_account_snapshot, is_valid_ptoken, load_account_config,
    pause_account_time, query_active_acc_state, refresh_web_token, resolve_p_token,
    save_account_config, webapi_post_with_auto_renew,
};
use crate::types::{DaemonSnapshot, ProfileSummary};
use crate::{load_host_port, CORE_SERVICE};
use serde::Serialize;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

const MAX_BODY_BYTES: usize = 512 * 1024;
const HTTP_WORKERS: usize = 6;

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

fn send_json<T: Serialize>(mut stream: TcpStream, status: u16, val: &T) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        _ => "OK",
    };
    let body = serde_json::to_vec(val).unwrap_or_else(|_| b"{}".to_vec());
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(&body);
}

fn parse_query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then_some(v)
    })
}

fn build_daemon_snapshot(known_rev: u64, include_account: bool) -> DaemonSnapshot {
    let (service_active, service_state) = check_plugin_service();
    let tuns = list_tun_interfaces();
    let (tun_interface, traffic_iface, rx_bytes, tx_bytes) = if let Some(first) = tuns.first() {
        let (rx, tx) = read_iface_bytes(first);
        (tuns.join(", "), first.clone(), rx, tx)
    } else {
        ("未创建 (开启加速后生成)".to_string(), String::new(), 0, 0)
    };

    let ini = read_ini_values();
    let (stored_profiles, rev) = scan_profiles();
    let acc = query_active_acc_state(&ini.rtoken);
    let last_area = load_account_config().last_area;

    let profiles = (known_rev != rev || known_rev == 0)
        .then(|| stored_profiles.into_iter().map(|p| p.summary).collect::<Vec<ProfileSummary>>());

    DaemonSnapshot {
        service_active,
        service_state,
        tun_interface,
        traffic_iface,
        rx_bytes,
        tx_bytes,
        udp_6066: probe_udp_6066().to_string(),
        version: ini.version,
        is_accelerating: acc.is_accelerating,
        scoped_procs: if is_scoped_mode() { count_game_cgroup_procs(None) } else { 0 },
        active_area: acc.active_area,
        last_area,
        net_delay: acc.net_delay,
        loss_rate: acc.loss_rate,
        profiles_rev: rev,
        profiles,
        account: include_account.then(fetch_account_snapshot),
    }
}

fn wait_for_tun_ready() {
    for _ in 0..45 {
        if has_active_tun() {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn execute_start_for_conf(catalog: i64, sub_catalog: i64, raw_conf: &Value) -> Result<Value, String> {
    set_stop_enforced(false);
    let acct_cfg = load_account_config();
    let p_tok = resolve_p_token(&acct_cfg);
    if !p_tok.is_empty() {
        let _ = ensure_router_bound(&p_tok, &acct_cfg.user_id, false);
    }

    let ini = read_ini_values();
    let uid = if !acct_cfg.user_id.trim().is_empty() { &acct_cfg.user_id } else { &ini.user_id };

    let dns_list = resolve_local_dns_list(&acct_cfg.custom_dns);
    let (sanitized, _) = sanitize_acc_conf(raw_conf, &dns_list);
    let area = sanitized["game"]["area"].as_str().unwrap_or("").trim().to_lowercase();
    if !area.is_empty() && acct_cfg.last_area != area {
        let mut updated_cfg = acct_cfg.clone();
        updated_cfg.last_area = area;
        save_account_config(&updated_cfg);
    }
    let live_conf = prepare_live_conf(sanitized, &p_tok, uid);

    install_mangle_guard();
    let resp = core_rpc(
        "STARTACC",
        json!({
            "token": ini.rtoken,
            "catalog": catalog,
            "subCatalog": sub_catalog,
            "conf": live_conf,
        }),
        5000,
    )?;
    if resp["code"].as_i64() == Some(0) {
        wait_for_tun_ready();
    }
    install_mangle_guard();
    flush_dns_cache();
    Ok(resp)
}

fn handle_acc_start(req_json: &Value) -> (u16, Value) {
    let mut area = req_json["area"].as_str().unwrap_or("").trim().to_lowercase();
    if area.is_empty() {
        area = load_account_config().last_area.trim().to_lowercase();
    }
    if area.is_empty() {
        return (
            400,
            json!({
                "ok": false,
                "msg": "未选择加速区服，首次使用请先选择区服（或执行 lgd -a <区服> <命令>，可用 lgd --list 查看），后续将自动记住上次选择"
            }),
        );
    }
    let (profiles, _) = scan_profiles();
    let Some(prof) = profiles.iter().find(|p| p.summary.area.eq_ignore_ascii_case(&area)) else {
        let avail: Vec<&str> = profiles.iter().map(|p| p.summary.area.as_str()).collect();
        return (
            400,
            json!({"ok": false, "msg": format!("未知区服 '{area}'，当前可用区服: {}", avail.join(", "))}),
        );
    };

    let cgroup_pid = req_json["cgroup_pid"].as_u64().and_then(|n| u32::try_from(n).ok());
    if let Some(pid) = cgroup_pid {
        if let Err(msg) = attach_pid_to_game_cgroup(pid) {
            return (500, json!({"ok": false, "msg": msg}));
        }
        thread::spawn(|| {
            for _ in 0..24 {
                thread::sleep(Duration::from_millis(250));
                if !is_scoped_mode() {
                    break;
                }
                let _ = count_game_cgroup_procs(None);
            }
        });
    } else if count_game_cgroup_procs(None) == 0 {
        set_scoped_mode(false);
    }

    if req_json["recover"].as_bool().unwrap_or(true) {
        let _ = webapi_post_with_auto_renew("/api/user/recover");
    }

    if cgroup_pid.is_some() && has_active_tun() {
        let cur = query_active_acc_state(&read_ini_values().rtoken);
        if cur.is_accelerating && cur.active_area.eq_ignore_ascii_case(&prof.summary.area) {
            install_mangle_guard();
            return (
                200,
                json!({
                    "ok": true,
                    "code": 0,
                    "area": prof.summary.area,
                    "name": prof.summary.name,
                }),
            );
        }
    }

    match execute_start_for_conf(prof.summary.catalog, prof.summary.sub_catalog, &prof.conf) {
        Ok(resp) if resp["code"].as_i64() == Some(0) => (
            200,
            json!({
                "ok": true,
                "code": 0,
                "area": prof.summary.area,
                "name": prof.summary.name,
            }),
        ),
        Ok(resp) => {
            if cgroup_pid.is_some() && count_game_cgroup_procs(cgroup_pid) == 0 {
                set_scoped_mode(false);
            }
            (502, json!({"ok": false, "code": resp["code"], "msg": "启动加速失败", "rpc": resp}))
        }
        Err(e) => {
            if cgroup_pid.is_some() && count_game_cgroup_procs(cgroup_pid) == 0 {
                set_scoped_mode(false);
            }
            (502, json!({"ok": false, "code": -1, "msg": e}))
        }
    }
}

fn handle_release_cgroup(req_json: &Value) -> Value {
    let exclude_pid = req_json["pid"].as_u64().and_then(|n| u32::try_from(n).ok());
    if let Some(pid) = exclude_pid {
        unregister_scoped_root_pid(pid);
    }
    let remaining = count_game_cgroup_procs(exclude_pid);
    if is_scoped_mode() && remaining == 0 {
        let pause_res = pause_account_time();
        json!({ "ok": true, "paused": true, "remaining": 0, "pause": pause_res })
    } else {
        json!({ "ok": true, "paused": false, "remaining": remaining })
    }
}

fn handle_device_repair() -> (u16, Value) {
    let cfg = load_account_config();
    let p_tok = resolve_p_token(&cfg);
    if !p_tok.is_empty() {
        let _ = ensure_router_bound(&p_tok, &cfg.user_id, true);
    }
    let ini = read_ini_values();
    match core_rpc("AUTOREPAIRBOUND", json!({"rToken": ini.rtoken, "userId": ini.user_id}), 5000) {
        Ok(resp) => (200, resp),
        Err(e) => (502, json!({"code": -1, "err": format!("连接 127.0.0.1:5588/ws 失败: {e}")})),
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
            ["account_token", "p_token", "token"]
                .into_iter()
                .find_map(|k| v[k].as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| trimmed.to_string())
}

fn handle_save_token(req_json: &Value) -> Value {
    let mut cfg = load_account_config();
    let mut need_web_check = false;

    if let Some(tok) = req_json["account_token"]
        .as_str()
        .map(extract_raw_token)
        .filter(|t| !t.is_empty())
    {
        need_web_check = true;
        if is_valid_ptoken(&tok) {
            cfg.p_token = tok.clone();
            if refresh_web_token(&mut cfg, &tok).is_some() && !cfg.user_id.is_empty() {
                let _ = ensure_router_bound(&tok, &cfg.user_id, false);
            }
        } else {
            cfg.account_token = tok;
            let p_tok = resolve_p_token(&cfg);
            if !p_tok.is_empty() {
                cfg.p_token = p_tok;
            }
        }
    }
    if let Some(ap) = req_json["auto_pause_on_stop"].as_bool() {
        cfg.auto_pause_on_stop = ap;
    }
    if let Some(la) = req_json.get("last_area").and_then(Value::as_str) {
        cfg.last_area = la.trim().to_lowercase();
    }
    let dns_changed = req_json
        .get("custom_dns")
        .and_then(Value::as_str)
        .map(|cdns| cfg.custom_dns = cdns.trim().to_string())
        .is_some();

    save_account_config(&cfg);
    if need_web_check || dns_changed {
        invalidate_profile_cache();
    }
    if dns_changed {
        let _ = scan_profiles();
    }

    let info = if need_web_check {
        webapi_post_with_auto_renew("/api/user/info").unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };
    json!({
        "ok": true,
        "auto_pause_on_stop": cfg.auto_pause_on_stop,
        "custom_dns": cfg.custom_dns,
        "last_area": cfg.last_area,
        "response": info,
    })
}

enum HttpReadResult {
    Ok {
        method: String,
        path: String,
        query: String,
        body: Value,
    },
    TooLarge,
    Invalid,
}

fn read_http_request(stream: &TcpStream) -> HttpReadResult {
    let Ok(cloned) = stream.try_clone() else { return HttpReadResult::Invalid };
    let mut reader = BufReader::new(cloned);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() || request_line.is_empty() {
        return HttpReadResult::Invalid;
    }
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(raw_target)) = (parts.next(), parts.next()) else {
        return HttpReadResult::Invalid;
    };
    let (path, query) = raw_target.split_once('?').unwrap_or((raw_target, ""));

    let mut content_length: usize = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || matches!(line.as_str(), "" | "\n" | "\r\n") {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
    }

    if content_length > MAX_BODY_BYTES {
        return HttpReadResult::TooLarge;
    }
    let mut body_bytes = vec![0u8; content_length];
    if content_length > 0 && reader.read_exact(&mut body_bytes).is_err() {
        return HttpReadResult::Invalid;
    }
    let body = if body_bytes.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&body_bytes).unwrap_or_else(|_| json!({}))
    };

    HttpReadResult::Ok {
        method: method.to_string(),
        path: path.to_string(),
        query: query.to_string(),
        body,
    }
}

fn handle_client(stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

    let (method, path, query, req_json) = match read_http_request(&stream) {
        HttpReadResult::Ok { method, path, query, body } => (method, path, query, body),
        HttpReadResult::TooLarge => {
            send_json(stream, 413, &json!({"error": "Payload too large"}));
            return;
        }
        HttpReadResult::Invalid => return,
    };

    match (method.as_str(), path.as_str()) {
        ("GET", "/api/status") => {
            let known_rev = parse_query_param(&query, "known_rev")
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let include_acct = parse_query_param(&query, "include_account") == Some("1");
            send_json(stream, 200, &build_daemon_snapshot(known_rev, include_acct));
        }
        ("POST", "/api/acc/start") => {
            let (code, val) = handle_acc_start(&req_json);
            send_json(stream, code, &val);
        }
        ("POST", "/api/acc/release-cgroup") => {
            send_json(stream, 200, &handle_release_cgroup(&req_json));
        }
        ("POST", "/api/device/repair") => {
            let (code, val) = handle_device_repair();
            send_json(stream, code, &val);
        }
        ("POST", "/api/cleanup-net") => {
            run_cleanup_net();
            send_json(
                stream,
                200,
                &json!({"ok": true, "msg": "已清理 GAMEACC 链、策略路由表 99、ipset 与残留 tun 接口"}),
            );
        }
        ("POST", "/api/profiles/refresh") => match refresh_cloud_profiles(8) {
            Ok(n) => send_json(
                stream,
                200,
                &json!({"ok": true, "count": n, "msg": format!("已从雷神云端同步 {n} 个区服最新节点池")}),
            ),
            Err(msg) => send_json(stream, 502, &json!({"ok": false, "msg": msg})),
        },
        ("POST", "/api/service-restart") => {
            let ok = Command::new("systemctl")
                .args(["restart", CORE_SERVICE])
                .status()
                .is_ok_and(|s| s.success());
            let msg = if ok {
                format!("已安全重启 {CORE_SERVICE} 守护服务")
            } else {
                "重启服务失败".to_string()
            };
            send_json(stream, if ok { 200 } else { 500 }, &json!({"ok": ok, "msg": msg}));
        }
        ("POST", "/api/account/save-token") => send_json(stream, 200, &handle_save_token(&req_json)),
        ("POST", "/api/account/pause") => send_json(stream, 200, &pause_account_time()),
        ("POST", "/api/account/recover") => match webapi_post_with_auto_renew("/api/user/recover") {
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
        thread::spawn(move || {
            while let Some(stream) = worker_rx.lock().ok().and_then(|g| g.recv().ok()) {
                handle_client(stream);
            }
        });
    }

    let wake_addr = bind_addr.clone();
    thread::spawn(move || {
        let mut tick = 0u32;
        let mut empty_scoped_ticks = 0u8;
        loop {
            if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
                let _ = TcpStream::connect(&wake_addr);
                break;
            }
            if tick.is_multiple_of(10) {
                let _ = scan_profiles();
                if is_scoped_mode() {
                    if count_game_cgroup_procs(None) == 0 {
                        empty_scoped_ticks = empty_scoped_ticks.saturating_add(1);
                        if empty_scoped_ticks >= 2 {
                            empty_scoped_ticks = 0;
                            let _ = pause_account_time();
                        }
                    } else {
                        empty_scoped_ticks = 0;
                    }
                } else {
                    empty_scoped_ticks = 0;
                }
                check_firewall_periodic(tick.is_multiple_of(150));
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
            let _ = tx.try_send(stream);
        }
    }

    let cfg = load_account_config();
    if (!cfg.account_token.trim().is_empty() || !resolve_p_token(&cfg).is_empty())
        && cfg.auto_pause_on_stop
    {
        let _ = pause_account_time();
    }
}
