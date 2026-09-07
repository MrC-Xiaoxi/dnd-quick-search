//! §7.1.1 ONNX 导出验收（golden 门禁）：
//! `testdata/embed_golden.json` 存放官方实现（FlagEmbedding / sentence-transformers，
//! CLS + L2，查询句加前缀）产出的参考向量；本测试对同一输入计算余弦并按量化档位设门槛。
//!
//! 文件格式：
//! {
//!   "quant": "int8",
//!   "threshold": 0.995,
//!   "cases": [
//!     {"text": "铁砧堡扼守山口", "vector": [0.01, ...], "as_query": false}
//!   ]
//! }
//!
//! 参考向量需 python 环境生成（见 docs/m2-语义检索说明.md 的待办）；文件缺失时跳过。

use std::fs;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn embedding_matches_reference_golden() {
    let path = repo_root().join("testdata/embed_golden.json");
    let Ok(raw) = fs::read_to_string(&path) else {
        eprintln!("跳过：无 testdata/embed_golden.json（参考向量待生成）");
        return;
    };
    let model_dir = repo_root().join("models/bge-small-zh-v1.5");
    let dll = repo_root().join("models/onnxruntime/onnxruntime.dll");
    if !model_dir.join("model.onnx").is_file() || !dll.is_file() {
        eprintln!("跳过：无模型文件");
        return;
    }
    let v: serde_json::Value = serde_json::from_str(&raw).expect("golden 格式错误");
    let threshold = v["threshold"].as_f64().unwrap_or(0.995);
    let cases = v["cases"].as_array().expect("cases 缺失").clone();
    assert!(cases.len() >= 10, "§7.1.1 要求 ≥10 句");

    let embedder = table_canon_core::Embedder::load(&model_dir, Some(&dll)).expect("加载模型");

    let mut worst = 1.0f64;
    for case in &cases {
        let text = case["text"].as_str().expect("text");
        let as_query = case["as_query"].as_bool().unwrap_or(false);
        let expected: Vec<f32> = case["vector"]
            .as_array()
            .expect("vector")
            .iter()
            .map(|x| x.as_f64().unwrap() as f32)
            .collect();
        let got = if as_query {
            embedder.encode_query(text)
        } else {
            embedder.encode_doc(text)
        }
        .expect("编码失败");
        let denom = (expected.len().max(got.len())) as f64;
        let dot: f64 = expected
            .iter()
            .zip(got.iter())
            .map(|(a, b)| (*a as f64) * (*b as f64))
            .sum();
        let cosine = dot; // 两侧均已 L2 归一
        let _ = denom;
        worst = worst.min(cosine);
        assert!(
            cosine >= threshold,
            "「{text}」余弦 {cosine:.6} < {threshold}（pooling/前缀/归一口径可能错了）"
        );
    }
    eprintln!("golden 最差余弦 {worst:.6} ≥ {threshold}");
}
