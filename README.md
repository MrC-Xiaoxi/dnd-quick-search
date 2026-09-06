# 席间索（Table Canon）M1 Demo

开席时 DM 对着玩家原话，5 秒内从模组 + 私设里翻出完整条目并复制出去。

本仓库是方案文档的 **M1 可运行 demo**（词法检索闭环），不是完整产品。语义检索、Tauri 外壳、全局热键按 `docs/席间索_技术实现方案.md` 后续替换；**检索引擎 API 已按文档切开**，外壳可换。

## 本版做了什么

- `crates/table-canon-core`：SQLite FTS5 trigram + BM25、长口语抽词（A 层 ≥3 字）、拼音（空格全拼 / 连写 / 首字母，含多音字）、简繁静态映射、别名/同义词、`chunk_corrections` 回套、相对路径 + hash 重命名、先删后插、`export_snapshot` 强制 `journal_mode=DELETE`、`open_portable` 不切 WAL、三种复制模板
- `apps/desktop`：Windows 置顶窗口。点「试用样例」即可检索/复制；自己的模组再新建库导入
- 样例战役：`testdata/sample-campaign`
- lexical 门禁：`testdata/eval.jsonl`（≥30 条，Recall@10）

明确未做（方案里标 M2/M3 或本 demo 砍掉的）：语义 embedding、全局热键、PDF 文本层、文件夹监视、Tauri。

## 环境

- Windows 10/11 x64
- Rust stable（MSVC）
- 中文显示依赖系统字体 `msyh.ttc`

## 运行

在仓库根目录双击 `run-demo.bat`，或：

```bat
cargo run -p table-canon-desktop --release
```

打开后：

1. 点 **试用样例**（自动建库并导入断桅港）
2. 回车检索预填的那句玩家口语
3. 点 **复制公开**，或再按 Enter（密谋段应被剥掉）
4. 方向键可换条。自己的模组用「新建库 / 导入文件夹 / 导入 Word」

也可搜 `gelimu`、`老格`、`斷桅酒館`、`玛拉`。

可导入 `.docx`（标题样式 + 段落 + 表格）。旧版 `.doc` 请在 Word 里另存为 `.docx`。试用样例里的人物卡就是 Word。

## 库文件

- 运行中可能是 `.tcs` + WAL
- 传到另一台机器只用 **导出便携库**，不要手拷正在打开的 `.tcs`

## 许可

MIT。样例文本为原创微型设定，仅供演示。
