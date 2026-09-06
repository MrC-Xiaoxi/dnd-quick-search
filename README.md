# 席间索（Table Canon）M1 Demo

开席时 DM 对着玩家原话，5 秒内从模组 + 私设里翻出完整条目并复制出去。

本仓库是方案文档的 **M1 可运行 demo**（词法检索闭环），不是完整产品。语义检索、Tauri 外壳、全局热键按 `docs/席间索_技术实现方案.md` 后续替换；**检索引擎 API 已按文档切开**，外壳可换。

## 本版做了什么

- `crates/table-canon-core`：SQLite FTS5 trigram、长口语抽词、拼音（含多音字）、别名/同义词、`chunk_corrections` 回套、先删后插避开 ordinal 撞键、`export_snapshot` 强制 `journal_mode=DELETE`、三种复制模板
- `apps/desktop`：Windows 置顶窗口（egui）。导入文件夹 → 粘贴玩家的话 → 检索 → 一键复制
- 样例战役：`testdata/sample-campaign`

明确未做（方案里标 M2/M3 或本 demo 砍掉的）：语义 embedding、全局热键、PDF 文本层、文件夹监视、Tauri。

## 环境

- Windows 10/11 x64
- Rust stable（MSVC）
- 中文显示依赖系统字体 `msyh.ttc`

## 运行

在仓库根目录：

```bat
cargo test -p table-canon-core
cargo run -p table-canon-desktop --release
```

桌面程序：

1. **新建库** → 选路径保存 `campaign.tcs`
2. **导入文件夹** → 选 `testdata/sample-campaign`
3. 搜索框粘贴：`我们之前在那个独眼酒保的店里拿到了货`
4. 点 **复制公开**（密谋段应被剥掉）

## 库文件

- 运行中可能是 `.tcs` + WAL
- 传到另一台机器只用 **导出便携库**，不要手拷正在打开的 `.tcs`

## 许可

MIT。样例文本为原创微型设定，仅供演示。
