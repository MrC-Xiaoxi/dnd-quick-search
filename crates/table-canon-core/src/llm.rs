use crate::normalize::sha1_hex;
use crate::split::{pack_pieces, parse_entries_json, segment_text, EntrySplitter, Piece};
use crate::types::ExtractedEntry;
use anyhow::{bail, Result};
use serde::Deserialize;
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// 相邻分片共享的段数：跨片边界的条目在两侧都看得完整。
const PIECE_OVERLAP_SEGS: usize = 1;

#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout_secs: u64,
    pub concurrency: u32,
    pub max_tokens: u32,
    pub temperature: f32,
    /// 解析/调用失败后的追加重试次数（带错误回喂），0 表示一次定生死。
    pub retries: u32,
    /// 请求 response_format=json_object；若服务端返回 4xx 会自动去掉重发。
    pub json_mode: bool,
    /// 单个分片的字符上限（按段打包）。
    pub piece_chars: usize,
}

impl LlmConfig {
    pub fn from_parts(base_url: &str, api_key: &str, model: &str) -> Option<Self> {
        let base_url = base_url.trim().trim_end_matches('/').to_string();
        let api_key = api_key.trim().to_string();
        let model = model.trim().to_string();
        if base_url.is_empty() || api_key.is_empty() || model.is_empty() {
            return None;
        }
        Some(Self {
            base_url,
            api_key,
            model,
            timeout_secs: 60,
            concurrency: 3,
            max_tokens: 2048,
            temperature: 0.1,
            retries: 1,
            json_mode: false,
            piece_chars: 3200,
        })
    }
}

pub struct LlmSplitter {
    cfg: LlmConfig,
    progress: Option<Arc<Mutex<String>>>,
    started: Instant,
    done: std::sync::atomic::AtomicUsize,
    total: std::sync::atomic::AtomicUsize,
    /// 累计带错误回喂的重试次数，用于观察模型输出质量。
    retries_used: std::sync::atomic::AtomicUsize,
}

impl LlmSplitter {
    pub fn new(cfg: LlmConfig) -> Self {
        Self {
            cfg,
            progress: None,
            started: Instant::now(),
            done: std::sync::atomic::AtomicUsize::new(0),
            total: std::sync::atomic::AtomicUsize::new(0),
            retries_used: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub fn with_progress(cfg: LlmConfig, progress: Arc<Mutex<String>>) -> Self {
        Self {
            cfg,
            progress: Some(progress),
            started: Instant::now(),
            done: std::sync::atomic::AtomicUsize::new(0),
            total: std::sync::atomic::AtomicUsize::new(0),
            retries_used: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// 本次导入累计的重试次数。
    pub fn retries_used(&self) -> usize {
        self.retries_used
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn note(&self, msg: impl Into<String>) {
        if let Some(p) = &self.progress {
            if let Ok(mut g) = p.lock() {
                *g = msg.into();
            }
        }
    }

    fn tick(&self, title: &str) {
        use std::sync::atomic::Ordering;
        let done = self.done.fetch_add(1, Ordering::Relaxed) + 1;
        let total = self.total.load(Ordering::Relaxed).max(1);
        let sec = self.started.elapsed().as_secs();
        self.note(format!(
            "LLM 拆条 {done}/{total} · 「{}」· 已用 {} 分 {:02} 秒",
            truncate(title, 18),
            sec / 60,
            sec % 60
        ));
    }

    /// 单次对话。`allow_json_mode=false` 用于重试轮次：第一轮已确认代理/模型
    /// 不认 response_format 时，后续轮次不再浪费一次 4xx。
    fn chat(&self, user: &str, allow_json_mode: bool) -> Result<String> {
        let url = format!("{}/chat/completions", self.cfg.base_url);
        let build_body = |json_mode: bool| {
            let mut body = json!({
                "model": self.cfg.model,
                "temperature": self.cfg.temperature,
                "max_tokens": self.cfg.max_tokens.max(256),
                "messages": [
                    {"role": "system", "content": SYSTEM},
                    {"role": "user", "content": user}
                ]
            });
            if json_mode {
                body["response_format"] = json!({"type": "json_object"});
            }
            body
        };
        let want_json = self.cfg.json_mode && allow_json_mode;
        type ChatErr = (Option<u16>, String);
        let send = |body: &serde_json::Value| -> std::result::Result<ChatResponse, ChatErr> {
            let resp = ureq::post(&url)
                .set("Authorization", &format!("Bearer {}", self.cfg.api_key))
                .set("Content-Type", "application/json")
                .timeout(std::time::Duration::from_secs(self.cfg.timeout_secs.max(20)))
                .send_json(body.clone());
            match resp {
                Ok(r) => r
                    .into_json()
                    .map_err(|e| (None, format!("LLM 返回不是 JSON: {e}"))),
                Err(ureq::Error::Status(code, r)) => {
                    let detail = r.into_string().unwrap_or_default();
                    Err((
                        Some(code),
                        format!("LLM HTTP {code}: {}", truncate(&detail, 200)),
                    ))
                }
                Err(e) => Err((None, format!("调用 LLM 失败: {url}: {e}"))),
            }
        };
        let mut res = send(&build_body(want_json));
        if want_json {
            if let Err((Some(code), _)) = &res {
                if (400..500).contains(code) {
                    res = send(&build_body(false));
                }
            }
        }
        let resp = res.map_err(|(_, e)| anyhow::anyhow!("{e}"))?;
        let text = resp
            .choices
            .first()
            .and_then(|c| c.message.content.clone())
            .unwrap_or_default();
        if text.trim().is_empty() {
            bail!("LLM 返回空内容");
        }
        Ok(text)
    }

    /// 调用 + 解析，失败时带错误信息回喂重试（Instructor 模式）。
    /// 成功才返回原文用于写缓存，失败结果绝不落缓存。
    fn split_piece_retry(&self, title: &str, piece: &str) -> Result<(String, Vec<ExtractedEntry>)> {
        let base = format!("章节标题：{title}\n正文（每行开头的 [nnn] 是框架标注的段号）：\n{piece}");
        let attempts = self.cfg.retries + 1;
        let mut last_err: Option<String> = None;
        for attempt in 0..attempts {
            let mut user = base.clone();
            if let Some(err) = &last_err {
                self.retries_used
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                user.push_str(&format!(
                    "\n\n你上一次的输出无法使用（{err}）。\n请严格按系统约定重新输出：只要一个 JSON 对象，禁止解释、禁止输出正文。"
                ));
            }
            match self.chat(&user, attempt == 0) {
                Ok(raw) => match parse_entries_json(&raw) {
                    Ok(entries) => return Ok((raw, entries)),
                    Err(e) => last_err = Some(format!("第{}次输出解析失败：{e}", attempt + 1)),
                },
                Err(e) => last_err = Some(format!("第{}次调用失败：{e}", attempt + 1)),
            }
        }
        Err(anyhow::anyhow!(
            "{}",
            last_err.unwrap_or_else(|| "LLM 调用失败".into())
        ))
    }

    fn cached_split(&self, title: &str, piece: &str) -> Result<Vec<ExtractedEntry>> {
        // prompt 版本参与哈希：改 prompt 自动失效旧缓存，不再手工 bump v2/v3
        let key = sha1_hex(&format!(
            "v3|{}|{}|{title}|{piece}",
            sha1_hex(SYSTEM),
            self.cfg.model
        ));
        let path = cache_dir().join(format!("{key}.json"));
        if let Ok(raw) = fs::read_to_string(&path) {
            if let Ok(entries) = parse_entries_json(&raw) {
                return Ok(entries);
            }
        }
        let (raw, entries) = self.split_piece_retry(title, piece)?;
        let _ = fs::create_dir_all(cache_dir());
        let _ = fs::write(path, raw);
        Ok(entries)
    }
}

impl EntrySplitter for LlmSplitter {
    fn split_chapter(&self, chapter_title: &str, body: &str) -> Result<Vec<ExtractedEntry>> {
        let segs = segment_text(body);
        let pieces = pack_pieces(body, &segs, self.cfg.piece_chars, PIECE_OVERLAP_SEGS);
        self.total
            .store(pieces.len(), std::sync::atomic::Ordering::Relaxed);
        self.split_pieces(chapter_title, &pieces)
    }

    fn split_many(&self, chapters: &[(String, String)]) -> Vec<Result<Vec<ExtractedEntry>>> {
        let mut jobs: Vec<(usize, String, String)> = Vec::new();
        for (ci, (title, body)) in chapters.iter().enumerate() {
            let segs = segment_text(body);
            for piece in pack_pieces(body, &segs, self.cfg.piece_chars, PIECE_OVERLAP_SEGS) {
                jobs.push((ci, title.clone(), piece.text));
            }
        }
        self.total
            .store(jobs.len(), std::sync::atomic::Ordering::Relaxed);
        self.done
            .store(0, std::sync::atomic::Ordering::Relaxed);
        if jobs.is_empty() {
            return chapters.iter().map(|_| Ok(Vec::new())).collect();
        }
        let workers = (self.cfg.concurrency.max(1) as usize).min(jobs.len());
        let piece_results = par_map(jobs, workers, |(ci, title, piece)| {
            let r = self.cached_split(&title, &piece);
            self.tick(&title);
            (ci, r)
        });
        let mut buckets: Vec<Vec<ExtractedEntry>> = vec![Vec::new(); chapters.len()];
        let mut first_err: Vec<Option<String>> = vec![None; chapters.len()];
        for (ci, r) in piece_results {
            match r {
                Ok(mut entries) => buckets[ci].append(&mut entries),
                Err(e) => {
                    if first_err[ci].is_none() {
                        first_err[ci] = Some(e.to_string());
                    }
                }
            }
        }
        buckets
            .into_iter()
            .zip(first_err)
            .map(|(v, err)| {
                if v.is_empty() {
                    Err(anyhow::anyhow!(
                        "{}",
                        err.unwrap_or_else(|| "LLM 未拆出任何条目".into())
                    ))
                } else {
                    Ok(v)
                }
            })
            .collect()
    }
}

impl LlmSplitter {
    fn split_pieces(&self, chapter_title: &str, pieces: &[Piece]) -> Result<Vec<ExtractedEntry>> {
        let jobs: Vec<(usize, String)> = pieces
            .iter()
            .map(|p| p.text.clone())
            .enumerate()
            .collect();
        let workers = (self.cfg.concurrency.max(1) as usize).min(jobs.len().max(1));
        let results = par_map(jobs, workers, |(_, piece)| {
            let r = self.cached_split(chapter_title, &piece);
            self.tick(chapter_title);
            r
        });
        let mut out = Vec::new();
        let mut last_err = None;
        for r in results {
            match r {
                Ok(mut v) => out.append(&mut v),
                Err(e) => last_err = Some(e),
            }
        }
        if out.is_empty() {
            return Err(last_err.unwrap_or_else(|| anyhow::anyhow!("LLM 未拆出任何条目")));
        }
        Ok(out)
    }
}

fn par_map<T, R>(items: Vec<T>, workers: usize, f: impl Fn(T) -> R + Sync) -> Vec<R>
where
    T: Send,
    R: Send,
{
    let n = items.len();
    if n == 0 {
        return Vec::new();
    }
    let workers = workers.max(1).min(n);
    if workers == 1 {
        return items.into_iter().map(f).collect();
    }
    let queue = Mutex::new(items.into_iter().enumerate());
    let out = Mutex::new(Vec::<(usize, R)>::new());
    std::thread::scope(|s| {
        let f = &f;
        for _ in 0..workers {
            s.spawn(|| loop {
                let next = queue.lock().ok().and_then(|mut q| q.next());
                let Some((i, item)) = next else { break };
                let r = f(item);
                if let Ok(mut g) = out.lock() {
                    g.push((i, r));
                }
            });
        }
    });
    let mut got = out.into_inner().unwrap_or_default();
    got.sort_by_key(|(i, _)| *i);
    got.into_iter().map(|(_, r)| r).collect()
}

fn cache_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("table-canon")
        .join("split-cache")
}

fn truncate(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        format!("{t}…")
    } else {
        t
    }
}

const SYSTEM: &str = r#"你是跑团资料库的切点标注员，不是作者。

正文每行开头的 [nnn] 是框架标注的段号。你只负责指出每个设定条目覆盖的段号范围，禁止输出正文，禁止扩写、改写、总结。

按条目在正文中的顺序输出一个 JSON 对象：
{"entries":[{"title":"短专名","aliases":["口头叫法"],"entity_type":"npc|location|item|faction|rule|plot","start":起始段号,"end":结束段号}]}

硬性：
1. start/end 用正文里 [nnn] 的编号（整数），end≥start。条目按顺序衔接，除前言外覆盖全部正文；不要跳段，不要重叠。
2. 禁止输出 body，框架会按段号从原文剪切。
3. title 2–16 字，用专名；不要用「第N章」。
4. entity_type 只能是 npc location item faction rule plot，不确定就省略。
5. 宁可少切，不要把两个专名糊成一条。
6. 只输出一个 JSON 对象，不要解释。

示例：
正文：
[000] 铁砧堡扼守山口，是防御核心。
[001] 城主荀岚兼管税收。
[002] 商队过山口必须缴费。
输出：
{"entries":[{"title":"铁砧堡","aliases":["山口要塞"],"entity_type":"location","start":0,"end":0},{"title":"荀岚","aliases":["城主"],"entity_type":"npc","start":1,"end":2}]}"#;

#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    message: Msg,
}

#[derive(Deserialize, Default)]
struct Msg {
    #[serde(default)]
    content: Option<String>,
}
