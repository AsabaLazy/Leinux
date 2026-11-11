use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};

pub const LEIGOD_DIR: &str = "/opt/leigod";
pub const WEBAPI_BASE: &str = "https://webapi.leigod.com";
pub const FALLBACK_SIGN: &str = "6aedef9ce4b01aa456e120f2";
pub const APP_ID: &str = "nnMobile_d0k3duup";
pub const BUSI_TYPE: &str = "nn_aksjfdasoifnkls";

pub const CONTINENT_ORDER: [&str; 8] = [
    "亚洲",
    "欧洲",
    "北美洲",
    "南美洲",
    "大洋洲",
    "非洲",
    "南极洲",
    "其他地区",
];

pub trait ValueExt {
    fn as_i64_lossy(&self) -> Option<i64>;
    fn as_str_lossy(&self) -> Option<String>;
}

impl ValueExt for Value {
    fn as_i64_lossy(&self) -> Option<i64> {
        self.as_i64().or_else(|| self.as_str()?.trim().parse().ok())
    }

    fn as_str_lossy(&self) -> Option<String> {
        self.as_str()
            .map(str::to_string)
            .or_else(|| self.as_i64().map(|n| n.to_string()))
    }
}

#[derive(Clone, Debug, Default)]
pub struct IniValues {
    pub version: String,
    pub rtoken: String,
    pub user_id: String,
}

#[derive(Clone, Debug)]
pub struct ActiveAccState {
    pub is_accelerating: bool,
    pub active_area: String,
    pub net_delay: i64,
    pub loss_rate: String,
}

impl Default for ActiveAccState {
    fn default() -> Self {
        Self {
            is_accelerating: false,
            active_area: String::new(),
            net_delay: 0,
            loss_rate: "0".to_string(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_catalog() -> i64 {
    2
}

pub fn deserialize_conf_value<'de, D>(deserializer: D) -> Result<Value, D::Error>
where
    D: Deserializer<'de>,
{
    let v = Value::deserialize(deserializer)?;
    match v {
        Value::String(s) => Ok(serde_json::from_str(&s).unwrap_or_else(|_| json!({}))),
        other => Ok(other),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccountConfig {
    #[serde(default)]
    pub account_token: String,
    #[serde(default)]
    pub p_token: String,
    #[serde(default)]
    pub user_id: String,
    #[serde(default = "default_true")]
    pub auto_pause_on_stop: bool,
    #[serde(default)]
    pub custom_dns: String,
    #[serde(default)]
    pub last_area: String,
}

impl Default for AccountConfig {
    fn default() -> Self {
        Self {
            account_token: String::new(),
            p_token: String::new(),
            user_id: String::new(),
            auto_pause_on_stop: true,
            custom_dns: String::new(),
            last_area: String::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileSummary {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub continent: String,
    #[serde(default = "default_catalog")]
    pub catalog: i64,
    #[serde(rename = "subCatalog", default)]
    pub sub_catalog: i64,
    pub area: String,
    #[serde(rename = "nodeCount", default)]
    pub node_count: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredProfile {
    #[serde(flatten)]
    pub summary: ProfileSummary,
    #[serde(deserialize_with = "deserialize_conf_value")]
    pub conf: Value,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AccountSnapshot {
    pub configured: bool,
    #[serde(default)]
    pub valid: bool,
    #[serde(default)]
    pub token_hint: String,
    pub paused: bool,
    pub hours: i64,
    pub mins: i64,
    pub sub_text: String,
    pub auto_pause_on_stop: bool,
    pub custom_dns: String,
    pub detected_gateway_dns: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DaemonSnapshot {
    pub service_active: bool,
    pub service_state: String,
    pub tun_interface: String,
    pub traffic_iface: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub udp_6066: String,
    pub version: String,
    pub is_accelerating: bool,
    #[serde(default)]
    pub scoped_procs: usize,
    pub active_area: String,
    #[serde(default)]
    pub last_area: String,
    pub net_delay: i64,
    pub loss_rate: String,
    pub profiles_rev: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profiles: Option<Vec<ProfileSummary>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<AccountSnapshot>,
}

pub fn is_internal_geo_key(area: &str) -> bool {
    matches!(
        area.trim().to_lowercase().as_str(),
        "" | "-" | "auto" | "global" | "cn" | "download" | "test" | "sjjc" | "mc"
    )
}

pub fn format_region_title(area: &str) -> String {
    let title = match area.trim().to_lowercase().as_str() {
        "hk" => "中国香港",
        "tw" => "中国台湾",
        "jp" => "日本",
        "kr" => "韩国",
        "asia" => "亚洲通用",
        "sea" => "新加坡",
        "my" => "马来西亚",
        "vn" => "越南",
        "ph" => "菲律宾",
        "idn" => "印度尼西亚",
        "ind" => "印度",
        "pk" => "巴基斯坦",
        "de" => "德国",
        "eu" => "欧洲通用",
        "ru" => "俄罗斯",
        "tr" => "土耳其",
        "ua" => "乌克兰",
        "kaz" => "哈萨克斯坦",
        "us" => "美国",
        "au" => "澳大利亚",
        "br" => "巴西",
        "arg" => "阿根廷",
        "mx" => "墨西哥",
        other => return other.to_uppercase(),
    };
    title.to_string()
}

pub fn format_region_continent(area: &str) -> &'static str {
    match area.trim().to_lowercase().as_str() {
        "hk" | "tw" | "jp" | "kr" | "asia" | "sea" | "sg" | "my" | "vn" | "ph" | "idn"
        | "ind" | "pk" | "kaz" | "tr" | "th" | "ae" | "sa" | "il" => "亚洲",
        "de" | "eu" | "ru" | "ua" | "uk" | "gb" | "fr" | "nl" | "it" | "es" | "pl" | "se"
        | "ch" => "欧洲",
        "us" | "mx" | "ca" => "北美洲",
        "br" | "arg" | "cl" | "co" | "pe" => "南美洲",
        "au" | "nz" => "大洋洲",
        "za" | "eg" | "ng" | "ke" | "ma" => "非洲",
        "aq" => "南极洲",
        _ => "其他地区",
    }
}

fn is_continent_wide_area(area: &str) -> bool {
    matches!(
        area.trim().to_lowercase().as_str(),
        "asia" | "eu" | "na" | "sa" | "oc" | "af" | "an"
    )
}

fn continent_sort_rank(continent: &str) -> usize {
    CONTINENT_ORDER.iter().position(|&c| c == continent).unwrap_or(99)
}

pub fn profile_sort_key(p: &ProfileSummary) -> (usize, bool, std::cmp::Reverse<usize>, String) {
    (
        continent_sort_rank(&p.continent),
        !is_continent_wide_area(&p.area),
        std::cmp::Reverse(p.node_count),
        p.area.clone(),
    )
}

pub fn canonical_geo_keys(area: &str) -> Vec<String> {
    let low = area.trim().to_lowercase();
    let mut keys = vec![low.clone()];
    if matches!(low.as_str(), "my" | "vn" | "arg") {
        keys.push(low.to_uppercase());
    }
    keys
}
