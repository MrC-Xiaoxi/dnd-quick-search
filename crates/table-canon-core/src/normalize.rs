use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::sync::OnceLock;

pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub fn sha1_hex(s: &str) -> String {
    let mut h = Sha1::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}

pub fn sha256_file_hex(bytes: &[u8]) -> String {
    hex::encode(crate::sha256::digest(bytes))
}

pub fn sha1_body_hex(s: &str) -> String {
    sha1_hex(&format!("body:{s}"))
}

fn t2s_map() -> &'static HashMap<char, char> {
    static MAP: OnceLock<HashMap<char, char>> = OnceLock::new();
    MAP.get_or_init(|| {
        let mut m = HashMap::new();
        for line in include_str!("../assets/t2s.txt").lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut chars = line.chars();
            if let (Some(from), Some(to)) = (chars.next(), chars.next()) {
                if from != to {
                    m.insert(from, to);
                }
            }
        }
        m
    })
}

pub fn to_simplified(s: &str) -> String {
    let map = t2s_map();
    s.chars().map(|c| *map.get(&c).unwrap_or(&c)).collect()
}

/// 简体优先：繁→简、全角→半角、压缩空白、去首尾。
pub fn normalize(s: &str) -> String {
    let s = to_simplified(s);
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

pub fn stable_key(parent_path: &str, title: &str) -> String {
    sha1_hex(&format!("{}|{}", normalize(parent_path), normalize(title)))
}

pub fn stable_key_with_ordinal(parent_path: &str, title: &str, ordinal: i64) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_is_64_hex() {
        let h = sha256_file_hex(b"hello");
        assert_eq!(h.len(), 64);
        assert_eq!(
            h,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_ne!(h, sha256_file_hex(b"hello!"));
    }

    #[test]
    fn t2s_bartender() {
        let n = normalize("獨眼酒保在斷桅酒館");
        assert!(n.contains("独眼酒保"), "{n}");
        assert!(n.contains("断桅酒馆"), "{n}");
    }
}
