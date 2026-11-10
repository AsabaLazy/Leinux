use crate::http_request;
use adw::prelude::*;
use gtk4 as gtk;
use gtk4::glib;
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const APP_ID: &str = "io.github.asabalazy.leinux";
const TOKEN_SNIPPET: &str = r#"copy(JSON.parse(localStorage.getItem("account_token")).account_token)"#;
const SPARK_POINTS: usize = 30;

#[derive(Clone, Default)]
struct ProfileItem {
    name: String,
    area: String,
    catalog: i64,
    sub_catalog: i64,
    conf: Value,
}

#[derive(Clone, Default)]
struct PollSnapshot {
    service_active: bool,
    service_state: String,
    version: String,
    token: String,
    user_id: String,
    udp_6066: String,
    traffic_iface: String,
    rx_bytes: u64,
    tx_bytes: u64,
    profiles: Vec<ProfileItem>,
    is_accelerating: bool,
    active_area: String,
    net_delay: i64,
    loss_rate: String,
    acct_configured: bool,
    acct_paused: bool,
    acct_hours: i64,
    acct_mins: i64,
    acct_sub_text: String,
    auto_pause_on_stop: bool,
    custom_dns: String,
    detected_gateway_dns: String,
}

enum WorkerEvent {
    Snapshot(PollSnapshot),
    Toast(String),
}

enum WorkerCmd {
    StartProfile(ProfileItem, String),
    PauseAccount,
    RecoverAccount,
    SaveToken(String, bool),
    SaveAutoPause(bool),
    SaveCustomDns(String),
    RefreshNodes,
    RestartService,
    RepairBound(String, String),
    CleanupNet,
}

fn format_bytes(bytes: f64, decimals: usize) -> String {
    let n = bytes.max(0.0);
    if n < 1024.0 {
        format!("{:.0} B", n.round())
    } else if n < 1048576.0 {
        format!("{:.*} KB", decimals, n / 1024.0)
    } else if n < 1073741824.0 {
        format!("{:.*} MB", decimals, n / 1048576.0)
    } else {
        format!("{:.*} GB", decimals, n / 1073741824.0)
    }
}

fn format_hms(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

fn fetch_snapshot(include_account: bool, prev_acct: Option<&PollSnapshot>) -> Option<PollSnapshot> {
    let st = http_request("GET", "/api/host-status", None, 3000).ok()?;
    let token = st["token"].as_str().unwrap_or("").to_string();

    let profiles: Vec<ProfileItem> = st["profiles"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|p| {
                    let conf_val = match &p["conf"] {
                        Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| json!({})),
                        other => other.clone(),
                    };
                    ProfileItem {
                        name: p["name"].as_str().unwrap_or("中国香港").to_string(),
                        area: p["area"].as_str().unwrap_or("hk").to_lowercase(),
                        catalog: p["catalog"].as_i64().unwrap_or(2),
                        sub_catalog: p["subCatalog"].as_i64().unwrap_or(0),
                        conf: conf_val,
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    let mut is_accelerating = false;
    let mut active_area = String::new();
    let mut net_delay = 0i64;
    let mut loss_rate = "0".to_string();

    if !token.is_empty() {
        if let Ok(dev_resp) = http_request(
            "POST",
            "/api/ws-rpc",
            Some(&json!({"cmd": "GETDEVICELIST", "tag": "1", "sn": "", "cli": "webui", "code": 0, "err": "", "data": {"token": token}})),
            3000,
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
                is_accelerating = true;
                active_area = active["area"].as_str().unwrap_or("hk").to_lowercase();
            }
        }

        if is_accelerating {
            if let Ok(ainfo_resp) = http_request(
                "POST",
                "/api/ws-rpc",
                Some(&json!({"cmd": "GETACCINFO", "tag": "2", "sn": "", "cli": "webui", "code": 0, "err": "", "data": {"token": token}})),
                3000,
            ) {
                let mut ainfo = ainfo_resp["data"].clone();
                if let Some(s) = ainfo.as_str() {
                    if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                        ainfo = parsed;
                    }
                }
                let obj = if ainfo["accInfo"].is_object() { &ainfo["accInfo"] } else { &ainfo };
                net_delay = obj["netDelay"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    .or_else(|| obj["netDelay"].as_i64())
                    .unwrap_or(0);
                loss_rate = obj["lossRate"]
                    .as_str()
                    .map(|s| s.to_string())
                    .or_else(|| obj["lossRate"].as_i64().map(|n| n.to_string()))
                    .unwrap_or_else(|| "0".to_string());
            }
        }
    }

    let (
        mut acct_configured,
        mut acct_paused,
        mut acct_hours,
        mut acct_mins,
        mut acct_sub_text,
        mut auto_pause_on_stop,
        mut custom_dns,
        mut detected_gateway_dns,
    ) = if let Some(prev) = prev_acct {
        (
            prev.acct_configured,
            prev.acct_paused,
            prev.acct_hours,
            prev.acct_mins,
            prev.acct_sub_text.clone(),
            prev.auto_pause_on_stop,
            prev.custom_dns.clone(),
            prev.detected_gateway_dns.clone(),
        )
    } else {
        (
            false,
            true,
            0,
            0,
            "未登录".to_string(),
            true,
            String::new(),
            "223.5.5.5:53".to_string(),
        )
    };

    if include_account {
        if let Ok(acct) = http_request("GET", "/api/account/status", None, 5000) {
            acct_configured = acct["configured"].as_bool().unwrap_or(false);
            auto_pause_on_stop = acct["auto_pause_on_stop"].as_bool().unwrap_or(true);
            custom_dns = acct["custom_dns"].as_str().unwrap_or("").to_string();
            detected_gateway_dns = acct["detected_gateway_dns"]
                .as_str()
                .unwrap_or("223.5.5.5:53")
                .to_string();

            if !acct_configured {
                acct_paused = true;
                acct_hours = 0;
                acct_mins = 0;
                acct_sub_text = "未配置 Token".to_string();
            } else if acct["response"]["code"].as_i64() == Some(0) && acct["response"]["data"].is_object() {
                let d = &acct["response"]["data"];
                acct_paused = d["pause_status_id"].as_i64().unwrap_or(1) == 1
                    || d["pause_status_id"].as_str() == Some("1");
                let sec = d["expiry_time_samp"]
                    .as_i64()
                    .or_else(|| d["expiry_time_samp"].as_str().and_then(|s| s.parse().ok()))
                    .or_else(|| d["user_pause_time"].as_i64())
                    .unwrap_or(0);
                acct_hours = sec / 3600;
                acct_mins = (sec % 3600) / 60;
                let nick = d["nickname"]
                    .as_str()
                    .or_else(|| d["mobile"].as_str())
                    .unwrap_or("已认证");
                acct_sub_text = nick.to_string();
            } else {
                acct_sub_text = "Token 已过期".to_string();
            }
        }
    }

    Some(PollSnapshot {
        service_active: st["service_active"].as_bool().unwrap_or(false),
        service_state: st["service_state"].as_str().unwrap_or("inactive").to_string(),
        version: st["version"].as_str().unwrap_or("1.3.0.02").to_string(),
        token,
        user_id: st["user_id"].as_str().unwrap_or("").to_string(),
        udp_6066: st["udp_6066"].as_str().unwrap_or("监听正常 (6066/udp)").to_string(),
        traffic_iface: st["traffic_iface"].as_str().unwrap_or("tun_PC").to_string(),
        rx_bytes: st["rx_bytes"].as_u64().unwrap_or(0),
        tx_bytes: st["tx_bytes"].as_u64().unwrap_or(0),
        profiles,
        is_accelerating,
        active_area,
        net_delay,
        loss_rate,
        acct_configured,
        acct_paused,
        acct_hours,
        acct_mins,
        acct_sub_text,
        auto_pause_on_stop,
        custom_dns,
        detected_gateway_dns,
    })
}

/// Row with a dimmed value label on the right, the GNOME Settings way.
fn value_row(title: &str) -> (adw::ActionRow, gtk::Label) {
    let row = adw::ActionRow::builder().title(title).build();
    let value = gtk::Label::new(Some("—"));
    value.add_css_class("dim-label");
    value.add_css_class("numeric");
    row.add_suffix(&value);
    (row, value)
}

/// Row with a single action button on the right.
fn button_row(title: &str, subtitle: &str, label: &str) -> (adw::ActionRow, gtk::Button) {
    let row = adw::ActionRow::builder().title(title).subtitle(subtitle).build();
    let btn = gtk::Button::with_label(label);
    btn.set_valign(gtk::Align::Center);
    row.add_suffix(&btn);
    (row, btn)
}

/// Tiny throughput sparkline drawn in the theme accent colour.
fn sparkline(history: Rc<RefCell<Vec<f64>>>) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.set_content_width(84);
    area.set_content_height(24);
    area.set_valign(gtk::Align::Center);
    area.set_draw_func(move |w, cr, width, height| {
        let h = history.borrow();
        if h.len() < 2 {
            return;
        }
        let max = h.iter().cloned().fold(1.0, f64::max);
        let (wf, hf) = (width as f64, height as f64);
        let step = wf / (SPARK_POINTS - 1) as f64;
        let x0 = wf - step * (h.len() - 1) as f64;
        let c = w.color();
        cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, 0.8);
        cr.set_line_width(1.5);
        for (i, v) in h.iter().enumerate() {
            let x = x0 + step * i as f64;
            let y = hf - 1.0 - (v / max) * (hf - 2.0);
            if i == 0 {
                cr.move_to(x, y);
            } else {
                cr.line_to(x, y);
            }
        }
        let _ = cr.stroke();
    });
    area
}

pub fn run_gui() {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run_with_args::<&str>(&[]);
}

fn build_ui(app: &adw::Application) {
    if let Some(win) = app.active_window() {
        win.present();
        return;
    }

    // Main window: header bar + clamped column of boxed lists.
    let win = adw::ApplicationWindow::builder()
        .application(app)
        .title("Leinux")
        .icon_name(APP_ID)
        .default_width(460)
        .default_height(760)
        .build();
    gtk::IconTheme::for_display(&gtk::prelude::WidgetExt::display(&win))
        .add_search_path("/opt/leigod/webui");

    let window_title = adw::WindowTitle::new("Leinux", "正在连接服务…");
    let settings_btn = gtk::Button::from_icon_name("emblem-system-symbolic");
    settings_btn.set_tooltip_text(Some("设置"));
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&window_title));
    header.pack_end(&settings_btn);

    let status_icon = gtk::Image::from_icon_name("io.github.asabalazy.leinux-symbolic");
    status_icon.set_pixel_size(96);
    status_icon.add_css_class("dim-label");
    let status_title = gtk::Label::new(Some("未加速"));
    status_title.add_css_class("title-1");
    let status_sub = gtk::Label::new(Some("非官方的雷神加速器Linux客户端"));
    status_sub.add_css_class("dim-label");
    status_sub.add_css_class("numeric");
    let main_btn = gtk::Button::with_label("启动加速");
    main_btn.add_css_class("pill");
    main_btn.add_css_class("suggested-action");
    main_btn.set_halign(gtk::Align::Center);
    main_btn.set_margin_top(12);

    let hero = gtk::Box::new(gtk::Orientation::Vertical, 6);
    hero.set_margin_top(18);
    hero.append(&status_icon);
    hero.append(&status_title);
    hero.append(&status_sub);
    hero.append(&main_btn);

    let region_model = gtk::StringList::new(&[]);
    let region_row = adw::ComboRow::builder().title("区服").model(&region_model).build();
    let region_group = adw::PreferencesGroup::builder().title("加速区服").build();
    region_group.add(&region_row);

    let speed_history: Rc<RefCell<Vec<f64>>> = Rc::new(RefCell::new(Vec::new()));
    let spark = sparkline(speed_history.clone());
    let (delay_row, delay_val) = value_row("延迟");
    let (loss_row, loss_val) = value_row("丢包率");
    let (speed_row, speed_val) = value_row("实时速率");
    speed_row.add_suffix(&spark);
    let (total_row, total_val) = value_row("累计流量");
    let (iface_row, iface_val) = value_row("隧道");
    let conn_group = adw::PreferencesGroup::builder().title("连接").build();
    for row in [&delay_row, &loss_row, &speed_row, &total_row, &iface_row] {
        conn_group.add(row);
    }

    let (acct_row, acct_btn) = button_row("—", "正在读取账号…", "恢复时长");
    acct_row.add_css_class("property");
    let acct_group = adw::PreferencesGroup::builder().title("雷神账号").build();
    acct_group.add(&acct_row);

    let column = gtk::Box::new(gtk::Orientation::Vertical, 24);
    column.set_margin_start(12);
    column.set_margin_end(12);
    column.set_margin_bottom(24);
    column.append(&hero);
    column.append(&region_group);
    column.append(&conn_group);
    column.append(&acct_group);

    let clamp = adw::Clamp::builder().maximum_size(560).child(&column).build();
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&clamp)
        .build();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&scroller));
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&view));
    win.set_content(Some(&toasts));

    // Preferences dialog.
    let token_row = adw::PasswordEntryRow::builder()
        .title("官网 account_token")
        .show_apply_button(true)
        .build();
    let (snippet_row, copy_btn) =
        button_row("获取 Token", "官网登录后按 F12，在控制台粘贴提取命令", "复制命令");
    let auto_pause_row = adw::SwitchRow::builder()
        .title("停止加速时暂停时长")
        .subtitle("关机或停止服务时同样生效")
        .build();
    let acct_prefs = adw::PreferencesGroup::builder().title("雷神账号").build();
    acct_prefs.add(&token_row);
    acct_prefs.add(&snippet_row);
    acct_prefs.add(&auto_pause_row);

    let dns_row = adw::EntryRow::builder().title("直连 DNS").show_apply_button(true).build();
    let net_prefs = adw::PreferencesGroup::builder().title("网络").build();
    net_prefs.add(&dns_row);

    let (refresh_nodes_row, refresh_nodes_btn) =
        button_row("同步云端节点", "直接从雷神云端拉取 9 大区服最新节点（免手机）", "同步");
    let (restart_row, restart_btn) = button_row("重启守护服务", "重启 leigod_plugin 服务", "重启");
    let (repair_row, repair_btn) = button_row("修复设备绑定", "重新向雷神同步本机设备", "修复");
    let (cleanup_row, cleanup_btn) = button_row("清理防火墙规则", "移除残留的 iptables 规则", "清理");
    cleanup_btn.add_css_class("destructive-action");
    let maint_prefs = adw::PreferencesGroup::builder().title("维护").build();
    for row in [&refresh_nodes_row, &restart_row, &repair_row, &cleanup_row] {
        maint_prefs.add(row);
    }

    let page = adw::PreferencesPage::new();
    page.add(&acct_prefs);
    page.add(&net_prefs);
    page.add(&maint_prefs);
    let prefs = adw::PreferencesDialog::new();
    prefs.set_title("设置");
    prefs.add(&page);

    let toast: Rc<dyn Fn(&str)> = {
        let toasts = toasts.clone();
        let prefs = prefs.clone();
        Rc::new(move |msg: &str| {
            let t = adw::Toast::new(msg);
            if prefs.is_mapped() {
                prefs.add_toast(t);
            } else {
                toasts.add_toast(t);
            }
        })
    };

    let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCmd>();
    let (evt_tx, evt_rx) = mpsc::channel::<WorkerEvent>();
    let evt_tx_worker = evt_tx.clone();

    thread::spawn(move || {
        let evt_tx = evt_tx_worker;
        let mut last_snap: Option<PollSnapshot> = None;
        let mut ticks: u64 = 0;
        let mut last_poll = Instant::now() - Duration::from_secs(10);

        loop {
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    WorkerCmd::StartProfile(prof, tok) => {
                        let _ = evt_tx.send(WorkerEvent::Toast(format!("正在恢复时长并启动 {}...", prof.name)));
                        let _ = http_request("POST", "/api/account/recover", Some(&json!({})), 6000);
                        let req = json!({
                            "cmd": "STARTACC",
                            "tag": "gui-start",
                            "sn": "",
                            "cli": "webui",
                            "code": 0,
                            "err": "",
                            "data": {
                                "token": tok,
                                "catalog": prof.catalog,
                                "subCatalog": prof.sub_catalog,
                                "conf": prof.conf,
                            }
                        });
                        match http_request("POST", "/api/ws-rpc", Some(&req), 8000) {
                            Ok(res) if res["code"].as_i64() == Some(0) => {
                                let _ = evt_tx.send(WorkerEvent::Toast(format!("已启动 {} 加速", prof.name)));
                            }
                            _ => {
                                let _ = evt_tx.send(WorkerEvent::Toast("启动加速失败".to_string()));
                            }
                        }
                        if let Some(s) = fetch_snapshot(true, last_snap.as_ref()) {
                            let _ = evt_tx.send(WorkerEvent::Snapshot(s.clone()));
                            last_snap = Some(s);
                        }
                    }
                    WorkerCmd::PauseAccount => {
                        let _ = evt_tx.send(WorkerEvent::Toast("正在断开隧道并暂停账号时长...".to_string()));
                        if let Ok(res) = http_request("POST", "/api/account/pause", Some(&json!({})), 8000) {
                            let msg = if res["local_only"].as_bool() == Some(true) {
                                res["msg"].as_str().unwrap_or("已停止本地加速").to_string()
                            } else if res["code"].as_i64() == Some(0) {
                                "已停止加速并成功暂停账号时长".to_string()
                            } else {
                                format!("已停止加速 ({})", res["msg"].as_str().unwrap_or("官网暂停失败"))
                            };
                            let _ = evt_tx.send(WorkerEvent::Toast(msg));
                        }
                        if let Some(s) = fetch_snapshot(true, last_snap.as_ref()) {
                            let _ = evt_tx.send(WorkerEvent::Snapshot(s.clone()));
                            last_snap = Some(s);
                        }
                    }
                    WorkerCmd::RecoverAccount => {
                        if let Ok(res) = http_request("POST", "/api/account/recover", Some(&json!({})), 6000) {
                            let msg = if res["code"].as_i64() == Some(0) {
                                "已恢复雷神账号时长".to_string()
                            } else {
                                format!("恢复返回: {}", res["msg"].as_str().unwrap_or("请先配置官网 Token"))
                            };
                            let _ = evt_tx.send(WorkerEvent::Toast(msg));
                        }
                        if let Some(s) = fetch_snapshot(true, last_snap.as_ref()) {
                            let _ = evt_tx.send(WorkerEvent::Snapshot(s.clone()));
                            last_snap = Some(s);
                        }
                    }
                    WorkerCmd::SaveToken(tok, auto_pause) => {
                        let _ = http_request(
                            "POST",
                            "/api/account/save-token",
                            Some(&json!({"account_token": tok, "auto_pause_on_stop": auto_pause})),
                            6000,
                        );
                        let _ = evt_tx.send(WorkerEvent::Toast("已保存官网 Token 并更新时长".to_string()));
                        if let Some(s) = fetch_snapshot(true, last_snap.as_ref()) {
                            let _ = evt_tx.send(WorkerEvent::Snapshot(s.clone()));
                            last_snap = Some(s);
                        }
                    }
                    WorkerCmd::SaveAutoPause(auto_pause) => {
                        let _ = http_request(
                            "POST",
                            "/api/account/save-token",
                            Some(&json!({"auto_pause_on_stop": auto_pause})),
                            4000,
                        );
                        let _ = evt_tx.send(WorkerEvent::Toast(if auto_pause {
                            "已开启关机自动暂停时长".to_string()
                        } else {
                            "已关闭关机自动暂停时长".to_string()
                        }));
                    }
                    WorkerCmd::SaveCustomDns(dns) => {
                        let _ = http_request(
                            "POST",
                            "/api/account/save-token",
                            Some(&json!({"custom_dns": dns})),
                            5000,
                        );
                        let _ = evt_tx.send(WorkerEvent::Toast(if dns.is_empty() {
                            "已恢复自动使用默认网关 DNS".to_string()
                        } else {
                            format!("已保存自定义直连 DNS: {dns}")
                        }));
                        if let Some(s) = fetch_snapshot(true, last_snap.as_ref()) {
                            let _ = evt_tx.send(WorkerEvent::Snapshot(s.clone()));
                            last_snap = Some(s);
                        }
                    }
                    WorkerCmd::RefreshNodes => {
                        let _ = evt_tx.send(WorkerEvent::Toast("正在从雷神云端拉取最新节点池...".to_string()));
                        match http_request("POST", "/api/profiles/refresh", Some(&json!({})), 10000) {
                            Ok(res) if res["ok"].as_bool() == Some(true) => {
                                let msg = res["msg"]
                                    .as_str()
                                    .unwrap_or("已同步雷神云端最新节点池")
                                    .to_string();
                                let _ = evt_tx.send(WorkerEvent::Toast(msg));
                            }
                            _ => {
                                let _ = evt_tx.send(WorkerEvent::Toast("同步云端节点失败".to_string()));
                            }
                        }
                        if let Some(s) = fetch_snapshot(false, last_snap.as_ref()) {
                            let _ = evt_tx.send(WorkerEvent::Snapshot(s.clone()));
                            last_snap = Some(s);
                        }
                    }
                    WorkerCmd::RestartService => {
                        let _ = evt_tx.send(WorkerEvent::Toast("正在重启加速守护服务...".to_string()));
                        let _ = http_request("POST", "/api/service-restart", Some(&json!({})), 10000);
                        thread::sleep(Duration::from_millis(800));
                        let _ = evt_tx.send(WorkerEvent::Toast("加速守护服务已重启".to_string()));
                        if let Some(s) = fetch_snapshot(true, last_snap.as_ref()) {
                            let _ = evt_tx.send(WorkerEvent::Snapshot(s.clone()));
                            last_snap = Some(s);
                        }
                    }
                    WorkerCmd::RepairBound(tok, uid) => {
                        let _ = http_request(
                            "POST",
                            "/api/ws-rpc",
                            Some(&json!({"cmd": "AUTOREPAIRBOUND", "tag": "repair", "sn": "", "cli": "webui", "code": 0, "err": "", "data": {"rToken": tok, "userId": uid}})),
                            5000,
                        );
                        let _ = evt_tx.send(WorkerEvent::Toast("已执行设备绑定修复".to_string()));
                    }
                    WorkerCmd::CleanupNet => {
                        let _ = http_request("POST", "/api/cleanup-net", Some(&json!({})), 5000);
                        let _ = evt_tx.send(WorkerEvent::Toast("已清理残留防火墙规则".to_string()));
                        if let Some(s) = fetch_snapshot(false, last_snap.as_ref()) {
                            let _ = evt_tx.send(WorkerEvent::Snapshot(s.clone()));
                            last_snap = Some(s);
                        }
                    }
                }
            }

            if last_poll.elapsed() >= Duration::from_secs(2) {
                let include_acct = ticks == 0 || ticks % 30 == 0;
                if let Some(s) = fetch_snapshot(include_acct, last_snap.as_ref()) {
                    let _ = evt_tx.send(WorkerEvent::Snapshot(s.clone()));
                    last_snap = Some(s);
                }
                ticks += 1;
                last_poll = Instant::now();
            }

            thread::sleep(Duration::from_millis(60));
        }
    });

    let ui_state: Rc<RefCell<PollSnapshot>> = Rc::new(RefCell::new(PollSnapshot::default()));
    let started_at: Rc<Cell<Option<Instant>>> = Rc::new(Cell::new(None));
    let suppress = Rc::new(Cell::new(false));

    let selected_profile = {
        let st = ui_state.clone();
        let row = region_row.clone();
        move || -> Option<ProfileItem> { st.borrow().profiles.get(row.selected() as usize).cloned() }
    };

    let refresh_hero = {
        let st = ui_state.clone();
        let started_at = started_at.clone();
        let (icon, title, sub, btn) =
            (status_icon.clone(), status_title.clone(), status_sub.clone(), main_btn.clone());
        Rc::new(move || {
            let cur = st.borrow();
            btn.set_sensitive(cur.service_active && !cur.profiles.is_empty());
            if cur.is_accelerating {
                let name = cur
                    .profiles
                    .iter()
                    .find(|p| p.area == cur.active_area)
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| cur.active_area.to_uppercase());
                let secs = started_at.get().map(|t| t.elapsed().as_secs()).unwrap_or(0);
                icon.set_css_classes(&["accent"]);
                title.set_text(&name);
                sub.set_text(&format!("加速中 · {}", format_hms(secs)));
                btn.set_label("停止加速");
                btn.set_css_classes(&["pill", "destructive-action"]);
            } else {
                icon.set_css_classes(&["dim-label"]);
                title.set_text(if cur.service_active { "未加速" } else { "服务未运行" });
                sub.set_text("非官方的雷神加速器Linux客户端");
                btn.set_label("启动加速");
                btn.set_css_classes(&["pill", "suggested-action"]);
            }
        })
    };

    // UI events.
    {
        let (prefs, win) = (prefs.clone(), win.clone());
        settings_btn.connect_clicked(move |_| prefs.present(Some(&win)));
    }
    {
        let (tx, st, sup) = (cmd_tx.clone(), ui_state.clone(), suppress.clone());
        let selected_profile = selected_profile.clone();
        let refresh = refresh_hero.clone();
        region_row.connect_selected_notify(move |_| {
            if sup.get() {
                return;
            }
            let cur = st.borrow();
            if let Some(p) = selected_profile() {
                if cur.is_accelerating && cur.active_area != p.area {
                    let _ = tx.send(WorkerCmd::StartProfile(p, cur.token.clone()));
                }
            }
            drop(cur);
            refresh();
        });
    }
    {
        let (tx, st) = (cmd_tx.clone(), ui_state.clone());
        main_btn.connect_clicked(move |_| {
            let cur = st.borrow();
            if cur.is_accelerating {
                let _ = tx.send(WorkerCmd::PauseAccount);
            } else if let Some(p) = selected_profile() {
                let _ = tx.send(WorkerCmd::StartProfile(p, cur.token.clone()));
            }
        });
    }
    {
        let (tx, st) = (cmd_tx.clone(), ui_state.clone());
        let (prefs, win) = (prefs.clone(), win.clone());
        acct_btn.connect_clicked(move |_| {
            let cur = st.borrow();
            if !cur.acct_configured {
                prefs.present(Some(&win));
            } else if cur.acct_paused {
                let _ = tx.send(WorkerCmd::RecoverAccount);
            } else {
                let _ = tx.send(WorkerCmd::PauseAccount);
            }
        });
    }
    {
        let toast = toast.clone();
        copy_btn.connect_clicked(move |btn| {
            btn.clipboard().set_text(TOKEN_SNIPPET);
            toast("已复制提取命令");
        });
    }
    {
        let (tx, sw) = (cmd_tx.clone(), auto_pause_row.clone());
        token_row.connect_apply(move |row| {
            let tok = row.text().trim().to_string();
            row.set_text("");
            let _ = tx.send(WorkerCmd::SaveToken(tok, sw.is_active()));
        });
    }
    {
        let (tx, sup) = (cmd_tx.clone(), suppress.clone());
        auto_pause_row.connect_active_notify(move |row| {
            if !sup.get() {
                let _ = tx.send(WorkerCmd::SaveAutoPause(row.is_active()));
            }
        });
    }
    {
        let tx = cmd_tx.clone();
        dns_row.connect_apply(move |row| {
            let _ = tx.send(WorkerCmd::SaveCustomDns(row.text().trim().to_string()));
        });
    }
    {
        let tx = cmd_tx.clone();
        refresh_nodes_btn.connect_clicked(move |_| {
            let _ = tx.send(WorkerCmd::RefreshNodes);
        });
    }
    {
        let tx = cmd_tx.clone();
        restart_btn.connect_clicked(move |_| {
            let _ = tx.send(WorkerCmd::RestartService);
        });
    }
    {
        let (tx, st) = (cmd_tx.clone(), ui_state.clone());
        repair_btn.connect_clicked(move |_| {
            let cur = st.borrow();
            let _ = tx.send(WorkerCmd::RepairBound(cur.token.clone(), cur.user_id.clone()));
        });
    }
    {
        let tx = cmd_tx.clone();
        cleanup_btn.connect_clicked(move |_| {
            let _ = tx.send(WorkerCmd::CleanupNet);
        });
    }

    // Apply worker events on the main loop; also ticks the session timer.
    let mut last_sample: Option<(u64, u64, Instant)> = None;
    glib::timeout_add_local(Duration::from_millis(250), move || {
        if started_at.get().is_some() {
            refresh_hero();
        }
        while let Ok(evt) = evt_rx.try_recv() {
            let snap = match evt {
                WorkerEvent::Toast(msg) => {
                    toast(&msg);
                    continue;
                }
                WorkerEvent::Snapshot(snap) => snap,
            };

            window_title.set_subtitle(&if snap.service_active {
                format!("服务运行中 · v{}", snap.version)
            } else {
                format!("服务异常（{}）", snap.service_state)
            });

            // Throughput.
            let now = Instant::now();
            if let Some((prx, ptx, pts)) = last_sample {
                let dt = now.duration_since(pts).as_secs_f64();
                if dt > 0.1 {
                    let rx = snap.rx_bytes.saturating_sub(prx) as f64 / dt;
                    let tx = snap.tx_bytes.saturating_sub(ptx) as f64 / dt;
                    speed_val.set_text(&format!("↓ {}/s  ↑ {}/s", format_bytes(rx, 0), format_bytes(tx, 0)));
                    let mut h = speed_history.borrow_mut();
                    h.push(rx + tx);
                    if h.len() > SPARK_POINTS {
                        h.remove(0);
                    }
                    drop(h);
                    spark.queue_draw();
                }
            }
            last_sample = Some((snap.rx_bytes, snap.tx_bytes, now));
            total_val.set_text(&format_bytes((snap.rx_bytes + snap.tx_bytes) as f64, 1));
            iface_val.set_text(&format!("{} · {}", snap.traffic_iface, snap.udp_6066));

            // Region list, rebuilt only when profiles change.
            suppress.set(true);
            let changed = {
                let prev = &ui_state.borrow().profiles;
                prev.len() != snap.profiles.len()
                    || prev.iter().zip(&snap.profiles).any(|(a, b)| a.area != b.area || a.name != b.name)
            };
            if changed {
                let names: Vec<&str> = snap.profiles.iter().map(|p| p.name.as_str()).collect();
                region_model.splice(0, region_model.n_items(), &names);
            }
            if snap.is_accelerating {
                if let Some(i) = snap.profiles.iter().position(|p| p.area == snap.active_area) {
                    region_row.set_selected(i as u32);
                }
            }

            // Session state.
            if snap.is_accelerating {
                if started_at.get().is_none() {
                    started_at.set(Some(Instant::now()));
                }
                delay_val.set_text(&if snap.net_delay > 0 { format!("{} ms", snap.net_delay) } else { "—".into() });
                loss_val.set_text(&format!("{} %", snap.loss_rate));
            } else {
                started_at.set(None);
                delay_val.set_text("—");
                loss_val.set_text("—");
            }

            // Account.
            if snap.acct_configured {
                acct_row.set_title(&format!("{} 小时 {} 分", snap.acct_hours, snap.acct_mins));
                acct_row.set_subtitle(&format!(
                    "{} · {}",
                    snap.acct_sub_text,
                    if snap.acct_paused { "已暂停" } else { "计时中" }
                ));
                acct_btn.set_label(if snap.acct_paused { "恢复时长" } else { "暂停时长" });
            } else {
                acct_row.set_title("未配置 Token");
                acct_row.set_subtitle("配置后可自动恢复与暂停时长");
                acct_btn.set_label("配置");
            }

            // Preferences.
            auto_pause_row.set_active(snap.auto_pause_on_stop);
            net_prefs.set_description(Some(&format!("留空则使用网关 DNS（{}）", snap.detected_gateway_dns)));
            if !dns_row.has_focus() && dns_row.text() != snap.custom_dns {
                dns_row.set_text(&snap.custom_dns);
            }
            suppress.set(false);

            *ui_state.borrow_mut() = snap;
            refresh_hero();
        }
        glib::ControlFlow::Continue
    });

    win.present();
}
