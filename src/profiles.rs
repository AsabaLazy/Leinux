use crate::net::{find_plugin_pid, read_ini_values, resolve_local_dns_list, state_dir};
use crate::rpc::{curl_json, is_valid_ptoken, load_account_config};
use crate::types::{
    canonical_geo_keys, format_region_continent, format_region_title, is_internal_geo_key,
    profile_sort_key, ProfileSummary, StoredProfile, APP_ID, BUSI_TYPE, FALLBACK_SIGN, LEIGOD_DIR,
};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

struct ProfileCache {
    last_sig: Vec<(u64, u64)>,
    profiles: Vec<StoredProfile>,
    rev: u64,
    p_token: String,
}

fn store() -> &'static Mutex<ProfileCache> {
    static STORE: OnceLock<Mutex<ProfileCache>> = OnceLock::new();
    STORE.get_or_init(|| {
        Mutex::new(ProfileCache {
            last_sig: Vec::new(),
            profiles: Vec::new(),
            rev: 1,
            p_token: String::new(),
        })
    })
}

fn profiles_file() -> PathBuf {
    state_dir().join("saved_profiles.json")
}

fn cloud_geo_file() -> PathBuf {
    state_dir().join("cloud_geo.json")
}

fn file_sig(p: &Path) -> (u64, u64) {
    fs::metadata(p).map(|m| (m.mtime_nsec() as u64, m.len())).unwrap_or((0, 0))
}

pub fn invalidate_profile_cache() {
    if let Ok(mut g) = store().lock() {
        g.last_sig.clear();
    }
}

pub fn cached_profile_p_token() -> String {
    if let Ok(g) = store().lock() {
        if !g.p_token.is_empty() {
            return g.p_token.clone();
        }
    }
    for p in load_saved_profiles() {
        let tok = p.conf["user"]["token"].as_str().unwrap_or("").trim();
        if is_valid_ptoken(tok) {
            if let Ok(mut g) = store().lock() {
                g.p_token = tok.to_string();
            }
            return tok.to_string();
        }
    }
    String::new()
}

pub fn load_saved_profiles() -> Vec<StoredProfile> {
    let Ok(text) = fs::read_to_string(profiles_file()) else {
        return Vec::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn load_cloud_geo() -> BTreeMap<String, BTreeSet<String>> {
    let Ok(text) = fs::read_to_string(cloud_geo_file()) else {
        return BTreeMap::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn save_cloud_geo(geo: &BTreeMap<String, BTreeSet<String>>) {
    if geo.is_empty() {
        return;
    }
    let dir = state_dir();
    let _ = fs::create_dir_all(&dir);
    let p = cloud_geo_file();
    let tmp = p.with_extension("tmp");
    if let Ok(s) = serde_json::to_string(geo) {
        if fs::write(&tmp, s).is_ok() {
            let _ = fs::rename(&tmp, &p);
        }
    }
}

pub fn save_profiles(profiles: &[StoredProfile]) {
    let dir = state_dir();
    let _ = fs::create_dir_all(&dir);
    let p = profiles_file();
    let tmp = p.with_extension("tmp");
    if let Ok(s) = serde_json::to_string_pretty(profiles) {
        if fs::write(&tmp, s).is_ok() {
            let _ = fs::rename(&tmp, &p);
        }
    }
    let found_ptok = profiles
        .iter()
        .filter_map(|p| p.conf["user"]["token"].as_str())
        .map(str::trim)
        .find(|t| is_valid_ptoken(t))
        .unwrap_or("")
        .to_string();

    if let Ok(mut g) = store().lock() {
        g.profiles = profiles.to_vec();
        g.rev = g.rev.wrapping_add(1).max(1);
        g.last_sig.clear();
        if !found_ptok.is_empty() {
            g.p_token = found_ptok;
        }
    }
}

fn make_stored_profile(
    area: &str,
    conf: Value,
    node_count: usize,
    catalog: i64,
    sub_catalog: i64,
) -> StoredProfile {
    let low = area.trim().to_lowercase();
    StoredProfile {
        summary: ProfileSummary {
            id: format!("region-{low}"),
            name: format_region_title(&low),
            continent: format_region_continent(&low).to_string(),
            catalog,
            sub_catalog,
            area: low,
            node_count,
        },
        conf,
    }
}

fn disable_telemetry_reports(parsed: &mut Value) -> bool {
    let Some(report) = parsed.get_mut("report").and_then(Value::as_object_mut) else {
        return false;
    };
    let mut modified = false;
    for (k, v) in report.iter_mut() {
        if k == "heart" {
            continue;
        }
        let Some(obj) = v.as_object_mut() else { continue };
        if obj.get("enable") != Some(&Value::Bool(false))
            || !obj.get("url").and_then(Value::as_str).unwrap_or("").is_empty()
        {
            obj.insert("enable".to_string(), Value::Bool(false));
            obj.insert("interval".to_string(), json!(0));
            obj.insert("url".to_string(), json!(""));
            modified = true;
        }
    }
    modified
}

pub fn sanitize_acc_conf(conf_input: &Value, dns_list: &[String]) -> (Value, bool) {
    let mut parsed: Value = match conf_input {
        Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| json!({})),
        other => other.clone(),
    };
    let mut modified = disable_telemetry_reports(&mut parsed);

    if let Some(log_cfg) = parsed.get_mut("log").and_then(Value::as_object_mut) {
        if log_cfg.get("level").and_then(Value::as_str) != Some("none")
            || log_cfg.get("outFile") != Some(&Value::Bool(false))
        {
            log_cfg.insert("level".to_string(), json!("none"));
            log_cfg.insert("outFile".to_string(), Value::Bool(false));
            modified = true;
        }
    }
    if let Some(limit_cfg) = parsed.get_mut("limit").and_then(Value::as_object_mut) {
        for k in ["play_kb", "down_kb", "up_kb"] {
            if limit_cfg.get(k).and_then(Value::as_i64).unwrap_or(0) < 8192 {
                limit_cfg.insert(k.to_string(), json!(8192));
                modified = true;
            }
        }
        if limit_cfg.get("data_kb").and_then(Value::as_i64).unwrap_or(0) < 16384 {
            limit_cfg.insert("data_kb".to_string(), json!(16384));
            modified = true;
        }
    }
    if let Some(ipdata_cfg) = parsed.get_mut("ipdata").and_then(Value::as_object_mut) {
        let xdb_path = "/opt/leigod/config/ipdatacloud_country.xdb";
        if ipdata_cfg.get("path").and_then(Value::as_str) != Some(xdb_path) {
            ipdata_cfg.insert("path".to_string(), json!(xdb_path));
            modified = true;
        }
    }
    if let Some(srv_cfg) = parsed.get_mut("server").and_then(Value::as_object_mut) {
        let want_dns = json!(dns_list);
        if srv_cfg.get("dns") != Some(&want_dns) {
            srv_cfg.insert("dns".to_string(), want_dns);
            modified = true;
        }
    }
    if let Some(game_cfg) = parsed.get_mut("game").and_then(Value::as_object_mut) {
        if game_cfg.get("dyn_line") != Some(&Value::Bool(false)) {
            game_cfg.insert("dyn_line".to_string(), Value::Bool(false));
            modified = true;
        }
    }
    let target_rules = json!([{"host": ["geo:cn"], "serv": ["direct"]}]);
    if parsed.get("rules") != Some(&target_rules) {
        if let Some(obj) = parsed.as_object_mut() {
            obj.insert("rules".to_string(), target_rules);
            modified = true;
        }
    }
    (parsed, modified)
}

fn minimal_region_geo(area: &str, uris: &[String]) -> Value {
    let mut m = Map::new();
    m.insert("cn".to_string(), json!(["direct"]));
    let arr: Vec<Value> = uris.iter().map(|u| json!(u)).collect();
    for key in canonical_geo_keys(area) {
        m.insert(key, Value::Array(arr.clone()));
    }
    Value::Object(m)
}

fn harvest_geo_from_conf(conf: &Value, pool: &mut BTreeMap<String, BTreeSet<String>>) -> bool {
    let Some(srv_geo) = conf["server"]["geo"].as_object() else {
        return false;
    };
    let mut added = false;
    for (gk, gv) in srv_geo {
        let low = gk.trim().to_lowercase();
        if is_internal_geo_key(&low) {
            continue;
        }
        let Some(arr) = gv.as_array() else { continue };
        let set = pool.entry(low).or_default();
        for u in arr.iter().filter_map(Value::as_str) {
            if !u.is_empty() && u != "direct" && set.insert(u.to_string()) {
                added = true;
            }
        }
    }
    added
}

pub fn consolidate_regional_profiles(
    profiles: Vec<StoredProfile>,
    extra_geo: Option<BTreeMap<String, BTreeSet<String>>>,
) -> (Vec<StoredProfile>, bool) {
    let acct_cfg = load_account_config();
    let dns_list = resolve_local_dns_list(&acct_cfg.custom_dns);

    let mut geo_pool = load_cloud_geo();
    let mut geo_dirty = false;
    if let Some(extra) = extra_geo {
        for (k, uris) in extra {
            let low = k.trim().to_lowercase();
            if is_internal_geo_key(&low) {
                continue;
            }
            let entry = geo_pool.entry(low).or_default();
            for u in uris {
                if entry.insert(u) {
                    geo_dirty = true;
                }
            }
        }
    }

    let prev_order: Vec<String> = profiles.iter().map(|p| p.summary.area.clone()).collect();
    let mut by_area: BTreeMap<String, StoredProfile> = BTreeMap::new();
    let mut nodes_by_area: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
    let mut changed = geo_dirty;

    for p in profiles {
        let area = p.summary.area.trim().to_lowercase();
        if is_internal_geo_key(&area) {
            changed = true;
            continue;
        }
        let (conf_obj, was_mod) = sanitize_acc_conf(&p.conf, &dns_list);
        if harvest_geo_from_conf(&conf_obj, &mut geo_pool) {
            geo_dirty = true;
        }

        let area_nodes = nodes_by_area.entry(area.clone()).or_default();
        if let Some(all_nodes) = conf_obj["server"]["allNode"].as_array() {
            for n in all_nodes {
                let Some(uri) = n["node"].as_str() else { continue };
                let sign = n["speed_sign"].as_str().unwrap_or(FALLBACK_SIGN);
                let replace = area_nodes
                    .get(uri)
                    .map(|old| old["speed_sign"].as_str() == Some(FALLBACK_SIGN) && sign != FALLBACK_SIGN)
                    .unwrap_or(true);
                if replace {
                    area_nodes.insert(
                        uri.to_string(),
                        json!({"node": uri, "ping": n["ping"].as_i64().unwrap_or(20), "speed_sign": sign}),
                    );
                }
            }
        }

        let nc = conf_obj["server"]["allNode"].as_array().map(Vec::len).unwrap_or(0);
        let keep = by_area.get(&area).map(|ex| nc >= ex.summary.node_count).unwrap_or(true);
        if !keep {
            changed = true;
            continue;
        }
        let next = make_stored_profile(
            &area,
            conf_obj,
            nc,
            p.summary.catalog,
            p.summary.sub_catalog,
        );
        if next.summary != p.summary || was_mod || by_area.contains_key(&area) {
            changed = true;
        }
        by_area.insert(area, next);
    }

    if geo_dirty {
        save_cloud_geo(&geo_pool);
    }

    let base_profile = by_area
        .get("hk")
        .or_else(|| by_area.get("jp"))
        .or_else(|| by_area.values().next())
        .cloned();

    if let Some(base) = base_profile {
        let default_sign = base.conf["user"]["speed_sign"]
            .as_str()
            .unwrap_or(FALLBACK_SIGN)
            .to_string();
        let all_areas: BTreeSet<String> = by_area
            .keys()
            .chain(geo_pool.keys())
            .filter(|k| !is_internal_geo_key(k))
            .cloned()
            .collect();

        for area in all_areas {
            let valid_uris = geo_pool.get(&area);
            let area_nodes = nodes_by_area.entry(area.clone()).or_default();
            if let Some(uris) = valid_uris {
                if !uris.is_empty() {
                    area_nodes.retain(|u, _| uris.contains(u));
                    for u in uris {
                        area_nodes.entry(u.clone()).or_insert_with(|| {
                            json!({"node": u, "ping": 25, "speed_sign": default_sign})
                        });
                    }
                }
            }
            if area_nodes.is_empty() {
                continue;
            }
            let node_list: Vec<Value> = area_nodes.values().cloned().collect();
            let uri_strings: Vec<String> = area_nodes.keys().cloned().collect();
            let first_uri = uri_strings[0].clone();
            let want_geo = minimal_region_geo(&area, &uri_strings);
            let want_nodes = Value::Array(node_list.clone());

            if let Some(existing) = by_area.get_mut(&area) {
                if existing.conf["server"]["allNode"] != want_nodes || existing.conf["server"]["geo"] != want_geo {
                    existing.conf["server"]["allNode"] = want_nodes;
                    existing.conf["server"]["tcp"] = json!([first_uri]);
                    existing.conf["server"]["geo"] = want_geo;
                    existing.summary.node_count = node_list.len();
                    changed = true;
                }
            } else {
                let mut syn_conf = base.conf.clone();
                syn_conf["game"]["area"] = json!(area);
                syn_conf["game"]["dyn_line"] = Value::Bool(false);
                syn_conf["server"]["tcp"] = json!([first_uri]);
                syn_conf["server"]["allNode"] = want_nodes;
                syn_conf["server"]["geo"] = want_geo;
                let (clean_conf, _) = sanitize_acc_conf(&syn_conf, &dns_list);
                by_area.insert(
                    area.clone(),
                    make_stored_profile(
                        &area,
                        clean_conf,
                        node_list.len(),
                        base.summary.catalog,
                        base.summary.sub_catalog,
                    ),
                );
                changed = true;
            }
        }
    }

    let mut ordered: Vec<StoredProfile> = by_area.into_values().collect();
    ordered.sort_by_key(|p| profile_sort_key(&p.summary));
    if !changed && ordered.iter().map(|p| p.summary.area.as_str()).ne(prev_order.iter().map(String::as_str)) {
        changed = true;
    }
    (ordered, changed)
}

pub fn fetch_cloud_seed(rounds: usize) -> Option<(StoredProfile, BTreeMap<String, BTreeSet<String>>)> {
    let mut merged_geo: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut s5_auth = "USER831173:PASS331250181986".to_string();
    let mut ss_auth = "aes-256-cfb:PASS331250181986".to_string();
    let mut proxy_url = "https://dfs01.nn.com/v2/default/1794272400/3393/pcipwhite.txt".to_string();

    let common_headers = [
        ("Content-Type", "application/json"),
        ("appId", APP_ID),
        ("version", "600"),
        ("reqChannel", "2"),
        ("platform", "2"),
        ("busiType", BUSI_TYPE),
    ];

    thread::scope(|s| {
        let white_h = s.spawn(|| {
            curl_json("GET", "https://opapi.xxghh.biz/speed/gamemaster/white/pc", &common_headers[1..], None, 4).ok()
        });
        let area_hs: Vec<_> = (0..rounds)
            .map(|_| {
                s.spawn(|| {
                    curl_json(
                        "POST",
                        "https://opapi.xxghh.biz/speed/gamemaster/nodeInfo/areaRule?handleMobileRule=1",
                        &common_headers,
                        Some("{}"),
                        4,
                    )
                    .ok()
                })
            })
            .collect();

        if let Ok(Some(wv)) = white_h.join() {
            if let Some(u) = wv["retData"]["fileUrl"].as_str().filter(|u| !u.is_empty()) {
                proxy_url = u.to_string();
            }
        }
        for h in area_hs {
            let Ok(Some(resp)) = h.join() else { continue };
            let Some(ret_data) = resp["retData"].as_object() else { continue };
            for (gk, gv) in ret_data {
                let low = gk.trim().to_lowercase();
                if is_internal_geo_key(&low) {
                    continue;
                }
                let Some(nodes) = gv["nodes"].as_array() else { continue };
                for n in nodes {
                    let ip = n["proxyIp"].as_str().unwrap_or("").trim();
                    if ip.is_empty() {
                        continue;
                    }
                    let port = n["proxyPort"].as_i64().unwrap_or(5438);
                    let user = n["proxyUser"].as_str().unwrap_or("USER831173").trim();
                    let pass = n["proxyPass"].as_str().unwrap_or("PASS331250181986").trim();
                    let enc = n["encryptedMethod"].as_str().unwrap_or("aes-256-cfb").trim();
                    s5_auth = format!("{user}:{pass}");
                    ss_auth = format!("{enc}:{pass}");
                    merged_geo.entry(low.clone()).or_default().insert(format!("s5://{user}:{pass}@{ip}:{port}"));
                }
            }
        }
    });

    if merged_geo.is_empty() {
        return None;
    }

    let acct_cfg = load_account_config();
    let dns_list = resolve_local_dns_list(&acct_cfg.custom_dns);
    let (_, rtoken, ini_uid) = read_ini_values();
    let uid = if !acct_cfg.user_id.trim().is_empty() {
        acct_cfg.user_id.trim().to_string()
    } else {
        ini_uid
    };
    let utok = if is_valid_ptoken(&acct_cfg.p_token) { acct_cfg.p_token.trim().to_string() } else { rtoken };

    let hk_uris: Vec<String> = merged_geo.get("hk").map(|s| s.iter().cloned().collect()).unwrap_or_default();
    let hk_nodes: Vec<Value> = hk_uris
        .iter()
        .map(|u| json!({"node": u, "ping": 25, "speed_sign": FALLBACK_SIGN}))
        .collect();
    let first_tcp = hk_uris.first().cloned().unwrap_or_default();

    let seed_conf = json!({
        "game": {
            "area": "hk",
            "dyn_line": false,
            "dynamic_geo_threshold": "1,50",
            "dynamic_geo_udp": false,
            "id": "2c92a50b86b5ad730186b5b58f980003",
            "proxy": proxy_url,
        },
        "ipdata": {"path": "/opt/leigod/config/ipdatacloud_country.xdb"},
        "limit": {"cdn_kb": 0, "data_kb": 16384, "down_kb": 8192, "play_kb": 8192, "up_kb": 8192},
        "log": {"level": "none", "outFile": false},
        "report": {
            "domain": {"enable": false, "interval": 0, "url": ""},
            "engineEventsReport": {"enable": false, "interval": 0, "url": ""},
            "flowReport": {"enable": false, "interval": 0, "url": ""},
            "heart": {"enable": true, "interval": 180, "url": "https://opapi.xxghh.biz/speed/router/heartbeat"},
            "ipPortProtocolReport": {"enable": false, "interval": 0, "url": ""},
            "statistics": {"enable": false, "interval": 0, "url": ""},
        },
        "rules": [{"host": ["geo:cn"], "serv": ["direct"]}],
        "server": {
            "allNode": hk_nodes,
            "auth": {"s5": s5_auth, "ss": ss_auth},
            "dns": dns_list,
            "dynNode": [],
            "geo": minimal_region_geo("hk", &hk_uris),
            "tagNodeList": [],
            "tcp": [first_tcp],
        },
        "user": {
            "app_id": APP_ID,
            "id": uid,
            "reqChannel": "2",
            "speed_sign": FALLBACK_SIGN,
            "token": utok,
            "version": "600",
        }
    });

    let seed = make_stored_profile("hk", seed_conf, hk_uris.len(), 2, 0);
    Some((seed, merged_geo))
}

fn capture_from_core_conf(
    core_file: &Path,
    dns_list: &[String],
    profiles: &mut Vec<StoredProfile>,
) -> bool {
    let Ok(text) = fs::read_to_string(core_file) else { return false };
    let Ok(mut core_meta) = serde_json::from_str::<Value>(&text) else { return false };
    let Some(acc_list) = core_meta.get_mut("acceleration").and_then(Value::as_array_mut) else {
        return false;
    };

    let mut core_mod = false;
    let mut changed = false;

    for item in acc_list {
        let Some(obj) = item.as_object_mut() else { continue };
        for (_dev_key, dev_val) in obj.iter_mut() {
            if dev_val.get("acc_conf").is_none() {
                continue;
            }
            let (parsed, was_mod) = sanitize_acc_conf(&dev_val["acc_conf"], dns_list);
            if was_mod {
                if let Ok(raw_text) = serde_json::to_string(&parsed) {
                    dev_val["acc_conf"] = json!(raw_text);
                    core_mod = true;
                }
            }
            let area = parsed["game"]["area"]
                .as_str()
                .or_else(|| dev_val["game_area"].as_str())
                .unwrap_or("")
                .trim()
                .to_lowercase();
            if is_internal_geo_key(&area) {
                continue;
            }
            let nc = parsed["server"]["allNode"].as_array().map(Vec::len).unwrap_or(0);
            let catalog = dev_val["device_type"]
                .as_i64()
                .or_else(|| dev_val["catalog_type"].as_i64())
                .unwrap_or(2);
            let sub_cat = dev_val["sub_type"].as_i64().unwrap_or(0);
            let entry = make_stored_profile(&area, parsed, nc, catalog, sub_cat);

            if let Some(old) = profiles.iter_mut().find(|p| p.summary.id == entry.summary.id) {
                if old.conf != entry.conf {
                    *old = entry;
                    changed = true;
                }
            } else {
                profiles.insert(0, entry);
                changed = true;
            }
        }
    }

    if core_mod {
        if let Ok(s) = serde_json::to_string(&core_meta) {
            let _ = fs::write(core_file, s);
        }
    }
    changed
}

pub fn scan_profiles() -> (Vec<StoredProfile>, u64) {
    let mut candidate_dirs = vec![PathBuf::from(LEIGOD_DIR).join("config"), PathBuf::from("/tmp/acc")];
    if let Some(pid) = find_plugin_pid() {
        candidate_dirs.push(PathBuf::from(format!("/proc/{pid}/root/tmp/acc")));
        candidate_dirs.push(PathBuf::from(format!("/proc/{pid}/root/opt/leigod/config")));
    }

    let mut sig = vec![
        file_sig(&profiles_file()),
        file_sig(&cloud_geo_file()),
        file_sig(&state_dir().join("account.json")),
    ];
    for d in &candidate_dirs {
        sig.push(file_sig(&d.join("acc_core_conf.json")));
    }

    if let Ok(guard) = store().lock() {
        if guard.last_sig == sig && !guard.profiles.is_empty() {
            return (guard.profiles.clone(), guard.rev);
        }
    }

    let acct_cfg = load_account_config();
    let dns_list = resolve_local_dns_list(&acct_cfg.custom_dns);
    let (mut profiles, mut changed) = consolidate_regional_profiles(load_saved_profiles(), None);

    for d in &candidate_dirs {
        let core_file = d.join("acc_core_conf.json");
        if core_file.is_file() {
            if capture_from_core_conf(&core_file, &dns_list, &mut profiles) {
                changed = true;
            }
            break;
        }
    }

    if profiles.is_empty() {
        if let Some((seed, geo)) = fetch_cloud_seed(6) {
            profiles.push(seed);
            let (cons, _) = consolidate_regional_profiles(profiles, Some(geo));
            save_profiles(&cons);
            let rev = store().lock().map(|g| g.rev).unwrap_or(1);
            return (cons, rev);
        }
    }

    if changed {
        let (cons, _) = consolidate_regional_profiles(profiles, None);
        profiles = cons;
        save_profiles(&profiles);
    }

    let mut rev = 1;
    if let Ok(mut guard) = store().lock() {
        guard.profiles = profiles.clone();
        guard.last_sig = sig;
        rev = guard.rev;
    }
    (profiles, rev)
}

pub fn refresh_cloud_profiles(rounds: usize) -> Result<usize, String> {
    let Some((seed, geo)) = fetch_cloud_seed(rounds) else {
        return Err("从雷神云端拉取节点失败，请检查网络连接".to_string());
    };
    let mut profiles = load_saved_profiles();
    profiles.push(seed);
    let (cons, _) = consolidate_regional_profiles(profiles, Some(geo));
    let count = cons.len();
    save_profiles(&cons);
    Ok(count)
}

fn probe_node_uri(uri: &str, timeout: Duration) -> Option<f64> {
    let hp = uri.rsplit('@').next()?.trim();
    let addr = hp.to_socket_addrs().ok()?.next()?;
    let t0 = Instant::now();
    TcpStream::connect_timeout(&addr, timeout).ok()?;
    Some(t0.elapsed().as_secs_f64() * 1000.0)
}

pub fn prepare_live_conf(mut conf_obj: Value, p_token: &str, user_id: &str) -> Value {
    let area = conf_obj["game"]["area"].as_str().unwrap_or("").trim().to_lowercase();
    if let Some(user_obj) = conf_obj.get_mut("user").and_then(Value::as_object_mut) {
        if !p_token.trim().is_empty() {
            user_obj.insert("token".to_string(), json!(p_token.trim()));
        }
        if !user_id.trim().is_empty() {
            user_obj.insert("id".to_string(), json!(user_id.trim()));
        }
    }

    let Some(srv) = conf_obj.get_mut("server").and_then(Value::as_object_mut) else {
        return conf_obj;
    };

    let all_nodes = srv.get("allNode").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut candidate_uris = BTreeSet::new();
    for u in all_nodes.iter().filter_map(|n| n["node"].as_str()) {
        candidate_uris.insert(u.to_string());
    }
    if let Some(geo_obj) = srv.get("geo").and_then(Value::as_object) {
        for (gk, gv) in geo_obj {
            if !gk.eq_ignore_ascii_case(&area) {
                continue;
            }
            if let Some(arr) = gv.as_array() {
                for u in arr.iter().filter_map(Value::as_str).filter(|u| !u.is_empty() && *u != "direct") {
                    candidate_uris.insert(u.to_string());
                }
            }
        }
    }
    if candidate_uris.is_empty() {
        return conf_obj;
    }

    let timeout = Duration::from_millis(250);
    let uris_vec: Vec<String> = candidate_uris.iter().cloned().collect();
    let mut rtt_map = HashMap::new();
    thread::scope(|s| {
        let handles: Vec<_> = uris_vec
            .iter()
            .map(|u| s.spawn(move || (u.as_str(), probe_node_uri(u, timeout))))
            .collect();
        for h in handles {
            if let Ok((u, Some(rtt))) = h.join() {
                rtt_map.insert(u.to_string(), rtt);
            }
        }
    });

    let live_nodes: Vec<Value> = all_nodes
        .into_iter()
        .filter(|n| n["node"].as_str().map(|u| rtt_map.contains_key(u)).unwrap_or(false))
        .collect();
    if !live_nodes.is_empty() {
        let non_tproxy: Vec<Value> = live_nodes
            .iter()
            .filter(|n| n["node"].as_str().and_then(|u| rtt_map.get(u)).copied().unwrap_or(0.0) >= 1.5)
            .cloned()
            .collect();
        let mut ranked = if non_tproxy.is_empty() { live_nodes } else { non_tproxy };
        ranked.sort_by(|a, b| {
            let ra = a["node"].as_str().and_then(|u| rtt_map.get(u)).copied().unwrap_or(999.0);
            let rb = b["node"].as_str().and_then(|u| rtt_map.get(u)).copied().unwrap_or(999.0);
            ra.partial_cmp(&rb).unwrap_or(std::cmp::Ordering::Equal)
        });
        for n in &mut ranked {
            if let Some(&rtt) = n["node"].as_str().and_then(|u| rtt_map.get(u)) {
                n["ping"] = json!((rtt.round() as i64).max(1));
            }
        }
        if let Some(first_uri) = ranked.first().and_then(|n| n["node"].as_str()) {
            srv.insert("tcp".to_string(), json!([first_uri]));
        }
        srv.insert("allNode".to_string(), Value::Array(ranked));
    }

    if let Some(geo_obj) = srv.get_mut("geo").and_then(Value::as_object_mut) {
        for gv in geo_obj.values_mut() {
            let Some(arr) = gv.as_array() else { continue };
            let mut live_gv: Vec<Value> = arr
                .iter()
                .filter(|u| {
                    let s = u.as_str().unwrap_or("");
                    s == "direct" || rtt_map.contains_key(s)
                })
                .cloned()
                .collect();
            if !live_gv.is_empty() {
                live_gv.sort_by(|a, b| {
                    let ra = a.as_str().and_then(|s| rtt_map.get(s)).copied().unwrap_or(0.0);
                    let rb = b.as_str().and_then(|s| rtt_map.get(s)).copied().unwrap_or(0.0);
                    ra.partial_cmp(&rb).unwrap_or(std::cmp::Ordering::Equal)
                });
                *gv = Value::Array(live_gv);
            }
        }
    }

    conf_obj
}
