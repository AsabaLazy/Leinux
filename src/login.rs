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
    for dir in env::split_paths(&path_var) {
        for name in BROWSER_CANDIDATES {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
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
        let v = b64_val(b)? as u32;
        buf = (buf << 6) | v;
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
    if is_valid_ptoken(trimmed) {
        Some(trimmed.to_string())
    } else {
        None
    }
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
            let mut exited = false;
            for _ in 0..6 {
                if let Ok(Some(_)) = c.try_wait() {
                    exited = true;
                    break;
                }
                thread::sleep(Duration::from_millis(40));
            }
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
    if path.is_empty() { None } else { Some((port, path)) }
}

pub fn run_browser_login_popup() -> Result<String, String> {
    let Some(browser) = find_chromium_binary() else {
        return Err("未检测到 Chrome / Chromium / Edge / Brave 浏览器，请在设置中手动粘贴 Token".to_string());
    };
    if LOGIN_POPUP_OPEN.swap(true, Ordering::SeqCst) {
        return Err("登录小窗口已打开，请在弹出的窗口中完成登录".to_string());
    };

    let profile_dir = match create_login_profile_dir() {
        Ok(d) => d,
        Err(e) => {
            LOGIN_POPUP_OPEN.store(false, Ordering::SeqCst);
            return Err(e);
        }
    };

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
        .spawn();

    let child = match child {
        Ok(c) => c,
        Err(e) => {
            let _ = fs::remove_dir_all(&profile_dir);
            LOGIN_POPUP_OPEN.store(false, Ordering::SeqCst);
            return Err(format!("拉起浏览器登录窗口失败: {e}"));
        }
    };

    let mut guard = PopupGuard {
        child: Some(child),
        profile_dir,
    };

    let mut endpoint = None;
    for _ in 0..80 {
        if let Some(ref mut c) = guard.child {
            if let Ok(Some(_)) = c.try_wait() {
                return Err("已取消登录".to_string());
            }
        }
        if let Some(ep) = read_devtools_endpoint(&guard.profile_dir) {
            endpoint = Some(ep);
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }

    let Some((port, ws_path)) = endpoint else {
        return Err("连接登录小窗口调试端口超时".to_string());
    };

    let deadline = Instant::now() + Duration::from_secs(300);
    while Instant::now() < deadline {
        if let Some(ref mut c) = guard.child {
            if let Ok(Some(_)) = c.try_wait() {
                return Err("已关闭登录窗口".to_string());
            }
        }

        if let Ok(resp) = ws_rpc_call(
            "127.0.0.1",
            port,
            &ws_path,
            &json!({"id": 1, "method": "Storage.getCookies"}),
            800,
        ) {
            if let Some(cookies) = resp["result"]["cookies"].as_array() {
                for ck in cookies {
                    let name = ck["name"].as_str().unwrap_or("");
                    let domain = ck["domain"].as_str().unwrap_or("");
                    if name == "AuthToken" && (domain.contains("xxghh") || domain.contains("nn.com")) {
                        if let Some(val) = ck["value"].as_str().and_then(decode_auth_token_cookie) {
                            let _ = ws_rpc_call(
                                "127.0.0.1",
                                port,
                                &ws_path,
                                &json!({"id": 2, "method": "Browser.close"}),
                                500,
                            );
                            return Ok(val);
                        }
                    }
                }
            }
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
        // Real Base64("220bee8af3a6452b94133051140e8da2") = "MjIwYmVlOGFmM2E2NDUyYjk0MTMzMDUxMTQwZThkYTI="
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
}
