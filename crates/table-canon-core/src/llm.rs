use crate::normalize::sha1_hex;
use crate::split::{parse_entries_json, EntrySplitter};
use crate::types::ExtractedEntry;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout_secs: u64,
    pub concurrency: u32,
    pub max_tokens: u32,
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
        })
    }
}

pub struct LlmSplitter {
    cfg: LlmConfig,
    progress: Option<Arc<Mutex<String>>>,
    started: Instant,
    done: std::sync::atomic::AtomicUsize,
    total: std::sync::atomic::AtomicUsize,
}

impl LlmSplitter {
    pub fn new(cfg: LlmConfig) -> Self {
        Self {
            cfg,
            progress: None,
            started: Instant::now(),
            done: std::sync::atomic::AtomicUsize::new(0),
            total: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub fn with_progress(cfg: LlmConfig, progress: Arc<Mutex<String>>) -> Self {
        Self {
            cfg,
            progress: Some(progress),
            started: Instant::now(),
            done: std::sync::atomic::AtomicUsize::new(0),
            total: std::sync::atomic::AtomicUsize::new(0),
        }
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

    fn chat(&self, user: &str) -> Result<String> {
        let url = format!("{}/chat/completions", self.cfg.base_url);
        let body = json!({
            "model": self.cfg.model,
            "temperature": 0.1,
            "max_tokens": self.cfg.max_tokens.max(256),
            "messages": [
                {"role": "system", "content": SYSTEM},
                {"role": "user", "content": user}
            ]
        });
        let resp: ChatResponse = ureq::post(&url)
            .set("Authorization", &format!("Bearer {}", self.cfg.api_key))
            .set("Content-Type", "application/json")
            .timeout(std::time::Duration::from_secs(self.cfg.timeout_secs.max(20)))
            .send_json(body)
            .with_context(|| format!("调用 LLM 失败: {url}"))?
            .into_json()
            .context("LLM 返回不是 JSON")?;
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

    fn cached_split(&self, title: &str, piece: &str, idx: usize) -> Result<Vec<ExtractedEntry>> {
        let user = format!("章节标题：{title}\n分段序号：{}\n正文：\n{piece}", idx + 1);
        let key = sha1_hex(&format!("v2|{}|{title}|{idx}|{piece}", self.cfg.model));
        let path = cache_dir().join(format!("{key}.json"));
        if let Ok(raw) = fs::read_to_string(&path) {
            if let Ok(entries) = parse_entries_json(&raw) {
                return Ok(entries);
            }
        }
        let raw = self.chat(&user)?;
        let entries = parse_entries_json(&raw)?;
        let _ = fs::create_dir_all(cache_dir());
        let _ = fs::write(path, raw);
        Ok(entries)
    }
}

impl EntrySplitter for LlmSplitter {
    fn split_chapter(&self, chapter_title: &str, body: &str) -> Result<Vec<ExtractedEntry>> {
        let pieces = split_for_context(body, 3200);
        self.total
            .store(pieces.len(), std::sync::atomic::Ordering::Relaxed);
        self.split_pieces(chapter_title, &pieces)
    }

    fn split_many(&self, chapters: &[(String, String)]) -> Vec<Result<Vec<ExtractedEntry>>> {
        let mut jobs: Vec<(usize, usize, String, String)> = Vec::new();
        for (ci, (title, body)) in chapters.iter().enumerate() {
            for (pi, piece) in split_for_context(body, 3200).into_iter().enumerate() {
                jobs.push((ci, pi, title.clone(), piece));
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
        let piece_results = par_map(jobs, workers, |(ci, pi, title, piece)| {
            let r = self.cached_split(&title, &piece, pi);
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
    fn split_pieces(&self, chapter_title: &str, pieces: &[String]) -> Result<Vec<ExtractedEntry>> {
        let jobs: Vec<(usize, String)> = pieces
            .iter()
            .cloned()
            .enumerate()
            .collect();
        let workers = (self.cfg.concurrency.max(1) as usize).min(jobs.len().max(1));
        let results = par_map(jobs, workers, |(i, piece)| {
            let r = self.cached_split(chapter_title, &piece, i);
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

fn split_for_context(body: &str, max_chars: usize) -> Vec<String> {
    if body.chars().count() <= max_chars {
        return vec![body.to_string()];
    }
    let mut parts = Vec::new();
    let mut acc = String::new();
    for para in body.split('\n') {
        if acc.chars().count() + para.chars().count() > max_chars && !acc.is_empty() {
            parts.push(acc.trim().to_string());
            acc.clear();
        }
        acc.push_str(para);
        acc.push('\n');
    }
    if !acc.trim().is_empty() {
        parts.push(acc.trim().to_string());
    }
    parts
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

只提出切点，禁止输出正文，禁止扩写、改写、总结。

按原文出现顺序输出 JSON 数组：
[{"title":"短专名","aliases":["口头叫法"],"entity_type":"npc|location|item|faction|rule|plot","anchor":"原文里连续出现的 8 到 24 个字，必须原样复制"}]

硬性：
1. 不要输出 body。框架会按 anchor/title 从原文剪切。
2. title 2–16 字，用专名；不要用「第N章」。
3. entity_type 只能是 npc location item faction rule plot，不确定就省略。
4. 宁可少切，不要把两个专名糊成一条。
5. 只输出 JSON 数组。
"#;

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
