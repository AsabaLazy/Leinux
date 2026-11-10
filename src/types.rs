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
    #[serde(rename = "gameId", default)]
    pub game_id: String,
    pub area: String,
    #[serde(rename = "nodeCount", default)]
    pub node_count: usize,
    #[serde(rename = "updatedAt", default)]
    pub updated_at: String,
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
    pub mac: String,
    pub tun_interface: String,
    pub traffic_iface: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub has_gameacc_chain: bool,
    pub udp_6066: String,
    pub version: String,
    pub token: String,
    pub user_id: String,
    pub is_accelerating: bool,
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
    match area.trim().to_lowercase().as_str() {
        "hk" => "中国香港".to_string(),
        "tw" => "中国台湾".to_string(),
        "jp" => "日本".to_string(),
        "kr" => "韩国".to_string(),
        "asia" => "亚洲通用".to_string(),
        "sea" => "新加坡".to_string(),
        "my" => "马来西亚".to_string(),
        "vn" => "越南".to_string(),
        "ph" => "菲律宾".to_string(),
        "idn" => "印度尼西亚".to_string(),
        "ind" => "印度".to_string(),
        "pk" => "巴基斯坦".to_string(),
        "de" => "德国".to_string(),
        "eu" => "欧洲通用".to_string(),
        "ru" => "俄罗斯".to_string(),
        "tr" => "土耳其".to_string(),
        "ua" => "乌克兰".to_string(),
        "kaz" => "哈萨克斯坦".to_string(),
        "us" => "美国".to_string(),
        "au" => "澳大利亚".to_string(),
        "br" => "巴西".to_string(),
        "arg" => "阿根廷".to_string(),
        "mx" => "墨西哥".to_string(),
        other => other.to_uppercase(),
    }
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

pub fn is_continent_wide_area(area: &str) -> bool {
    matches!(
        area.trim().to_lowercase().as_str(),
        "asia" | "eu" | "na" | "sa" | "oc" | "af" | "an"
    )
}

pub fn continent_sort_rank(continent: &str) -> usize {
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
