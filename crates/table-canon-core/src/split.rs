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
    // 1) 严格裸数组
    if let Ok(entries) = serde_json::from_str::<Vec<ExtractedEntry>>(trimmed.trim()) {
        return finish_entries(entries);
    }
    // 2) {"entries":[...]} 对象包装（json_object 模式的常见形态）
    if let Ok(obj) = serde_json::from_str::<EntriesObj>(trimmed.trim()) {
        return finish_entries(obj.entries);
    }
    // 3) 容错截取：从混杂文本里抓第一个 [ 到最后一个 ]
    let start = trimmed
        .find('[')
        .with_context(|| "LLM 输出里没有 JSON 数组")?;
    let end = trimmed
        .rfind(']')
        .with_context(|| "LLM 输出里 JSON 数组未闭合")?;
    let slice = &trimmed[start..=end];
    let entries: Vec<ExtractedEntry> =
        serde_json::from_str(slice).with_context(|| "无法解析拆条 JSON")?;
    finish_entries(entries)
}

#[derive(serde::Deserialize)]
struct EntriesObj {
    #[serde(default)]
    entries: Vec<ExtractedEntry>,
}

fn finish_entries(entries: Vec<ExtractedEntry>) -> Result<Vec<ExtractedEntry>> {
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

/// 正文按语义段编号后的一个片段：`idx` 是章内全局段号，对应正文的字节区间 `[offset, offset+len)`。
#[derive(Debug, Clone)]
pub struct Segment {
    pub idx: usize,
    pub offset: usize,
    pub len: usize,
}

impl Segment {
    pub fn text<'a>(&self, body: &'a str) -> &'a str {
        &body[self.offset..self.offset + self.len]
    }
}

/// 单段字数上限：段越细，模型给的切点越准，但 prompt 里的编号开销也越大。
const MAX_SEG_CHARS: usize = 500;

/// 把正文切成带全局段号的语义段：按行分段，超长行按句读（。！？；）再切。
/// llm.rs 用它给正文编号，materialize_entries 用同一函数把段号映射回字符偏移，两侧必须一致。
pub fn segment_text(body: &str) -> Vec<Segment> {
    let mut segs: Vec<Segment> = Vec::new();
    let mut pos = 0usize;
    for line in body.split('\n') {
        let (s, e) = trim_range(body, pos, pos + line.len());
        pos += line.len() + 1; // +1 吃掉 '\n'；末行时越界无妨，循环随即结束
        if e > s {
            let line_str = &body[s..e];
            if line_str.chars().count() <= MAX_SEG_CHARS {
                push_seg(&mut segs, s, e);
            } else {
                for (a, b) in sentence_ranges(line_str, MAX_SEG_CHARS) {
                    let (a2, b2) = trim_range(line_str, a, b);
                    if b2 > a2 {
                        push_seg(&mut segs, s + a2, s + b2);
                    }
                }
            }
        }
    }
    segs
}

fn push_seg(segs: &mut Vec<Segment>, offset: usize, end: usize) {
    let idx = segs.len();
    segs.push(Segment {
        idx,
        offset,
        len: end - offset,
    });
}

/// 在字节区间内收缩到首/末个非空白字符，保持 UTF-8 边界安全。
fn trim_range(s: &str, from: usize, to: usize) -> (usize, usize) {
    let to = to.min(s.len());
    let mut a = from.min(to);
    let mut b = to;
    while a < b && !s.is_char_boundary(a) {
        a += 1;
    }
    while b > a && !s.is_char_boundary(b) {
        b -= 1;
    }
    let rest = &s[a..b];
    let lead = rest.len() - rest.trim_start().len();
    let trail = rest.len() - rest.trim_end().len();
    (a + lead, b - trail)
}

/// 超长行按句读切成 ≤max_chars 字的块，返回每块的字节区间。
fn sentence_ranges(s: &str, max_chars: usize) -> Vec<(usize, usize)> {
    let mut bounds: Vec<usize> = s.char_indices().map(|(i, _)| i).collect();
    bounds.push(s.len());
    let total = bounds.len() - 1;
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut start_k = 0usize;
    while start_k < total {
        if total - start_k <= max_chars {
            out.push((bounds[start_k], s.len()));
            break;
        }
        let cap_k = start_k + max_chars;
        let min_k = start_k + (max_chars / 2).max(1);
        let mut cut_k = cap_k;
        for k in (min_k..=cap_k).rev() {
            let c = s[bounds[k]..].chars().next().unwrap_or('\0');
            if "。！？；".contains(c) {
                cut_k = k + 1;
                break;
            }
        }
        out.push((bounds[start_k], bounds[cut_k]));
        start_k = cut_k;
    }
    out
}

/// 分片：把段号连续的段打包成 ≤max_chars 的分片，正文每行带 `[nnn]` 段号前缀。
/// 相邻分片共享 `overlap` 个段，跨片边界的条目在两侧都看得完整。
#[derive(Debug, Clone)]
pub struct Piece {
    pub first_seg: usize,
    pub last_seg: usize,
    pub text: String,
}

pub fn pack_pieces(body: &str, segs: &[Segment], max_chars: usize, overlap: usize) -> Vec<Piece> {
    if segs.is_empty() {
        return Vec::new();
    }
    let overlap = overlap.min(segs.len().saturating_sub(1));
    let mut out: Vec<Piece> = Vec::new();
    let mut start = 0usize;
    while start < segs.len() {
        let mut len_chars = 0usize;
        let mut end = start;
        while end < segs.len() {
            let need = segs[end].text(body).chars().count() + 8; // 8 ≈ "[nnn] " 前缀加换行
            if end > start && len_chars + need > max_chars {
                break;
            }
            len_chars += need;
            end += 1;
        }
        let text = segs[start..end]
            .iter()
            .map(|sg| format!("[{:03}] {}\n", sg.idx, sg.text(body).trim()))
            .collect();
        out.push(Piece {
            first_seg: start,
            last_seg: end - 1,
            text,
        });
        if end >= segs.len() {
            break;
        }
        let prev = start;
        let next = end.saturating_sub(overlap);
        start = if next > prev { next } else { prev + 1 };
    }
    out
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
    merge_leadin_drafts(&mut drafts);
    (drafts, notes)
}

/// 导语条前并：正文很短且以冒号结尾的条目（「温度分层：……如下：」这类一句话导语），
/// 其完整解释在下一条里，单独成条让用户光凭一句话看不到解释内容。
/// 把它并入同章节的下一条（body 前置、标题转为别名），ordinals 重新连续编号。
fn merge_leadin_drafts(drafts: &mut Vec<DraftChunk>) {
    const LEADIN_MAX_CHARS: usize = 80;
    let is_lead = |d: &DraftChunk| {
        if d.body.chars().count() >= LEADIN_MAX_CHARS {
            return false;
        }
        let body = d.body.trim_end();
        if body.ends_with(':') || body.ends_with('：') {
            return true;
        }
        // 标题带冒号而正文是列表首项的碎片（如「结算步骤：」+「1. …」）
        let title = d.title.trim_end();
        title.ends_with(':') || title.ends_with('：')
    };
    let mut i = 0usize;
    while i + 1 < drafts.len() {
        let lead = &drafts[i];
        if is_lead(lead)
            && chapter_prefix(&lead.parent_path) == chapter_prefix(&drafts[i + 1].parent_path)
        {
            let absorbed = lead.title.clone();
            let lead_body = lead.body.clone();
            let next = &mut drafts[i + 1];
            next.body = format!("{lead_body}\n{}", next.body);
            if !absorbed.is_empty()
                && absorbed != next.title
                && !next.aliases.contains(&absorbed)
            {
                next.aliases.insert(0, absorbed);
            }
            next.content_hash = sha1_body_hex(&next.body);
            next.alt_anchor = alt_anchor(&next.parent_path, &next.body);
            drafts.remove(i);
        } else {
            i += 1;
        }
    }
    // ordinal 必须连续：阅读页按 ordinal±1 找上下文条目
    for (k, d) in drafts.iter_mut().enumerate() {
        d.ordinal = k as i64;
    }
}

/// parent_path 去掉末段标题后的章节前缀："stem / 章 / 条" → "stem / 章"。
fn chapter_prefix(parent_path: &str) -> &str {
    parent_path.rsplit_once(" / ").map_or("", |(p, _)| p)
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
            start: -1,
            end: -1,
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
    if !(2..=16).contains(&n) {
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
/// 定位顺序：段号（确定性映射，与模型复写能力解耦）→ anchor/title 精确匹配 → 段级模糊匹配兜底。
pub fn materialize_entries(source: &str, proposals: Vec<ExtractedEntry>) -> Vec<ExtractedEntry> {
    let segs = segment_text(source);
    let mut cuts: Vec<(usize, ExtractedEntry)> = Vec::new();
    let mut from = 0usize;
    for p in proposals {
        if let Some(i) = locate_cut(source, &segs, &p, from) {
            from = i.saturating_add(1);
            cuts.push((i, p));
        }
    }
    cuts.sort_by_key(|(i, _)| *i);
    // 切点相距不超过一个 anchor 长度（约定 8–24 字）视为跨分片对同一条目的重复提议：
    // 别名并入前者，丢弃后者。再远的相邻切点就是正常条目边界了。
    const DUP_CUT_CHARS: usize = 24;
    cuts.dedup_by(|later, earlier| {
        let (off_l, e_l) = later;
        let (off_e, e_e) = earlier;
        if off_l.saturating_sub(*off_e) < DUP_CUT_CHARS {
            for al in &e_l.aliases {
                if !e_e.aliases.contains(al) {
                    e_e.aliases.push(al.clone());
                }
            }
            if e_e.title.trim().is_empty() {
                e_e.title = e_l.title.clone();
            }
            true
        } else {
            false
        }
    });
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
                start: -1,
                end: -1,
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
            start: -1,
            end: -1,
            heading_level: 2,
        });
    }
    merge_tiny(out)
}

/// 为一条提议定位切点字节偏移。段号路径失败才走文本精确匹配，最后模糊兜底。
fn locate_cut(source: &str, segs: &[Segment], p: &ExtractedEntry, from: usize) -> Option<usize> {
    if p.start >= 0 {
        if let Some(seg) = segs.get(p.start as usize) {
            let mut off = seg.offset;
            let anchor = p.anchor.trim();
            if anchor.chars().count() >= 2 {
                // anchor 降级为校验贴齐：仅在条目开头 200 字窗口内精确命中才采纳
                let win = (off + 200).min(source.len());
                if let Some(i) = find_in(source, anchor, off, win) {
                    off = i;
                }
            }
            return Some(off);
        }
    }
    let body_hint = first_chars(&p.body, 16);
    for n in [p.anchor.trim(), p.title.trim(), body_hint.trim()] {
        if n.chars().count() >= 2 {
            if let Some(i) = find_in(source, n, from, source.len()) {
                return Some(i);
            }
        }
    }
    fuzzy_find_cut(source, segs, p, from)
}

/// 模糊兜底：anchor/title 与各段做字符二元组相似度，≥阈值即认为该段开头是切点。
/// 应对弱模型复写 anchor 时的漏字、多字与标点差异（业界数据：精确引文匹配有 5–10% 失败率）。
fn fuzzy_find_cut(source: &str, segs: &[Segment], p: &ExtractedEntry, from: usize) -> Option<usize> {
    let anchor = p.anchor.trim();
    let needle = if anchor.chars().count() >= 6 {
        anchor
    } else {
        p.title.trim()
    };
    if needle.chars().count() < 4 {
        return None;
    }
    let mut best: Option<(f64, usize)> = None;
    for seg in segs {
        if seg.offset + seg.len <= from {
            continue;
        }
        let sim = bigram_dice(needle, seg.text(source));
        if sim >= 0.7 && best.is_none_or(|(s, _)| sim > s) {
            best = Some((sim, seg.offset));
        }
    }
    best.map(|(_, off)| off)
}

/// 字符二元组 Dice 相似度：2·|A∩B| / (|A|+|B|)。
fn bigram_dice(a: &str, b: &str) -> f64 {
    let grams = |s: &str| -> Vec<(char, char)> {
        let cs: Vec<char> = s.chars().filter(|c| !c.is_whitespace()).collect();
        cs.windows(2).map(|w| (w[0], w[1])).collect()
    };
    let ga = grams(a);
    let gb = grams(b);
    if ga.is_empty() || gb.is_empty() {
        return 0.0;
    }
    let mut used = vec![false; gb.len()];
    let mut hit = 0usize;
    for g in &ga {
        for (k, x) in gb.iter().enumerate() {
            if !used[k] && x == g {
                used[k] = true;
                hit += 1;
                break;
            }
        }
    }
    2.0 * hit as f64 / (ga.len() + gb.len()) as f64
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

/// 在 `source[from..upto]` 里找 `needle` 的绝对字节偏移，自动回落到 UTF-8 边界。
fn find_in(source: &str, needle: &str, from: usize, upto: usize) -> Option<usize> {
    let from = boundary_floor(source, from);
    let upto = boundary_ceil(source, upto.min(source.len()));
    if from >= upto {
        return None;
    }
    source.get(from..upto)?.find(needle).map(|i| from + i)
}

fn boundary_floor(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn boundary_ceil(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
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
                start: -1,
                end: -1,
                heading_level: 2,
            },
            ExtractedEntry {
                title: "登记员".into(),
                aliases: vec![],
                entity_type: "npc".into(),
                body: "也是编的。".into(),
                anchor: String::new(),
                start: -1,
                end: -1,
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

    #[test]
    fn segment_text_numbers_lines_and_splits_long_lines() {
        let long = "铁砧堡扼守山口。".repeat(80); // 960 字，超过 MAX_SEG_CHARS
        let body = format!("短行一。\n\n   \n{long}");
        let segs = segment_text(&body);
        assert_eq!(segs[0].idx, 0);
        assert_eq!(segs[0].text(&body), "短行一。");
        // 空行不占段号
        assert!(segs.len() >= 3);
        // 每段都不超上限，且段区间拼回原文无缺
        assert!(segs.iter().all(|s| s.text(&body).chars().count() <= 500));
        let joined: String = segs.iter().map(|s| s.text(&body)).collect();
        assert!(joined.contains("铁砧堡扼守山口。"));
    }

    #[test]
    fn pack_pieces_overlap_keeps_global_numbering() {
        let body = (0..40).map(|i| format!("第{i}段的内容。")).collect::<Vec<_>>().join("\n");
        let segs = segment_text(&body);
        let pieces = pack_pieces(&body, &segs, 120, 1);
        assert!(pieces.len() >= 3);
        // 相邻分片共享 1 段：下一片的 first_seg == 上一片的 last_seg
        for w in pieces.windows(2) {
            assert_eq!(w[1].first_seg, w[0].last_seg);
        }
        // 段号全局连续，每行都带 [nnn] 前缀
        let first = &pieces[0];
        assert!(first.text.starts_with("[000] "));
        let last = pieces.last().unwrap();
        assert_eq!(last.last_seg, segs.len() - 1);
    }

    #[test]
    fn materialize_prefers_segment_indices() {
        let body = "铁砧堡扼守山口，是防御核心。\n城主荀岚兼管税收与熔炉。\n商队过山口必须缴费。\n登记员负责记录伤亡。";
        let segs = segment_text(body);
        assert_eq!(segs.len(), 4);
        let proposals = vec![
            ExtractedEntry {
                title: "铁砧堡".into(),
                aliases: vec![],
                entity_type: "location".into(),
                body: String::new(),
                anchor: String::new(),
                start: 0,
                end: 0,
                heading_level: 2,
            },
            ExtractedEntry {
                title: "荀岚".into(),
                aliases: vec![],
                entity_type: "npc".into(),
                body: String::new(),
                anchor: String::new(),
                start: 1,
                end: 3,
                heading_level: 2,
            },
        ];
        let got = materialize_entries(body, proposals);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].body, "铁砧堡扼守山口，是防御核心。");
        // 负样本：模型编造的 anchor 与 title 都贴不回，也不影响段号定位
        assert!(got[1].body.contains("城主荀岚"));
        assert!(got[1].body.ends_with("登记员负责记录伤亡。"));
    }

    #[test]
    fn materialize_merges_cross_piece_duplicate_proposals() {
        let body = "铁砧堡扼守山口。\n城主荀岚兼管税收。\n商队过山口必须缴费。\n登记员负责记录伤亡。\n守卫昼夜巡逻。";
        // 两个分片对同一条目各提了一嘴：一个用段号从 [000] 切入，一个用 anchor 从条目中间切入，切点相距很近
        let proposals = vec![
            ExtractedEntry {
                title: "铁砧堡".into(),
                aliases: vec![],
                entity_type: "location".into(),
                body: String::new(),
                anchor: String::new(),
                start: 0,
                end: 1,
                heading_level: 2,
            },
            ExtractedEntry {
                title: "铁砧堡要塞".into(),
                aliases: vec!["山口要塞".into()],
                entity_type: "location".into(),
                body: String::new(),
                anchor: "扼守山口".into(),
                start: -1,
                end: -1,
                heading_level: 2,
            },
            ExtractedEntry {
                title: "登记员".into(),
                aliases: vec![],
                entity_type: "npc".into(),
                body: String::new(),
                anchor: "登记员负责".into(),
                start: -1,
                end: -1,
                heading_level: 2,
            },
        ];
        let got = materialize_entries(body, proposals);
        assert_eq!(got.len(), 2, "重复提议应被合并: {got:?}");
        assert_eq!(got[0].title, "铁砧堡");
        assert!(got[0].aliases.contains(&"山口要塞".to_string()));
        assert!(got[1].body.contains("登记员"));
    }

    #[test]
    fn materialize_fuzzy_recovers_misspelled_anchor() {
        let source = "熔炉区商会每旬开一次集会，商会会长格里姆负责定价。会员缴纳会费后可在北区摆摊。";
        let proposals = vec![ExtractedEntry {
            title: "熔炉区商会".into(),
            aliases: vec![],
            entity_type: "faction".into(),
            body: String::new(),
            // 模型复写漏了两个字还改了标点
            anchor: "熔炉区商每旬一次集会，会长格里".into(),
            start: -1,
            end: -1,
            heading_level: 2,
        }];
        let got = materialize_entries(source, proposals);
        assert_eq!(got.len(), 1);
        assert!(got[0].body.starts_with("熔炉区商会每旬"));
    }

    #[test]
    fn parse_accepts_object_wrapper_and_string_indices() {
        let raw = r#"{"entries":[{"title":"铁砧堡","entity_type":"location","start":"000","end":"1"}]}"#;
        let v = parse_entries_json(raw).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].start, 0);
        assert_eq!(v[0].end, 1);
    }

    #[test]
    fn parse_tolerates_prose_around_array() {
        let raw = "好的，以下是拆条结果：\n[{\"title\":\"格里姆\",\"entity_type\":\"npc\",\"start\":2,\"end\":3}]\n希望有帮助。";
        let v = parse_entries_json(raw).unwrap();
        assert_eq!(v[0].title, "格里姆");
        assert_eq!(v[0].start, 2);
    }

    #[test]
    fn leadin_draft_merges_into_next_within_same_chapter() {
        // 还原真实案例：温度分层(27字导语，冒号结尾) / 霜顶区(完整解释)
        let blocks = vec![
            Block {
                heading_level: 2,
                title: "温度分层".into(),
                text: "终燃城由三层构成，温度随深度递增，生存难度随高度递增：".into(),
            },
            Block {
                heading_level: 2,
                title: "霜顶区（最上层）".into(),
                text: "• 基础温度：极寒，零下30度至零下50度\n• 环境描述：永冻荒原，狂风呼啸，暴露在外的皮肤会在几分钟内冻伤。".into(),
            },
        ];
        let (drafts, _) = refine_drafts(&blocks, "罪业之潮", None);
        assert_eq!(drafts.len(), 1, "导语条应并入下一条");
        assert_eq!(drafts[0].title, "霜顶区（最上层）");
        assert!(drafts[0].body.starts_with("终燃城由三层构成"));
        assert!(drafts[0].aliases.contains(&"温度分层".to_string()));
        // ordinal 重新连续，阅读页按 ordinal±1 才能找到上下文
        assert_eq!(drafts[0].ordinal, 0);
    }

    #[test]
    fn complete_short_entry_and_cross_chapter_lead_are_kept() {
        // 完整短条目（句号结尾）不算导语，不能被吞
        let blocks = vec![
            Block {
                heading_level: 2,
                title: "终燃城".into(),
                text: "终燃城是一座倒置的坟墓，深入山骸与地壳，温暖既是生存必须品，更是流通的货币。".into(),
            },
            Block {
                heading_level: 2,
                title: "暖廊".into(),
                text: "连接三层的恒温通道，每半小时巡逻一次。".into(),
            },
        ];
        let (drafts, _) = refine_drafts(&blocks, "罪业之潮", None);
        assert_eq!(drafts.len(), 2);
        assert_eq!(drafts[0].title, "终燃城");
    }
}
