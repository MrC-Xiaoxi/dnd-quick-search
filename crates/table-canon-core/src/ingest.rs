use crate::normalize::{alt_anchor, hanzi_count, normalize, sha1_body_hex, stable_key};
use crate::pinyin_idx::search_pinyin_blob;
use crate::types::{Block, DraftChunk, VIS_DM_ONLY, VIS_PUBLIC, VIS_SECRET};
use anyhow::{Context, Result};
use encoding_rs::{Encoding, GB18030, UTF_8};
use regex::Regex;
use std::fs;
use std::io::Read;
use std::path::Path;

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
        "md" | "markdown" | "txt" | "html" | "htm" | "rtf" => {
            let text = read_text_file(path)?;
            if ext == "html" || ext == "htm" {
                Ok(blocks_from_plain(&strip_html(&text)))
            } else if ext == "md" || ext == "markdown" {
                Ok(blocks_from_markdown(&text))
            } else {
                Ok(blocks_from_plain(&text))
            }
        }
        "docx" => parse_docx(path),
        "pdf" => anyhow::bail!("PDF 文本层抽取未纳入本 demo（见方案：扫描件/缺 CMap 不进 M1 闭环）"),
        "doc" => anyhow::bail!("不支持 .doc，请另存为 .docx"),
        _ => {
            let text = read_text_file(path)?;
            Ok(blocks_from_plain(&text))
        }
    }
}

fn strip_html(s: &str) -> String {
    let re = Regex::new(r"(?is)<script[^>]*>.*?</script>|<style[^>]*>.*?</style>|<[^>]+>").unwrap();
    re.replace_all(s, " ").to_string()
}

fn blocks_from_markdown(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut cur_level: u8 = 1;
    let mut cur_title = "文档".to_string();
    let mut buf = String::new();
    let flush = |blocks: &mut Vec<Block>, title: &str, level: u8, buf: &mut String| {
        let body = buf.trim().to_string();
        buf.clear();
        if body.is_empty() && title == "文档" {
            return;
        }
        if body.is_empty() {
            return;
        }
        blocks.push(Block {
            heading_level: level,
            title: title.to_string(),
            text: body,
        });
    };
    for line in text.lines() {
        let t = line.trim_end();
        if let Some(rest) = t.strip_prefix('#') {
            let mut level = 1u8;
            let mut r = rest;
            while let Some(x) = r.strip_prefix('#') {
                level += 1;
                r = x;
                if level >= 4 {
                    break;
                }
            }
            if r.starts_with(' ') || r.starts_with('\t') || level >= 1 {
                flush(&mut blocks, &cur_title, cur_level, &mut buf);
                cur_level = level.min(4);
                cur_title = r.trim().trim_start_matches('#').trim().to_string();
                if cur_title.is_empty() {
                    cur_title = "未命名".into();
                }
                continue;
            }
        }
        if t == "---" || t == "***" {
            flush(&mut blocks, &cur_title, cur_level, &mut buf);
            continue;
        }
        buf.push_str(line);
        buf.push('\n');
    }
    flush(&mut blocks, &cur_title, cur_level, &mut buf);
    if blocks.is_empty() {
        blocks_from_plain(text)
    } else {
        split_long_blocks(blocks)
    }
}

fn blocks_from_plain(text: &str) -> Vec<Block> {
    let paras: Vec<&str> = text
        .split(|c| c == '\n')
        .map(str::trim_end)
        .collect();
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
            let title = crate::normalize::first_n_chars(&normalize(&text), 40);
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
    let para_re = Regex::new(r"(?s)<w:p[ >](.*?)</w:p>").unwrap();
    let t_re = Regex::new(r"(?s)<w:t[^>]*>(.*?)</w:t>").unwrap();
    let style_re = Regex::new(r#"w:val="(Heading[1-4]|heading[1-4])""#).unwrap();
    let mut cur_title = "文档".to_string();
    let mut cur_level: u8 = 1;
    let mut buf = String::new();
    for cap in para_re.captures_iter(&xml) {
        let p = &cap[1];
        let mut text = String::new();
        for t in t_re.captures_iter(p) {
            text.push_str(&decode_xml_entities(&t[1]));
        }
        text = text.trim().to_string();
        if let Some(st) = style_re.captures(p) {
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
    let h = format!("{title}\n{}", crate::normalize::first_n_chars(body, 80));
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
    let re = Regex::new(r"(?:又名|别名|亦称)[:：]\s*([^\n]+)").unwrap();
    let blob = format!("{title}\n{body}");
    for cap in re.captures_iter(&blob) {
        for part in cap[1].split(|c| "、,/，;；".contains(c)) {
            let t = part.trim();
            if !t.is_empty() {
                out.push(t.to_string());
            }
        }
    }
    let paren = Regex::new(r"(.+?)[（(]([^）)]+)[）)]").unwrap();
    if let Some(c) = paren.captures(title) {
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

pub fn blocks_to_drafts(blocks: &[Block], file_stem: &str) -> Vec<DraftChunk> {
    let mut path_stack: Vec<(u8, String)> = vec![(0, file_stem.to_string())];
    let mut drafts = Vec::new();
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
            crate::normalize::first_n_chars(&normalize(&b.text), 40)
        } else {
            b.title.clone()
        };
        let ordinal = i as i64;
        let aliases = extract_aliases(&title, &b.text);
        drafts.push(DraftChunk {
            stable_key: stable_key(&parent_path, &title, ordinal),
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
        search_pinyin_blob(title, body, aliases),
        normalize(body),
    ];
    parts.retain(|s| !s.trim().is_empty());
    parts.join("\n")
}
