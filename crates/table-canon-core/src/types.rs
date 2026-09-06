use serde::{Deserialize, Serialize};

pub const VIS_PUBLIC: i64 = 1;
pub const VIS_DM_ONLY: i64 = 2;
pub const VIS_SECRET: i64 = 4;
pub const VIS_HIDDEN: i64 = 8;

pub const APP_ID: i64 = 0x5443_5331; // TCS1
pub const SCHEMA_VERSION: i32 = 2;

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

#[derive(Debug, Clone)]
pub struct Block {
    pub heading_level: u8,
    pub title: String,
    pub text: String,
}
