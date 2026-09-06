use crate::normalize::{
    alt_anchor, first_n_chars, hanzi_count, normalize, sha1_body_hex, stable_key,
    stable_key_with_ordinal,
};
use crate::pinyin_idx::search_pinyin_blob;
use crate::types::{Block, DraftChunk, VIS_DM_ONLY, VIS_PUBLIC, VIS_SECRET};
use anyhow::Result;
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

fn tbl_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<w:tbl[\s>].*?</w:tbl>").expect("docx tbl re"))
}

fn tr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<w:tr[\s>].*?</w:tr>").expect("docx tr re"))
}

fn tc_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<w:tc[\s>].*?</w:tc>").expect("docx tc re"))
}

fn del_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<w:del\b[^>]*>.*?</w:del>").expect("docx del re"))
}

fn instr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?s)<w:instrText\b[^>]*>.*?</w:instrText>").expect("docx instr re")
    })
}

fn br_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"<w:br\b[^>]*/?>").expect("docx br re"))
}

fn tab_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"<w:tab\b[^>]*/?>").expect("docx tab re"))
}

fn pstyle_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"<w:pStyle\b[^>]*\bw:val="([^"]+)""#).expect("docx pstyle re")
    })
}

fn outline_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"<w:outlineLvl\b[^>]*\bw:val="(\d+)""#).expect("docx outline re")
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

fn heading_n(s: &str) -> Option<u8> {
    let n: u8 = s.trim().parse().ok()?;
    (1..=4).contains(&n).then_some(n)
}

fn heading_level_from_style(val: &str) -> Option<u8> {
    let v = val.trim();
    if v.is_empty() {
        return None;
    }
    let lower = v.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("heading") {
        return heading_n(rest);
    }
    for prefix in ["标题", "標題"] {
        if let Some(rest) = v.strip_prefix(prefix) {
            return heading_n(rest);
        }
    }
    if v.chars().all(|c| c.is_ascii_digit()) {
        return heading_n(v);
    }
    None
}

fn heading_level_from_p(p: &str) -> Option<u8> {
    if let Some(c) = pstyle_re().captures(p) {
        if let Some(lv) = heading_level_from_style(&c[1]) {
            return Some(lv);
        }
    }
    if let Some(c) = outline_re().captures(p) {
        let n: u8 = c[1].parse().ok()?;
        if n <= 3 {
            return Some(n + 1);
        }
    }
    None
}

fn paragraph_text(p: &str) -> String {
    let with_br = br_re().replace_all(p, "\n");
    let with_tab = tab_re().replace_all(&with_br, " ");
    let mut text = String::new();
    for t in wt_re().captures_iter(&with_tab) {
        text.push_str(&decode_xml_entities(&t[1]));
    }
    scrub_xml_leak(&text)
}

/// Word 偶发未闭合 <w:t> 会把 pPr/rFonts 整段吞进正文。检出后剥标签。
fn scrub_xml_leak(s: &str) -> String {
    if !s.contains('<') {
        return s.to_string();
    }
    static TAG: OnceLock<Regex> = OnceLock::new();
    let re = TAG.get_or_init(|| Regex::new(r"(?s)<[^>]+>").expect("xml tag re"));
    let stripped = re.replace_all(s, " ");
    stripped.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn looks_like_heading_text(text: &str) -> Option<u8> {
    let t = text.trim();
    let n = t.chars().count();
    if n == 0 || n > 32 {
        return None;
    }
    if t.contains('<') {
        return None;
    }
    if t.starts_with('第') && (t.contains('章') || t.contains('节') || t.contains('回')) {
        return Some(1);
    }
    None
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn cell_text(tc: &str) -> String {
    let mut parts = Vec::new();
    for cap in para_re().captures_iter(tc) {
        let t = paragraph_text(&cap[1]);
        let t = t.trim();
        if !t.is_empty() {
            parts.push(t.to_string());
        }
    }
    parts.join(" ")
}

fn flatten_table(tbl: &str) -> String {
    let mut out = String::new();
    for row in tr_re().captures_iter(tbl) {
        let mut cells: Vec<String> = tc_re()
            .captures_iter(&row[0])
            .map(|c| cell_text(&c[0]))
            .collect();
        while cells.last().is_some_and(|s| s.is_empty()) {
            cells.pop();
        }
        if cells.is_empty() || cells.iter().all(|s| s.is_empty()) {
            continue;
        }
        let line = if cells.len() == 2 {
            format!("{}：{}", cells[0], cells[1])
        } else {
            cells.join(" / ")
        };
        out.push_str("<w:p><w:r><w:t>");
        out.push_str(&xml_escape(&line));
        out.push_str("</w:t></w:r></w:p>");
    }
    out
}

fn blocks_from_docx_xml(xml: &str) -> Vec<Block> {
    let xml = del_re().replace_all(xml, "");
    let xml = instr_re().replace_all(&xml, "");
    let xml = tbl_re().replace_all(&xml, |caps: &regex::Captures| flatten_table(&caps[0]));
    let mut blocks = Vec::new();
    let mut cur_title = "文档".to_string();
    let mut cur_level: u8 = 1;
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
    for cap in para_re().captures_iter(&xml) {
        let p = &cap[1];
        let trimmed = paragraph_text(p).trim().to_string();
        let heading = heading_level_from_p(p).or_else(|| looks_like_heading_text(&trimmed));
        if let Some(level) = heading {
            flush(&mut blocks, &cur_title, cur_level, &mut buf);
            cur_title = if trimmed.is_empty() {
                "未命名".into()
            } else {
                trimmed
            };
            cur_level = level;
            continue;
        }
        if trimmed.is_empty() {
            buf.push('\n');
        } else {
            if !buf.is_empty() {
                buf.push('\n');
            }
            buf.push_str(&trimmed);
        }
    }
    flush(&mut blocks, &cur_title, cur_level, &mut buf);
    blocks
}

fn parse_docx(path: &Path) -> Result<Vec<Block>> {
    let file = fs::File::open(path)?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| {
        anyhow::anyhow!("不是有效的 .docx，请在 Word 里另存为 .docx（不要用旧版 .doc）：{e}")
    })?;
    let mut xml_file = zip.by_name("word/document.xml").map_err(|_| {
        anyhow::anyhow!("不是有效的 .docx，缺少 word/document.xml，请另存为 .docx")
    })?;
    let mut xml = String::new();
    xml_file.read_to_string(&mut xml)?;
    drop(xml_file);
    let blocks = blocks_from_docx_xml(&xml);
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

    fn write_min_docx(path: &Path, body_xml: &str) {
        use std::io::Write;
        let file = fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file("[Content_Types].xml", opts).unwrap();
        zip.write_all(
            br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"#,
        )
        .unwrap();
        zip.start_file("_rels/.rels", opts).unwrap();
        zip.write_all(
            br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#,
        )
        .unwrap();
        zip.start_file("word/document.xml", opts).unwrap();
        let doc = format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>{body_xml}</w:body>
</w:document>"#
        );
        zip.write_all(doc.as_bytes()).unwrap();
        zip.finish().unwrap();
    }

    fn titles(blocks: &[Block]) -> Vec<&str> {
        blocks.iter().map(|b| b.title.as_str()).collect()
    }

    #[test]
    fn docx_heading_and_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("npc.docx");
        write_min_docx(
            &path,
            r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>潮汐帮</w:t></w:r></w:p>
<w:p><w:r><w:t>势力。港口走私网。</w:t></w:r><w:del><w:r><w:t>不该出现</w:t></w:r></w:del></w:p>
<w:p><w:pPr><w:pStyle w:val="Heading2"/></w:pPr><w:r><w:t>玛拉（账房）</w:t></w:r></w:p>
<w:p><w:r><w:t>NPC。潮汐帮管账。</w:t></w:r></w:p>
<w:tbl><w:tr>
<w:tc><w:p><w:r><w:t>别名</w:t></w:r></w:p></w:tc>
<w:tc><w:p><w:r><w:t>账房玛拉、小玛</w:t></w:r></w:p></w:tc>
</w:tr></w:tbl>"#,
        );
        let blocks = parse_file(&path).unwrap();
        assert!(
            blocks.iter().any(|b| b.title == "潮汐帮"),
            "{:?}",
            titles(&blocks)
        );
        let mara = blocks
            .iter()
            .find(|b| b.title.contains("玛拉"))
            .unwrap_or_else(|| panic!("missing heading 2: {:?}", titles(&blocks)));
        assert!(mara.text.contains("别名：账房玛拉"), "{}", mara.text);
        assert!(
            !blocks.iter().any(|b| b.text.contains("不该出现")),
            "{:?}",
            blocks.iter().map(|b| &b.text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn docx_chinese_style_and_outline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cn.docx");
        write_min_docx(
            &path,
            r#"<w:p><w:pPr><w:pStyle w:val="标题1"/></w:pPr><w:r><w:t>港口</w:t></w:r></w:p>
<w:p><w:r><w:t>地点。湿咸的风。</w:t></w:r></w:p>
<w:p><w:pPr><w:pStyle w:val="2"/><w:outlineLvl w:val="1"/></w:pPr><w:r><w:t>账房</w:t></w:r></w:p>
<w:p><w:r><w:t>二楼。</w:t></w:r></w:p>
<w:p><w:pPr><w:outlineLvl w:val="0"/></w:pPr><w:r><w:t>潮汐帮</w:t></w:r></w:p>
<w:p><w:r><w:t>势力。</w:t></w:r></w:p>"#,
        );
        let blocks = parse_file(&path).unwrap();
        assert!(
            blocks.iter().any(|b| b.title == "港口" && b.heading_level == 1),
            "{:?}",
            titles(&blocks)
        );
        assert!(
            blocks.iter().any(|b| b.title == "账房" && b.heading_level == 2),
            "{:?}",
            titles(&blocks)
        );
        assert!(
            blocks.iter().any(|b| b.title == "潮汐帮" && b.heading_level == 1),
            "{:?}",
            titles(&blocks)
        );
    }

    #[test]
    fn docx_invalid_and_old_doc() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad.docx");
        fs::write(&bad, b"not-a-zip").unwrap();
        let err = parse_file(&bad).unwrap_err().to_string();
        assert!(err.contains("另存为"), "{err}");
        let old = dir.path().join("old.doc");
        fs::write(&old, b"x").unwrap();
        let err = parse_file(&old).unwrap_err().to_string();
        assert!(err.contains(".doc"), "{err}");
    }

    #[test]
    fn sample_docx_mara() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/sample-campaign/03-人物.docx");
        let blocks = parse_file(&path).unwrap();
        assert!(
            blocks.iter().any(|b| b.title.contains("玛拉")),
            "{:?}",
            titles(&blocks)
        );
        assert!(
            blocks.iter().any(|b| b.text.contains("账房玛拉")),
            "{:?}",
            blocks.iter().map(|b| &b.text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn docx_unclosed_wt_does_not_leak_ppr() {
        let inner = r#"<w:pPr><w:pBdr><w:left w:val="none" w:sz="0"/></w:pBdr><w:rPr><w:rFonts w:ascii="华文中宋"/></w:rPr></w:pPr><w:r><w:t>第1章 接受任务"#;
        // 故意不闭合第一个 w:t，模拟吞进 rFonts 的文档
        let leaked = format!("{inner}</w:t></w:r></w:p>");
        let text = paragraph_text(&leaked);
        assert!(!text.contains("<w:"), "{text}");
        assert!(text.contains("第1章") || text.contains("接受任务"), "{text}");
    }

    #[test]
    fn chapter_line_is_heading() {
        assert_eq!(looks_like_heading_text("第1章 接受任务"), Some(1));
        assert_eq!(looks_like_heading_text("猎人工会的登记员在统计伤亡"), None);
    }
}
