//! 分条质量回归测试：验证「框架约束兜住模型下限」。
//!
//! - GoodModel：标准段号制输出 → 应得到干净的原文切片条目。
//! - SloppyModel：段号正确但 title/anchor 全是模型瞎编 → 段号路径必须救回来。
//! - HopelessModel：段号无效、anchor 贴不回 → 必须回退启发式切块并透出警告。

use anyhow::Result;
use table_canon_core::split::{refine_drafts, segment_text};
use table_canon_core::types::{Block, ExtractedEntry};
use table_canon_core::EntrySplitter;

/// 5 段、每段约 170 字的章节，总长超过 800 字以进入 LLM 路径。
fn fixture_chapter() -> String {
    let base = [
        "铁砧堡扼守灰鹰山口的北麓，城墙由黑曜岩砌成，塔楼上的守军昼夜瞭望。三百驻军轮班戍卫，商队必须在日出之后才能通关，入夜后山道封闭，违者以走私论处。",
        "城主荀岚出身铁卫团，兼管税收与熔炉区治安。商队背后叫她铁面城主，说她算账比税吏还快三分，处事却比律法多留一寸情面。",
        "登记员格里姆在南门塔楼办公，负责记录每日伤亡与悬赏发放。他的册子从不错漏，猎人们说宁可被巨魔追三条街，也不愿被格里姆记上一笔。",
        "熔炉区商会每旬开一次集会，现任会长是矮人巴尔多。商会垄断山口的过路税代理，会员缴纳会费后可在北区摆摊，违者货物充公。",
        "山口下方的旧驿道通往废弃的哨站，据说埋着前朝的军械。冒险者公会多次悬赏探路，回来的队伍却都说不清哨站里到底有什么。",
    ];
    base.iter()
        .map(|p| std::iter::repeat(*p).take(3).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn chapter_block() -> Block {
    Block {
        heading_level: 1,
        title: "第三章 灰鹰山口".into(),
        text: fixture_chapter(),
    }
}

fn entry(title: &str, start: i64, end: i64, anchor: &str) -> ExtractedEntry {
    ExtractedEntry {
        title: title.into(),
        aliases: Vec::new(),
        entity_type: String::new(),
        body: String::new(),
        anchor: anchor.into(),
        start,
        end,
        heading_level: 2,
    }
}

/// 标准好模型：按段号精确提议，条目衔接覆盖全部正文。
struct GoodModel;
impl EntrySplitter for GoodModel {
    fn split_chapter(&self, _t: &str, body: &str) -> Result<Vec<ExtractedEntry>> {
        Ok(vec![
            entry("铁砧堡", 0, 0, ""),
            entry("荀岚", 1, 1, ""),
            entry("格里姆", 2, 2, ""),
            entry("巴尔多商会", 3, 4, ""),
        ])
    }
}

/// 手滑模型：title 是原文没有的叫法、anchor 纯属编造，但段号是对的。
/// 真实场景：弱模型复写原文必错，框架的段号路径必须兜住。
struct SloppyModel;
impl EntrySplitter for SloppyModel {
    fn split_chapter(&self, _t: &str, _body: &str) -> Result<Vec<ExtractedEntry>> {
        Ok(vec![
            entry("灰鹰要塞", 0, 0, "这座堡垒的名字原文里没有"),
            entry("铁面城主", 1, 1, "完全编造的锚点文本"),
            entry("南门登记官", 2, 2, "又一句编造的锚点文本"),
            entry("商会", 3, 4, "还是编造的锚点文本"),
        ])
    }
}

/// 烂模型：段号越界、anchor 贴不回、title 也是原文没有的词——什么都定位不了。
struct HopelessModel;
impl EntrySplitter for HopelessModel {
    fn split_chapter(&self, _t: &str, _body: &str) -> Result<Vec<ExtractedEntry>> {
        Ok(vec![
            entry("北岭哨塔", 99, 120, "这段文字在原文里不存在"),
            entry("税务官", -1, -1, "另一段不存在的文字"),
        ])
    }
}

#[test]
fn good_model_yields_clean_source_sliced_entries() {
    let block = chapter_block();
    let (drafts, notes) = refine_drafts(std::slice::from_ref(&block), "模组", Some(&GoodModel));
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(drafts.len(), 4, "{:?}", drafts.iter().map(|d| &d.title).collect::<Vec<_>>());
    let titles: Vec<&str> = drafts.iter().map(|d| d.title.as_str()).collect();
    assert_eq!(titles, vec!["铁砧堡", "荀岚", "格里姆", "巴尔多商会"]);
    // body 一律是原文切片
    assert!(drafts[0].body.starts_with("铁砧堡扼守灰鹰山口"));
    assert!(drafts[1].body.starts_with("城主荀岚出身铁卫团"));
    assert!(drafts[3].body.contains("巴尔多"));
    // 条目衔接：正文没有被丢掉
    let total: usize = drafts.iter().map(|d| d.body.chars().count()).sum();
    assert!(total * 100 / block.text.chars().count() >= 95);
}

#[test]
fn sloppy_model_saved_by_segment_indices() {
    let block = chapter_block();
    let (drafts, notes) = refine_drafts(std::slice::from_ref(&block), "模组", Some(&SloppyModel));
    assert!(notes.is_empty(), "段号有效时不应回退: {notes:?}");
    assert_eq!(drafts.len(), 4);
    // body 仍取原文切片，而不是模型编的 title/anchor
    assert_eq!(drafts[0].title, "灰鹰要塞");
    assert!(drafts[0].body.starts_with("铁砧堡扼守灰鹰山口"));
    assert!(drafts[1].body.starts_with("城主荀岚出身铁卫团"));
    assert!(!drafts[2].body.contains("编造"));
}

#[test]
fn hopeless_model_falls_back_to_heuristic_with_warning() {
    let block = chapter_block();
    let (drafts, notes) = refine_drafts(std::slice::from_ref(&block), "模组", Some(&HopelessModel));
    assert!(
        notes.iter().any(|n| n.contains("已用规则切开")),
        "应透出回退警告: {notes:?}"
    );
    // 兜底切块保证至少有内容可检索
    assert!(!drafts.is_empty());
    assert!(drafts[0].body.contains("铁砧堡"));
}

#[test]
fn segment_numbering_is_stable_between_splitter_and_materializer() {
    // 同一段号系统：GoodModel 提议的边界正好落在段落起点
    let body = fixture_chapter();
    let segs = segment_text(&body);
    assert_eq!(segs.len(), 5, "每段一行，共 5 段");
    assert_eq!(segs[0].text(&body).starts_with("铁砧堡"), true);
    assert_eq!(segs[1].text(&body).starts_with("城主荀岚"), true);
    assert_eq!(segs[4].text(&body).starts_with("山口下方"), true);
}
