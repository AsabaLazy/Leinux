mod daemon;
mod gui;
mod login;
mod net;
mod profiles;
mod rpc;
mod types;

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
use types::{DaemonSnapshot, CONTINENT_ORDER};

pub const CONF_FILE: &str = "/etc/default/leigod-webui";
pub const SERVICES: [&str; 2] = ["leigod_plugin.service", "leigod_webui.service"];

pub fn load_host_port() -> (String, u16) {
    let mut host = env::var("LEIGOD_WEBUI_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let mut port = env::var("LEIGOD_WEBUI_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(8088);

    if let Ok(content) = fs::read_to_string(CONF_FILE) {
        for (k, v) in content
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .filter_map(|l| l.split_once('='))
        {
            let val = v.trim().trim_matches(|c| c == '\'' || c == '"');
            match k.trim() {
                "LEIGOD_WEBUI_HOST" if !val.is_empty() => host = val.to_string(),
                "LEIGOD_WEBUI_PORT" => {
                    if let Ok(p) = val.parse::<u16>() {
                        port = p;
                    }
                }
                _ => {}
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

    let payload = body.map(Value::to_string).unwrap_or_default();
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
        if http_request("GET", "/api/status?known_rev=1", None, 300).is_ok() {
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
  --login              弹出官方登录小窗口，登录成功后自动完成设备绑定与凭证保存
  --start [AREA]       免开界面恢复时长并启动加速 (不传参数则使用上次选择的区服)
  --list               按七大洲分类列出云端动态同步的全部可用区服及节点数
  --pause              仅断开加速隧道并暂停官网账号时长 (保持后台服务运行)
  --stop               暂停官网账号时长并停止后台服务
  --status             查看服务运行状态、当前加速区服与账号剩余时长
  --port <PORT>        修改本地守护服务端口 (写入 {CONF_FILE}) 并重启服务
  --daemon             以守护进程模式运行后台 RPC 与路由服务
  -h, --help           显示此帮助信息"
    );
}

fn fetch_status(include_account: bool) -> Result<DaemonSnapshot, String> {
    let url = if include_account { "/api/status?include_account=1" } else { "/api/status" };
    let val = http_request("GET", url, None, 6000)?;
    serde_json::from_value(val).map_err(|e| e.to_string())
}

fn cmd_login() -> i32 {
    ensure_services();
    println!("正在打开雷神登录小窗口，请在窗口中完成手机号或密码登录...");
    let ptok = match login::run_browser_login_popup() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("登录未完成: {e}");
            return 1;
        }
    };
    match http_request("POST", "/api/account/save-token", Some(&json!({"account_token": ptok})), 12000) {
        Ok(res) if res["ok"].as_bool() == Some(true) => {
            println!("登录成功！已自动完成本机硬件绑定与官网凭证同步。");
            cmd_status()
        }
        Ok(res) => {
            eprintln!("保存凭证失败: {res}");
            1
        }
        Err(e) => {
            eprintln!("保存凭证失败: {e}");
            1
        }
    }
}

fn cmd_list() -> i32 {
    ensure_services();
    let snap = match fetch_status(false) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("错误: 无法连接后台服务: {e}");
            return 1;
        }
    };
    let profiles = snap.profiles.unwrap_or_default();
    for cont in CONTINENT_ORDER {
        let items: Vec<_> = profiles.iter().filter(|p| p.continent == cont).collect();
        if items.is_empty() {
            continue;
        }
        println!("[{cont}]");
        for p in items {
            println!("  {:<6} {} ({} 节点)", p.area, p.name, p.node_count);
        }
    }
    0
}

fn cmd_start(area: &str) -> i32 {
    let area = area.trim().to_lowercase();
    ensure_services();
    match http_request("POST", "/api/acc/start", Some(&json!({"area": area, "recover": true})), 10000) {
        Ok(res) if res["ok"].as_bool() == Some(true) => {
            let resolved_area = res["area"].as_str().unwrap_or(&area);
            let name = res["name"].as_str().unwrap_or(resolved_area);
            println!("已恢复时长并启动加速: {name} ({resolved_area})");
            0
        }
        Ok(res) => {
            eprintln!("错误: {}", res["msg"].as_str().unwrap_or("启动加速失败"));
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
    let Ok(snap) = fetch_status(true) else {
        println!("服务状态: 异常 (无法连接 http://{host}:{port})");
        return 1;
    };

    let profiles = snap.profiles.unwrap_or_default();
    let (acc_str, delay_str) = if snap.is_accelerating {
        let name = profiles
            .iter()
            .find(|p| p.area == snap.active_area)
            .map(|p| p.name.as_str())
            .unwrap_or(&snap.active_area);
        let delay = if snap.net_delay > 0 { format!("{} ms", snap.net_delay) } else { "- ms".to_string() };
        (format!("{name} · 加速中 ({})", snap.tun_interface), delay)
    } else {
        ("待加速".to_string(), "- ms".to_string())
    };

    let acct_line = match snap.account {
        Some(acct) if acct.configured => {
            let state_lbl = if acct.paused { "已暂停" } else { "计时中" };
            format!("{state_lbl} (剩余 {} 小时 {} 分)", acct.hours, acct.mins)
        }
        _ => "未配置官网 Token".to_string(),
    };

    println!("服务状态: 运行中 (http://{host}:{port})");
    println!("加速状态: {acc_str} | 节点延迟: {delay_str}");
    println!("账号时长: {acct_line}");
    0
}

fn cmd_port(port_str: &str) -> i32 {
    let Some(port) = port_str.parse::<u16>().ok().filter(|&p| p > 0) else {
        eprintln!("错误: 请提供合法的端口号 (1-65535)，例如: lgd --port 8090");
        return 1;
    };
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
    match args.first().map(String::as_str) {
        Some("-h" | "--help") => print_help(),
        Some("--daemon") => daemon::run_daemon(),
        Some("--login") => process::exit(cmd_login()),
        Some("--start") => process::exit(cmd_start(args.get(1).map(String::as_str).unwrap_or(""))),
        Some("--list" | "--regions") => process::exit(cmd_list()),
        Some("--pause") => process::exit(cmd_pause()),
        Some("--stop") => process::exit(cmd_stop()),
        Some("--status") => process::exit(cmd_status()),
        Some("--port") => {
            let Some(p) = args.get(1) else {
                eprintln!("错误: --port 需要指定端口号");
                process::exit(1);
            };
            process::exit(cmd_port(p));
        }
        Some(other) if other.starts_with("--host") => daemon::run_daemon(),
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
