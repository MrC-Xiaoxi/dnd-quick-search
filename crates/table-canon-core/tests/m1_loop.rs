use std::fs;
use std::path::{Path, PathBuf};
use table_canon_core::{MetaPatch, Store, VIS_SECRET};

fn testdata() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/sample-campaign")
}

fn copy_dir(src: &Path, dst: &Path) {
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

#[test]
fn import_search_copy_reimport_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let campaign_copy = dir.path().join("campaign");
    copy_dir(&testdata(), &campaign_copy);

    let store_path = dir.path().join("demo.tcs");
    let mut store = Store::create(&store_path, "断桅港战役").unwrap();

    let report = store.import_paths(std::slice::from_ref(&campaign_copy)).unwrap();
    assert!(report.files_ok >= 1, "{report:?}");
    assert!(report.chunks >= 1);

    let paths = store.source_paths().unwrap();
    assert!(
        paths.iter().any(|p| p == "02-港口.md" || p.ends_with("/02-港口.md") || p.ends_with("02-港口.md")),
        "expected campaign-relative path, got {paths:?}"
    );
    assert!(
        paths.iter().all(|p| !p.contains(":\\") && !p.starts_with('/')),
        "paths should be relative, got {paths:?}"
    );

    let mara = store.search("玛拉").unwrap();
    assert!(
        mara.hits.iter().any(|h| h.chunk.title.contains("玛拉")),
        "sample docx should index 玛拉, titles={:?}",
        mara.hits.iter().map(|h| &h.chunk.title).collect::<Vec<_>>()
    );

    let r0 = store
        .search("我们之前在那个独眼酒保的店里拿到了货")
        .unwrap();
    assert!(
        r0.hits
            .iter()
            .any(|h| h.chunk.title.contains("格里姆") || h.chunk.body.contains("格里姆")),
        "lexical hit without synonym, terms={:?} titles={:?}",
        r0.terms,
        r0.hits.iter().map(|h| &h.chunk.title).collect::<Vec<_>>()
    );

    store.upsert_synonym("独眼酒保", "格里姆").unwrap();

    let r = store
        .search("我们之前在那个独眼酒保的店里拿到了货")
        .unwrap();
    assert!(!r.terms.is_empty(), "抽词为空");
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
    let report2 = store.import_paths(std::slice::from_ref(&campaign_copy)).unwrap();
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

    let moved = dir.path().join("campaign-moved");
    copy_dir(&campaign_copy, &moved);
    let n = store.info().unwrap().chunk_count;
    let skip_report = store.import_paths(&[moved]).unwrap();
    assert!(skip_report.files_skip >= 1, "{skip_report:?}");
    assert_eq!(store.info().unwrap().chunk_count, n, "moved folder must not duplicate chunks");

    let snap = dir.path().join("portable.tcs");
    let (_bytes, mode) = store.export_snapshot(&snap).unwrap();
    assert_eq!(mode.to_ascii_lowercase(), "delete");

    let opened = Store::open_portable(&snap).unwrap();
    let r3 = opened.search("格里姆").unwrap();
    assert!(!r3.hits.is_empty());
    let detail = opened.get_chunk(r3.hits[0].chunk.id).unwrap();
    let _ = (detail.prev_id, detail.next_id);
    assert_eq!(opened.journal_mode().unwrap().to_ascii_lowercase(), "delete");

    store.close().unwrap();
}

#[test]
fn hash_rename_does_not_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let campaign_copy = dir.path().join("campaign");
    copy_dir(&testdata(), &campaign_copy);
    let store_path = dir.path().join("demo.tcs");
    let mut store = Store::create(&store_path, "断桅港战役").unwrap();
    store.import_paths(std::slice::from_ref(&campaign_copy)).unwrap();
    let n = store.info().unwrap().chunk_count;
    let old = campaign_copy.join("02-港口.md");
    let new = campaign_copy.join("港口-renamed.md");
    fs::rename(&old, &new).unwrap();
    let report = store.import_paths(std::slice::from_ref(&campaign_copy)).unwrap();
    assert_eq!(store.info().unwrap().chunk_count, n, "rename by hash duplicated: {report:?}");
    let paths = store.source_paths().unwrap();
    assert!(
        paths.iter().any(|p| p.ends_with("港口-renamed.md")),
        "{paths:?}"
    );
}

#[test]
fn public_body_strips_markup_without_vis_bits() {
    let body = "公开介绍\n【秘密】走私线\n【DM】后厨门\n仍公开";
    let out = table_canon_core::db::public_body(body, table_canon_core::VIS_PUBLIC);
    assert!(!out.contains("走私"), "{out}");
    assert!(!out.contains("后厨门"), "{out}");
    assert!(out.contains("公开介绍"), "{out}");
}

#[test]
fn two_char_alias_like() {
    let dir = tempfile::tempdir().unwrap();
    let campaign_copy = dir.path().join("campaign");
    copy_dir(&testdata(), &campaign_copy);
    let mut store = Store::create(dir.path().join("demo.tcs"), "x").unwrap();
    store.import_paths(&[campaign_copy]).unwrap();
    let r = store.search("老格").unwrap();
    assert!(
        r.hits.iter().any(|h| h.chunk.title.contains("格里姆")),
        "titles={:?} via={:?}",
        r.hits.iter().map(|h| (&h.chunk.title, &h.via)).collect::<Vec<_>>(),
        r.terms
    );
}
