use std::fs;
use std::path::PathBuf;
use table_canon_core::{MetaPatch, Store, VIS_SECRET};

fn testdata() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/sample-campaign")
}

#[test]
fn import_search_copy_reimport_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let campaign_copy = dir.path().join("campaign");
    copy_dir(&testdata(), &campaign_copy);

    let store_path = dir.path().join("demo.tcs");
    let mut store = Store::create(&store_path, "断桅港战役").unwrap();

    let report = store.import_paths(&[campaign_copy.clone()]).unwrap();
    assert!(report.files_ok >= 1, "{report:?}");
    assert!(report.chunks >= 1);

    store.upsert_synonym("独眼酒保", "格里姆").unwrap();

    let r = store
        .search("我们之前在那个独眼酒保的店里拿到了货")
        .unwrap();
    assert!(!r.terms.is_empty(), "抽词为空");
    assert!(
        r.hits
            .iter()
            .any(|h| h.chunk.title.contains("格里姆") || h.chunk.body.contains("格里姆")),
        "hits={:?}",
        r.hits.iter().map(|h| &h.chunk.title).collect::<Vec<_>>()
    );
    let grim = r
        .hits
        .iter()
        .find(|h| h.chunk.title.contains("格里姆") || h.chunk.body.contains("格里姆"))
        .unwrap()
        .chunk
        .id;

    store
        .update_chunk_meta(
            grim,
            MetaPatch {
                visibility: Some(VIS_SECRET | 1),
                aliases: Some(vec!["独眼酒保".into(), "老格".into()]),
                entity_type: Some("npc".into()),
            },
        )
        .unwrap();

    let player = store.copy_payload(grim, "player").unwrap();
    assert!(!player.contains("走私"), "公开复制不应含密谋: {player}");

    let harbor = campaign_copy.join("02-港口.md");
    let orig = fs::read_to_string(&harbor).unwrap();
    fs::write(
        &harbor,
        orig.replace("码头上总有湿咸的风。", "码头上总有湿咸的风（错字测试）。"),
    )
    .unwrap();
    let report2 = store.import_paths(&[campaign_copy.clone()]).unwrap();
    assert!(report2.files_ok + report2.files_skip >= 1);

    let r2 = store.search("独眼酒保").unwrap();
    let grim2 = r2
        .hits
        .iter()
        .find(|h| h.chunk.title.contains("格里姆") || h.chunk.body.contains("格里姆"))
        .expect("重导入后应仍能搜到格里姆");
    assert!(
        grim2.chunk.visibility & VIS_SECRET != 0,
        "重导入丢了 SECRET vis={}",
        grim2.chunk.visibility
    );
    assert!(
        grim2.chunk.aliases.iter().any(|a| a.contains("独眼")),
        "重导入丢了别名 {:?}",
        grim2.chunk.aliases
    );

    let snap = dir.path().join("portable.tcs");
    let (_bytes, mode) = store.export_snapshot(&snap).unwrap();
    assert_eq!(mode.to_ascii_lowercase(), "delete");

    let opened = Store::open(&snap).unwrap();
    let r3 = opened.search("格里姆").unwrap();
    assert!(!r3.hits.is_empty());

    let detail = opened.get_chunk(r3.hits[0].chunk.id).unwrap();
    let _ = (detail.prev_id, detail.next_id);

    store.close().unwrap();
}

fn copy_dir(src: &std::path::Path, dst: &std::path::Path) {
    fs::create_dir_all(dst).unwrap();
    for e in fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        let t = dst.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &t);
        } else {
            fs::copy(e.path(), t).unwrap();
        }
    }
}
