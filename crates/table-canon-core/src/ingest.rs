use crate::normalize::{
    alt_anchor, first_n_chars, hanzi_count, normalize, sha1_body_hex, stable_key,
    stable_key_with_ordinal,
};
use crate::pinyin_idx::search_pinyin_blob;
use crate::types::{Block, DraftChunk, VIS_DM_ONLY, VIS_PUBLIC, VIS_SECRET};
use anyhow::{Context, Result};
use encoding_rs::{Encoding, GB18030, UTF_8};
use regex::Regex;
use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;

pub fn read_text_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path)?;
    decode_bytes(&bytes)
}

pub fn decode_bytes(bytes: &[u8]) -> Result<String> {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return Ok(s.to_string());
    }
    decode_with(UTF_8, bytes).or_else(|_| decode_with(GB18030, bytes))
}

fn decode_with(enc: &'static Encoding, bytes: &[u8]) -> Result<String> {
    let (cow, _, had_errors) = enc.decode(bytes);
    if had_errors && enc == UTF_8 {
        anyhow::bail!("utf8 invalid");
    }
    Ok(cow.into_owned())
}

pub fn parse_file(path: &Path) -> Result<Vec<Block>> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "md" | "markdown" => {
            let text = read_text_file(path)?;
            Ok(blocks_from_markdown(&text))
        }
        "html" | "htm" => {
            let text = read_text_file(path)?;
            Ok(blocks_from_html(&text))
        }
        "txt" | "rtf" => {
            let text = read_text_file(path)?;
            Ok(blocks_from_plain(&text))
        }
        "docx" => parse_docx(path),
        "pdf" => anyhow::bail!("PDF 文本层抽取未纳入本 demo"),
        "doc" => anyhow::bail!("不支持 .doc，请另存为 .docx"),
        _ => anyhow::bail!("不支持的文件类型: {ext}"),
    }
}

fn html_heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<h([1-4])[^>]*>(.*?)</h[1-4]>").expect("html heading re"))
}

fn strip_tag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?is)<script[^>]*>.*?</script>|<style[^>]*>.*?</style>|<[^>]+>")
            .expect("strip tag re")
    })
}

fn para_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<w:p[ >](.*?)</w:p>").expect("docx p re"))
}

fn wt_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<w:t[^>]*>(.*?)</w:t>").expect("docx t re"))
}

fn style_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"w:val="(Heading[1-4]|heading[1-4])""#).expect("docx style re")
    })
}

fn alias_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:又名|别名|亦称)[:：]\s*([^\n]+)").expect("alias re"))
}

fn title_paren_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(.+?)[（(]([^）)]+)[）)]").expect("title paren re"))
}

fn strip_html(s: &str) -> String {
    strip_tag_re().replace_all(s, " ").to_string()
}

fn blocks_from_html(html: &str) -> Vec<Block> {
    let marked = html_heading_re().replace_all(html, |caps: &regex::Captures| {
        let level: usize = caps[1].parse().unwrap_or(1);
        let inner = strip_html(&caps[2]);
        format!("\n{} {}\n", "#".repeat(level.clamp(1, 4)), inner.trim())
    });
    blocks_from_markdown(&strip_html(&marked))
}

fn atx_heading(t: &str) -> Option<(u8, String)> {
    if !t.starts_with('#') {
        return None;
    }
    let mut level = 0u8;
    let mut rest = t;
    while let Some(stripped) = rest.strip_prefix('#') {
        level += 1;
        rest = stripped;
        if level >= 4 {
            break;
        }
    }
    if !(rest.starts_with(' ') || rest.starts_with('\t')) {
        return None;
    }
    let title = rest.trim().trim_end_matches('#').trim();
    Some((
        level.max(1),
        if title.is_empty() {
            "未命名".into()
        } else {
            title.to_string()
        },
    ))
}

fn is_setext_underline(t: &str, ch: char) -> bool {
    let t = t.trim();
    t.len() >= 3 && t.chars().all(|c| c == ch)
}

fn blocks_from_markdown(text: &str) -> Vec<Block> {
    let lines: Vec<&str> = text.lines().collect();
    let mut blocks = Vec::new();
    let mut cur_level: u8 = 1;
    let mut cur_title = "文档".to_string();
    let mut buf = String::new();
    let flush = |blocks: &mut Vec<Block>, title: &str, level: u8, buf: &mut String| {
        let body = buf.trim().to_string();
        buf.clear();
        if body.is_empty() {
            return;
        }
        blocks.push(Block {
            heading_level: level,
            title: title.to_string(),
            text: body,
        });
    };
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let t = line.trim_end();
        if let Some((level, title)) = atx_heading(t) {
            flush(&mut blocks, &cur_title, cur_level, &mut buf);
            cur_level = level;
            cur_title = title;
            i += 1;
            continue;
        }
        if i + 1 < lines.len() {
            let next = lines[i + 1].trim();
            let heading = t.trim();
            if !heading.is_empty() && heading.chars().count() <= 80 {
                if is_setext_underline(next, '=') {
                    flush(&mut blocks, &cur_title, cur_level, &mut buf);
                    cur_level = 1;
                    cur_title = heading.to_string();
                    i += 2;
                    continue;
                }
                if is_setext_underline(next, '-') {
                    flush(&mut blocks, &cur_title, cur_level, &mut buf);
                    cur_level = 2;
                    cur_title = heading.to_string();
                    i += 2;
                    continue;
                }
            }
        }
        if t == "***" || t == "---" || t == "___" {
            flush(&mut blocks, &cur_title, cur_level, &mut buf);
            i += 1;
            continue;
        }
        buf.push_str(line);
        buf.push('\n');
        i += 1;
    }
    flush(&mut blocks, &cur_title, cur_level, &mut buf);
    if blocks.is_empty() {
        blocks_from_plain(text)
    } else {
        split_long_blocks(blocks)
    }
}

fn blocks_from_plain(text: &str) -> Vec<Block> {
    let paras: Vec<&str> = text.split('\n').map(str::trim_end).collect();
    let mut chunks = Vec::new();
    let mut buf = String::new();
    for p in paras {
        if p.trim().is_empty() {
            if hanzi_count(&buf) > 80 || buf.chars().count() > 200 {
                chunks.push(std::mem::take(&mut buf));
            } else {
                buf.push('\n');
            }
            continue;
        }
        if !buf.is_empty() {
            buf.push('\n');
        }
        buf.push_str(p);
        if buf.chars().count() >= 2000 {
            chunks.push(std::mem::take(&mut buf));
        }
    }
    if !buf.trim().is_empty() {
        chunks.push(buf);
    }
    if chunks.is_empty() && !text.trim().is_empty() {
        chunks.push(text.to_string());
    }
    chunks
        .into_iter()
        .filter(|c| !c.trim().is_empty())
        .enumerate()
        .map(|(i, text)| {
            let title = first_n_chars(&normalize(&text), 40);
            Block {
                heading_level: 1,
                title: if title.is_empty() {
                    format!("段落{}", i + 1)
                } else {
                    title
                },
                text,
            }
        })
        .collect()
}

fn split_long_blocks(blocks: Vec<Block>) -> Vec<Block> {
    let mut out = Vec::new();
    for b in blocks {
        if b.text.chars().count() <= 2000 {
            out.push(b);
            continue;
        }
        let mut acc = String::new();
        for para in b.text.split('\n') {
            if acc.chars().count() + para.chars().count() > 2000 && !acc.is_empty() {
                out.push(Block {
                    heading_level: b.heading_level,
                    title: b.title.clone(),
                    text: acc.trim().to_string(),
                });
                let overlap: String = acc
                    .chars()
                    .rev()
                    .take(80)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect();
                acc = overlap;
            }
            acc.push_str(para);
            acc.push('\n');
        }
        if !acc.trim().is_empty() {
            out.push(Block {
                heading_level: b.heading_level,
                title: b.title.clone(),
                text: acc.trim().to_string(),
            });
        }
    }
    out
}

fn parse_docx(path: &Path) -> Result<Vec<Block>> {
    let file = fs::File::open(path)?;
    let mut zip = zip::ZipArchive::new(file).context("打开 docx")?;
    let mut xml_file = zip
        .by_name("word/document.xml")
        .context("docx 缺少 word/document.xml")?;
    let mut xml = String::new();
    xml_file.read_to_string(&mut xml)?;
    drop(xml_file);
    let mut blocks = Vec::new();
    let mut cur_title = "文档".to_string();
    let mut cur_level: u8 = 1;
    let mut buf = String::new();
    for cap in para_re().captures_iter(&xml) {
        let p = &cap[1];
        let mut text = String::new();
        for t in wt_re().captures_iter(p) {
            text.push_str(&decode_xml_entities(&t[1]));
        }
        text = text.trim().to_string();
        if let Some(st) = style_re().captures(p) {
            if !buf.trim().is_empty() {
                blocks.push(Block {
                    heading_level: cur_level,
                    title: cur_title.clone(),
                    text: buf.trim().to_string(),
                });
                buf.clear();
            }
            cur_title = if text.is_empty() {
                "未命名".into()
            } else {
                text.clone()
            };
            let n = st[1]
                .chars()
                .last()
                .and_then(|c| c.to_digit(10))
                .unwrap_or(1) as u8;
            cur_level = n.clamp(1, 4);
            continue;
        }
        if text.is_empty() {
            buf.push('\n');
        } else {
            if !buf.is_empty() {
                buf.push('\n');
            }
            buf.push_str(&text);
        }
    }
    if !buf.trim().is_empty() {
        blocks.push(Block {
            heading_level: cur_level,
            title: cur_title,
            text: buf.trim().to_string(),
        });
    }
    if blocks.is_empty() {
        anyhow::bail!("docx 未抽出文本");
    }
    Ok(split_long_blocks(blocks))
}

fn decode_xml_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

pub fn guess_entity_type(title: &str, body: &str) -> String {
    let h = format!("{title}\n{}", first_n_chars(body, 80));
    let keys = [
        ("npc", &["NPC", "人物", "角色"][..]),
        ("location", &["地点", "城镇", "酒馆", "港口", "城市", "村庄"]),
        ("item", &["物品", "魔法物品", "道具", "武器"]),
        ("faction", &["势力", "组织", "公会", "教会"]),
        ("rule", &["规则", "裁定"]),
        ("plot", &["密谋", "线索", "剧情"]),
    ];
    for (ty, kws) in keys {
        if kws.iter().any(|k| h.contains(k)) {
            return ty.to_string();
        }
    }
    "other".into()
}

pub fn extract_aliases(title: &str, body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let blob = format!("{title}\n{body}");
    for cap in alias_re().captures_iter(&blob) {
        for part in cap[1].split(|c| "、,/，;；".contains(c)) {
            let t = part.trim();
            if !t.is_empty() {
                out.push(t.to_string());
            }
        }
    }
    if let Some(c) = title_paren_re().captures(title) {
        let inner = c[2].trim();
        if !inner.is_empty() && inner.chars().count() <= 20 {
            out.push(inner.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

pub fn guess_visibility(body: &str) -> i64 {
    let mut v = VIS_PUBLIC;
    for line in body.lines() {
        let t = line.trim();
        if t.starts_with("【秘密】") || t.starts_with("【密谋】") || t.to_ascii_lowercase().contains("secret")
        {
            v |= VIS_SECRET;
        }
        if t.starts_with("【DM】") || t.starts_with("DM only") || t.contains("对玩家隐藏") {
            v |= VIS_DM_ONLY;
        }
    }
    v
}

pub fn is_secret_markup_line(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("【秘密】")
        || t.starts_with("【DM】")
        || t.starts_with("【密谋】")
        || t.contains("对玩家隐藏")
}

pub fn blocks_to_drafts(blocks: &[Block], file_stem: &str) -> Vec<DraftChunk> {
    let mut path_stack: Vec<(u8, String)> = vec![(0, file_stem.to_string())];
    let mut drafts = Vec::new();
    let mut used_keys = HashSet::new();
    for (i, b) in blocks.iter().enumerate() {
        while path_stack
            .last()
            .map(|(lv, _)| *lv >= b.heading_level)
            .unwrap_or(false)
            && path_stack.len() > 1
        {
            path_stack.pop();
        }
        path_stack.push((b.heading_level, b.title.clone()));
        let parent_path = path_stack
            .iter()
            .map(|(_, n)| n.as_str())
            .collect::<Vec<_>>()
            .join(" / ");
        let title = if b.title.trim().is_empty() {
            first_n_chars(&normalize(&b.text), 40)
        } else {
            b.title.clone()
        };
        let ordinal = i as i64;
        let aliases = extract_aliases(&title, &b.text);
        let mut key = stable_key(&parent_path, &title);
        if !used_keys.insert(key.clone()) {
            key = stable_key_with_ordinal(&parent_path, &title, ordinal);
            used_keys.insert(key.clone());
        }
        drafts.push(DraftChunk {
            stable_key: key,
            alt_anchor: alt_anchor(&parent_path, &b.text),
            ordinal,
            entity_type: guess_entity_type(&title, &b.text),
            title,
            body: b.text.clone(),
            parent_path,
            visibility: guess_visibility(&b.text),
            aliases,
            content_hash: sha1_body_hex(&b.text),
        });
    }
    drafts
}

pub fn build_search_text(
    title: &str,
    body: &str,
    aliases: &[String],
    extra_synonyms: &[String],
) -> String {
    let mut parts = vec![
        title.to_string(),
        body.to_string(),
        aliases.join("\n"),
        extra_synonyms.join("\n"),
        search_pinyin_blob(title, aliases),
        normalize(body),
    ];
    parts.retain(|s| !s.trim().is_empty());
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atx_requires_space() {
        let blocks = blocks_from_markdown("#not-heading\n\n# Real Title\n\nbody");
        assert!(
            blocks.iter().any(|b| b.title == "Real Title"),
            "{:?}",
            blocks.iter().map(|b| &b.title).collect::<Vec<_>>()
        );
        assert!(
            !blocks.iter().any(|b| b.title == "not-heading"),
            "{:?}",
            blocks.iter().map(|b| &b.title).collect::<Vec<_>>()
        );
    }

    #[test]
    fn html_headings() {
        let blocks = blocks_from_html("<h2>断桅酒馆</h2><p>地点。一楼卖劣酒。</p>");
        assert!(
            blocks.iter().any(|b| b.title.contains("断桅酒馆")),
            "{:?}",
            blocks.iter().map(|b| &b.title).collect::<Vec<_>>()
        );
    }

    #[test]
    fn gb18030_roundtrip() {
        let (bytes, _, _) = encoding_rs::GB18030.encode("独眼酒保");
        let s = decode_bytes(&bytes).unwrap();
        assert!(s.contains("独眼酒保"), "{s}");
    }
}
