use crate::db::{load_chunk, load_synonyms};
use crate::normalize::{hanzi_count, now_rfc3339};
use crate::terms::{extract_terms, extract_terms_relaxed, unescape_fts_term};
use crate::types::{Chunk, Hit, SearchResult};
use anyhow::Result;
use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

pub struct SearchQuery<'a> {
    pub text: &'a str,
}

const RRF_K: f64 = 60.0;
const HARD_TIMEOUT_MS: u128 = 4500;
const QUERY_CHAR_CAP: usize = 2000;

pub fn search(conn: &Connection, campaign_id: i64, q: SearchQuery<'_>) -> Result<SearchResult> {
    let started = Instant::now();
    let mut q0 = q.text.to_string();
    if q0.chars().count() > QUERY_CHAR_CAP {
        q0 = q0.chars().take(QUERY_CHAR_CAP).collect();
    }
    let mut terms = extract_terms(&q0);
    let syn = load_synonyms(conn, campaign_id);
    let mut extra = Vec::new();
    for t in &terms {
        let raw = unescape_fts_term(t);
        if let Some(c) = syn.get(&raw) {
            extra.push(crate::terms::escape_fts_term(c));
        }
        if let Some(c) = syn.get(t) {
            extra.push(crate::terms::escape_fts_term(c));
        }
    }
    terms.extend(extra);
    terms.sort();
    terms.dedup();

    let (hanzi_terms, pinyin_terms): (Vec<_>, Vec<_>) = terms.iter().cloned().partition(|t| {
        unescape_fts_term(t)
            .chars()
            .any(crate::normalize::is_hanzi)
    });

    let mut ranks: HashMap<i64, (f64, Vec<String>)> = HashMap::new();

    if !hanzi_terms.is_empty() && !timed_out(started) {
        run_fts(conn, campaign_id, &hanzi_terms, 50, &mut ranks);
    }

    if ranks.len() < 3 && has_long_hanzi_run(&q0) && !timed_out(started) {
        let relaxed: Vec<String> = extract_terms_relaxed(&q0)
            .into_iter()
            .filter(|t| {
                unescape_fts_term(t)
                    .chars()
                    .any(crate::normalize::is_hanzi)
            })
            .collect();
        run_fts(conn, campaign_id, &relaxed, 50, &mut ranks);
    }

    let qn = crate::normalize::normalize(&q0);
    if hanzi_count(&qn) >= 1 && hanzi_count(&qn) <= 4 && !timed_out(started) {
        like_scan(conn, campaign_id, &qn, &mut ranks);
    }

    if ranks.len() < 3 && !pinyin_terms.is_empty() && !timed_out(started) {
        run_fts(conn, campaign_id, &pinyin_terms, 30, &mut ranks);
    }

    let mut scored: Vec<(i64, f64, Vec<String>)> =
        ranks.into_iter().map(|(id, (s, v))| (id, s, v)).collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(40);

    let mut hits = Vec::new();
    let mut seen_title: HashSet<(i64, String)> = HashSet::new();
    for (id, score, via) in scored {
        if timed_out(started) && hits.len() >= 5 {
            break;
        }
        if let Ok(chunk) = load_chunk(conn, id) {
            let key = (
                chunk.source_document_id,
                crate::normalize::normalize(&chunk.title),
            );
            if !seen_title.insert(key) {
                continue;
            }
            let score = score + contains_boost(&chunk, &qn);
            hits.push(Hit { chunk, score, via });
        }
    }
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.chunk.source_rank.cmp(&b.chunk.source_rank))
            .then(a.chunk.title.cmp(&b.chunk.title))
    });
    hits.truncate(20);

    let latency_ms = started.elapsed().as_millis();
    let _ = conn.execute(
        "INSERT INTO query_log(campaign_id, query, latency_ms, used_semantic, created_at)
         VALUES(?1,?2,?3,0,?4)",
        params![campaign_id, q.text, latency_ms as i64, now_rfc3339()],
    );
    let _ = conn.execute(
        "DELETE FROM query_log WHERE campaign_id=?1 AND id NOT IN (
            SELECT id FROM query_log WHERE campaign_id=?1 ORDER BY id DESC LIMIT 500
         )",
        [campaign_id],
    );

    let shown_terms = if hanzi_terms.is_empty() {
        terms
    } else {
        hanzi_terms
    };

    Ok(SearchResult {
        hits,
        latency_ms,
        used_semantic: false,
        truncated: latency_ms >= HARD_TIMEOUT_MS,
        terms: shown_terms,
    })
}

fn contains_boost(chunk: &Chunk, q: &str) -> f64 {
    if q.is_empty() {
        return 0.0;
    }
    let title = crate::normalize::normalize(&chunk.title);
    let body = crate::normalize::normalize(&chunk.body);
    if title == *q {
        return 12.0;
    }
    if title.contains(q) {
        return 8.0;
    }
    if chunk
        .aliases
        .iter()
        .any(|a| crate::normalize::normalize(a).contains(q))
    {
        return 6.0;
    }
    let hits = body.matches(q).count() as f64;
    if hits <= 0.0 {
        return 0.0;
    }
    // 短条目里点名，压过整章背景里顺带提到一次
    let len = body.chars().count().max(1) as f64;
    4.0 * hits / (1.0 + len / 160.0)
}

fn like_scan(
    conn: &Connection,
    campaign_id: i64,
    needle: &str,
    ranks: &mut HashMap<i64, (f64, Vec<String>)>,
) {
    let like = format!("%{}%", escape_like(needle));
    let sql = "SELECT id FROM chunks
         WHERE campaign_id=?1 AND (
            title LIKE ?2 ESCAPE '\\'
            OR aliases_json LIKE ?2 ESCAPE '\\'
            OR body LIKE ?2 ESCAPE '\\'
         )
         LIMIT 50";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return;
    };
    let Ok(ids) = stmt
        .query_map(params![campaign_id, like], |r| r.get::<_, i64>(0))
        .and_then(|rows| rows.collect::<rusqlite::Result<Vec<i64>>>())
    else {
        return;
    };
    for (i, id) in ids.into_iter().enumerate() {
        add_rank(ranks, id, i, 1.2, "like");
    }
}

fn timed_out(started: Instant) -> bool {
    started.elapsed().as_millis() >= HARD_TIMEOUT_MS
}

fn has_long_hanzi_run(q: &str) -> bool {
    let mut n = 0usize;
    for ch in crate::normalize::normalize(q).chars() {
        if crate::normalize::is_hanzi(ch) {
            n += 1;
            if n > 6 {
                return true;
            }
        } else {
            n = 0;
        }
    }
    false
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

fn run_fts(
    conn: &Connection,
    campaign_id: i64,
    terms: &[String],
    limit: i64,
    ranks: &mut HashMap<i64, (f64, Vec<String>)>,
) {
    if terms.is_empty() {
        return;
    }
    let match_expr = terms.join(" OR ");
    match fts_ids(conn, campaign_id, &match_expr, limit) {
        Ok(ids) => {
            for (i, id) in ids.into_iter().enumerate() {
                add_rank(ranks, id, i, 1.0, "fts");
            }
        }
        Err(_) => {
            for t in terms {
                if let Ok(ids) = fts_ids(conn, campaign_id, t, 20) {
                    for (i, id) in ids.into_iter().enumerate() {
                        add_rank(ranks, id, i, 1.0, "fts");
                    }
                }
            }
        }
    }
}

fn fts_ids(conn: &Connection, campaign_id: i64, match_expr: &str, limit: i64) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT chunk_fts.rowid
         FROM chunk_fts
         JOIN chunks ON chunks.id = chunk_fts.rowid
         WHERE chunk_fts MATCH ?1 AND chunks.campaign_id = ?2
         ORDER BY bm25(chunk_fts)
         LIMIT ?3",
    )?;
    let ids = stmt
        .query_map(params![match_expr, campaign_id, limit], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<i64>>>()?;
    Ok(ids)
}

fn add_rank(
    ranks: &mut HashMap<i64, (f64, Vec<String>)>,
    id: i64,
    rank: usize,
    weight: f64,
    via: &str,
) {
    let add = weight / (RRF_K + rank as f64 + 1.0);
    let e = ranks.entry(id).or_insert((0.0, Vec::new()));
    e.0 += add;
    if !e.1.iter().any(|x| x == via) {
        e.1.push(via.to_string());
    }
}
