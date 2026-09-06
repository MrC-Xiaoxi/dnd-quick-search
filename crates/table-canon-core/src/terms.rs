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

/// 长口语抽词（方案 6.3.1）。A 层只发 >=3 字/3 字符 term，供 FTS5 trigram。
pub fn extract_terms(q1: &str) -> Vec<String> {
    extract_terms_limited(q1, 48, 32)
}

/// A2 放宽：多留一些 3/4-gram。
pub fn extract_terms_relaxed(q1: &str) -> Vec<String> {
    extract_terms_limited(q1, 48, 48)
}

pub fn extract_terms_limited(q1: &str, term_cap: usize, gram_cap: usize) -> Vec<String> {
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
            if run.chars().count() >= 3 {
                push(80, run);
            }
            continue;
        }
        let n = run.chars().count();
        if n <= 6 {
            if n >= 3 && !is_stopword(&run) {
                push(100 + n as i32, run.clone());
            }
            if n >= 2 {
                for py in phrase_pinyin_variants(&run) {
                    if py.chars().filter(|c| *c != ' ').count() >= 3 {
                        push(70, py);
                    }
                }
            }
        } else {
            let grams = ngrams(&run, 3, 4);
            for g in grams.into_iter().take(gram_cap) {
                if is_stopword(&g) {
                    continue;
                }
                let w = g.chars().count() as i32;
                push(50 + w, g.clone());
                for py in phrase_pinyin_variants(&g) {
                    if py.chars().filter(|c| *c != ' ').count() >= 3 {
                        push(40, py);
                    }
                }
            }
        }
    }

    if hanzi_count(&q) == 0 {
        let compact: String = q.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        if compact.len() >= 3 {
            push(95, compact);
        }
        if q.contains(' ') {
            push(90, q.clone());
        }
    } else if hanzi_count(&q) <= 6 && q.chars().count() >= 3 {
        if !is_stopword(&q) {
            push(90, q.clone());
        }
        for py in phrase_pinyin_variants(&q) {
            if py.chars().filter(|c| *c != ' ').count() >= 3 {
                push(60, py);
            }
        }
    }

    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.len().cmp(&b.1.len())));
    scored.truncate(term_cap);
    scored
        .into_iter()
        .map(|(_, t)| escape_fts_term(&t))
        .collect()
}

fn extract_runs(q: &str) -> Vec<String> {
    let mut runs = Vec::new();
    let mut cur = String::new();
    let mut kind = 0u8; // 1 hanzi, 2 latin
    let flush = |runs: &mut Vec<String>, cur: &mut String| {
        if !cur.is_empty() {
            runs.push(std::mem::take(cur));
        }
    };
    for ch in q.chars() {
        if is_hanzi(ch) {
            if kind != 1 {
                flush(&mut runs, &mut cur);
                kind = 1;
            }
            cur.push(ch);
        } else if is_latin_or_digit(ch) {
            if kind != 2 {
                flush(&mut runs, &mut cur);
                kind = 2;
            }
            cur.push(ch.to_ascii_lowercase());
        } else {
            flush(&mut runs, &mut cur);
            kind = 0;
        }
    }
    flush(&mut runs, &mut cur);
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

const FTS_KEYWORDS: &[&str] = &["AND", "OR", "NOT", "NEAR", "MATCH"];

/// 转义 FTS5 特殊字符。含空格的拼音必须整体加引号。AND/OR/NOT/NEAR 必须加引号。
pub fn escape_fts_term(term: &str) -> String {
    let escaped = term.replace('"', "\"\"");
    let keyword = FTS_KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(term));
    let special = term.contains(' ')
        || term.contains('-')
        || term
            .chars()
            .any(|c| matches!(c, '*' | '(' | ')' | ':' | '^' | '{' | '}'));
    if keyword || special {
        format!("\"{escaped}\"")
    } else if term.chars().all(|c| c.is_ascii_alphanumeric()) {
        escaped
    } else {
        format!("\"{escaped}\"")
    }
}

pub fn unescape_fts_term(term: &str) -> String {
    let t = term.trim().trim_matches('"');
    t.replace("\"\"", "\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_oral_extracts_bartender_4gram() {
        let terms = extract_terms("我们之前在那个独眼酒保的店里拿到了货");
        let raw: Vec<String> = terms.iter().map(|t| unescape_fts_term(t)).collect();
        assert!(
            raw.iter().any(|t| t == "独眼酒保"),
            "expected 4-gram 独眼酒保, terms={raw:?}"
        );
        assert!(
            !raw.iter().any(|t| t == "独眼" || t == "酒保" || t == "店" || t == "货"),
            "1-2 char terms must not enter A-layer FTS, terms={raw:?}"
        );
    }

    #[test]
    fn and_is_quoted() {
        let t = escape_fts_term("AND");
        assert_eq!(t, "\"AND\"");
        let terms = extract_terms("AND 格里姆");
        assert!(
            terms.iter().any(|x| unescape_fts_term(x).eq_ignore_ascii_case("AND")),
            "{terms:?}"
        );
    }

    #[test]
    fn compact_pinyin_from_spaced() {
        let terms = extract_terms("ge li mu");
        let raw: Vec<String> = terms.iter().map(|t| unescape_fts_term(t)).collect();
        assert!(raw.iter().any(|t| t == "gelimu"), "{raw:?}");
    }
}
