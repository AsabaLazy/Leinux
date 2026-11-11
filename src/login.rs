use crate::rpc::{is_valid_ptoken, WsClient};
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LOGIN_URL: &str = "https://www.xxghh.biz/m/login";

const CHROMIUM_BINS: [&str; 12] = [
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

const FIREFOX_BINS: [&str; 7] = [
    "firefox",
    "firefox-esr",
    "firefox-developer-edition",
    "librewolf",
    "zen-browser",
    "floorp",
    "waterfox",
];

const CHROMIUM_FLATPAKS: [&str; 4] = [
    "com.google.Chrome",
    "org.chromium.Chromium",
    "com.brave.Browser",
    "com.microsoft.Edge",
];

const FIREFOX_FLATPAKS: [&str; 4] = [
    "org.mozilla.firefox",
    "io.gitlab.librewolf-community",
    "app.zen_browser.zen",
    "one.ablaze.floorp",
];

const FIREFOX_USER_JS: &str = r#"user_pref("toolkit.legacyUserProfileCustomizations.stylesheets", true);
user_pref("browser.tabs.inTitlebar", 0);
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.startup.homepage_override.mstone", "ignore");
user_pref("datareporting.policy.dataSubmissionPolicyBypassNotification", true);
user_pref("browser.aboutwelcome.enabled", false);
user_pref("trailhead.firstrun.didSeeAboutWelcome", true);
user_pref("browser.tabs.warnOnClose", false);
user_pref("dom.disable_beforeunload", true);
"#;

const FIREFOX_USER_CHROME_CSS: &str =
    "#TabsToolbar, #nav-bar, #PersonalToolbar, #sidebar-box, #sidebar-splitter { visibility: collapse !important; }\n";

static LOGIN_POPUP_OPEN: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BrowserEngine {
    Chromium,
    Firefox,
}

struct BrowserCandidate {
    engine: BrowserEngine,
    program: PathBuf,
    flatpak_app: Option<&'static str>,
}

fn find_in_path(names: &[&str]) -> Option<PathBuf> {
    let path_var = env::var_os("PATH")?;
    env::split_paths(&path_var)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
}

fn flatpak_installed(app_id: &str) -> bool {
    if Path::new("/var/lib/flatpak/app").join(app_id).is_dir() {
        return true;
    }
    let user_data = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    user_data.is_some_and(|base| base.join("flatpak/app").join(app_id).is_dir())
}

fn detect_login_browser() -> Option<BrowserCandidate> {
    if let Some(program) = find_in_path(&CHROMIUM_BINS) {
        return Some(BrowserCandidate {
            engine: BrowserEngine::Chromium,
            program,
            flatpak_app: None,
        });
    }
    if let Some(program) = find_in_path(&FIREFOX_BINS) {
        return Some(BrowserCandidate {
            engine: BrowserEngine::Firefox,
            program,
            flatpak_app: None,
        });
    }
    let flatpak = find_in_path(&["flatpak"])?;
    for &app in &CHROMIUM_FLATPAKS {
        if flatpak_installed(app) {
            return Some(BrowserCandidate {
                engine: BrowserEngine::Chromium,
                program: flatpak,
                flatpak_app: Some(app),
            });
        }
    }
    for &app in &FIREFOX_FLATPAKS {
        if flatpak_installed(app) {
            return Some(BrowserCandidate {
                engine: BrowserEngine::Firefox,
                program: flatpak,
                flatpak_app: Some(app),
            });
        }
    }
    None
}

pub fn has_login_browser() -> bool {
    detect_login_browser().is_some()
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

fn extract_auth_token_from_cookies(resp: &Value) -> Option<String> {
    resp["result"]["cookies"].as_array()?.iter().find_map(|ck| {
        let name = ck["name"].as_str()?;
        let domain = ck["domain"].as_str()?;
        let is_leigod_auth = name == "AuthToken"
            && ["xxghh", "nn.com", "leigod"].iter().any(|d| domain.contains(d));
        let raw_val = ck["value"]
            .as_str()
            .or_else(|| ck["value"]["value"].as_str())?;
        is_leigod_auth
            .then(|| decode_auth_token_cookie(raw_val))
            .flatten()
    })
}

fn create_login_profile_dir() -> Result<PathBuf, String> {
    let base = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
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

fn prepare_firefox_profile(profile_dir: &Path) -> Result<(), String> {
    fs::write(profile_dir.join("user.js"), FIREFOX_USER_JS)
        .map_err(|e| format!("写入 Firefox 临时配置失败: {e}"))?;
    let chrome_dir = profile_dir.join("chrome");
    fs::create_dir_all(&chrome_dir).map_err(|e| format!("创建 Firefox 样式目录失败: {e}"))?;
    fs::write(chrome_dir.join("userChrome.css"), FIREFOX_USER_CHROME_CSS)
        .map_err(|e| format!("写入 Firefox 弹窗样式失败: {e}"))
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

fn read_chromium_endpoint(profile_dir: &Path) -> Option<(String, u16, String)> {
    let content = fs::read_to_string(profile_dir.join("DevToolsActivePort")).ok()?;
    let mut lines = content.lines();
    let port = lines.next()?.trim().parse::<u16>().ok()?;
    let path = lines.next()?.trim().to_string();
    (!path.is_empty()).then(|| ("127.0.0.1".to_string(), port, path))
}

fn read_firefox_bidi_endpoint(profile_dir: &Path) -> Option<(String, u16, String)> {
    let content = fs::read_to_string(profile_dir.join("WebDriverBiDiServer.json")).ok()?;
    let val: Value = serde_json::from_str(&content).ok()?;
    let port = val["ws_port"].as_u64().and_then(|p| u16::try_from(p).ok())?;
    let host = val["ws_host"].as_str().unwrap_or("127.0.0.1").to_string();
    Some((host, port, "/session".to_string()))
}

fn wait_for_ws_endpoint(
    guard: &mut PopupGuard,
    engine: BrowserEngine,
) -> Result<(String, u16, String), String> {
    for _ in 0..100 {
        if guard.child.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(Some(_)))) {
            return Err("已取消登录".to_string());
        }
        let ep = match engine {
            BrowserEngine::Chromium => read_chromium_endpoint(&guard.profile_dir),
            BrowserEngine::Firefox => read_firefox_bidi_endpoint(&guard.profile_dir),
        };
        if let Some(found) = ep {
            return Ok(found);
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err("连接登录小窗口调试端口超时".to_string())
}

fn spawn_browser_popup(browser: &BrowserCandidate, profile_dir: &Path) -> Result<Child, String> {
    let mut cmd = Command::new(&browser.program);
    if let Some(app_id) = browser.flatpak_app {
        cmd.args([
            "run",
            &format!("--filesystem={}", profile_dir.display()),
            app_id,
        ]);
    }
    match browser.engine {
        BrowserEngine::Chromium => {
            cmd.args([
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
            ]);
        }
        BrowserEngine::Firefox => {
            prepare_firefox_profile(profile_dir)?;
            cmd.env("MOZ_NO_REMOTE", "1").args([
                "--no-remote",
                "--profile",
                &profile_dir.to_string_lossy(),
                "--width",
                "420",
                "--height",
                "660",
                "--remote-debugging-port=0",
                "--new-window",
                LOGIN_URL,
            ]);
        }
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("拉起浏览器登录窗口失败: {e}"))
}

pub fn run_browser_login_popup() -> Result<String, String> {
    let browser = detect_login_browser()
        .ok_or("未检测到可用浏览器（Chromium / Firefox），请在设置中手动粘贴 Token")?;

    if LOGIN_POPUP_OPEN.swap(true, Ordering::SeqCst) {
        return Err("登录小窗口已打开，请在弹出的窗口中完成登录".to_string());
    }

    let profile_dir = create_login_profile_dir().inspect_err(|_| {
        LOGIN_POPUP_OPEN.store(false, Ordering::SeqCst);
    })?;

    let child = spawn_browser_popup(&browser, &profile_dir).inspect_err(|_| {
        let _ = fs::remove_dir_all(&profile_dir);
        LOGIN_POPUP_OPEN.store(false, Ordering::SeqCst);
    })?;

    let mut guard = PopupGuard {
        child: Some(child),
        profile_dir,
    };

    let (host, port, ws_path) = wait_for_ws_endpoint(&mut guard, browser.engine)?;
    let mut ws = WsClient::connect(&host, port, &ws_path, 2500)?;
    let mut seq: u64 = 1;

    if browser.engine == BrowserEngine::Firefox {
        let init = ws.call(&json!({
            "id": seq,
            "method": "session.new",
            "params": { "capabilities": {} }
        }))?;
        seq += 1;
        if init["type"].as_str() != Some("success") {
            return Err("初始化 Firefox 登录会话失败".to_string());
        }
    }

    ws.set_timeout(1000);
    let deadline = Instant::now() + Duration::from_secs(300);

    while Instant::now() < deadline {
        if guard.child.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(Some(_)))) {
            return Err("已关闭登录窗口".to_string());
        }
        let poll_req = match browser.engine {
            BrowserEngine::Chromium => json!({ "id": seq, "method": "Storage.getCookies" }),
            BrowserEngine::Firefox => json!({ "id": seq, "method": "storage.getCookies", "params": {} }),
        };
        seq += 1;

        if let Ok(resp) = ws.call(&poll_req) {
            if let Some(ptok) = extract_auth_token_from_cookies(&resp) {
                ws.set_timeout(500);
                let close_req = match browser.engine {
                    BrowserEngine::Chromium => json!({ "id": seq, "method": "Browser.close" }),
                    BrowserEngine::Firefox => json!({ "id": seq, "method": "browser.close", "params": {} }),
                };
                let _ = ws.call(&close_req);
                return Ok(ptok);
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
        let Some(browser) = find_in_path(&CHROMIUM_BINS) else { return };
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
            .expect("spawn headless chromium");
        let mut guard = PopupGuard {
            child: Some(child),
            profile_dir,
        };
        let (host, port, ws_path) =
            wait_for_ws_endpoint(&mut guard, BrowserEngine::Chromium).expect("DevToolsActivePort");
        let mut ws = WsClient::connect(&host, port, &ws_path, 1500).expect("connect CDP");
        ws.call(&json!({
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
        }))
        .expect("Storage.setCookies");

        let resp = ws
            .call(&json!({ "id": 2, "method": "Storage.getCookies" }))
            .expect("Storage.getCookies");
        assert_eq!(
            extract_auth_token_from_cookies(&resp).as_deref(),
            Some("220bee8af3a6452b94133051140e8da2")
        );
        let _ = ws.call(&json!({ "id": 3, "method": "Browser.close" }));
    }

    #[test]
    fn test_real_firefox_bidi_ws_rpc_call() {
        let Some(browser) = find_in_path(&FIREFOX_BINS) else { return };
        let profile_dir = create_login_profile_dir().expect("create temp profile");
        prepare_firefox_profile(&profile_dir).expect("prepare firefox profile");
        let child = Command::new(browser)
            .env("MOZ_NO_REMOTE", "1")
            .args([
                "--headless",
                "--no-remote",
                "--profile",
                &profile_dir.to_string_lossy(),
                "--remote-debugging-port=0",
                "about:blank",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn headless firefox");
        let mut guard = PopupGuard {
            child: Some(child),
            profile_dir,
        };
        let (host, port, ws_path) =
            wait_for_ws_endpoint(&mut guard, BrowserEngine::Firefox).expect("WebDriverBiDiServer.json");
        let mut ws = WsClient::connect(&host, port, &ws_path, 2500).expect("connect BiDi");
        let init = ws
            .call(&json!({ "id": 1, "method": "session.new", "params": { "capabilities": {} } }))
            .expect("session.new");
        assert_eq!(init["type"].as_str(), Some("success"));

        ws.call(&json!({
            "id": 2,
            "method": "storage.setCookie",
            "params": {
                "cookie": {
                    "name": "AuthToken",
                    "value": { "type": "string", "value": "MjIwYmVlOGFmM2E2NDUyYjk0MTMzMDUxMTQwZThkYTI=" },
                    "domain": "www.xxghh.biz",
                    "path": "/"
                }
            }
        }))
        .expect("storage.setCookie");

        let resp = ws
            .call(&json!({ "id": 3, "method": "storage.getCookies", "params": {} }))
            .expect("storage.getCookies");
        assert_eq!(
            extract_auth_token_from_cookies(&resp).as_deref(),
            Some("220bee8af3a6452b94133051140e8da2")
        );
        let _ = ws.call(&json!({ "id": 4, "method": "browser.close", "params": {} }));
    }
}
