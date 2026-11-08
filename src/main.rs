mod daemon;
mod gui;

use serde_json::{json, Value};
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{self, Command, Stdio};
use std::thread;
use std::time::Duration;

pub const CONF_FILE: &str = "/etc/default/leigod-webui";
pub const SERVICES: [&str; 2] = ["leigod_plugin.service", "leigod_webui.service"];
pub const VALID_AREAS: [&str; 10] = ["auto", "hk", "jp", "kr", "tw", "sea", "us", "eu", "ru", "au"];

pub fn load_host_port() -> (String, u16) {
    let mut host = env::var("LEIGOD_WEBUI_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let mut port = env::var("LEIGOD_WEBUI_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(8088);

    if let Ok(content) = fs::read_to_string(CONF_FILE) {
        for raw in content.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                let k = k.trim();
                let v = v.trim().trim_matches(|c| c == '\'' || c == '"');
                if k == "LEIGOD_WEBUI_HOST" && !v.is_empty() {
                    host = v.to_string();
                } else if k == "LEIGOD_WEBUI_PORT" {
                    if let Ok(p) = v.parse::<u16>() {
                        port = p;
                    }
                }
            }
        }
    }
    (host, port)
}

pub fn http_request(method: &str, path: &str, body: Option<&Value>, timeout_ms: u64) -> Result<Value, String> {
    let (host, port) = load_host_port();
    let addr = format!("{host}:{port}")
        .parse()
        .map_err(|e| format!("Invalid address: {e}"))?;
    let timeout = Duration::from_millis(timeout_ms);
    let mut stream = TcpStream::connect_timeout(&addr, timeout).map_err(|e| e.to_string())?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));

    let payload = body.map(|b| b.to_string()).unwrap_or_default();
    let req = if method == "POST" {
        format!(
            "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        )
    } else {
        format!("GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n")
    };

    stream.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut resp_buf = Vec::new();
    stream.read_to_end(&mut resp_buf).map_err(|e| e.to_string())?;
    let resp_str = String::from_utf8_lossy(&resp_buf);
    let (_, body_str) = resp_str.split_once("\r\n\r\n").ok_or("Malformed HTTP response")?;
    serde_json::from_str(body_str.trim()).map_err(|e| format!("JSON parse error: {e}"))
}

pub fn service_active(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", unit])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn ensure_services() {
    if !SERVICES.iter().all(|u| service_active(u)) {
        let _ = Command::new("systemctl").arg("start").args(SERVICES).status();
    }
    for _ in 0..25 {
        if http_request("GET", "/api/host-status", None, 300).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn print_help() {
    println!(
        "用法: lgd [选项]  (或 leigod-webui [选项])

Leinux — 雷神加速器 Linux 原生 GTK4 客户端与命令行工具

选项:
  (无参数)             自动拉起后台服务并打开原生 GTK4 控制面板窗口
  --start [AREA]       免开界面直接恢复时长并启动加速 (默认: auto)
                       可选区服: {}
  --pause              仅断开加速隧道并暂停官网账号时长 (保持后台服务运行)
  --stop               暂停官网账号时长并停止后台服务
  --status             查看服务运行状态、当前加速区服与账号剩余时长
  --port <PORT>        修改本地守护服务端口 (写入 {CONF_FILE}) 并重启服务
  --daemon             以守护进程模式运行后台 RPC 与路由服务
  -h, --help           显示此帮助信息",
        VALID_AREAS.join(", ")
    );
}

fn cmd_start(area: &str) -> i32 {
    let area = area.trim().to_lowercase();
    ensure_services();
    let st = match http_request("GET", "/api/host-status", None, 5000) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("错误: 无法连接后台服务: {e}");
            return 1;
        }
    };

    let profiles = st["profiles"].as_array().cloned().unwrap_or_default();
    let Some(prof) = profiles
        .iter()
        .find(|p| p["area"].as_str().unwrap_or("").eq_ignore_ascii_case(&area))
    else {
        eprintln!("错误: 未知区服 '{area}'，可选区服: {}", VALID_AREAS.join(", "));
        return 1;
    };

    let _ = http_request("POST", "/api/account/recover", Some(&json!({})), 6000);
    let conf_val: Value = match &prof["conf"] {
        Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| json!({})),
        other => other.clone(),
    };

    let req = json!({
        "cmd": "STARTACC",
        "tag": "cli-start",
        "sn": "",
        "cli": "webui",
        "code": 0,
        "err": "",
        "data": {
            "token": st["token"].as_str().unwrap_or(""),
            "catalog": prof["catalog"].as_i64().unwrap_or(2),
            "subCatalog": prof["subCatalog"].as_i64().unwrap_or(0),
            "conf": conf_val,
        }
    });

    match http_request("POST", "/api/ws-rpc", Some(&req), 8000) {
        Ok(res) if res["code"].as_i64() == Some(0) => {
            println!(
                "已恢复时长并启动加速: {} ({area})",
                prof["name"].as_str().unwrap_or(&area)
            );
            0
        }
        Ok(res) => {
            eprintln!("启动加速失败: {res}");
            1
        }
        Err(e) => {
            eprintln!("启动加速失败: {e}");
            1
        }
    }
}

fn cmd_pause() -> i32 {
    ensure_services();
    match http_request("POST", "/api/account/pause", Some(&json!({})), 8000) {
        Ok(res) => {
            if res["local_only"].as_bool() == Some(true) {
                println!("{}", res["msg"].as_str().unwrap_or("已停止本地加速"));
            } else if res["code"].as_i64() == Some(0) {
                println!("已断开加速隧道并暂停雷神账号时长");
            } else {
                println!(
                    "已断开加速隧道，官网暂停返回: {}",
                    res["msg"].as_str().unwrap_or("请检查官网 Token")
                );
            }
            0
        }
        Err(e) => {
            eprintln!("暂停失败: {e}");
            1
        }
    }
}

fn cmd_stop() -> i32 {
    if service_active("leigod_webui.service") {
        let _ = http_request("POST", "/api/account/pause", Some(&json!({})), 5000);
    }
    let _ = Command::new("systemctl")
        .args(["stop", "leigod_webui.service", "leigod_plugin.service"])
        .status();
    println!("已暂停账号时长并停止 leigod_webui.service 与 leigod_plugin.service");
    0
}

fn cmd_status() -> i32 {
    if !service_active("leigod_webui.service") {
        println!("服务状态: 已停止 (可执行 'lgd' 或 'lgd --start hk' 启动)");
        return 0;
    }

    let (host, port) = load_host_port();
    let Ok(st) = http_request("GET", "/api/host-status", None, 5000) else {
        println!("服务状态: 异常 (无法连接 http://{host}:{port})");
        return 1;
    };

    let token = st["token"].as_str().unwrap_or("");
    let mut acc_str = "待加速".to_string();
    let mut delay_str = "- ms".to_string();

    if !token.is_empty() {
        if let Ok(dev_resp) = http_request(
            "POST",
            "/api/ws-rpc",
            Some(&json!({"cmd": "GETDEVICELIST", "tag": "1", "sn": "", "cli": "webui", "code": 0, "err": "", "data": {"token": token}})),
            4000,
        ) {
            let mut d = dev_resp["data"].clone();
            if let Some(s) = d.as_str() {
                if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                    d = parsed;
                }
            }
            let list = d
                .as_array()
                .cloned()
                .or_else(|| d["listGameDevices"]["list"].as_array().cloned())
                .or_else(|| d["list"].as_array().cloned())
                .unwrap_or_default();

            if let Some(active) = list.iter().find(|x| {
                x["accState"].as_i64() == Some(1)
                    || x["accState"].as_str() == Some("1")
                    || x["accState"].as_str() == Some("started")
                    || x["state"].as_i64() == Some(1)
            }) {
                let area = active["area"].as_str().unwrap_or("hk").to_lowercase();
                let name = st["profiles"]
                    .as_array()
                    .and_then(|arr| {
                        arr.iter()
                            .find(|p| p["area"].as_str().unwrap_or("").eq_ignore_ascii_case(&area))
                    })
                    .and_then(|p| p["name"].as_str())
                    .unwrap_or(&area);
                let tun = st["tun_interface"].as_str().unwrap_or("tun_PC");
                acc_str = format!("{name} · 加速中 ({tun})");

                if let Ok(ainfo_resp) = http_request(
                    "POST",
                    "/api/ws-rpc",
                    Some(&json!({"cmd": "GETACCINFO", "tag": "2", "sn": "", "cli": "webui", "code": 0, "err": "", "data": {"token": token}})),
                    4000,
                ) {
                    let mut ainfo = ainfo_resp["data"].clone();
                    if let Some(s) = ainfo.as_str() {
                        if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                            ainfo = parsed;
                        }
                    }
                    let nd = ainfo["accInfo"]["netDelay"]
                        .as_str()
                        .or_else(|| ainfo["netDelay"].as_str())
                        .and_then(|s| s.parse::<i64>().ok())
                        .or_else(|| ainfo["accInfo"]["netDelay"].as_i64())
                        .or_else(|| ainfo["netDelay"].as_i64())
                        .unwrap_or(0);
                    if nd > 0 {
                        delay_str = format!("{nd} ms");
                    }
                }
            }
        }
    }

    let mut acct_line = "未配置官网 Token".to_string();
    if let Ok(acct) = http_request("GET", "/api/account/status", None, 5000) {
        if acct["configured"].as_bool() == Some(true) {
            let info = &acct["response"]["data"];
            if info.is_object() {
                let paused = info["pause_status_id"].as_i64().unwrap_or(1) == 1
                    || info["pause_status_id"].as_str() == Some("1");
                let sec = info["expiry_time_samp"]
                    .as_i64()
                    .or_else(|| info["expiry_time_samp"].as_str().and_then(|s| s.parse().ok()))
                    .or_else(|| info["user_pause_time"].as_i64())
                    .unwrap_or(0);
                let state_lbl = if paused { "已暂停" } else { "计时中" };
                acct_line = format!("{state_lbl} (剩余 {} 小时 {} 分)", sec / 3600, (sec % 3600) / 60);
            }
        }
    }

    println!("服务状态: 运行中 (http://{host}:{port})");
    println!("加速状态: {acc_str} | 节点延迟: {delay_str}");
    println!("账号时长: {acct_line}");
    0
}

fn cmd_port(port_str: &str) -> i32 {
    let Ok(port) = port_str.parse::<u16>() else {
        eprintln!("错误: 请提供合法的端口号 (1-65535)，例如: lgd --port 8090");
        return 1;
    };
    if port == 0 {
        eprintln!("错误: 端口号必须在 1-65535 之间");
        return 1;
    }
    if unsafe { libc::geteuid() } != 0 {
        let exe = env::current_exe().unwrap_or_else(|_| Path::new("/usr/bin/lgd").to_path_buf());
        let err = Command::new("sudo").arg(exe).args(["--port", port_str]).exec();
        eprintln!("错误: 无法调用 sudo: {err}");
        return 1;
    }
    if let Some(parent) = Path::new(CONF_FILE).parent() {
        let _ = fs::create_dir_all(parent);
    }
    let content = format!("LEIGOD_WEBUI_HOST=127.0.0.1\nLEIGOD_WEBUI_PORT={port}\n");
    if let Err(e) = fs::write(CONF_FILE, content) {
        eprintln!("错误: 写入 {CONF_FILE} 失败: {e}");
        return 1;
    }
    let _ = Command::new("systemctl")
        .args(["restart", "leigod_webui.service"])
        .status();
    println!("已将守护服务端口更新为 {port} 并重启服务: http://127.0.0.1:{port}");
    0
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("-h" | "--help") => {
            print_help();
        }
        Some("--daemon") => {
            daemon::run_daemon();
        }
        Some("--start") => {
            let area = args.get(1).map(|s| s.as_str()).unwrap_or("auto");
            process::exit(cmd_start(area));
        }
        Some("--pause") => {
            process::exit(cmd_pause());
        }
        Some("--stop") => {
            process::exit(cmd_stop());
        }
        Some("--status") => {
            process::exit(cmd_status());
        }
        Some("--port") => {
            let Some(p) = args.get(1) else {
                eprintln!("错误: --port 需要指定端口号");
                process::exit(1);
            };
            process::exit(cmd_port(p));
        }
        Some(other) if other.starts_with("--host") => {
            daemon::run_daemon();
        }
        Some(other) => {
            eprintln!("未知选项: {other}\n");
            print_help();
            process::exit(1);
        }
        None => {
            ensure_services();
            if env::var_os("DISPLAY").is_some() || env::var_os("WAYLAND_DISPLAY").is_some() {
                gui::run_gui();
            } else {
                let (host, port) = load_host_port();
                let _ = Command::new("xdg-open")
                    .arg(format!("http://{host}:{port}"))
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
                println!("雷神服务已就绪: http://{host}:{port}");
            }
        }
    }
}
