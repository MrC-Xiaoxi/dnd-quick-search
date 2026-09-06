use std::fs;
use tempfile::tempdir;
use table_canon_core::Store;

#[test]
fn two_char_query_hits_body_not_only_title() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("lore.md");
    fs::write(
        &src,
        "# 终燃城\n\n猎人工会驻扎在熔炉区。登记员每天统计伤亡。\n\n# 无关港口\n\n这里只有潮水和修船工。\n",
    )
    .unwrap();
    let mut store = Store::create(dir.path().join("t.tcs"), "t").unwrap();
    store.import_paths(&[src]).unwrap();

    let miss = store.search("工会").unwrap();
    assert!(
        miss.hits
            .iter()
            .any(|h| h.chunk.body.contains("猎人工会") || h.chunk.title.contains("终燃")),
        "搜「工会」应命中正文，实际 titles={:?}",
        miss.hits.iter().map(|h| &h.chunk.title).collect::<Vec<_>>()
    );

    let full = store.search("猎人工会").unwrap();
    assert!(!full.hits.is_empty(), "搜「猎人工会」不应为空");
    assert!(
        full.hits[0].chunk.body.contains("猎人工会")
            || full.hits[0].chunk.title.contains("终燃"),
        "第一条应是相关块，got title={} body={}",
        full.hits[0].chunk.title,
        full.hits[0].chunk.body
    );
    assert!(
        !full.hits[0].chunk.body.contains("<w:"),
        "结果不应含 Word XML"
    );
}
