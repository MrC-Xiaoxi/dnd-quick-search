use sha1::{Digest, Sha1};

pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub fn sha1_hex(s: &str) -> String {
    let mut h = Sha1::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}

pub fn sha256_file_hex(bytes: &[u8]) -> String {
    use sha1::Sha1;
    // 方案写的是 SHA-256；demo 用 SHA-1 即可区分变更。文件级用同一 hasher 前缀 f:
    let mut h = Sha1::new();
    h.update(b"file:");
    h.update(bytes);
    hex::encode(h.finalize())
}

pub fn sha1_body_hex(s: &str) -> String {
    sha1_hex(&format!("body:{s}"))
}

/// 简体优先：全角→半角、压缩空白、去首尾。
pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for ch in s.chars() {
        let c = fullwidth_to_half(ch);
        if c.is_whitespace() {
            if !prev_space && !out.is_empty() {
                out.push(' ');
                prev_space = true;
            }
            continue;
        }
        prev_space = false;
        out.push(c);
    }
    out.trim().to_string()
}

fn fullwidth_to_half(c: char) -> char {
    match c {
        '\u{3000}' => ' ',
        '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
        _ => c,
    }
}

pub fn hanzi_count(s: &str) -> usize {
    s.chars().filter(|c| is_hanzi(*c)).count()
}

pub fn is_hanzi(c: char) -> bool {
    let u = c as u32;
    (0x4E00..=0x9FFF).contains(&u) || (0x3400..=0x4DBF).contains(&u)
}

pub fn is_latin_or_digit(c: char) -> bool {
    c.is_ascii_alphanumeric()
}

pub fn first_n_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

pub fn stable_key(parent_path: &str, title: &str, ordinal: i64) -> String {
    sha1_hex(&format!(
        "{}|{}|{}",
        normalize(parent_path),
        normalize(title),
        ordinal
    ))
}

pub fn alt_anchor(parent_path: &str, body: &str) -> String {
    let head = first_n_chars(&normalize(body), 80);
    sha1_hex(&format!("{}|{}", normalize(parent_path), head))
}
