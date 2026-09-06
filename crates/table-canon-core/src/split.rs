use crate::ingest::blocks_to_drafts;
use crate::normalize::{alt_anchor, sha1_body_hex, stable_key, stable_key_with_ordinal};
use crate::types::{Block, DraftChunk, ExtractedEntry};
use anyhow::{bail, Context, Result};
use std::collections::HashSet;

pub trait EntrySplitter: Send + Sync {
    /// 把一章正文拆成若干设定条目。失败则调用方保留启发式切块。
    fn split_chapter(&self, chapter_title: &str, body: &str) -> Result<Vec<ExtractedEntry>>;

    /// 多章并行拆条。默认逐章调用 `split_chapter`。
    fn split_many(&self, chapters: &[(String, String)]) -> Vec<Result<Vec<ExtractedEntry>>> {
        chapters
            .iter()
            .map(|(t, b)| self.split_chapter(t, b))
            .collect()
    }
}

pub fn parse_entries_json(raw: &str) -> Result<Vec<ExtractedEntry>> {
    let trimmed = strip_code_fence(raw);
    let start = trimmed
        .find('[')
        .with_context(|| "LLM 输出里没有 JSON 数组")?;
    let end = trimmed
        .rfind(']')
        .with_context(|| "LLM 输出里 JSON 数组未闭合")?;
    let slice = &trimmed[start..=end];
    let entries: Vec<ExtractedEntry> =
        serde_json::from_str(slice).with_context(|| "无法解析拆条 JSON")?;
    let cleaned: Vec<ExtractedEntry> = entries
        .into_iter()
        .map(normalize_entry)
        .filter(|e| !e.title.trim().is_empty())
        .collect();
    if cleaned.is_empty() {
        bail!("拆条结果为空");
    }
    Ok(cleaned)
}

fn strip_code_fence(raw: &str) -> String {
    let t = raw.trim();
    if let Some(rest) = t.strip_prefix("```json") {
        return rest.trim().trim_end_matches("```").trim().to_string();
    }
    if let Some(rest) = t.strip_prefix("```") {
        return rest.trim().trim_end_matches("```").trim().to_string();
    }
    t.to_string()
}

fn normalize_entry(mut e: ExtractedEntry) -> ExtractedEntry {
    e.title = e.title.trim().to_string();
    e.body = e.body.trim().to_string();
    e.anchor = e.anchor.trim().to_string();
    e.entity_type = match e.entity_type.trim() {
        "npc" | "location" | "item" | "faction" | "rule" | "plot" => e.entity_type.trim().into(),
        _ => String::new(),
    };
    if e.heading_level == 0 || e.heading_level > 3 {
        e.heading_level = 2;
    }
    e.aliases = e
        .aliases
        .into_iter()
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty() && a != &e.title)
        .collect();
    e
}

/// 对启发式切出的章再精炼。
/// 顺序：规则切点 →（不够才）LLM 提议切点 → 一律用原文切片回填。
pub fn refine_drafts(
    blocks: &[Block],
    file_stem: &str,
    splitter: Option<&dyn EntrySplitter>,
) -> (Vec<DraftChunk>, Vec<String>) {
    let coalesced = coalesce_same_title(blocks);
    let mut drafts = Vec::new();
    let mut notes = Vec::new();
    let mut used_keys = HashSet::new();
    let mut ordinal = 0i64;

    enum Plan {
        Ready(Block, Vec<ExtractedEntry>),
        Llm(Block),
        Heuristic(Block),
    }

    let mut plans: Vec<Plan> = Vec::new();
    let mut llm_ch: Vec<(String, String)> = Vec::new();
    for b in coalesced {
        if let Some(entries) = maybe_local(&b) {
            plans.push(Plan::Ready(b, entries));
        } else if splitter.is_some() && should_llm(&b) {
            llm_ch.push((b.title.clone(), b.text.clone()));
            plans.push(Plan::Llm(b));
        } else {
            plans.push(Plan::Heuristic(b));
        }
    }

    let llm_out = match splitter {
        Some(s) if !llm_ch.is_empty() => s.split_many(&llm_ch),
        _ => Vec::new(),
    };
    let mut llm_iter = llm_out.into_iter();

    for plan in plans {
        match plan {
            Plan::Ready(b, entries) => {
                for e in entries {
                    push_extracted(
                        &mut drafts,
                        &mut used_keys,
                        file_stem,
                        &b.title,
                        e,
                        &mut ordinal,
                    );
                }
            }
            Plan::Heuristic(b) => {
                push_heuristic(&mut drafts, &mut used_keys, file_stem, &b, &mut ordinal)
            }
            Plan::Llm(b) => {
                let body_len = b.text.chars().count();
                match llm_iter.next() {
                    Some(Ok(proposals)) => {
                        let entries = materialize_entries(&b.text, proposals);
                        if entries.len() >= 2 || (entries.len() == 1 && body_len <= 800) {
                            for e in entries {
                                push_extracted(
                                    &mut drafts,
                                    &mut used_keys,
                                    file_stem,
                                    &b.title,
                                    e,
                                    &mut ordinal,
                                );
                            }
                        } else if entries.len() == 1 {
                            push_extracted(
                                &mut drafts,
                                &mut used_keys,
                                file_stem,
                                &b.title,
                                entries.into_iter().next().unwrap(),
                                &mut ordinal,
                            );
                        } else {
                            notes.push(format!(
                                "「{}」LLM 切点无法贴回原文，已用规则切开",
                                b.title
                            ));
                            push_heuristic(
                                &mut drafts,
                                &mut used_keys,
                                file_stem,
                                &b,
                                &mut ordinal,
                            );
                        }
                    }
                    Some(Err(err)) => {
                        notes.push(format!("「{}」拆条失败，已用规则切开：{err}", b.title));
                        push_heuristic(&mut drafts, &mut used_keys, file_stem, &b, &mut ordinal);
                    }
                    None => {
                        push_heuristic(&mut drafts, &mut used_keys, file_stem, &b, &mut ordinal)
                    }
                }
            }
        }
    }
    (drafts, notes)
}

fn coalesce_same_title(blocks: &[Block]) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    for b in blocks {
        if let Some(last) = out.last_mut() {
            if !b.title.is_empty() && last.title == b.title {
                last.text.push('\n');
                last.text.push_str(&b.text);
                continue;
            }
        }
        out.push(b.clone());
    }
    out
}

fn should_llm(b: &Block) -> bool {
    let n = b.text.chars().count();
    if n < 800 {
        return false;
    }
    let chapterish = looks_like_chapter_title(&b.title);
    if !chapterish && n < 2200 {
        return false;
    }
    true
}

fn looks_like_chapter_title(t: &str) -> bool {
    let t = t.trim();
    t.starts_with('第') || t.contains('章') || t.contains('节') || t.contains('回')
}

/// 规则层先按「短标题行」切开。切得够好就不再调用 LLM。
fn maybe_local(b: &Block) -> Option<Vec<ExtractedEntry>> {
    let v = local_structure_split(&b.text);
    if v.len() < 2 {
        return None;
    }
    let covered: usize = v.iter().map(|e| e.body.chars().count()).sum();
    let n = b.text.chars().count().max(1);
    if covered * 100 / n >= 50 {
        Some(v)
    } else {
        None
    }
}

pub fn local_structure_split(text: &str) -> Vec<ExtractedEntry> {
    let mut entries: Vec<(String, String)> = Vec::new();
    let mut title = String::new();
    let mut body = String::new();
    let flush = |entries: &mut Vec<(String, String)>, title: &mut String, body: &mut String| {
        let t = title.trim().to_string();
        let b = body.trim().to_string();
        if !t.is_empty() && b.chars().count() >= 12 {
            entries.push((t, b));
        } else if t.is_empty() && b.chars().count() >= 40 {
            entries.push(("未命名块".into(), b));
        }
        title.clear();
        body.clear();
    };
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            if !body.is_empty() {
                body.push('\n');
            }
            continue;
        }
        if is_entry_heading(t) {
            flush(&mut entries, &mut title, &mut body);
            title = strip_heading_marks(t);
            continue;
        }
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(t);
    }
    flush(&mut entries, &mut title, &mut body);
    entries
        .into_iter()
        .map(|(title, body)| ExtractedEntry {
            title,
            aliases: Vec::new(),
            entity_type: String::new(),
            body,
            anchor: String::new(),
            heading_level: 2,
        })
        .collect()
}

fn strip_heading_marks(t: &str) -> String {
    t.trim()
        .trim_matches(|c| c == '【' || c == '】' || c == '「' || c == '」' || c == '[' || c == ']')
        .trim()
        .to_string()
}

fn is_entry_heading(t: &str) -> bool {
    let t = t.trim();
    let n = t.chars().count();
    if n < 2 || n > 16 {
        return false;
    }
    if looks_like_chapter_title(t) {
        return false;
    }
    if t.starts_with('【') && t.ends_with('】') && n <= 16 {
        return true;
    }
    if "。！？；;".contains(t.chars().last().unwrap_or('\0')) {
        return false;
    }
    if t.contains('。') || t.contains('！') || t.contains('？') {
        return false;
    }
    if t.contains('：') || t.contains(':') {
        return false;
    }
    true
}

/// 把模型提议的切点贴回原文：body 一律取 source 的切片，丢弃模型扩写。
pub fn materialize_entries(source: &str, proposals: Vec<ExtractedEntry>) -> Vec<ExtractedEntry> {
    let mut cuts: Vec<(usize, ExtractedEntry)> = Vec::new();
    let mut from = 0usize;
    for p in proposals {
        let body_hint = first_chars(&p.body, 16);
        let needles = [p.anchor.as_str(), p.title.as_str(), body_hint.as_str()];
        let mut found = None;
        for n in needles {
            let n = n.trim();
            if n.chars().count() < 2 {
                continue;
            }
            if let Some(i) = find_from(source, n, from) {
                found = Some(i);
                break;
            }
        }
        if found.is_none() {
            if let Some(i) = find_from(source, p.title.trim(), 0) {
                if i >= from || cuts.is_empty() {
                    found = Some(i);
                }
            }
        }
        if let Some(i) = found {
            from = i.saturating_add(1).min(source.len());
            while from < source.len() && !source.is_char_boundary(from) {
                from += 1;
            }
            cuts.push((i, p));
        }
    }
    cuts.sort_by_key(|(i, _)| *i);
    cuts.dedup_by(|(a, _), (b, _)| a == b);
    if cuts.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    if cuts[0].0 >= 40 {
        let head = source[..cuts[0].0].trim();
        if head.chars().count() >= 40 {
            out.push(ExtractedEntry {
                title: "本章前言".into(),
                aliases: Vec::new(),
                entity_type: String::new(),
                body: head.to_string(),
                anchor: String::new(),
                heading_level: 2,
            });
        }
    }
    for (k, (start, p)) in cuts.iter().enumerate() {
        let end = cuts
            .get(k + 1)
            .map(|(i, _)| *i)
            .unwrap_or(source.len());
        if *start >= end || !source.is_char_boundary(*start) || !source.is_char_boundary(end) {
            continue;
        }
        let body = source[*start..end].trim().to_string();
        if body.chars().count() < 8 {
            continue;
        }
        out.push(ExtractedEntry {
            title: p.title.clone(),
            aliases: p.aliases.clone(),
            entity_type: p.entity_type.clone(),
            body,
            anchor: String::new(),
            heading_level: 2,
        });
    }
    merge_tiny(out)
}

fn merge_tiny(mut entries: Vec<ExtractedEntry>) -> Vec<ExtractedEntry> {
    if entries.len() < 2 {
        return entries;
    }
    let mut out: Vec<ExtractedEntry> = Vec::new();
    for e in entries.drain(..) {
        let nameless = e.title == "未命名块" || e.title == "本章前言";
        if nameless && e.body.chars().count() < 24 {
            if let Some(prev) = out.last_mut() {
                prev.body.push('\n');
                prev.body.push_str(&e.body);
                continue;
            }
        }
        out.push(e);
    }
    out
}

fn first_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn find_from(source: &str, needle: &str, from: usize) -> Option<usize> {
    let from = (0..=from.min(source.len()))
        .rev()
        .find(|i| source.is_char_boundary(*i))
        .unwrap_or(0);
    source.get(from..)?.find(needle).map(|i| from + i)
}

fn push_heuristic(
    drafts: &mut Vec<DraftChunk>,
    used_keys: &mut HashSet<String>,
    file_stem: &str,
    b: &Block,
    ordinal: &mut i64,
) {
    let mut one = blocks_to_drafts(std::slice::from_ref(b), file_stem);
    for d in &mut one {
        d.ordinal = *ordinal;
        if !used_keys.insert(d.stable_key.clone()) {
            d.stable_key = stable_key_with_ordinal(&d.parent_path, &d.title, *ordinal);
            used_keys.insert(d.stable_key.clone());
        }
        *ordinal += 1;
    }
    drafts.extend(one);
}

fn push_extracted(
    drafts: &mut Vec<DraftChunk>,
    used_keys: &mut HashSet<String>,
    file_stem: &str,
    chapter: &str,
    e: ExtractedEntry,
    ordinal: &mut i64,
) {
    let parent_path = if chapter.is_empty() || chapter == e.title {
        format!("{file_stem} / {}", e.title)
    } else {
        format!("{file_stem} / {chapter} / {}", e.title)
    };
    let mut aliases = crate::ingest::extract_aliases(&e.title, &e.body);
    for a in e.aliases {
        if !aliases.iter().any(|x| x == &a) {
            aliases.push(a);
        }
    }
    let entity_type = if e.entity_type.is_empty() {
        crate::ingest::guess_entity_type(&e.title, &e.body)
    } else {
        e.entity_type
    };
    let mut key = stable_key(&parent_path, &e.title);
    if !used_keys.insert(key.clone()) {
        key = stable_key_with_ordinal(&parent_path, &e.title, *ordinal);
        used_keys.insert(key.clone());
    }
    drafts.push(DraftChunk {
        stable_key: key,
        alt_anchor: alt_anchor(&parent_path, &e.body),
        ordinal: *ordinal,
        entity_type,
        title: e.title,
        body: e.body.clone(),
        parent_path,
        visibility: crate::ingest::guess_visibility(&e.body),
        aliases,
        content_hash: sha1_body_hex(&e.body),
    });
    *ordinal += 1;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_fenced_json() {
        let raw = r#"```json
[{"title":"猎人工会","aliases":["猎人公会"],"entity_type":"faction","anchor":"猎人工会驻扎熔炉区"}]
```"#;
        let v = parse_entries_json(raw).unwrap();
        assert_eq!(v[0].title, "猎人工会");
        assert_eq!(v[0].aliases[0], "猎人公会");
        assert_eq!(v[0].entity_type, "faction");
        assert!(!v[0].body.contains("别名："));
    }

    #[test]
    fn materialize_discards_rewritten_body() {
        let source = "猎人工会驻扎在熔炉区。登记员每天统计伤亡。工会负责悬赏。";
        let proposals = vec![
            ExtractedEntry {
                title: "猎人工会".into(),
                aliases: vec!["猎人公会".into()],
                entity_type: "faction".into(),
                body: "这是模型编的摘要，原文里没有。".into(),
                anchor: String::new(),
                heading_level: 2,
            },
            ExtractedEntry {
                title: "登记员".into(),
                aliases: vec![],
                entity_type: "npc".into(),
                body: "也是编的。".into(),
                anchor: String::new(),
                heading_level: 2,
            },
        ];
        let got = materialize_entries(source, proposals);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].title, "猎人工会");
        assert!(got[0].body.starts_with("猎人工会驻扎在熔炉区"));
        assert!(!got[0].body.contains("模型编的"));
        assert!(got[1].body.contains("登记员每天统计伤亡"));
    }

    #[test]
    fn local_split_uses_short_headings() {
        let text = "猎人工会\n驻扎熔炉区，负责悬赏与救援。成员遍布城邦。\n登记员\n每天统计伤亡，并把名单交给会长。";
        let v = local_structure_split(text);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].title, "猎人工会");
        assert!(v[0].body.contains("驻扎熔炉区"));
    }
}
