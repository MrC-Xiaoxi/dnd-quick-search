use pinyin::{ToPinyin, ToPinyinMulti};
use std::collections::BTreeSet;

const CARTESIAN_CAP: usize = 16;

/// 单字全部读音（去调）。
pub fn char_readings(ch: char) -> Vec<String> {
    let s = ch.to_string();
    let mut set = BTreeSet::new();
    for multi in s.as_str().to_pinyin_multi() {
        if let Some(multi) = multi {
            for p in multi {
                set.insert(p.plain().to_string());
            }
        }
    }
    if set.is_empty() {
        if let Some(Some(p)) = s.as_str().to_pinyin().next() {
            set.insert(p.plain().to_string());
        }
    }
    set.into_iter().collect()
}

/// 一段汉字的全部读音组合（全拼空格分词 + 首字母串）。
pub fn phrase_pinyin_variants(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().filter(|c| super::normalize::is_hanzi(*c)).collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let mut syll: Vec<Vec<String>> = Vec::new();
    for ch in &chars {
        let r = char_readings(*ch);
        if r.is_empty() {
            syll.push(vec!["?".to_string()]);
        } else {
            syll.push(r);
        }
    }
    let combos = cartesian(&syll, CARTESIAN_CAP);
    let mut out = Vec::new();
    for combo in combos {
        out.push(combo.join(" "));
        let initials: String = combo
            .iter()
            .filter_map(|s| s.chars().next())
            .collect();
        if initials.len() >= 2 {
            out.push(initials);
        }
    }
    out.sort();
    out.dedup();
    out
}

fn cartesian(sets: &[Vec<String>], cap: usize) -> Vec<Vec<String>> {
    let mut acc: Vec<Vec<String>> = vec![vec![]];
    for set in sets {
        let mut next = Vec::new();
        for prefix in &acc {
            for item in set {
                if next.len() >= cap {
                    break;
                }
                let mut row = prefix.clone();
                row.push(item.clone());
                next.push(row);
            }
            if next.len() >= cap {
                break;
            }
        }
        if next.is_empty() {
            return acc;
        }
        acc = next;
    }
    acc
}

/// 写入 search_text 的拼音袋：标题/别名全组合 + 正文逐字全读音。
pub fn search_pinyin_blob(title: &str, body: &str, aliases: &[String]) -> String {
    let mut parts = Vec::new();
    parts.extend(phrase_pinyin_variants(title));
    for a in aliases {
        parts.extend(phrase_pinyin_variants(a));
    }
    for ch in body.chars() {
        if super::normalize::is_hanzi(ch) {
            for r in char_readings(ch) {
                parts.push(r);
            }
        }
    }
    parts.join("\n")
}
