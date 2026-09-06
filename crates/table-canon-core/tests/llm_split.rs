use table_canon_core::types::{Block, ExtractedEntry};
use table_canon_core::{parse_entries_json, EntrySplitter};

struct MockSplitter;
impl EntrySplitter for MockSplitter {
    fn split_chapter(&self, _t: &str, _b: &str) -> anyhow::Result<Vec<ExtractedEntry>> {
        Ok(vec![
            ExtractedEntry {
                title: "猎人工会".into(),
                aliases: vec!["猎人公会".into()],
                entity_type: "faction".into(),
                body: "驻扎熔炉区。".into(),
                heading_level: 2,
                anchor: String::new(),
            },
            ExtractedEntry {
                title: "登记员".into(),
                aliases: vec![],
                entity_type: "npc".into(),
                body: "每天统计伤亡。".into(),
                heading_level: 2,
                anchor: String::new(),
            },
        ])
    }
}

#[test]
fn refine_turns_chapter_into_entries() {
    let body = format!(
        "猎人工会驻扎在熔炉区。登记员每天统计伤亡。{}",
        "工会的历史悠久，成员遍布城邦各处，负责协调猎魔、悬赏与救援。".repeat(30)
    );
    let blocks = vec![Block {
        heading_level: 1,
        title: "第0章 背景".into(),
        text: body,
    }];
    let (drafts, notes) = table_canon_core::split::refine_drafts(&blocks, "模组", Some(&MockSplitter));
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(drafts.len(), 2);
    assert_eq!(drafts[0].title, "猎人工会");
    assert_eq!(drafts[0].entity_type, "faction");
    assert!(drafts[0].aliases.contains(&"猎人公会".to_string()));
}

#[test]
fn parse_fenced_ok() {
    let v = parse_entries_json(
        r#"```json
[{"title":"格里姆","entity_type":"npc","body":"酒保。"}]
```"#,
    )
    .unwrap();
    assert_eq!(v[0].title, "格里姆");
}
