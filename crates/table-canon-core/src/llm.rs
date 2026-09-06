use crate::split::{parse_entries_json, EntrySplitter};
use crate::types::ExtractedEntry;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout_secs: u64,
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
            timeout_secs: 180,
        })
    }
}

pub struct LlmSplitter {
    cfg: LlmConfig,
    progress: Option<Arc<Mutex<String>>>,
}

impl LlmSplitter {
    pub fn new(cfg: LlmConfig) -> Self {
        Self {
            cfg,
            progress: None,
        }
    }

    pub fn with_progress(cfg: LlmConfig, progress: Arc<Mutex<String>>) -> Self {
        Self {
            cfg,
            progress: Some(progress),
        }
    }

    fn note(&self, msg: impl Into<String>) {
        if let Some(p) = &self.progress {
            if let Ok(mut g) = p.lock() {
                *g = msg.into();
            }
        }
    }

    fn chat(&self, user: &str) -> Result<String> {
        let url = format!("{}/chat/completions", self.cfg.base_url);
        let body = json!({
            "model": self.cfg.model,
            "temperature": 0.2,
            "messages": [
                {"role": "system", "content": SYSTEM},
                {"role": "user", "content": user}
            ]
        });
        let resp: ChatResponse = ureq::post(&url)
            .set("Authorization", &format!("Bearer {}", self.cfg.api_key))
            .set("Content-Type", "application/json")
            .timeout(std::time::Duration::from_secs(self.cfg.timeout_secs.max(30)))
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
}

impl EntrySplitter for LlmSplitter {
    fn split_chapter(&self, chapter_title: &str, body: &str) -> Result<Vec<ExtractedEntry>> {
        let mut out = Vec::new();
        let pieces = split_for_context(body, 4500);
        let n = pieces.len();
        for (i, piece) in pieces.into_iter().enumerate() {
            self.note(format!(
                "LLM 拆条「{}」({}/{})，窗口可继续点，请等本段返回…",
                if chapter_title.is_empty() {
                    "未命名"
                } else {
                    chapter_title
                },
                i + 1,
                n
            ));
            let user = format!(
                "章节标题：{chapter_title}\n分段序号：{}\n正文：\n{piece}",
                i + 1
            );
            let raw = self.chat(&user)?;
            out.extend(parse_entries_json(&raw)?);
        }
        if out.is_empty() {
            bail!("LLM 未拆出任何条目");
        }
        Ok(out)
    }
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

const SYSTEM: &str = r#"你是跑团资料库的归档员。把用户给出的一章模组/私设正文，拆成「一条设定一张卡片」。

规则：
1. 每张卡片对应一个可检索条目：NPC、地点、势力、物品、规则、剧情线索等。
2. 卡片正文必须完整、可直接复制给 DM 使用，不要只写摘要。秘密信息单独起行，以【秘密】开头。
3. 标题用短专名（如「猎人工会」「格里姆」），不要用「第0章 故事和背景介绍」这种章名当唯一条目，除非整章确实只有一个主题。
4. 别名包括简称、英文名、玩家可能的叫法。
5. entity_type 只能是 npc、location、item、faction、rule、plot 之一；不确定就省略。
6. 只输出 JSON 数组，不要 markdown，不要解释。格式：
[{"title":"...","aliases":["..."],"entity_type":"faction","body":"..."}]
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
