//! M2 语义检索端到端：模型文件存在才运行，缺失则打印跳过（CI/无模型环境不算失败）。
//! 下载模型：bash scripts/fetch-model.sh

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use table_canon_core::embed::Embedder;
use table_canon_core::{ImportOpts, Store};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn model_paths() -> Option<(PathBuf, PathBuf)> {
    let root = repo_root();
    let model_dir = root.join("models/bge-small-zh-v1.5");
    let dll = root.join("models/onnxruntime/onnxruntime.dll");
    let ok = model_dir.join("model.onnx").is_file()
        && model_dir.join("tokenizer.json").is_file()
        && dll.is_file();
    ok.then_some((model_dir, dll))
}

#[test]
fn semantic_end_to_end_when_model_present() -> Result<()> {
    let Some((model_dir, dll)) = model_paths() else {
        eprintln!("跳过：models/ 下没有模型或 onnxruntime.dll（bash scripts/fetch-model.sh 可下载）");
        return Ok(());
    };
    let embedder = Arc::new(Embedder::load(&model_dir, Some(&dll))?);

    let dir = tempfile::tempdir()?;
    let mut store = Store::create(dir.path().join("sem.tcs"), "语义测试战役")?;

    let opts = ImportOpts {
        reprocess: true,
        embedder: Some(embedder.as_ref()),
        ..Default::default()
    };
    let sample = repo_root().join("testdata/sample-campaign");
    let report = store.import_with(&[sample], opts)?;
    assert_eq!(report.files_ok, 3);
    assert!(report.warnings.is_empty(), "不应有拆条/向量警告: {:?}", report.warnings);

    let info = store.info()?;
    assert!(info.semantic_ready, "导入后应有向量、semantic_ready=true");

    // 口语查询：无共享词（正文里没有「小酌」「一杯」这类原文措辞），语义路应命中断桅酒馆
    let r = store.search_with("想找个地方小酌一杯，听说有家酒馆招牌挺特别", Some(embedder.as_ref()))?;
    let titles: Vec<String> = r.hits.iter().map(|h| h.chunk.title.clone()).collect();
    eprintln!("语义 top5: {:?}", &titles[..titles.len().min(5)]);
    assert!(r.used_semantic, "应当走了语义路");
    assert!(
        titles.first().is_some_and(|t| t.contains("酒馆")),
        "断桅酒馆应为语义第 1 名，实际: {titles:?}"
    );

    // 降级路径：不带编码器 → 纯词法，照常工作
    let r2 = store.search("独眼酒保")?;
    assert!(!r2.used_semantic);
    assert!(!r2.hits.is_empty(), "词法路仍应命中");

    // 重复补算应无事可做：向量已齐备时幂等，不产生重复行
    let (n, notes) = store.backfill_embeddings(embedder.as_ref(), None)?;
    assert_eq!(n, 0, "向量已齐备，补算不应再算任何条目");
    assert!(notes.is_empty(), "补算不应有告警: {notes:?}");
    assert!(store.info()?.semantic_ready);

    Ok(())
}

/// 「同库打开后补算」路径：先无模型导入（只有词法），模型就位后再补算向量。
#[test]
fn backfill_after_import_without_model_restores_semantic() -> Result<()> {
    let Some((model_dir, dll)) = model_paths() else {
        eprintln!("跳过：models/ 下没有模型或 onnxruntime.dll（bash scripts/fetch-model.sh 可下载）");
        return Ok(());
    };
    let embedder = Arc::new(Embedder::load(&model_dir, Some(&dll))?);

    let dir = tempfile::tempdir()?;
    let mut store = Store::create(dir.path().join("backfill.tcs"), "补算测试")?;

    let sample = repo_root().join("testdata/sample-campaign");
    let report = store.import_with(&[sample], ImportOpts::default())?;
    assert_eq!(report.files_ok, 3);
    assert!(!store.info()?.semantic_ready, "无编码器导入不应有向量");

    let (n, notes) = store.backfill_embeddings(embedder.as_ref(), None)?;
    assert!(n > 0, "应补算到条目，实际 {n}");
    assert!(notes.is_empty(), "补算不应有告警: {notes:?}");
    assert!(store.info()?.semantic_ready, "补算后 semantic_ready 应恢复");

    let r = store.search_with(
        "想找个地方小酌一杯，听说有家酒馆招牌挺特别",
        Some(embedder.as_ref()),
    )?;
    assert!(r.used_semantic, "补算后应能走语义路");
    let titles: Vec<String> = r.hits.iter().map(|h| h.chunk.title.clone()).collect();
    assert!(
        titles.first().is_some_and(|t| t.contains("酒馆")),
        "补算后语义第 1 名应为断桅酒馆，实际: {titles:?}"
    );
    Ok(())
}

#[test]
fn degradation_without_embedder_keeps_lexical() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::create(dir.path().join("lex.tcs"), "降级测试")?;
    let sample = repo_root().join("testdata/sample-campaign");
    let report = store.import_with(&[sample], ImportOpts::default())?;
    assert_eq!(report.files_ok, 3);
    let r = store.search("独眼酒保")?;
    assert!(!r.used_semantic);
    assert!(!r.hits.is_empty());
    Ok(())
}
