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
        .filter(|e| !e.title.trim().is_empty() && !e.body.trim().is_empty())
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
        .filter(|a| !a.is_empty())
        .collect();
    if !e.aliases.is_empty() && !e.body.contains("别名") {
        e.body = format!("别名：{}\n\n{}", e.aliases.join("、"), e.body);
    }
    e
}

/// 对启发式切出的章再精炼。短块、已是专名的条目不调用拆条器。
pub fn refine_drafts(
    blocks: &[Block],
    file_stem: &str,
    splitter: Option<&dyn EntrySplitter>,
) -> (Vec<DraftChunk>, Vec<String>) {
    let Some(splitter) = splitter else {
        return (blocks_to_drafts(blocks, file_stem), Vec::new());
    };
    let coalesced = coalesce_same_title(blocks);
    let mut drafts = Vec::new();
    let mut notes = Vec::new();
    let mut used_keys = HashSet::new();
    let mut ordinal = 0i64;

    let mut keep: Vec<Block> = Vec::new();
    let mut llm_idx: Vec<usize> = Vec::new();
    let mut llm_ch: Vec<(String, String)> = Vec::new();
    for b in coalesced {
        if should_llm(&b) {
            llm_idx.push(keep.len());
            llm_ch.push((b.title.clone(), b.text.clone()));
            keep.push(b);
        } else {
            keep.push(b);
        }
    }
    let llm_out = if llm_ch.is_empty() {
        Vec::new()
    } else {
        splitter.split_many(&llm_ch)
    };
    let mut llm_iter = llm_out.into_iter();
    for (i, b) in keep.into_iter().enumerate() {
        if llm_idx.first() == Some(&i) {
            llm_idx.remove(0);
            let body_len = b.text.chars().count();
            match llm_iter.next() {
                Some(Ok(entries)) if entries.len() >= 2 || body_len > 800 => {
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
                Some(Ok(entries)) if entries.len() == 1 => {
                    push_extracted(
                        &mut drafts,
                        &mut used_keys,
                        file_stem,
                        &b.title,
                        entries.into_iter().next().unwrap(),
                        &mut ordinal,
                    );
                }
                Some(Ok(_)) => push_heuristic(&mut drafts, &mut used_keys, file_stem, &b, &mut ordinal),
                Some(Err(err)) => {
                    notes.push(format!("「{}」拆条失败，已用规则切开：{err}", b.title));
                    push_heuristic(&mut drafts, &mut used_keys, file_stem, &b, &mut ordinal);
                }
                None => push_heuristic(&mut drafts, &mut used_keys, file_stem, &b, &mut ordinal),
            }
        } else {
            push_heuristic(&mut drafts, &mut used_keys, file_stem, &b, &mut ordinal);
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
[{"title":"猎人工会","aliases":["猎人公会"],"entity_type":"faction","body":"驻扎熔炉区。"}]
```"#;
        let v = parse_entries_json(raw).unwrap();
        assert_eq!(v[0].title, "猎人工会");
        assert!(v[0].body.contains("别名"));
        assert_eq!(v[0].entity_type, "faction");
    }
}
