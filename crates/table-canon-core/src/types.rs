use serde::{Deserialize, Serialize};

pub const VIS_PUBLIC: i64 = 1;
pub const VIS_DM_ONLY: i64 = 2;
pub const VIS_SECRET: i64 = 4;
pub const VIS_HIDDEN: i64 = 8;

pub const APP_ID: i64 = 0x5443_5331; // TCS1
pub const SCHEMA_VERSION: i32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreInfo {
    pub campaign_id: i64,
    pub campaign_name: String,
    pub chunk_count: i64,
    pub semantic_ready: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub id: i64,
    pub source_document_id: i64,
    pub stable_key: String,
    pub ordinal: i64,
    pub entity_type: String,
    pub title: String,
    pub body: String,
    pub parent_path: String,
    pub visibility: i64,
    pub aliases: Vec<String>,
    pub source_rank: i64,
    pub file_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkDetail {
    pub chunk: Chunk,
    pub prev_id: Option<i64>,
    pub next_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hit {
    pub chunk: Chunk,
    pub score: f64,
    pub via: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub hits: Vec<Hit>,
    pub latency_ms: u128,
    pub used_semantic: bool,
    pub truncated: bool,
    pub terms: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportReport {
    pub files_ok: usize,
    pub files_skip: usize,
    pub files_fail: usize,
    pub chunks: usize,
    pub unmatched_corrections: usize,
    pub errors: Vec<String>,
    /// 非致命警告（如「某章拆条失败已回退规则切块」），导入报告里透出。
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct MetaPatch {
    pub entity_type: Option<String>,
    pub visibility: Option<i64>,
    pub aliases: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct DraftChunk {
    pub stable_key: String,
    pub alt_anchor: String,
    pub ordinal: i64,
    pub entity_type: String,
    pub title: String,
    pub body: String,
    pub parent_path: String,
    pub visibility: i64,
    pub aliases: Vec<String>,
    pub content_hash: String,
}

fn neg_one() -> i64 {
    -1
}

/// 模型给出的段号可能是数字、字符串（"7"、"[7]"、"段7"）甚至 null。
/// 解析不了就返回 -1（视为未提供），宁可退回 anchor 匹配也不让整个数组解析失败。
fn deserialize_flex_i64<'de, D>(d: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    Ok(match v {
        serde_json::Value::Null => -1,
        serde_json::Value::Number(n) => n.as_i64().unwrap_or(-1),
        serde_json::Value::String(s) => {
            let digits: String = s
                .chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(|c| c.is_ascii_digit())
                .collect();
            digits.parse().unwrap_or(-1)
        }
        _ => -1,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExtractedEntry {
    pub title: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub entity_type: String,
    #[serde(default)]
    pub body: String,
    /// 旧契约字段：原文中连续出现的短句。段号制下降级为可选校验/兜底匹配手段；可空。
    #[serde(default)]
    pub anchor: String,
    /// 条目起始全局段号（含），-1 表示未提供。段号由框架编号，定位与模型复写能力解耦。
    #[serde(default = "neg_one", deserialize_with = "deserialize_flex_i64")]
    pub start: i64,
    /// 条目结束全局段号（含），-1 表示未提供。
    #[serde(default = "neg_one", deserialize_with = "deserialize_flex_i64")]
    pub end: i64,
    #[serde(default)]
    pub heading_level: u8,
}

#[derive(Debug, Clone)]
pub struct Block {
    pub heading_level: u8,
    pub title: String,
    pub text: String,
}
