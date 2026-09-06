use crate::db::{load_chunk, load_synonyms};
use crate::normalize::{hanzi_count, now_rfc3339};
use crate::terms::extract_terms;
use crate::types::{Hit, SearchResult};
use anyhow::Result;
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::time::Instant;

pub struct SearchQuery<'a> {
    pub text: &'a str,
}

const RRF_K: f64 = 60.0;

pub fn search(conn: &Connection, campaign_id: i64, q: SearchQuery<'_>) -> Result<SearchResult> {
    let started = Instant::now();
    let mut terms = extract_terms(q.text);
    let syn = load_synonyms(conn, campaign_id);
    let mut extra = Vec::new();
    for t in &terms {
        let raw = t.trim_matches('"');
        if let Some(c) = syn.get(raw) {
            extra.push(crate::terms::escape_fts_term(c));
        }
        if let Some(c) = syn.get(t) {
            extra.push(crate::terms::escape_fts_term(c));
        }
    }
    terms.extend(extra);
    terms.sort();
    terms.dedup();

    let mut ranks: HashMap<i64, (f64, Vec<String>)> = HashMap::new();

    if !terms.is_empty() {
        let match_expr = terms.join(" OR ");
        let mut stmt =
            conn.prepare("SELECT rowid FROM chunk_fts WHERE chunk_fts MATCH ?1 LIMIT 50")?;
        let fts_ids: rusqlite::Result<Vec<i64>> = stmt
            .query_map([&match_expr], |r| r.get::<_, i64>(0))
            .and_then(|rows| rows.collect());
        match fts_ids {
            Ok(ids) => {
                for (i, id) in ids.into_iter().enumerate() {
                    add_rank(&mut ranks, id, i, 1.0, "fts");
                }
            }
            Err(_) => {
                for t in &terms {
                    let mut st = conn
                        .prepare("SELECT rowid FROM chunk_fts WHERE chunk_fts MATCH ?1 LIMIT 20")?;
                    let ids: rusqlite::Result<Vec<i64>> = st
                        .query_map([t], |r| r.get::<_, i64>(0))
                        .and_then(|rows| rows.collect());
                    if let Ok(ids) = ids {
                        for (i, id) in ids.into_iter().enumerate() {
                            add_rank(&mut ranks, id, i, 1.0, "fts");
                        }
                    }
                }
            }
        }
    }

    let qn = crate::normalize::normalize(q.text);
    if hanzi_count(&qn) <= 4 && !qn.is_empty() {
        let like = format!("%{qn}%");
        let mut stmt = conn.prepare(
            "SELECT id FROM chunks
             WHERE campaign_id=?1 AND (title LIKE ?2 OR aliases_json LIKE ?2)
             LIMIT 30",
        )?;
        let ids: rusqlite::Result<Vec<i64>> = stmt
            .query_map(params![campaign_id, like], |r| r.get::<_, i64>(0))
            .and_then(|rows| rows.collect());
        if let Ok(ids) = ids {
            for (i, id) in ids.into_iter().enumerate() {
                add_rank(&mut ranks, id, i, 0.7, "like");
            }
        }
    }

    let mut scored: Vec<(i64, f64, Vec<String>)> =
        ranks.into_iter().map(|(id, (s, v))| (id, s, v)).collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(20);

    let mut hits = Vec::new();
    for (id, score, via) in scored {
        if let Ok(chunk) = load_chunk(conn, id) {
            hits.push(Hit { chunk, score, via });
        }
    }

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

    Ok(SearchResult {
        hits,
        latency_ms,
        used_semantic: false,
        truncated: latency_ms > 4500,
        terms,
    })
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
