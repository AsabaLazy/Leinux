use crate::rpc::{is_valid_ptoken, ws_rpc_call};
use serde_json::json;
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LOGIN_URL: &str = "https://www.xxghh.biz/m/login";

const BROWSER_CANDIDATES: [&str; 12] = [
    "google-chrome-stable",
    "google-chrome",
    "chromium",
    "chromium-browser",
    "brave",
    "brave-browser",
    "microsoft-edge-stable",
    "microsoft-edge",
    "vivaldi-stable",
    "vivaldi",
    "ungoogled-chromium",
    "thorium-browser",
];

static LOGIN_POPUP_OPEN: AtomicBool = AtomicBool::new(false);

pub fn find_chromium_binary() -> Option<PathBuf> {
    let path_var = env::var_os("PATH")?;
    env::split_paths(&path_var)
        .flat_map(|dir| BROWSER_CANDIDATES.into_iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            ) {
                out.push(((h << 4) | l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn b64_val(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    }
}

fn base64_decode(input: &str) -> Option<String> {
    let clean: Vec<u8> = input
        .bytes()
        .filter(|&b| b != b'=' && !b.is_ascii_whitespace())
        .collect();
    if clean.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u8;
    for b in clean {
        buf = (buf << 6) | (b64_val(b)? as u32);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    String::from_utf8(out).ok()
}

pub fn decode_auth_token_cookie(raw_val: &str) -> Option<String> {
    let unescaped = percent_decode(raw_val.trim());
    if is_valid_ptoken(&unescaped) {
        return Some(unescaped);
    }
    let decoded = base64_decode(&unescaped)?;
    let trimmed = decoded.trim();
    is_valid_ptoken(trimmed).then(|| trimmed.to_string())
}

fn create_login_profile_dir() -> Result<PathBuf, String> {
    let base = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(env::temp_dir);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dir = base.join(format!("leinux-login-{}-{stamp}", std::process::id()));
    fs::create_dir_all(&dir).map_err(|e| format!("无法创建临时登录目录: {e}"))?;
    let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    Ok(dir)
}

struct PopupGuard {
    child: Option<Child>,
    profile_dir: PathBuf,
}

impl Drop for PopupGuard {
    fn drop(&mut self) {
        if let Some(ref mut c) = self.child {
            let exited = (0..6).any(|_| {
                if matches!(c.try_wait(), Ok(Some(_))) {
                    true
                } else {
                    thread::sleep(Duration::from_millis(40));
                    false
                }
            });
            if !exited {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
        let _ = fs::remove_dir_all(&self.profile_dir);
        LOGIN_POPUP_OPEN.store(false, Ordering::SeqCst);
    }
}

fn read_devtools_endpoint(profile_dir: &Path) -> Option<(u16, String)> {
    let content = fs::read_to_string(profile_dir.join("DevToolsActivePort")).ok()?;
    let mut lines = content.lines();
    let port = lines.next()?.trim().parse::<u16>().ok()?;
    let path = lines.next()?.trim().to_string();
    (!path.is_empty()).then_some((port, path))
}

fn wait_for_devtools(guard: &mut PopupGuard) -> Result<(u16, String), String> {
    for _ in 0..80 {
        if guard.child.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(Some(_)))) {
            return Err("已取消登录".to_string());
        }
        if let Some(ep) = read_devtools_endpoint(&guard.profile_dir) {
            return Ok(ep);
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err("连接登录小窗口调试端口超时".to_string())
}

fn poll_auth_token_cookie(port: u16, ws_path: &str) -> Option<String> {
    let resp = ws_rpc_call(
        "127.0.0.1",
        port,
        ws_path,
        &json!({"id": 1, "method": "Storage.getCookies"}),
        800,
    )
    .ok()?;

    resp["result"]["cookies"].as_array()?.iter().find_map(|ck| {
        let name = ck["name"].as_str()?;
        let domain = ck["domain"].as_str()?;
        let is_leigod_auth = name == "AuthToken"
            && ["xxghh", "nn.com", "leigod"].iter().any(|d| domain.contains(d));
        is_leigod_auth
            .then(|| ck["value"].as_str())
            .flatten()
            .and_then(decode_auth_token_cookie)
    })
}

pub fn run_browser_login_popup() -> Result<String, String> {
    let browser = find_chromium_binary()
        .ok_or("未检测到 Chrome / Chromium / Edge / Brave 浏览器，请在设置中手动粘贴 Token")?;

    if LOGIN_POPUP_OPEN.swap(true, Ordering::SeqCst) {
        return Err("登录小窗口已打开，请在弹出的窗口中完成登录".to_string());
    }

    let profile_dir = create_login_profile_dir().inspect_err(|_| {
        LOGIN_POPUP_OPEN.store(false, Ordering::SeqCst);
    })?;

    let child = Command::new(&browser)
        .args([
            &format!("--app={LOGIN_URL}"),
            "--window-size=420,660",
            "--no-first-run",
            "--no-default-browser-check",
            "--password-store=basic",
            "--disable-extensions",
            "--disable-sync",
            "--disable-translate",
            "--remote-debugging-port=0",
            &format!("--user-data-dir={}", profile_dir.display()),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            let _ = fs::remove_dir_all(&profile_dir);
            LOGIN_POPUP_OPEN.store(false, Ordering::SeqCst);
            format!("拉起浏览器登录窗口失败: {e}")
        })?;

    let mut guard = PopupGuard {
        child: Some(child),
        profile_dir,
    };

    let (port, ws_path) = wait_for_devtools(&mut guard)?;
    let deadline = Instant::now() + Duration::from_secs(300);

    while Instant::now() < deadline {
        if guard.child.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(Some(_)))) {
            return Err("已关闭登录窗口".to_string());
        }
        if let Some(ptok) = poll_auth_token_cookie(port, &ws_path) {
            let _ = ws_rpc_call(
                "127.0.0.1",
                port,
                &ws_path,
                &json!({"id": 2, "method": "Browser.close"}),
                500,
            );
            return Ok(ptok);
        }
        thread::sleep(Duration::from_millis(350));
    }

    Err("登录等待超时（5 分钟）".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_auth_token_cookie() {
        assert_eq!(
            decode_auth_token_cookie("MjIwYmVlOGFmM2E2NDUyYjk0MTMzMDUxMTQwZThkYTI%3D").as_deref(),
            Some("220bee8af3a6452b94133051140e8da2")
        );
        assert_eq!(
            decode_auth_token_cookie("MjIwYmVlOGFmM2E2NDUyYjk0MTMzMDUxMTQwZThkYTI=").as_deref(),
            Some("220bee8af3a6452b94133051140e8da2")
        );
        assert_eq!(
            decode_auth_token_cookie("220bee8af3a6452b94133051140e8da2").as_deref(),
            Some("220bee8af3a6452b94133051140e8da2")
        );
        assert_eq!(decode_auth_token_cookie("not-a-valid-token"), None);
        assert_eq!(decode_auth_token_cookie(""), None);
    }

    #[test]
    fn test_real_chromium_cdp_ws_rpc_call() {
        let Some(browser) = find_chromium_binary() else { return };
        let profile_dir = create_login_profile_dir().expect("create temp profile");
        let child = Command::new(browser)
            .args([
                "--headless=new",
                "--no-first-run",
                "--no-default-browser-check",
                "--password-store=basic",
                "--disable-extensions",
                "--remote-debugging-port=0",
                &format!("--user-data-dir={}", profile_dir.display()),
                "about:blank",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn headless browser");
        let mut guard = PopupGuard {
            child: Some(child),
            profile_dir,
        };
        let (port, ws_path) = wait_for_devtools(&mut guard).expect("DevToolsActivePort endpoint");
        ws_rpc_call(
            "127.0.0.1",
            port,
            &ws_path,
            &json!({
                "id": 1,
                "method": "Storage.setCookies",
                "params": {
                    "cookies": [{
                        "name": "AuthToken",
                        "value": "MjIwYmVlOGFmM2E2NDUyYjk0MTMzMDUxMTQwZThkYTI%3D",
                        "domain": "www.xxghh.biz",
                        "path": "/"
                    }]
                }
            }),
            1500,
        )
        .expect("Storage.setCookies via ws_rpc_call");

        assert_eq!(
            poll_auth_token_cookie(port, &ws_path).as_deref(),
            Some("220bee8af3a6452b94133051140e8da2")
        );

        let _ = ws_rpc_call(
            "127.0.0.1",
            port,
            &ws_path,
            &json!({"id": 3, "method": "Browser.close"}),
            500,
        );
    }
}
