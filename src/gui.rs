use crate::http_request;
use crate::login::{has_login_browser, run_browser_login_popup};
use crate::types::{AccountSnapshot, DaemonSnapshot, ProfileSummary, CONTINENT_ORDER};
use adw::prelude::*;
use gtk4 as gtk;
use gtk4::{gio, glib};
use serde_json::json;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const APP_ID: &str = "io.github.asabalazy.leinux";
const SPARK_POINTS: usize = 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BusyOp {
    Starting,
    Stopping,
    AccountToggle,
    LoginPopup,
    RefreshNodes,
    RestartService,
}

enum WorkerEvent {
    Snapshot(Box<DaemonSnapshot>),
    Toast(String),
    ClearBusy(BusyOp),
}

enum WorkerCmd {
    StartArea(String),
    SaveLastArea(String),
    StopAccelerating,
    PauseAccountOnly,
    RecoverAccount,
    SaveToken { token: String, auto_pause: bool },
    SaveAutoPause(bool),
    SaveCustomDns(String),
    RefreshNodes,
    RestartService,
    RepairBound,
    CleanupNet,
}

#[derive(Default)]
struct GuiState {
    snap: DaemonSnapshot,
    profiles: Vec<ProfileSummary>,
    account: AccountSnapshot,
    selected_area: String,
    continent_filter: String,
    busy: Option<(BusyOp, String)>,
    started_at: Option<Instant>,
    last_sample: Option<(u64, u64, Instant)>,
}

impl GuiState {
    fn selected_profile(&self) -> Option<&ProfileSummary> {
        (!self.selected_area.is_empty())
            .then(|| self.profiles.iter().find(|p| p.area == self.selected_area))
            .flatten()
    }

    fn is_busy(&self, op: BusyOp) -> bool {
        self.busy.as_ref().is_some_and(|(cur, _)| *cur == op)
    }
}

#[derive(Clone)]
struct SpinButton {
    btn: gtk::Button,
    spinner: adw::Spinner,
    label: gtk::Label,
}

impl SpinButton {
    fn new(text: &str) -> Self {
        let spinner = adw::Spinner::new();
        spinner.set_visible(false);
        let label = gtk::Label::new(Some(text));
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        content.set_halign(gtk::Align::Center);
        content.append(&spinner);
        content.append(&label);
        let btn = gtk::Button::builder().child(&content).valign(gtk::Align::Center).build();
        Self { btn, spinner, label }
    }

    fn set_state(&self, spinning: bool, text: &str, sensitive: bool) {
        self.spinner.set_visible(spinning);
        self.label.set_text(text);
        self.btn.set_sensitive(sensitive && !spinning);
    }
}

#[derive(Clone)]
struct MainWidgets {
    win: adw::ApplicationWindow,
    window_title: adw::WindowTitle,
    status_icon: gtk::Image,
    status_title: gtk::Label,
    status_sub: gtk::Label,
    main_btn: SpinButton,
    region_row: adw::ActionRow,
    region_val: gtk::Label,
    delay_val: gtk::Label,
    loss_val: gtk::Label,
    speed_val: gtk::Label,
    total_val: gtk::Label,
    iface_val: gtk::Label,
    spark: gtk::DrawingArea,
    speed_history: Rc<RefCell<VecDeque<f64>>>,
    acct_row: adw::ActionRow,
    acct_btn: SpinButton,
    toasts: adw::ToastOverlay,
}

#[derive(Clone)]
struct PrefsWidgets {
    dialog: adw::PreferencesDialog,
    login_row: adw::ActionRow,
    login_btn: SpinButton,
    token_row: adw::PasswordEntryRow,
    auto_pause_row: adw::SwitchRow,
    dns_row: gtk::Editable,
    dns_entry: adw::EntryRow,
    net_prefs: adw::PreferencesGroup,
    refresh_nodes_btn: SpinButton,
    restart_btn: SpinButton,
    repair_btn: gtk::Button,
    cleanup_btn: gtk::Button,
}

#[derive(Clone)]
struct RegionPicker {
    dialog: adw::Dialog,
    filter_group: adw::ToggleGroup,
    list_box: gtk::Box,
    toasts: adw::ToastOverlay,
}

#[derive(Clone)]
struct UiCtx {
    state: Rc<RefCell<GuiState>>,
    suppress: Rc<Cell<bool>>,
    prefs_open: Rc<Cell<bool>>,
    picker_open: Rc<Cell<bool>>,
    active_toast: Rc<RefCell<Option<adw::Toast>>>,
    mw: MainWidgets,
    pw: PrefsWidgets,
    picker: RegionPicker,
    cmd_tx: mpsc::Sender<WorkerCmd>,
    evt_tx: mpsc::Sender<WorkerEvent>,
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

fn spin_button_row(title: &str, subtitle: &str, label: &str) -> (adw::ActionRow, SpinButton) {
    let row = adw::ActionRow::builder().title(title).subtitle(subtitle).build();
    let sbtn = SpinButton::new(label);
    row.add_suffix(&sbtn.btn);
    (row, sbtn)
}

fn clear_box(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

fn sparkline(history: Rc<RefCell<VecDeque<f64>>>) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.set_content_width(84);
    area.set_content_height(24);
    area.set_valign(gtk::Align::Center);
    area.set_draw_func(move |w, cr, width, height| {
        let h = history.borrow();
        if h.len() < 2 {
            return;
        }
        let max = h.iter().copied().fold(1.0, f64::max);
        let (wf, hf) = (width as f64, height as f64);
        let step = wf / (SPARK_POINTS - 1) as f64;
        let x0 = wf - step * (h.len() - 1) as f64;
        let c = w.color();
        cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, 0.8);
        cr.set_line_width(1.5);
        for (i, &v) in h.iter().enumerate() {
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
        u8::from(include_account)
    );
    let timeout = if include_account { 6000 } else { 2500 };
    http_request("GET", &path, None, timeout)
        .ok()
        .and_then(|v| serde_json::from_value(v).ok())
}

fn handle_worker_cmd(cmd: WorkerCmd, evt_tx: &mpsc::Sender<WorkerEvent>, known_rev: &mut u64) {
    let toast = |msg: &str| {
        let _ = evt_tx.send(WorkerEvent::Toast(msg.to_string()));
    };
    let clear_busy = |op: BusyOp| {
        let _ = evt_tx.send(WorkerEvent::ClearBusy(op));
    };
    let mut refresh_account = false;
    let mut finished_busy: Option<BusyOp> = None;

        match cmd {
        WorkerCmd::StartArea(area) => {
            match http_request("POST", "/api/acc/start", Some(&json!({"area": area, "recover": true})), 12000) {
                Ok(res) if res["ok"].as_bool() == Some(true) => {}
                Ok(res) => toast(res["msg"].as_str().unwrap_or("启动加速失败")),
                Err(_) => toast("启动加速失败，请检查守护服务状态"),
            }
            refresh_account = true;
            finished_busy = Some(BusyOp::Starting);
        }
        WorkerCmd::SaveLastArea(area) => {
            let _ = http_request("POST", "/api/account/save-token", Some(&json!({"last_area": area})), 3000);
            return;
        }
        WorkerCmd::StopAccelerating => {
            match http_request("POST", "/api/account/pause", Some(&json!({})), 8000) {
                Ok(res) if res["local_only"].as_bool() == Some(true) || res["code"].as_i64() == Some(0) => {}
                Ok(res) => toast(&format!("已断开隧道，但暂停时长失败：{}", res["msg"].as_str().unwrap_or("请检查账号状态"))),
                Err(_) => toast("停止加速失败"),
            }
            refresh_account = true;
            finished_busy = Some(BusyOp::Stopping);
        }
        WorkerCmd::PauseAccountOnly => {
            match http_request("POST", "/api/account/pause", Some(&json!({})), 8000) {
                Ok(res) if res["code"].as_i64() == Some(0) => {}
                Ok(res) => toast(res["msg"].as_str().unwrap_or("暂停时长失败")),
                Err(_) => toast("暂停时长失败"),
            }
            refresh_account = true;
            finished_busy = Some(BusyOp::AccountToggle);
        }
        WorkerCmd::RecoverAccount => {
            match http_request("POST", "/api/account/recover", Some(&json!({})), 6000) {
                Ok(res) if res["code"].as_i64() == Some(0) => {}
                Ok(res) => toast(res["msg"].as_str().unwrap_or("恢复时长失败")),
                Err(_) => toast("恢复时长失败"),
            }
            refresh_account = true;
            finished_busy = Some(BusyOp::AccountToggle);
        }
        WorkerCmd::SaveToken { token, auto_pause } => {
            match http_request(
                "POST",
                "/api/account/save-token",
                Some(&json!({"account_token": token, "auto_pause_on_stop": auto_pause})),
                12000,
            ) {
                Ok(res) if res["response"]["code"].as_i64() == Some(0) => {
                    toast("凭证验证通过，已完成绑定与同步");
                }
                Ok(_) => toast("已写入凭证，但官网校验未通过"),
                Err(_) => toast("保存凭证失败"),
            }
            refresh_account = true;
            finished_busy = Some(BusyOp::LoginPopup);
        }
        WorkerCmd::SaveAutoPause(auto_pause) => {
            let _ = http_request("POST", "/api/account/save-token", Some(&json!({"auto_pause_on_stop": auto_pause})), 4000);
            return;
        }
        WorkerCmd::SaveCustomDns(dns) => {
            let _ = http_request("POST", "/api/account/save-token", Some(&json!({"custom_dns": dns})), 5000);
            if dns.is_empty() {
                toast("已恢复使用默认网关 DNS");
            } else {
                toast(&format!("已保存直连 DNS：{dns}"));
            }
            *known_rev = 0;
            refresh_account = true;
        }
        WorkerCmd::RefreshNodes => {
            match http_request("POST", "/api/profiles/refresh", Some(&json!({})), 10000) {
                Ok(res) if res["ok"].as_bool() == Some(true) => {
                    toast(res["msg"].as_str().unwrap_or("已同步雷神云端最新节点池"));
                }
                _ => toast("同步云端节点失败"),
            }
            *known_rev = 0;
            finished_busy = Some(BusyOp::RefreshNodes);
        }
        WorkerCmd::RestartService => {
            let _ = http_request("POST", "/api/service-restart", Some(&json!({})), 10000);
            thread::sleep(Duration::from_millis(800));
            toast("加速守护服务已重启");
            refresh_account = true;
            finished_busy = Some(BusyOp::RestartService);
        }
        WorkerCmd::RepairBound => {
            let _ = http_request("POST", "/api/device/repair", Some(&json!({})), 6000);
            toast("已执行设备绑定修复");
            return;
        }
        WorkerCmd::CleanupNet => {
            let _ = http_request("POST", "/api/cleanup-net", Some(&json!({})), 5000);
            toast("已清理残留防火墙规则");
        }
    }

    if let Some(snap) = fetch_daemon_snapshot(*known_rev, refresh_account) {
        *known_rev = snap.profiles_rev;
        let _ = evt_tx.send(WorkerEvent::Snapshot(Box::new(snap)));
    }
    if let Some(op) = finished_busy {
        clear_busy(op);
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
                if let Some(snap) = fetch_daemon_snapshot(known_rev, ticks.is_multiple_of(30)) {
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

    let window_title = adw::WindowTitle::new("Leinux", "");
    let menu = gio::Menu::new();
    menu.append(Some("首选项"), Some("app.preferences"));
    menu.append(Some("关于 Leinux"), Some("app.about"));
    let menu_btn = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .primary(true)
        .tooltip_text("主菜单")
        .build();
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&window_title));
    header.pack_end(&menu_btn);

    let status_icon = gtk::Image::from_icon_name("io.github.asabalazy.leinux-symbolic");
    status_icon.set_pixel_size(88);
    status_icon.add_css_class("dim-label");
    let status_title = gtk::Label::new(Some("未加速"));
    status_title.add_css_class("title-1");
    let status_sub = gtk::Label::new(Some("非官方的雷神加速器 Linux 客户端"));
    status_sub.add_css_class("dim-label");

    let main_btn = SpinButton::new("启动加速");
    main_btn.btn.set_halign(gtk::Align::Center);
    main_btn.btn.set_margin_top(6);
    main_btn.btn.set_css_classes(&["pill", "suggested-action"]);

    let hero = gtk::Box::new(gtk::Orientation::Vertical, 8);
    hero.set_halign(gtk::Align::Center);
    hero.set_margin_top(16);
    hero.set_margin_bottom(8);
    for w in [
        status_icon.upcast_ref::<gtk::Widget>(),
        status_title.upcast_ref(),
        status_sub.upcast_ref(),
        main_btn.btn.upcast_ref(),
    ] {
        hero.append(w);
    }

    let region_row = adw::ActionRow::builder().title("区服").activatable(true).build();
    let region_val = gtk::Label::new(Some("未选择"));
    region_val.add_css_class("dim-label");
    region_row.add_suffix(&region_val);
    region_row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    let region_group = adw::PreferencesGroup::new();
    region_group.add(&region_row);

    let speed_history = Rc::new(RefCell::new(VecDeque::with_capacity(SPARK_POINTS)));
    let spark = sparkline(speed_history.clone());
    spark.set_visible(false);

    let (delay_row, delay_val) = value_row("节点延迟");
    let (loss_row, loss_val) = value_row("丢包率");
    let (speed_row, speed_val) = value_row("实时速率");
    speed_row.add_suffix(&spark);
    let (total_row, total_val) = value_row("累计流量");
    let (iface_row, iface_val) = value_row("隧道");
    let conn_group = adw::PreferencesGroup::builder().title("连接").build();
    for row in [&delay_row, &loss_row, &speed_row, &total_row, &iface_row] {
        conn_group.add(row);
    }

    let (acct_row, acct_btn) = spin_button_row("—", "正在读取账号…", "恢复时长");
    let acct_group = adw::PreferencesGroup::builder().title("雷神账号").build();
    acct_group.add(&acct_row);

    let column = gtk::Box::new(gtk::Orientation::Vertical, 24);
    column.set_margin_top(8);
    column.set_margin_start(24);
    column.set_margin_end(24);
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
    let (login_row, login_btn) = spin_button_row(
        "网页一键登录",
        "弹出官方小窗口登录，自动提取凭证并绑定本机",
        "打开登录窗",
    );
    login_btn.btn.add_css_class("suggested-action");

    let token_row = adw::PasswordEntryRow::builder()
        .title("手动填入 Token（备用）")
        .show_apply_button(true)
        .build();
    if let Some(delegate) = gtk::prelude::EditableExt::delegate(&token_row) {
        let row_for_focus = token_row.clone();
        delegate.connect_has_focus_notify(move |w| {
            if w.has_focus() && row_for_focus.text().contains('•') {
                let r = row_for_focus.clone();
                glib::idle_add_local_once(move || {
                    r.select_region(0, -1);
                });
            }
        });
    }

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
        spin_button_row("同步云端节点", "直接从雷神云端动态拉取全部区服最新节点（免手机）", "同步");
    let (restart_row, restart_btn) =
        spin_button_row("重启守护服务", "重启 leinux-core 加速核心服务", "重启");
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
    dialog.set_title("首选项");
    dialog.add(&page);

    PrefsWidgets {
        dialog,
        login_row,
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

fn build_region_picker() -> RegionPicker {
    let filter_group = adw::ToggleGroup::new();
    filter_group.add_css_class("round");
    filter_group.set_halign(gtk::Align::Center);
    filter_group.set_margin_top(6);
    filter_group.set_margin_bottom(10);
    filter_group.set_margin_start(18);
    filter_group.set_margin_end(18);

    let list_box = gtk::Box::new(gtk::Orientation::Vertical, 18);
    list_box.set_margin_top(6);
    list_box.set_margin_bottom(24);
    list_box.set_margin_start(18);
    list_box.set_margin_end(18);

    let clamp = adw::Clamp::builder().maximum_size(500).child(&list_box).build();
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&clamp)
        .build();
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
    vbox.append(&filter_group);
    vbox.append(&scroller);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&vbox));

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&view));

    let dialog = adw::Dialog::builder()
        .title("选择加速区服")
        .content_width(480)
        .content_height(560)
        .child(&toasts)
        .build();

    RegionPicker { dialog, filter_group, list_box, toasts }
}

impl MainWidgets {
    fn update_region_label(&self, st: &GuiState) {
        match st.selected_profile() {
            Some(p) => self.region_val.set_text(&format!("{} · {}", p.continent, p.name)),
            None => self.region_val.set_text("未选择"),
        }
    }

    fn refresh_hero(&self, st: &GuiState) {
        let can_operate = st.snap.service_active && !st.profiles.is_empty();
        match st.busy {
            Some((BusyOp::Starting, ref target_name)) => {
                self.status_icon.set_css_classes(&["accent"]);
                self.status_title.set_text("正在连接");
                self.status_sub.set_text(&format!("正在恢复时长并连接 {target_name}…"));
                self.main_btn.btn.set_css_classes(&["pill", "suggested-action"]);
                self.main_btn.set_state(true, "连接中…", false);
                return;
            }
            Some((BusyOp::Stopping, _)) => {
                self.status_title.set_text("正在断开");
                self.status_sub.set_text("正在断开隧道并暂停时长…");
                self.main_btn.btn.set_css_classes(&["pill", "destructive-action"]);
                self.main_btn.set_state(true, "断开中…", false);
                return;
            }
            _ => {}
        }

        if st.snap.is_accelerating {
            let name = st
                .profiles
                .iter()
                .find(|p| p.area == st.snap.active_area)
                .map_or(st.snap.active_area.as_str(), |p| p.name.as_str());
            let secs = st.started_at.map_or(0, |t| t.elapsed().as_secs());
            self.status_icon.set_css_classes(&["accent"]);
            self.status_title.set_text("已加速");
            self.status_sub.set_text(&format!("{name} · {}", format_hms(secs)));
            self.main_btn.btn.set_css_classes(&["pill", "destructive-action"]);
            self.main_btn.set_state(false, "停止加速", can_operate);
        } else {
            self.status_icon.set_css_classes(&["dim-label"]);
            self.status_title.set_text(if st.snap.service_active { "未加速" } else { "服务未运行" });
            self.status_sub.set_text("非官方的雷神加速器 Linux 客户端");
            self.main_btn.btn.set_css_classes(&["pill", "suggested-action"]);
            self.main_btn.set_state(false, "启动加速", can_operate);
        }
    }

    fn refresh_action_spinners(&self, pw: &PrefsWidgets, st: &GuiState) {
        let acct = &st.account;
        let login_busy = st.is_busy(BusyOp::LoginPopup);
        let acct_busy = st.is_busy(BusyOp::AccountToggle);

        if acct.configured && acct.valid {
            let lbl = if acct.paused { "恢复时长" } else { "暂停时长" };
            self.acct_btn.set_state(acct_busy, if acct_busy { "处理中…" } else { lbl }, true);
            pw.login_btn.set_state(login_busy, if login_busy { "等待登录…" } else { "重新登录" }, true);
        } else {
            let base_lbl = if acct.configured { "重新登录" } else { "登录账号" };
            self.acct_btn.set_state(login_busy, if login_busy { "等待登录…" } else { base_lbl }, true);
            let pw_lbl = if acct.configured { "重新登录" } else { "打开登录窗" };
            pw.login_btn.set_state(login_busy, if login_busy { "等待登录…" } else { pw_lbl }, true);
        }

        let sync_busy = st.is_busy(BusyOp::RefreshNodes);
        pw.refresh_nodes_btn.set_state(sync_busy, if sync_busy { "同步中…" } else { "同步" }, true);

        let restart_busy = st.is_busy(BusyOp::RestartService);
        pw.restart_btn.set_state(restart_busy, if restart_busy { "重启中…" } else { "重启" }, true);
    }

    fn update_traffic(&self, st: &mut GuiState, snap: &DaemonSnapshot) {
        if snap.is_accelerating {
            self.spark.set_visible(true);
            let now = Instant::now();
            if let Some((prx, ptx, pts)) = st.last_sample {
                let dt = now.duration_since(pts).as_secs_f64();
                if dt > 0.1 {
                    let rx = snap.rx_bytes.saturating_sub(prx) as f64 / dt;
                    let tx = snap.tx_bytes.saturating_sub(ptx) as f64 / dt;
                    self.speed_val.set_text(&format!("↓ {}/s  ↑ {}/s", format_bytes(rx, 0), format_bytes(tx, 0)));
                    let mut h = self.speed_history.borrow_mut();
                    if h.len() >= SPARK_POINTS {
                        h.pop_front();
                    }
                    h.push_back(rx + tx);
                    drop(h);
                    self.spark.queue_draw();
                }
            }
            st.last_sample = Some((snap.rx_bytes, snap.tx_bytes, now));
        } else {
            self.spark.set_visible(false);
            self.speed_history.borrow_mut().clear();
            st.last_sample = None;
            self.speed_val.set_text("—");
        }
        self.total_val.set_text(&format_bytes((snap.rx_bytes + snap.tx_bytes) as f64, 1));
        if snap.traffic_iface.is_empty() {
            self.iface_val.set_text(&snap.udp_6066);
        } else if snap.scoped_procs > 0 {
            self.iface_val.set_text(&format!("进程专享 · {} · {}", snap.traffic_iface, snap.udp_6066));
        } else {
            self.iface_val.set_text(&format!("{} · {}", snap.traffic_iface, snap.udp_6066));
        }
    }

    fn sync_token_entry(pw: &PrefsWidgets, hint: &str, expired: bool) {
        pw.token_row.set_title(if expired {
            "手动填入 Token（凭证已失效）"
        } else {
            "手动填入 Token（备用）"
        });
        if expired {
            pw.token_row.add_css_class("error");
        } else {
            pw.token_row.remove_css_class("error");
        }
        let focused = gtk::prelude::EditableExt::delegate(&pw.token_row)
            .is_some_and(|w| w.has_focus());
        if !focused && pw.token_row.text() != hint {
            pw.token_row.set_show_apply_button(false);
            pw.token_row.set_text(hint);
            pw.token_row.set_show_apply_button(true);
        }
    }

    fn update_account_section(&self, pw: &PrefsWidgets, st: &GuiState) {
        let acct = &st.account;
        if acct.configured && acct.valid {
            self.acct_row.set_title(&format!("{} 小时 {} 分", acct.hours, acct.mins));
            self.acct_row.set_subtitle(if acct.paused { "时长已暂停" } else { "时长计时中" });

            pw.login_row.set_subtitle(&format!("当前已绑定：{} · 可重新登录切换账号", acct.sub_text));
            pw.login_btn.btn.remove_css_class("suggested-action");
            Self::sync_token_entry(pw, &acct.token_hint, false);
        } else if acct.configured {
            self.acct_row.set_title("账号凭证已失效");
            self.acct_row.set_subtitle("点击右侧重新打开登录窗，自动刷新凭证与绑定");

            pw.login_row.set_subtitle("当前凭证已过期，请点击右侧重新登录");
            pw.login_btn.btn.add_css_class("suggested-action");
            Self::sync_token_entry(pw, &acct.token_hint, true);
        } else {
            self.acct_row.set_title("未登录雷神账号");
            self.acct_row.set_subtitle("点击右侧一键弹出登录小窗口，自动完成绑定与同步");

            pw.login_row.set_subtitle("弹出官方小窗口登录，自动提取凭证并绑定本机");
            pw.login_btn.btn.add_css_class("suggested-action");
            Self::sync_token_entry(pw, "", false);
        }
        self.refresh_action_spinners(pw, st);
    }

    fn apply_snapshot(&self, pw: &PrefsWidgets, st: &mut GuiState, mut snap: DaemonSnapshot) {
        if let Some(new_profiles) = snap.profiles.take() {
            st.profiles = new_profiles;
        }
        if let Some(new_acct) = snap.account.take() {
            st.account = new_acct;
        }

        self.window_title.set_subtitle(&if snap.service_active {
            String::new()
        } else {
            format!("服务异常（{}）", snap.service_state)
        });

        self.update_traffic(st, &snap);

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
            st.started_at.get_or_insert_with(Instant::now);
            self.delay_val.set_text(&if snap.net_delay > 0 { format!("{} ms", snap.net_delay) } else { "—".into() });
            self.loss_val.set_text(&format!("{} %", snap.loss_rate));
        } else {
            st.started_at = None;
            self.delay_val.set_text("—");
            self.loss_val.set_text("—");
        }

        st.snap = snap;
        self.update_account_section(pw, st);
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

        self.update_region_label(st);
        self.refresh_hero(st);
    }
}

impl UiCtx {
    fn send(&self, cmd: WorkerCmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    fn set_busy(&self, op: BusyOp, label: impl Into<String>) {
        let mut st = self.state.borrow_mut();
        st.busy = Some((op, label.into()));
        self.mw.refresh_hero(&st);
        self.mw.refresh_action_spinners(&self.pw, &st);
    }

    fn clear_busy(&self, op: BusyOp) {
        let mut st = self.state.borrow_mut();
        if st.is_busy(op) {
            st.busy = None;
            self.mw.refresh_hero(&st);
            self.mw.refresh_action_spinners(&self.pw, &st);
        }
    }

    fn dismiss_active_toast(&self) {
        if let Some(prev) = self.active_toast.borrow_mut().take() {
            prev.dismiss();
        }
    }

    fn toast(&self, msg: &str) {
        self.dismiss_active_toast();
        let t = adw::Toast::builder()
            .title(msg)
            .priority(adw::ToastPriority::High)
            .timeout(3)
            .build();
        *self.active_toast.borrow_mut() = Some(t.clone());

        if self.prefs_open.get() {
            self.pw.dialog.add_toast(t);
        } else if self.picker_open.get() {
            self.picker.toasts.add_toast(t);
        } else {
            self.mw.toasts.add_toast(t);
        }
    }

    fn present_prefs(&self) {
        self.dismiss_active_toast();
        self.prefs_open.set(true);
        self.pw.dialog.present(Some(&self.mw.win));
    }

    fn present_about(&self) {
        let ver = env!("CARGO_PKG_VERSION");
        let core_ver = self.state.borrow().snap.version.clone();
        let debug_info = if core_ver.is_empty() {
            format!("Leinux: v{ver}")
        } else {
            format!("Leinux: v{ver}\nLeigod Core: {core_ver}")
        };
        let about = adw::AboutDialog::builder()
            .application_name("Leinux")
            .application_icon(APP_ID)
            .developer_name("AsabaLazy")
            .version(ver)
            .debug_info(debug_info)
            .comments("雷神加速器 Linux 原生 GTK4 客户端")
            .website("https://github.com/AsabaLazy/Leinux")
            .issue_url("https://github.com/AsabaLazy/Leinux/issues")
            .license_type(gtk::License::MitX11)
            .build();
        about.present(Some(&self.mw.win));
    }

    fn rebuild_region_list(&self) {
        clear_box(&self.picker.list_box);
        let st = self.state.borrow();
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
                let ctx = self.clone();
                row.connect_activated(move |_| ctx.select_region(&prof.area, &prof.name));
                grp.add(&row);
            }
            self.picker.list_box.append(&grp);
        }
    }

    fn rebuild_region_picker(&self) {
        let active_filter = {
            let mut st = self.state.borrow_mut();
            if !st.continent_filter.is_empty()
                && !st.profiles.iter().any(|p| p.continent == st.continent_filter)
            {
                st.continent_filter.clear();
            }
            st.continent_filter.clone()
        };

        let mut tabs = vec![("全部", "all")];
        {
            let st = self.state.borrow();
            tabs.extend(
                CONTINENT_ORDER
                    .iter()
                    .copied()
                    .filter(|&cont| st.profiles.iter().any(|p| p.continent == cont))
                    .map(|cont| (cont, cont)),
            );
        }

        self.suppress.set(true);
        self.picker.filter_group.remove_all();
        for (label, name) in tabs {
            self.picker.filter_group.add(
                adw::Toggle::builder()
                    .name(name)
                    .label(label)
                    .build(),
            );
        }
        let active_name = if active_filter.is_empty() { "all" } else { active_filter.as_str() };
        self.picker.filter_group.set_active_name(Some(active_name));
        self.suppress.set(false);

        self.rebuild_region_list();
    }

    fn open_region_picker(&self) {
        self.dismiss_active_toast();
        self.rebuild_region_picker();
        self.picker_open.set(true);
        self.picker.dialog.present(Some(&self.mw.win));
    }

    fn select_region(&self, area: &str, name: &str) {
        let should_switch = {
            let mut st = self.state.borrow_mut();
            st.selected_area = area.to_string();
            self.mw.update_region_label(&st);
            self.mw.refresh_hero(&st);
            st.snap.is_accelerating && st.snap.active_area != area
        };
        self.picker_open.set(false);
        self.picker.dialog.close();
        self.send(WorkerCmd::SaveLastArea(area.to_string()));
        if should_switch {
            self.set_busy(BusyOp::Starting, name);
            self.send(WorkerCmd::StartArea(area.to_string()));
        }
    }

    fn on_main_clicked(&self) {
        let decision = {
            let st = self.state.borrow();
            if st.snap.is_accelerating {
                Some((BusyOp::Stopping, String::new(), WorkerCmd::StopAccelerating))
            } else {
                st.selected_profile().map(|p| {
                    (BusyOp::Starting, p.name.clone(), WorkerCmd::StartArea(p.area.clone()))
                })
            }
        };
        match decision {
            Some((op, label, cmd)) => {
                self.set_busy(op, label);
                self.send(cmd);
            }
            None => {
                self.open_region_picker();
                self.toast("请先选择加速区服");
            }
        }
    }

    fn start_popup_login(&self, open_prefs_on_missing_browser: bool) {
        if !has_login_browser() {
            if open_prefs_on_missing_browser {
                self.present_prefs();
            }
            self.toast("未检测到可用浏览器（Chromium / Firefox），请手动填入 Token");
            return;
        }
        let auto_pause = self.pw.auto_pause_row.is_active();
        let tx = self.cmd_tx.clone();
        let evt_tx = self.evt_tx.clone();
        self.set_busy(BusyOp::LoginPopup, "");
        thread::spawn(move || match run_browser_login_popup() {
            Ok(token) => {
                let _ = tx.send(WorkerCmd::SaveToken { token, auto_pause });
            }
            Err(msg) => {
                let _ = evt_tx.send(WorkerEvent::ClearBusy(BusyOp::LoginPopup));
                let _ = evt_tx.send(WorkerEvent::Toast(msg));
            }
        });
    }

    fn on_account_clicked(&self) {
        let (configured, valid, paused, is_acc) = {
            let st = self.state.borrow();
            (
                st.account.configured,
                st.account.valid,
                st.account.paused,
                st.snap.is_accelerating,
            )
        };
        if !configured || !valid {
            self.start_popup_login(true);
        } else if paused {
            self.set_busy(BusyOp::AccountToggle, "");
            self.send(WorkerCmd::RecoverAccount);
        } else if is_acc {
            self.set_busy(BusyOp::Stopping, "");
            self.send(WorkerCmd::StopAccelerating);
        } else {
            self.set_busy(BusyOp::AccountToggle, "");
            self.send(WorkerCmd::PauseAccountOnly);
        }
    }

    fn bind_signals(&self, app: &adw::Application) {
        let prefs_action = gio::SimpleAction::new("preferences", None);
        {
            let c = self.clone();
            prefs_action.connect_activate(move |_, _| c.present_prefs());
        }
        app.add_action(&prefs_action);

        let about_action = gio::SimpleAction::new("about", None);
        {
            let c = self.clone();
            about_action.connect_activate(move |_, _| c.present_about());
        }
        app.add_action(&about_action);

        let quit_action = gio::SimpleAction::new("quit", None);
        {
            let a = app.clone();
            quit_action.connect_activate(move |_, _| a.quit());
        }
        app.add_action(&quit_action);

        app.set_accels_for_action("app.preferences", &["<Primary>comma"]);
        app.set_accels_for_action("app.quit", &["<Primary>q"]);
        app.set_accels_for_action("window.close", &["<Primary>w"]);

        let c = self.clone();
        self.picker.filter_group.connect_active_name_notify(move |grp| {
            if c.suppress.get() {
                return;
            }
            let name = grp.active_name().map(|s| s.to_string()).unwrap_or_default();
            c.state.borrow_mut().continent_filter = if name == "all" { String::new() } else { name };
            c.rebuild_region_list();
        });

        let c = self.clone();
        self.pw.dialog.connect_closed(move |_| {
            c.dismiss_active_toast();
            c.prefs_open.set(false);
        });

        let c = self.clone();
        self.picker.dialog.connect_closed(move |_| {
            c.dismiss_active_toast();
            c.picker_open.set(false);
        });

        let c = self.clone();
        self.mw.region_row.connect_activated(move |_| c.open_region_picker());

        let c = self.clone();
        self.mw.main_btn.btn.connect_clicked(move |_| c.on_main_clicked());

        let c = self.clone();
        self.mw.acct_btn.btn.connect_clicked(move |_| c.on_account_clicked());

        let c = self.clone();
        self.pw.login_btn.btn.connect_clicked(move |_| c.start_popup_login(false));

        if let Some(delegate) = gtk::prelude::EditableExt::delegate(&self.pw.token_row) {
            let c = self.clone();
            delegate.connect_has_focus_notify(move |w| {
                if !w.has_focus() && c.pw.token_row.text().trim().is_empty() {
                    let st = c.state.borrow();
                    MainWidgets::sync_token_entry(
                        &c.pw,
                        &st.account.token_hint,
                        st.account.configured && !st.account.valid,
                    );
                }
            });
        }

        let c = self.clone();
        self.pw.token_row.connect_apply(move |row| {
            let token = row.text().trim().to_string();
            if token.is_empty() || token.contains('•') {
                c.toast("请粘贴完整的新 Token");
                let st = c.state.borrow();
                MainWidgets::sync_token_entry(
                    &c.pw,
                    &st.account.token_hint,
                    st.account.configured && !st.account.valid,
                );
                return;
            }
            row.set_text("");
            c.set_busy(BusyOp::LoginPopup, "");
            c.send(WorkerCmd::SaveToken { token, auto_pause: c.pw.auto_pause_row.is_active() });
        });

        let c = self.clone();
        self.pw.auto_pause_row.connect_active_notify(move |row| {
            if !c.suppress.get() {
                c.send(WorkerCmd::SaveAutoPause(row.is_active()));
            }
        });

        let c = self.clone();
        self.pw.dns_entry.connect_apply(move |row| {
            c.send(WorkerCmd::SaveCustomDns(row.text().trim().to_string()));
        });

        let c = self.clone();
        self.pw.refresh_nodes_btn.btn.connect_clicked(move |_| {
            c.set_busy(BusyOp::RefreshNodes, "");
            c.send(WorkerCmd::RefreshNodes);
        });

        let c = self.clone();
        self.pw.restart_btn.btn.connect_clicked(move |_| {
            c.set_busy(BusyOp::RestartService, "");
            c.send(WorkerCmd::RestartService);
        });

        let c = self.clone();
        self.pw.repair_btn.connect_clicked(move |_| c.send(WorkerCmd::RepairBound));

        let c = self.clone();
        self.pw.cleanup_btn.connect_clicked(move |_| c.send(WorkerCmd::CleanupNet));
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

    let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCmd>();
    let (evt_tx, evt_rx) = mpsc::channel::<WorkerEvent>();
    spawn_worker(cmd_rx, evt_tx.clone());

    let ctx = UiCtx {
        state: Rc::new(RefCell::new(GuiState::default())),
        suppress: Rc::new(Cell::new(false)),
        prefs_open: Rc::new(Cell::new(false)),
        picker_open: Rc::new(Cell::new(false)),
        active_toast: Rc::new(RefCell::new(None)),
        mw: build_main_window(app),
        pw: build_prefs_dialog(),
        picker: build_region_picker(),
        cmd_tx,
        evt_tx,
    };
    ctx.bind_signals(app);

    let loop_ctx = ctx.clone();
    glib::timeout_add_local(Duration::from_millis(250), move || {
        if loop_ctx.state.borrow().started_at.is_some() {
            loop_ctx.mw.refresh_hero(&loop_ctx.state.borrow());
        }
        while let Ok(evt) = evt_rx.try_recv() {
            match evt {
                WorkerEvent::Toast(msg) => loop_ctx.toast(&msg),
                WorkerEvent::ClearBusy(op) => loop_ctx.clear_busy(op),
                WorkerEvent::Snapshot(snap) => {
                    let profiles_updated = snap.profiles.is_some();
                    loop_ctx.suppress.set(true);
                    loop_ctx.mw.apply_snapshot(&loop_ctx.pw, &mut loop_ctx.state.borrow_mut(), *snap);
                    loop_ctx.suppress.set(false);
                    if profiles_updated && loop_ctx.picker_open.get() {
                        loop_ctx.rebuild_region_picker();
                    }
                }
            }
        }
        glib::ControlFlow::Continue
    });

    ctx.mw.win.present();
}
