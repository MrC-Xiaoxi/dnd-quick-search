use crate::normalize::{hanzi_count, is_hanzi, is_latin_or_digit, normalize};
use crate::pinyin_idx::phrase_pinyin_variants;
use std::collections::HashSet;
use std::sync::OnceLock;

pub static STOPWORDS: OnceLock<HashSet<String>> = OnceLock::new();

pub fn stopwords() -> &'static HashSet<String> {
    STOPWORDS.get_or_init(|| {
        include_str!("../assets/stopwords.zh.txt")
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect()
    })
}

pub fn is_stopword(s: &str) -> bool {
    stopwords().contains(s)
}

/// 长口语抽词（方案 6.3.1）。返回已转义、可进 FTS 的 term。
pub fn extract_terms(q1: &str) -> Vec<String> {
    let q = normalize(q1);
    if q.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(i32, String)> = Vec::new();
    let mut seen = HashSet::new();

    let mut push = |weight: i32, raw: String| {
        if raw.is_empty() {
            return;
        }
        if !seen.insert(raw.clone()) {
            return;
        }
        scored.push((weight, raw));
    };

    for run in extract_runs(&q) {
        if run.chars().all(is_latin_or_digit) {
            push(80, run);
            continue;
        }
        let n = run.chars().count();
        if n <= 6 {
            if !is_stopword(&run) {
                push(100 + n as i32, run.clone());
            }
            for py in phrase_pinyin_variants(&run) {
                push(70, py);
            }
        } else {
            let grams = ngrams(&run, 2, 4);
            for g in grams.into_iter().take(32) {
                if is_stopword(&g) {
                    continue;
                }
                let w = g.chars().count() as i32;
                push(50 + w, g.clone());
                for py in phrase_pinyin_variants(&g) {
                    push(40, py);
                }
            }
        }
    }

    if hanzi_count(&q) <= 6 {
        push(90, q.clone());
        for py in phrase_pinyin_variants(&q) {
            push(60, py);
        }
    }

    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.len().cmp(&b.1.len())));
    scored.truncate(48);
    scored
        .into_iter()
        .map(|(_, t)| escape_fts_term(&t))
        .collect()
}

fn extract_runs(q: &str) -> Vec<String> {
    let mut runs = Vec::new();
    let mut cur = String::new();
    let mut kind = 0u8; // 1 hanzi, 2 latin
    let flush = |runs: &mut Vec<String>, cur: &mut String, _kind: u8| {
        if !cur.is_empty() {
            runs.push(std::mem::take(cur));
        }
    };
    for ch in q.chars() {
        if is_hanzi(ch) {
            if kind != 1 {
                flush(&mut runs, &mut cur, kind);
                kind = 1;
            }
            cur.push(ch);
        } else if is_latin_or_digit(ch) {
            if kind != 2 {
                flush(&mut runs, &mut cur, kind);
                kind = 2;
            }
            cur.push(ch.to_ascii_lowercase());
        } else {
            flush(&mut runs, &mut cur, kind);
            kind = 0;
        }
    }
    flush(&mut runs, &mut cur, kind);
    runs
}

fn ngrams(run: &str, min: usize, max: usize) -> Vec<String> {
    let chars: Vec<char> = run.chars().collect();
    let n = chars.len();
    let mut out = Vec::new();
    for w in (min..=max).rev() {
        if w > n {
            continue;
        }
        for i in 0..=n - w {
            let g: String = chars[i..i + w].iter().collect();
            let first: String = chars[i..i + 1].iter().collect();
            let last: String = chars[i + w - 1..i + w].iter().collect();
            if is_stopword(&first) || is_stopword(&last) {
                continue;
            }
            out.push(g);
        }
    }
    out
}

/// 转义 FTS5 特殊字符。含空格的拼音必须整体加引号。
pub fn escape_fts_term(term: &str) -> String {
    let escaped = term.replace('"', "\"\"");
    if term.contains(' ') || term.chars().any(|c| matches!(c, '*' | '(' | ')' | ':')) {
        format!("\"{escaped}\"")
    } else if term.chars().all(|c| c.is_ascii_lowercase() || c == ' ') && term.contains(' ') {
        format!("\"{escaped}\"")
    } else {
        // 纯拼音无空格（首字母）或汉字：trigram 可直接用；仍加引号更稳
        if term.chars().all(|c| c.is_ascii_alphanumeric()) {
            escaped
        } else {
            format!("\"{escaped}\"")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_oral_extracts_bartender() {
        let terms = extract_terms("我们之前在那个独眼酒保的店里拿到了货");
        let joined = terms.join(" ");
        assert!(
            joined.contains("独眼") || joined.contains("酒保") || joined.contains("格里"),
            "terms={terms:?}"
        );
    }
}
