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
use std::process::{self, Command};
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread;
use std::time::Duration;
use types::{DaemonSnapshot, CONTINENT_ORDER};

pub const CONF_FILE: &str = "/etc/default/leinux";
pub const CORE_SERVICE: &str = "leinux-core.service";
pub const DAEMON_SERVICE: &str = "leinux.service";
pub const SERVICES: [&str; 2] = [CORE_SERVICE, DAEMON_SERVICE];

pub fn load_host_port() -> (String, u16) {
    let mut host = env::var("LEINUX_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let mut port = env::var("LEINUX_PORT")
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
            let val = v.trim().trim_matches(['\'', '"']);
            match k.trim() {
                "LEINUX_HOST" if !val.is_empty() => host = val.to_string(),
                "LEINUX_PORT" => {
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

fn service_active(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", unit])
        .status()
        .is_ok_and(|s| s.success())
}

fn ensure_services() {
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
        "用法: lgd [选项]  或  lgd [-a 区服] <程序命令...>

Leinux — 雷神加速器 Linux 原生 GTK4 客户端与命令行工具

选项:
  (无参数)             自动拉起后台服务并打开原生 GTK4 控制面板窗口
  [-a 区服] <命令...>  按进程树专享加速运行程序，退出自动暂停时长 (Steam 填: lgd %command%)
  --login              弹出官方登录小窗口，登录成功后自动完成设备绑定与凭证保存
  --start [AREA]       免开界面恢复时长并启动全局加速 (不传参数则使用上次选择的区服)
  --list               按七大洲分类列出云端动态同步的全部可用区服及节点数
  --pause              仅断开加速隧道并暂停官网账号时长 (保持后台服务运行)
  --stop               暂停官网账号时长并停止后台服务
  --status             查看服务运行状态、当前加速区服与账号剩余时长
  --port <PORT>        修改本地守护服务端口 (写入 {CONF_FILE}) 并重启服务
  --daemon             以守护进程模式运行后台 RPC 与路由服务
  -V, --version        显示客户端版本信息
  -h, --help           显示此帮助信息"
    );
}

fn fetch_status(include_account: bool) -> Result<DaemonSnapshot, String> {
    let url = if include_account { "/api/status?include_account=1" } else { "/api/status" };
    let val = http_request("GET", url, None, 6000)?;
    serde_json::from_value(val).map_err(|e| e.to_string())
}

fn cmd_login() -> Result<(), String> {
    ensure_services();
    println!("正在打开雷神登录小窗口，请在窗口中完成手机号或密码登录...");
    let ptok = login::run_browser_login_popup()?;
    let res = http_request("POST", "/api/account/save-token", Some(&json!({"account_token": ptok})), 12000)?;
    if res["ok"].as_bool() != Some(true) {
        return Err(format!("保存凭证失败: {res}"));
    }
    println!("登录成功！已自动完成本机硬件绑定与官网凭证同步。");
    cmd_status()
}

fn cmd_list() -> Result<(), String> {
    ensure_services();
    let profiles = fetch_status(false)
        .map_err(|e| format!("无法连接后台服务: {e}"))?
        .profiles
        .unwrap_or_default();

    for cont in CONTINENT_ORDER {
        let items: Vec<_> = profiles.iter().filter(|p| p.continent == cont).collect();
        if !items.is_empty() {
            println!("[{cont}]");
            for p in items {
                println!("  {:<6} {} ({} 节点)", p.area, p.name, p.node_count);
            }
        }
    }
    Ok(())
}

fn cmd_start(area: &str) -> Result<(), String> {
    let area = area.trim().to_lowercase();
    ensure_services();
    let res = http_request("POST", "/api/acc/start", Some(&json!({"area": area, "recover": true})), 12000)?;
    if res["ok"].as_bool() != Some(true) {
        return Err(res["msg"].as_str().unwrap_or("启动加速失败").to_string());
    }
    let resolved = res["area"].as_str().unwrap_or(&area);
    let name = res["name"].as_str().unwrap_or(resolved);
    println!("已恢复时长并启动加速: {name} ({resolved})");
    Ok(())
}

fn cmd_pause() -> Result<(), String> {
    ensure_services();
    let res = http_request("POST", "/api/account/pause", Some(&json!({})), 8000)?;
    if res["local_only"].as_bool() == Some(true) {
        println!("{}", res["msg"].as_str().unwrap_or("已停止本地加速"));
    } else if res["code"].as_i64() == Some(0) {
        println!("已断开加速隧道并暂停雷神账号时长");
    } else {
        println!("已断开加速隧道，官网暂停返回: {}", res["msg"].as_str().unwrap_or("请检查官网 Token"));
    }
    Ok(())
}

fn cmd_stop() -> Result<(), String> {
    if service_active(DAEMON_SERVICE) {
        let _ = http_request("POST", "/api/account/pause", Some(&json!({})), 5000);
    }
    let _ = Command::new("systemctl")
        .args(["stop", DAEMON_SERVICE, CORE_SERVICE])
        .status();
    println!("已暂停账号时长并停止 {DAEMON_SERVICE} 与 {CORE_SERVICE}");
    Ok(())
}

fn cmd_status() -> Result<(), String> {
    if !service_active(DAEMON_SERVICE) {
        println!("服务状态: 已停止 (可执行 'lgd' 或 'lgd --start hk' 启动)");
        return Ok(());
    }

    let (host, port) = load_host_port();
    let snap = fetch_status(true).map_err(|_| format!("服务状态: 异常 (无法连接 http://{host}:{port})"))?;
    let profiles = snap.profiles.unwrap_or_default();

    let (acc_str, delay_str) = if snap.is_accelerating {
        let name = profiles
            .iter()
            .find(|p| p.area == snap.active_area)
            .map_or(snap.active_area.as_str(), |p| p.name.as_str());
        let delay = if snap.net_delay > 0 { format!("{} ms", snap.net_delay) } else { "- ms".to_string() };
        let mode = if snap.scoped_procs > 0 { "进程专享" } else { "加速中" };
        (format!("{name} · {mode} ({})", snap.tun_interface), delay)
    } else {
        ("待加速".to_string(), "- ms".to_string())
    };

    let acct_line = match snap.account {
        Some(acct) if acct.configured => {
            let state_lbl = if acct.paused { "已暂停" } else { "计时中" };
            format!("{state_lbl} (剩余 {} 小时 {} 分)", acct.hours, acct.mins)
        }
        _ => "未登录雷神账号 (可执行 'lgd --login')".to_string(),
    };

    println!("服务状态: 运行中 (http://{host}:{port})");
    println!("加速状态: {acc_str} | 节点延迟: {delay_str}");
    println!("账号时长: {acct_line}");
    Ok(())
}

fn cmd_port(port_str: &str) -> Result<(), String> {
    let port = port_str
        .parse::<u16>()
        .ok()
        .filter(|&p| p > 0)
        .ok_or("请提供合法的端口号 (1-65535)，例如: lgd --port 8090")?;

    if unsafe { libc::geteuid() } != 0 {
        let exe = env::current_exe().unwrap_or_else(|_| Path::new("/usr/bin/lgd").to_path_buf());
        let err = Command::new("sudo").arg(exe).args(["--port", port_str]).exec();
        return Err(format!("无法调用 sudo: {err}"));
    }
    if let Some(parent) = Path::new(CONF_FILE).parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(CONF_FILE, format!("LEINUX_HOST=127.0.0.1\nLEINUX_PORT={port}\n"))
        .map_err(|e| format!("写入 {CONF_FILE} 失败: {e}"))?;

    let _ = Command::new("systemctl").args(["restart", DAEMON_SERVICE]).status();
    println!("已将守护服务端口更新为 {port} 并重启服务: http://127.0.0.1:{port}");
    Ok(())
}

static WRAPPER_CHILD_PID: AtomicI32 = AtomicI32::new(0);
static WRAPPER_LAST_SIG: AtomicI32 = AtomicI32::new(0);

extern "C" fn forward_wrapper_sig(sig: libc::c_int) {
    WRAPPER_LAST_SIG.store(sig, Ordering::SeqCst);
    let pid = WRAPPER_CHILD_PID.load(Ordering::SeqCst);
    if pid > 0 {
        unsafe {
            libc::kill(pid, sig);
        }
    }
}

fn build_wrapped_argv(cmd_args: &[String]) -> Vec<String> {
    let is_steam_wrapper = Path::new(&cmd_args[0])
        .file_name()
        .and_then(|s| s.to_str())
        == Some("steam-launch-wrapper");
    if !is_steam_wrapper || cmd_args.iter().any(|a| a == "--no-scope") {
        return cmd_args.to_vec();
    }
    let mut out = Vec::with_capacity(cmd_args.len() + 1);
    out.push(cmd_args[0].clone());
    out.push("--no-scope".to_string());
    out.extend_from_slice(&cmd_args[1..]);
    out
}

fn cmd_wrap_exec(area: &str, cmd_args: &[String]) -> Result<i32, String> {
    let area = area.trim().to_lowercase();
    ensure_services();
    let self_pid = process::id();
    let res = http_request(
        "POST",
        "/api/acc/start",
        Some(&json!({ "area": area, "recover": true, "cgroup_pid": self_pid })),
        15000,
    )?;
    if res["ok"].as_bool() != Some(true) {
        return Err(res["msg"].as_str().unwrap_or("启动进程专享加速失败").to_string());
    }

    let argv = build_wrapped_argv(cmd_args);
    let mut child = match Command::new(&argv[0])
        .args(&argv[1..])
        .env("STEAM_LAUNCH_WRAPPER_SCOPE", "0")
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = http_request("POST", "/api/acc/release-cgroup", Some(&json!({ "pid": self_pid })), 5000);
            return Err(format!("无法启动目标程序 '{}': {e}", argv[0]));
        }
    };

    WRAPPER_CHILD_PID.store(child.id() as i32, Ordering::SeqCst);
    unsafe {
        let handler = forward_wrapper_sig as *const () as libc::sighandler_t;
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGHUP, handler);
    }

    let wait_res = child.wait();
    WRAPPER_CHILD_PID.store(0, Ordering::SeqCst);
    let _ = http_request("POST", "/api/acc/release-cgroup", Some(&json!({ "pid": self_pid })), 8000);

    let code = match wait_res {
        Ok(st) => st.code().unwrap_or_else(|| {
            let sig = WRAPPER_LAST_SIG.load(Ordering::SeqCst);
            if sig > 0 { 128 + sig } else { 1 }
        }),
        Err(_) => 1,
    };
    Ok(code)
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("-h" | "--help") => print_help(),
        Some("-V" | "--version") => println!("leinux {}", env!("CARGO_PKG_VERSION")),
        Some("--daemon") => daemon::run_daemon(),
        Some("--login") => cmd_login()?,
        Some("--start") => cmd_start(args.get(1).map_or("", String::as_str))?,
        Some("--list" | "--regions") => cmd_list()?,
        Some("--pause") => cmd_pause()?,
        Some("--stop") => cmd_stop()?,
        Some("--status") => cmd_status()?,
        Some("--port") => {
            let port_str = args.get(1).ok_or("--port 需要指定端口号")?;
            cmd_port(port_str)?;
        }
        Some("-a" | "--area") => {
            let area = args.get(1).ok_or("-a/--area 需要指定区服代号，例如: lgd -a jp %command%")?;
            let rest = &args[2..];
            let cmd = rest.strip_prefix(&["--".to_string()]).unwrap_or(rest);
            if cmd.is_empty() {
                return Err("请指定要加速运行的程序命令，例如: lgd -a jp %command%".to_string());
            }
            let code = cmd_wrap_exec(area, cmd)?;
            if code != 0 {
                process::exit(code);
            }
        }
        Some("--") => {
            let cmd = &args[1..];
            if cmd.is_empty() {
                return Err("请指定要加速运行的程序命令，例如: lgd %command%".to_string());
            }
            let code = cmd_wrap_exec("", cmd)?;
            if code != 0 {
                process::exit(code);
            }
        }
        Some(other) if other.starts_with('-') => {
            print_help();
            return Err(format!("未知选项: {other}"));
        }
        Some(_) => {
            let code = cmd_wrap_exec("", &args)?;
            if code != 0 {
                process::exit(code);
            }
        }
        None => {
            ensure_services();
            if env::var_os("DISPLAY").is_some() || env::var_os("WAYLAND_DISPLAY").is_some() {
                gui::run_gui();
            } else {
                cmd_status()?;
                println!("当前无图形界面环境，可使用 'lgd --list' 或 'lgd --start <区服>' 进行管理。");
            }
        }
    }
    Ok(())
}

fn main() {
    if let Err(err) = run() {
        eprintln!("错误: {err}");
        process::exit(1);
    }
}
