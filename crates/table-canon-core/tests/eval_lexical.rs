use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use table_canon_core::Store;

fn testdata() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/sample-campaign")
}

fn eval_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/eval.jsonl")
}

fn term_hits(terms: &[String], must: &[String]) -> bool {
    if must.is_empty() {
        return true;
    }
    must.iter().any(|m| {
        terms.iter().any(|t| {
            let raw = t.trim_matches('"');
            raw == m || raw.contains(m)
        })
    })
}

#[test]
fn lexical_eval_recall_at_10() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::create(dir.path().join("eval.tcs"), "eval").unwrap();
    store.import_paths(&[testdata()]).unwrap();

    let text = fs::read_to_string(eval_path()).unwrap();
    let mut n = 0usize;
    let mut miss = Vec::new();
    let mut extract_fail = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let v: Value = serde_json::from_str(line).unwrap();
        if v["kind"].as_str() != Some("lexical") {
            continue;
        }
        n += 1;
        let q = v["query"].as_str().unwrap();
        let r = store.search(q).unwrap();
        let must: Vec<String> = v["must_extract_any"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .filter_map(|x| x.as_str().map(|s| s.to_string()))
            .collect();
        if !term_hits(&r.terms, &must) {
            extract_fail.push(format!("{q} terms={:?}", r.terms));
        }
        let relevant: Vec<String> = v["relevant_chunk_ids"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|x| x.as_str().map(|s| s.to_string()))
            .collect();
        let hit = r.hits.iter().take(10).any(|h| {
            relevant
                .iter()
                .any(|t| h.chunk.title == *t || h.chunk.title.contains(t))
        });
        if !hit {
            miss.push(format!(
                "{q} -> {:?}",
                r.hits.iter().map(|h| &h.chunk.title).collect::<Vec<_>>()
            ));
        }
    }
    assert!(n >= 30, "need >=30 lexical items, got {n}");
    let recall = (n - miss.len()) as f64 / n as f64;
    assert!(
        extract_fail.is_empty(),
        "must_extract_any failed:\n{}",
        extract_fail.join("\n")
    );
    assert!(
        recall >= 0.85,
        "lexical Recall@10={recall:.3} ({}/{}), miss:\n{}",
        n - miss.len(),
        n,
        miss.join("\n")
    );
}
