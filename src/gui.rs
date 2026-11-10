use crate::http_request;
use crate::login::{find_chromium_binary, run_browser_login_popup};
use crate::types::{AccountSnapshot, DaemonSnapshot, ProfileSummary, CONTINENT_ORDER};
use adw::prelude::*;
use gtk4 as gtk;
use gtk4::glib;
use serde_json::json;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const APP_ID: &str = "io.github.asabalazy.leinux";
const SPARK_POINTS: usize = 30;

enum WorkerEvent {
    Snapshot(Box<DaemonSnapshot>),
    Toast(String),
}

enum WorkerCmd {
    StartArea(String, String),
    SaveLastArea(String),
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

#[derive(Default)]
struct GuiState {
    snap: DaemonSnapshot,
    profiles: Vec<ProfileSummary>,
    account: AccountSnapshot,
    selected_area: String,
    continent_filter: String,
    started_at: Option<Instant>,
    last_sample: Option<(u64, u64, Instant)>,
}

impl GuiState {
    fn selected_profile(&self) -> Option<&ProfileSummary> {
        if self.selected_area.is_empty() {
            return None;
        }
        self.profiles.iter().find(|p| p.area == self.selected_area)
    }
}

#[derive(Clone)]
struct MainWidgets {
    win: adw::ApplicationWindow,
    window_title: adw::WindowTitle,
    status_icon: gtk::Image,
    status_title: gtk::Label,
    status_sub: gtk::Label,
    main_btn: gtk::Button,
    settings_btn: gtk::Button,
    region_row: adw::ActionRow,
    region_val: gtk::Label,
    delay_val: gtk::Label,
    loss_val: gtk::Label,
    speed_val: gtk::Label,
    total_val: gtk::Label,
    iface_val: gtk::Label,
    spark: gtk::DrawingArea,
    speed_history: Rc<RefCell<Vec<f64>>>,
    acct_row: adw::ActionRow,
    acct_btn: gtk::Button,
    toasts: adw::ToastOverlay,
}

#[derive(Clone)]
struct PrefsWidgets {
    dialog: adw::PreferencesDialog,
    login_btn: gtk::Button,
    token_row: adw::PasswordEntryRow,
    auto_pause_row: adw::SwitchRow,
    dns_row: gtk::Editable,
    dns_entry: adw::EntryRow,
    net_prefs: adw::PreferencesGroup,
    refresh_nodes_btn: gtk::Button,
    restart_btn: gtk::Button,
    repair_btn: gtk::Button,
    cleanup_btn: gtk::Button,
}

fn format_bytes(bytes: f64, decimals: usize) -> String {
    let n = bytes.max(0.0);
    if n < 1024.0 {
        format!("{:.0} B", n.round())
    } else if n < 1_048_576.0 {
        format!("{:.*} KB", decimals, n / 1024.0)
    } else if n < 1_073_741_824.0 {
        format!("{:.*} MB", decimals, n / 1_048_576.0)
    } else {
        format!("{:.*} GB", decimals, n / 1_073_741_824.0)
    }
}

fn format_hms(secs: u64) -> String {
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
}

fn value_row(title: &str) -> (adw::ActionRow, gtk::Label) {
    let row = adw::ActionRow::builder().title(title).build();
    let value = gtk::Label::new(Some("—"));
    value.add_css_class("dim-label");
    value.add_css_class("numeric");
    row.add_suffix(&value);
    (row, value)
}

fn button_row(title: &str, subtitle: &str, label: &str) -> (adw::ActionRow, gtk::Button) {
    let row = adw::ActionRow::builder().title(title).subtitle(subtitle).build();
    let btn = gtk::Button::with_label(label);
    btn.set_valign(gtk::Align::Center);
    row.add_suffix(&btn);
    (row, btn)
}

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
            if i == 0 { cr.move_to(x, y); } else { cr.line_to(x, y); }
        }
        let _ = cr.stroke();
    });
    area
}

fn fetch_daemon_snapshot(known_rev: u64, include_account: bool) -> Option<DaemonSnapshot> {
    let path = format!(
        "/api/status?known_rev={known_rev}&include_account={}",
        if include_account { 1 } else { 0 }
    );
    let timeout = if include_account { 6000 } else { 2500 };
    let val = http_request("GET", &path, None, timeout).ok()?;
    serde_json::from_value(val).ok()
}

fn handle_worker_cmd(cmd: WorkerCmd, evt_tx: &mpsc::Sender<WorkerEvent>, known_rev: &mut u64) {
    let toast = |msg: String| {
        let _ = evt_tx.send(WorkerEvent::Toast(msg));
    };
    let mut refresh_account = false;

    match cmd {
        WorkerCmd::StartArea(area, name) => {
            toast(format!("正在恢复时长并启动 {name}..."));
            match http_request("POST", "/api/acc/start", Some(&json!({"area": area, "recover": true})), 10000) {
                Ok(res) if res["ok"].as_bool() == Some(true) => toast(format!("已启动 {name} 加速")),
                Ok(res) => toast(res["msg"].as_str().unwrap_or("启动加速失败").to_string()),
                Err(_) => toast("启动加速失败".to_string()),
            }
            refresh_account = true;
        }
        WorkerCmd::SaveLastArea(area) => {
            let _ = http_request("POST", "/api/account/save-token", Some(&json!({"last_area": area})), 3000);
            return;
        }
        WorkerCmd::PauseAccount => {
            toast("正在断开隧道并暂停账号时长...".to_string());
            if let Ok(res) = http_request("POST", "/api/account/pause", Some(&json!({})), 8000) {
                let msg = if res["local_only"].as_bool() == Some(true) {
                    res["msg"].as_str().unwrap_or("已停止本地加速").to_string()
                } else if res["code"].as_i64() == Some(0) {
                    "已停止加速并成功暂停账号时长".to_string()
                } else {
                    format!("已停止加速 ({})", res["msg"].as_str().unwrap_or("官网暂停失败"))
                };
                toast(msg);
            }
            refresh_account = true;
        }
        WorkerCmd::RecoverAccount => {
            if let Ok(res) = http_request("POST", "/api/account/recover", Some(&json!({})), 6000) {
                let msg = if res["code"].as_i64() == Some(0) {
                    "已恢复雷神账号时长".to_string()
                } else {
                    format!("恢复返回: {}", res["msg"].as_str().unwrap_or("请先配置官网 Token"))
                };
                toast(msg);
            }
            refresh_account = true;
        }
        WorkerCmd::SaveToken(tok, auto_pause) => {
            let _ = http_request(
                "POST",
                "/api/account/save-token",
                Some(&json!({"account_token": tok, "auto_pause_on_stop": auto_pause})),
                12000,
            );
            toast("已保存凭证并同步本机绑定与账号状态".to_string());
            refresh_account = true;
        }
        WorkerCmd::SaveAutoPause(auto_pause) => {
            let _ = http_request("POST", "/api/account/save-token", Some(&json!({"auto_pause_on_stop": auto_pause})), 4000);
            toast(if auto_pause { "已开启关机自动暂停时长" } else { "已关闭关机自动暂停时长" }.to_string());
            return;
        }
        WorkerCmd::SaveCustomDns(dns) => {
            let _ = http_request("POST", "/api/account/save-token", Some(&json!({"custom_dns": dns})), 5000);
            toast(if dns.is_empty() {
                "已恢复自动使用默认网关 DNS".to_string()
            } else {
                format!("已保存自定义直连 DNS: {dns}")
            });
            *known_rev = 0;
            refresh_account = true;
        }
        WorkerCmd::RefreshNodes => {
            toast("正在从雷神云端拉取最新节点池...".to_string());
            match http_request("POST", "/api/profiles/refresh", Some(&json!({})), 10000) {
                Ok(res) if res["ok"].as_bool() == Some(true) => {
                    toast(res["msg"].as_str().unwrap_or("已同步雷神云端最新节点池").to_string());
                }
                _ => toast("同步云端节点失败".to_string()),
            }
            *known_rev = 0;
        }
        WorkerCmd::RestartService => {
            toast("正在重启加速守护服务...".to_string());
            let _ = http_request("POST", "/api/service-restart", Some(&json!({})), 10000);
            thread::sleep(Duration::from_millis(800));
            toast("加速守护服务已重启".to_string());
            refresh_account = true;
        }
        WorkerCmd::RepairBound(tok, uid) => {
            let _ = http_request(
                "POST",
                "/api/ws-rpc",
                Some(&json!({"cmd": "AUTOREPAIRBOUND", "tag": "repair", "sn": "", "cli": "webui", "code": 0, "err": "", "data": {"rToken": tok, "userId": uid}})),
                6000,
            );
            toast("已执行设备绑定修复".to_string());
            return;
        }
        WorkerCmd::CleanupNet => {
            let _ = http_request("POST", "/api/cleanup-net", Some(&json!({})), 5000);
            toast("已清理残留防火墙规则".to_string());
        }
    }

    if let Some(snap) = fetch_daemon_snapshot(*known_rev, refresh_account) {
        *known_rev = snap.profiles_rev;
        let _ = evt_tx.send(WorkerEvent::Snapshot(Box::new(snap)));
    }
}

fn spawn_worker(cmd_rx: mpsc::Receiver<WorkerCmd>, evt_tx: mpsc::Sender<WorkerEvent>) {
    thread::spawn(move || {
        let mut known_rev = 0u64;
        let mut ticks = 0u64;
        let mut last_poll = Instant::now() - Duration::from_secs(10);

        loop {
            while let Ok(cmd) = cmd_rx.try_recv() {
                handle_worker_cmd(cmd, &evt_tx, &mut known_rev);
            }
            if last_poll.elapsed() >= Duration::from_secs(2) {
                let include_acct = ticks.is_multiple_of(30);
                if let Some(snap) = fetch_daemon_snapshot(known_rev, include_acct) {
                    known_rev = snap.profiles_rev;
                    let _ = evt_tx.send(WorkerEvent::Snapshot(Box::new(snap)));
                }
                ticks += 1;
                last_poll = Instant::now();
            }
            thread::sleep(Duration::from_millis(60));
        }
    });
}

fn build_main_window(app: &adw::Application) -> MainWidgets {
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

    let region_row = adw::ActionRow::builder().title("区服").activatable(true).build();
    let region_val = gtk::Label::new(Some("未选择"));
    region_val.add_css_class("dim-label");
    let region_arrow = gtk::Image::from_icon_name("go-next-symbolic");
    region_arrow.add_css_class("dim-label");
    region_row.add_suffix(&region_val);
    region_row.add_suffix(&region_arrow);
    let region_group = adw::PreferencesGroup::builder().title("加速区服").build();
    region_group.add(&region_row);

    let speed_history = Rc::new(RefCell::new(Vec::new()));
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

    MainWidgets {
        win,
        window_title,
        status_icon,
        status_title,
        status_sub,
        main_btn,
        settings_btn,
        region_row,
        region_val,
        delay_val,
        loss_val,
        speed_val,
        total_val,
        iface_val,
        spark,
        speed_history,
        acct_row,
        acct_btn,
        toasts,
    }
}

fn build_prefs_dialog() -> PrefsWidgets {
    let (login_row, login_btn) = button_row(
        "网页一键登录",
        "弹出官方小窗口登录，自动提取凭证、绑定本机并永久自动续期",
        "打开登录窗",
    );
    login_btn.add_css_class("suggested-action");
    let token_row = adw::PasswordEntryRow::builder()
        .title("手动填入 Token（备用）")
        .show_apply_button(true)
        .build();
    let auto_pause_row = adw::SwitchRow::builder()
        .title("停止加速时暂停时长")
        .subtitle("关机或停止服务时同样生效")
        .build();
    let acct_prefs = adw::PreferencesGroup::builder().title("雷神账号").build();
    acct_prefs.add(&login_row);
    acct_prefs.add(&token_row);
    acct_prefs.add(&auto_pause_row);

    let dns_entry = adw::EntryRow::builder().title("直连 DNS").show_apply_button(true).build();
    let net_prefs = adw::PreferencesGroup::builder().title("网络").build();
    net_prefs.add(&dns_entry);

    let (refresh_nodes_row, refresh_nodes_btn) =
        button_row("同步云端节点", "直接从雷神云端动态拉取全部区服最新节点（免手机）", "同步");
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
    let dialog = adw::PreferencesDialog::new();
    dialog.set_title("设置");
    dialog.add(&page);

    PrefsWidgets {
        dialog,
        login_btn,
        token_row,
        auto_pause_row,
        dns_row: dns_entry.clone().upcast(),
        dns_entry,
        net_prefs,
        refresh_nodes_btn,
        restart_btn,
        repair_btn,
        cleanup_btn,
    }
}

fn build_region_dialog(
    state: Rc<RefCell<GuiState>>,
    mw: MainWidgets,
    cmd_tx: mpsc::Sender<WorkerCmd>,
) -> (adw::Dialog, Rc<dyn Fn()>) {
    let dialog = adw::Dialog::builder()
        .title("选择加速区服")
        .content_width(480)
        .content_height(560)
        .build();
    let filter_bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    filter_bar.add_css_class("linked");
    filter_bar.set_halign(gtk::Align::Center);
    filter_bar.set_margin_top(6);
    filter_bar.set_margin_bottom(8);
    filter_bar.set_margin_start(12);
    filter_bar.set_margin_end(12);

    let list_box = gtk::Box::new(gtk::Orientation::Vertical, 18);
    list_box.set_margin_top(6);
    list_box.set_margin_bottom(18);
    list_box.set_margin_start(12);
    list_box.set_margin_end(12);

    let clamp = adw::Clamp::builder().maximum_size(500).child(&list_box).build();
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&clamp)
        .build();
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
    vbox.append(&filter_bar);
    vbox.append(&scroller);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&vbox));
    dialog.set_child(Some(&view));

    let rebuild_list: Rc<dyn Fn()> = {
        let state = state.clone();
        let list_box = list_box.clone();
        let dlg = dialog.clone();
        Rc::new(move || {
            while let Some(child) = list_box.first_child() {
                list_box.remove(&child);
            }
            let st = state.borrow();
            for &cont in &CONTINENT_ORDER {
                if !st.continent_filter.is_empty() && st.continent_filter != cont {
                    continue;
                }
                let items: Vec<ProfileSummary> = st
                    .profiles
                    .iter()
                    .filter(|p| p.continent == cont)
                    .cloned()
                    .collect();
                if items.is_empty() {
                    continue;
                }
                let grp = adw::PreferencesGroup::builder().title(cont).build();
                for prof in items {
                    let row = adw::ActionRow::builder()
                        .title(&prof.name)
                        .subtitle(format!("{} · {} 个云端入口节点", prof.area.to_uppercase(), prof.node_count))
                        .activatable(true)
                        .build();
                    if prof.area == st.selected_area {
                        let chk = gtk::Image::from_icon_name("object-select-symbolic");
                        chk.add_css_class("accent");
                        row.add_suffix(&chk);
                    }
                    let state = state.clone();
                    let mw = mw.clone();
                    let dlg = dlg.clone();
                    let tx = cmd_tx.clone();
                    row.connect_activated(move |_| {
                        let (should_switch, area, name) = {
                            let mut s = state.borrow_mut();
                            s.selected_area = prof.area.clone();
                            mw.update_region_label(&s);
                            mw.refresh_hero(&s);
                            (
                                s.snap.is_accelerating && s.snap.active_area != prof.area,
                                prof.area.clone(),
                                prof.name.clone(),
                            )
                        };
                        dlg.close();
                        let _ = tx.send(WorkerCmd::SaveLastArea(area.clone()));
                        if should_switch {
                            let _ = tx.send(WorkerCmd::StartArea(area, name));
                        }
                    });
                    grp.add(&row);
                }
                list_box.append(&grp);
            }
        })
    };

    let rebuild: Rc<dyn Fn()> = {
        let state = state.clone();
        let filter_bar = filter_bar.clone();
        let rebuild_list = rebuild_list.clone();
        Rc::new(move || {
            while let Some(child) = filter_bar.first_child() {
                filter_bar.remove(&child);
            }
            let active_filter = {
                let mut st = state.borrow_mut();
                if !st.continent_filter.is_empty()
                    && !st.profiles.iter().any(|p| p.continent == st.continent_filter)
                {
                    st.continent_filter.clear();
                }
                st.continent_filter.clone()
            };

            let mut tabs: Vec<(&str, &str)> = vec![("全部", "")];
            {
                let st = state.borrow();
                for &cont in &CONTINENT_ORDER {
                    if st.profiles.iter().any(|p| p.continent == cont) {
                        tabs.push((cont, cont));
                    }
                }
            }

            let mut first_btn: Option<gtk::ToggleButton> = None;
            for (label, cont_val) in tabs {
                let b = gtk::ToggleButton::with_label(label);
                if let Some(ref f) = first_btn {
                    b.set_group(Some(f));
                } else {
                    first_btn = Some(b.clone());
                }
                if cont_val == active_filter {
                    b.set_active(true);
                }
                let state = state.clone();
                let rebuild_list = rebuild_list.clone();
                let cont_str = cont_val.to_string();
                b.connect_toggled(move |btn| {
                    if btn.is_active() {
                        state.borrow_mut().continent_filter = cont_str.clone();
                        rebuild_list();
                    }
                });
                filter_bar.append(&b);
            }
            rebuild_list();
        })
    };

    (dialog, rebuild)
}

impl MainWidgets {
    fn update_region_label(&self, st: &GuiState) {
        if let Some(p) = st.selected_profile() {
            self.region_val.set_text(&format!("{} · {}", p.continent, p.name));
        } else {
            self.region_val.set_text("未选择");
        }
    }

    fn refresh_hero(&self, st: &GuiState) {
        self.main_btn.set_sensitive(st.snap.service_active && !st.profiles.is_empty());
        if st.snap.is_accelerating {
            let name = st
                .profiles
                .iter()
                .find(|p| p.area == st.snap.active_area)
                .map(|p| p.name.as_str())
                .unwrap_or(&st.snap.active_area);
            let secs = st.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
            self.status_icon.set_css_classes(&["accent"]);
            self.status_title.set_text(name);
            self.status_sub.set_text(&format!("加速中 · {}", format_hms(secs)));
            self.main_btn.set_label("停止加速");
            self.main_btn.set_css_classes(&["pill", "destructive-action"]);
        } else {
            self.status_icon.set_css_classes(&["dim-label"]);
            self.status_title.set_text(if st.snap.service_active { "未加速" } else { "服务未运行" });
            self.status_sub.set_text("非官方的雷神加速器Linux客户端");
            self.main_btn.set_label("启动加速");
            self.main_btn.set_css_classes(&["pill", "suggested-action"]);
        }
    }

    fn apply_snapshot(&self, pw: &PrefsWidgets, st: &mut GuiState, mut snap: DaemonSnapshot) {
        if let Some(new_profiles) = snap.profiles.take() {
            st.profiles = new_profiles;
        }
        if let Some(new_acct) = snap.account.take() {
            st.account = new_acct;
        }

        self.window_title.set_subtitle(&if snap.service_active {
            format!("服务运行中 · v{}", snap.version)
        } else {
            format!("服务异常（{}）", snap.service_state)
        });

        let now = Instant::now();
        if let Some((prx, ptx, pts)) = st.last_sample {
            let dt = now.duration_since(pts).as_secs_f64();
            if dt > 0.1 {
                let rx = snap.rx_bytes.saturating_sub(prx) as f64 / dt;
                let tx = snap.tx_bytes.saturating_sub(ptx) as f64 / dt;
                self.speed_val.set_text(&format!("↓ {}/s  ↑ {}/s", format_bytes(rx, 0), format_bytes(tx, 0)));
                let mut h = self.speed_history.borrow_mut();
                h.push(rx + tx);
                if h.len() > SPARK_POINTS {
                    h.remove(0);
                }
                drop(h);
                self.spark.queue_draw();
            }
        }
        st.last_sample = Some((snap.rx_bytes, snap.tx_bytes, now));
        self.total_val.set_text(&format_bytes((snap.rx_bytes + snap.tx_bytes) as f64, 1));
        self.iface_val.set_text(&format!("{} · {}", snap.traffic_iface, snap.udp_6066));

        if snap.is_accelerating && !snap.active_area.is_empty() {
            st.selected_area = snap.active_area.clone();
        } else if st.selected_area.is_empty()
            && !snap.last_area.is_empty()
            && st.profiles.iter().any(|p| p.area == snap.last_area)
        {
            st.selected_area = snap.last_area.clone();
        } else if !st.selected_area.is_empty()
            && !st.profiles.is_empty()
            && !st.profiles.iter().any(|p| p.area == st.selected_area)
        {
            st.selected_area.clear();
        }

        if snap.is_accelerating {
            if st.started_at.is_none() {
                st.started_at = Some(Instant::now());
            }
            self.delay_val.set_text(&if snap.net_delay > 0 { format!("{} ms", snap.net_delay) } else { "—".into() });
            self.loss_val.set_text(&format!("{} %", snap.loss_rate));
        } else {
            st.started_at = None;
            self.delay_val.set_text("—");
            self.loss_val.set_text("—");
        }

        if st.account.configured {
            self.acct_row.set_title(&format!("{} 小时 {} 分", st.account.hours, st.account.mins));
            self.acct_row.set_subtitle(&format!(
                "{} · {}",
                st.account.sub_text,
                if st.account.paused { "已暂停" } else { "计时中" }
            ));
            self.acct_btn.set_label(if st.account.paused { "恢复时长" } else { "暂停时长" });
        } else {
            self.acct_row.set_title("未登录雷神账号");
            self.acct_row.set_subtitle("点击右侧一键弹出登录小窗口，自动完成绑定与同步");
            self.acct_btn.set_label("登录账号");
        }

        pw.auto_pause_row.set_active(st.account.auto_pause_on_stop);
        let gw_dns = if st.account.detected_gateway_dns.is_empty() {
            "223.5.5.5:53"
        } else {
            &st.account.detected_gateway_dns
        };
        pw.net_prefs.set_description(Some(&format!("留空则使用网关 DNS（{gw_dns}）")));
        if !pw.dns_entry.has_focus() && pw.dns_row.text() != st.account.custom_dns {
            pw.dns_row.set_text(&st.account.custom_dns);
        }

        st.snap = snap;
        self.update_region_label(st);
        self.refresh_hero(st);
    }
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

    let mw = build_main_window(app);
    let pw = build_prefs_dialog();
    let state = Rc::new(RefCell::new(GuiState::default()));
    let suppress = Rc::new(Cell::new(false));

    let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCmd>();
    let (evt_tx, evt_rx) = mpsc::channel::<WorkerEvent>();
    spawn_worker(cmd_rx, evt_tx.clone());

    let (region_dialog, rebuild_region_dialog) =
        build_region_dialog(state.clone(), mw.clone(), cmd_tx.clone());

    let toast = {
        let toasts = mw.toasts.clone();
        let prefs = pw.dialog.clone();
        Rc::new(move |msg: &str| {
            let t = adw::Toast::new(msg);
            if prefs.is_mapped() { prefs.add_toast(t); } else { toasts.add_toast(t); }
        })
    };

    {
        let (prefs, win) = (pw.dialog.clone(), mw.win.clone());
        mw.settings_btn.connect_clicked(move |_| prefs.present(Some(&win)));
    }
    {
        let (dlg, win, rebuild) = (region_dialog.clone(), mw.win.clone(), rebuild_region_dialog.clone());
        mw.region_row.connect_activated(move |_| {
            rebuild();
            dlg.present(Some(&win));
        });
    }
    {
        let (tx, st) = (cmd_tx.clone(), state.clone());
        let (dlg, win, rebuild, toast) = (
            region_dialog.clone(),
            mw.win.clone(),
            rebuild_region_dialog.clone(),
            toast.clone(),
        );
        mw.main_btn.connect_clicked(move |_| {
            let action = {
                let cur = st.borrow();
                if cur.snap.is_accelerating {
                    Some(WorkerCmd::PauseAccount)
                } else {
                    cur.selected_profile()
                        .map(|p| WorkerCmd::StartArea(p.area.clone(), p.name.clone()))
                }
            };
            if let Some(cmd) = action {
                let _ = tx.send(cmd);
            } else {
                toast("请先选择加速区服");
                rebuild();
                dlg.present(Some(&win));
            }
        });
    }
    let start_popup_login = {
        let tx = cmd_tx.clone();
        let evt_tx = evt_tx.clone();
        let sw = pw.auto_pause_row.clone();
        let prefs = pw.dialog.clone();
        let win = mw.win.clone();
        Rc::new(move |open_prefs_on_missing_browser: bool| {
            if find_chromium_binary().is_none() {
                let _ = evt_tx.send(WorkerEvent::Toast(
                    "未检测到 Chrome / Chromium / Edge 浏览器，请手动填入 Token".to_string(),
                ));
                if open_prefs_on_missing_browser {
                    prefs.present(Some(&win));
                }
                return;
            }
            let auto_pause = sw.is_active();
            let tx = tx.clone();
            let evt_tx = evt_tx.clone();
            let _ = evt_tx.send(WorkerEvent::Toast("正在打开雷神登录小窗口，登录成功后将自动关闭...".to_string()));
            thread::spawn(move || match run_browser_login_popup() {
                Ok(ptok) => {
                    let _ = evt_tx.send(WorkerEvent::Toast("已自动获取凭证，正在绑定设备并同步账号...".to_string()));
                    let _ = tx.send(WorkerCmd::SaveToken(ptok, auto_pause));
                }
                Err(msg) => {
                    let _ = evt_tx.send(WorkerEvent::Toast(msg));
                }
            });
        })
    };

    {
        let (tx, st) = (cmd_tx.clone(), state.clone());
        let login = start_popup_login.clone();
        mw.acct_btn.connect_clicked(move |_| {
            let (configured, paused) = {
                let cur = st.borrow();
                (cur.account.configured, cur.account.paused)
            };
            if !configured {
                login(true);
            } else if paused {
                let _ = tx.send(WorkerCmd::RecoverAccount);
            } else {
                let _ = tx.send(WorkerCmd::PauseAccount);
            }
        });
    }
    {
        let login = start_popup_login.clone();
        pw.login_btn.connect_clicked(move |_| login(false));
    }
    {
        let (tx, sw) = (cmd_tx.clone(), pw.auto_pause_row.clone());
        pw.token_row.connect_apply(move |row| {
            let tok = row.text().trim().to_string();
            row.set_text("");
            let _ = tx.send(WorkerCmd::SaveToken(tok, sw.is_active()));
        });
    }
    {
        let (tx, sup) = (cmd_tx.clone(), suppress.clone());
        pw.auto_pause_row.connect_active_notify(move |row| {
            if !sup.get() {
                let _ = tx.send(WorkerCmd::SaveAutoPause(row.is_active()));
            }
        });
    }
    {
        let tx = cmd_tx.clone();
        pw.dns_entry.connect_apply(move |row| {
            let _ = tx.send(WorkerCmd::SaveCustomDns(row.text().trim().to_string()));
        });
    }
    {
        let tx = cmd_tx.clone();
        pw.refresh_nodes_btn.connect_clicked(move |_| {
            let _ = tx.send(WorkerCmd::RefreshNodes);
        });
    }
    {
        let tx = cmd_tx.clone();
        pw.restart_btn.connect_clicked(move |_| {
            let _ = tx.send(WorkerCmd::RestartService);
        });
    }
    {
        let (tx, st) = (cmd_tx.clone(), state.clone());
        pw.repair_btn.connect_clicked(move |_| {
            let cur = st.borrow();
            let _ = tx.send(WorkerCmd::RepairBound(cur.snap.token.clone(), cur.snap.user_id.clone()));
        });
    }
    {
        let tx = cmd_tx.clone();
        pw.cleanup_btn.connect_clicked(move |_| {
            let _ = tx.send(WorkerCmd::CleanupNet);
        });
    }

    let mw_loop = mw.clone();
    glib::timeout_add_local(Duration::from_millis(250), move || {
        {
            let st = state.borrow();
            if st.started_at.is_some() {
                mw_loop.refresh_hero(&st);
            }
        }
        while let Ok(evt) = evt_rx.try_recv() {
            match evt {
                WorkerEvent::Toast(msg) => toast(&msg),
                WorkerEvent::Snapshot(snap) => {
                    let profiles_updated = snap.profiles.is_some();
                    suppress.set(true);
                    mw_loop.apply_snapshot(&pw, &mut state.borrow_mut(), *snap);
                    suppress.set(false);
                    if profiles_updated && region_dialog.is_mapped() {
                        rebuild_region_dialog();
                    }
                }
            }
        }
        glib::ControlFlow::Continue
    });

    mw.win.present();
}
