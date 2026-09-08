// 发布版隐藏 Windows 控制台窗口；debug 保留方便看日志
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use eframe::egui::{
    self, text::LayoutJob, Color32, FontData, FontDefinitions, FontFamily, FontId, RichText,
    TextFormat, ViewportBuilder,
};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;
use table_canon_core::{
    Hit, ImportOpts, LlmConfig, LlmSplitter, MetaPatch, SearchResult, Store, StoreInfo, VIS_PUBLIC,
    VIS_SECRET,
};
use table_canon_core::embed::{default_dll_path, default_model_dir, Embedder};

/// M2 语义检索状态（§7.3 预热与降级）。
#[derive(Clone)]
enum SemanticState {
    /// 机器上没有模型文件，语义天然关闭。
    Idle,
    Loading,
    Ready(Arc<Embedder>),
    Failed(String),
}

const SAMPLE_QUERY: &str = "我们之前在那个独眼酒保的店里拿到了货";

fn main() -> eframe::Result<()> {
    let viewport = ViewportBuilder::default()
        .with_inner_size([960.0, 680.0])
        .with_min_inner_size([720.0, 480.0])
        .with_always_on_top()
        .with_title("席间索 · demo");
    let opts = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "席间索",
        opts,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

enum Pending {
    Sample,
    Import(Vec<PathBuf>),
}

struct ImportJob {
    progress: Arc<Mutex<String>>,
    rx: mpsc::Receiver<ImportOutcome>,
}

struct ImportOutcome {
    store: Store,
    result: Result<table_canon_core::ImportReport, String>,
    /// 导入后补算的摘要（条数 / 警告），非空则并进状态栏，不再静默丢弃。
    backfill: Option<String>,
}

struct BackfillJob {
    progress: Arc<Mutex<String>>,
    rx: mpsc::Receiver<BackfillOutcome>,
}

struct BackfillOutcome {
    store: Store,
    result: Result<(usize, Vec<String>), String>,
}

struct App {
    store: Option<Store>,
    info: Option<StoreInfo>,
    query: String,
    last: Option<SearchResult>,
    status: String,
    toast: String,
    always_on_top: bool,
    selected: Option<i64>,
    pending: Option<Pending>,
    focus_search: bool,
    search_focused: bool,
    expanded: HashSet<i64>,
    llm_enabled: bool,
    import_job: Option<ImportJob>,
    backfill_job: Option<BackfillJob>,
    reading_id: Option<i64>,
    semantic: Arc<Mutex<SemanticState>>,
    /// 预热完成只触发一次自动补算，避免每帧重复起线程。
    semantic_ready_seen: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_cjk_fonts(&cc.egui_ctx);
        let mut style = (*cc.egui_ctx.style()).clone();
        style.visuals.dark_mode = true;
        cc.egui_ctx.set_style(style);
        let mut app = Self {
            store: None,
            info: None,
            query: SAMPLE_QUERY.to_string(),
            last: None,
            status: "点「试用样例」可一键导入断桅港，然后回车检索。".into(),
            toast: String::new(),
            always_on_top: true,
            selected: None,
            pending: None,
            focus_search: true,
            search_focused: false,
            expanded: HashSet::new(),
            llm_enabled: load_app_config().llm.enabled,
            import_job: None,
            backfill_job: None,
            reading_id: None,
            semantic: Arc::new(Mutex::new(SemanticState::Loading)),
            semantic_ready_seen: false,
        };
        app.start_semantic_prewarm();
        if let Some(p) = load_last_store_path() {
            match Store::open(&p) {
                Ok(s) => {
                    app.store = Some(s);
                    app.refresh_info();
                    app.status.push_str(" 已打开上次的库。");
                }
                Err(e) => app.status = format!("上次的库打不开（{}）。请点「试用样例」或「打开库」。", e),
            }
        }
        app
    }

    fn swap_store(&mut self, next: Store) {
        if let Some(old) = self.store.take() {
            let _ = old.close();
        }
        remember_store(&next.path);
        self.store = Some(next);
        self.last = None;
        self.selected = None;
        self.reading_id = None;
        self.refresh_info();
        // 打开的可能是别的模型算过的库，或压根没向量：模型已就绪就补算
        self.start_backfill();
    }

    fn refresh_info(&mut self) {
        if let Some(s) = &self.store {
            match s.info() {
                Ok(i) => {
                    self.status = format!("「{}」· {} 条 · 回车检索 · Enter 复制公开", i.campaign_name, i.chunk_count);
                    self.info = Some(i);
                }
                Err(e) => self.status = format!("读取库失败: {e}"),
            }
        }
    }

    /// §7.3 预热：启动即后台加载 ONNX，不挡主窗；失败永久降级到关键词。
    fn start_semantic_prewarm(&mut self) {
        let state = self.semantic.clone();
        thread::spawn(move || {
            // ort 的 load-dynamic 在 DLL 缺失/损坏时是 panic 而不是返回 Err（内部 unwrap）。
            // 不兜住的话线程直接死掉，底栏会永远停在「准备中」，而 §7.3 要求显示不可用。
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let model_dir = default_model_dir()?;
                let dll = default_dll_path();
                Some(Embedder::load(&model_dir, dll.as_deref()))
            }));
            let next = match outcome {
                Ok(None) => SemanticState::Idle,
                Ok(Some(Ok(e))) => SemanticState::Ready(Arc::new(e)),
                Ok(Some(Err(err))) => SemanticState::Failed(format!(
                    "语义模型加载失败，已降级关键词检索：{err}"
                )),
                Err(_) => SemanticState::Failed(
                    "语义模型加载异常（onnxruntime.dll 缺失或损坏？），已降级关键词检索".into(),
                ),
            };
            if let Ok(mut g) = state.lock() {
                *g = next;
            }
        });
    }

    fn semantic_state(&self) -> SemanticState {
        self.semantic
            .lock()
            .map(|g| g.clone())
            .unwrap_or(SemanticState::Idle)
    }

    fn semantic_ready_embedder(&self) -> Option<Arc<Embedder>> {
        match self.semantic_state() {
            SemanticState::Ready(e) => Some(e),
            _ => None,
        }
    }

    /// 模型就绪后给缺向量的条目补算（§7.2）。没库 / 没模型 / 已有任务时不做事。
    /// 补算期间库被移进后台线程，检索与导入暂时让路（与导入任务同一套取舍）。
    fn start_backfill(&mut self) {
        if self.backfill_job.is_some() || self.import_job.is_some() {
            return;
        }
        let Some(embedder) = self.semantic_ready_embedder() else {
            return;
        };
        let Some(store) = self.store.take() else {
            return;
        };
        let progress = Arc::new(Mutex::new("检查/补算语义向量…".to_string()));
        let (tx, rx) = mpsc::channel();
        let p = progress.clone();
        thread::spawn(move || {
            let result = store
                .backfill_embeddings(embedder.as_ref(), Some(&p))
                .map_err(|e| e.to_string());
            let _ = tx.send(BackfillOutcome { store, result });
        });
        self.backfill_job = Some(BackfillJob { progress, rx });
    }

    fn poll_backfill_job(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.backfill_job else {
            return;
        };
        if let Ok(msg) = job.progress.lock() {
            if !msg.is_empty() && *msg != self.status {
                self.status = msg.clone();
            }
        }
        match job.rx.try_recv() {
            Ok(out) => {
                self.backfill_job = None;
                self.store = Some(out.store);
                // 先刷新基础状态，再压上补算结论（refresh_info 会覆盖 status）
                self.refresh_info();
                match out.result {
                    Ok((n, notes)) => {
                        let mut s = if n > 0 {
                            format!("语义向量补算完成：{n} 条")
                        } else {
                            String::new()
                        };
                        if !notes.is_empty() {
                            let mut w = notes.join("；");
                            if w.chars().count() > 200 {
                                w = w.chars().take(200).collect::<String>() + "…";
                            }
                            if !s.is_empty() {
                                s.push_str(" · ");
                            }
                            s.push_str(&w);
                        }
                        if !s.is_empty() {
                            self.status = s;
                        }
                    }
                    Err(e) => self.status = format!("语义向量补算失败：{e}"),
                }
            }
            Err(mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(200));
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.backfill_job = None;
                self.status = "语义向量补算线程意外退出。".into();
            }
        }
    }

    /// 导入 / 补算进行中时库不在手上，统一给出人话提示。
    fn busy_hint(&self) -> Option<&'static str> {
        if self.import_job.is_some() {
            Some("正在后台导入，请等拆条完成后再操作。")
        } else if self.backfill_job.is_some() {
            Some("正在补算语义向量，请稍候。")
        } else {
            None
        }
    }

    fn do_search(&mut self) {
        let q = self.query.trim().to_string();
        if q.is_empty() {
            return;
        }
        if let Some(hint) = self.busy_hint() {
            self.status = hint.into();
            return;
        }
        let Some(s) = self.store.as_ref() else {
            self.status = "还没有打开库。请先点「试用样例」。".into();
            return;
        };
        let embedder = self.semantic_ready_embedder();
        match s.search_with(&q, embedder.as_deref()) {
            Ok(r) => {
                let terms: String = r
                    .terms
                    .iter()
                    .take(8)
                    .map(|t| t.trim_matches('"'))
                    .collect::<Vec<_>>()
                    .join(" / ");
                let mut st = format!("{} ms · 抽词 {} · 命中 {}", r.latency_ms, terms, r.hits.len());
                if embedder.is_some() && !r.used_semantic {
                    // §7.3：语义未参与（模型未就绪/库内无向量）时明示「仅关键词」
                    st.push_str(" · 仅关键词");
                }
                self.status = st;
                self.selected = r.hits.first().map(|h| h.chunk.id);
                self.expanded.clear();
                self.reading_id = None;
                self.last = Some(r);
            }
            Err(e) => self.status = format!("检索失败: {e}"),
        }
    }

    fn copy(&mut self, id: i64, key: &str) {
        let Some(s) = self.store.as_ref() else { return };
        match s.copy_payload(id, key) {
            Ok(text) => match arboard::Clipboard::new() {
                Ok(mut c) => {
                    if c.set_text(text.clone()).is_ok() {
                        let label = match key {
                            "player" => "公开",
                            "full" => "全文",
                            "source" => "来源",
                            other => other,
                        };
                        self.toast = format!("已复制{label} {} 字，可粘到聊天/笔记", text.chars().count());
                    }
                }
                Err(e) => self.status = format!("剪贴板: {e}"),
            },
            Err(e) => self.status = format!("复制失败: {e}"),
        }
    }

    fn load_sample(&mut self) {
        if let Some(hint) = self.busy_hint() {
            self.status = hint.into();
            return;
        }
        let Some(src) = sample_campaign_dir() else {
            self.status = "找不到 testdata/sample-campaign。请在仓库根目录运行，或用「导入文件夹」。".into();
            return;
        };
        let path = sample_store_path();
        if let Err(e) = fs::create_dir_all(appdata_dir()) {
            self.status = format!("无法创建数据目录: {e}");
            return;
        }
        if let Some(old) = self.store.take() {
            let _ = old.close();
        }
        let mut store = if path.exists() {
            match Store::open(&path) {
                Ok(s) => s,
                Err(e) => {
                    self.status = format!("打开样例库失败: {e}");
                    return;
                }
            }
        } else {
            match Store::create(&path, "断桅港战役") {
                Ok(s) => s,
                Err(e) => {
                    self.status = format!("创建样例库失败: {e}");
                    return;
                }
            }
        };
        // 带编码器导入：模型已就绪时新条目直接落向量，不必等补算
        let embedder = self.semantic_ready_embedder();
        let opts = ImportOpts {
            embedder: embedder.as_deref(),
            ..Default::default()
        };
        match store.import_with(&[src], opts) {
            Ok(r) => {
                if r.files_fail > 0 {
                    self.status = format!("样例导入有失败：{}", r.errors.join("；"));
                }
                remember_store(&path);
                self.store = Some(store);
                self.query = SAMPLE_QUERY.to_string();
                self.focus_search = true;
                self.do_search();
                if let Some(info) = self.store.as_ref().and_then(|s| s.info().ok()) {
                    self.info = Some(info.clone());
                    if r.files_fail == 0 {
                        self.status = format!(
                            "样例就绪 · {} 条 · 成功 {} / 跳过 {} · 回车可再搜",
                            info.chunk_count, r.files_ok, r.files_skip
                        );
                    }
                }
            }
            Err(e) => self.status = format!("导入样例失败: {e}"),
        }
        // 样例库可能是早先没有模型时建的（条目已存在 → 导入是 Skip，不会补向量）
        self.start_backfill();
    }

    fn import_selected(&mut self, paths: Vec<PathBuf>) {
        if let Some(hint) = self.busy_hint() {
            self.status = hint.into();
            return;
        }
        if self.store.is_none() {
            self.status = "请先新建或打开库，再导入。".into();
            return;
        }
        let cfg = load_app_config();
        let llm_cfg = if self.llm_enabled {
            match LlmConfig::from_parts(&cfg.llm.base_url, &cfg.llm.api_key, &cfg.llm.model) {
                Some(mut c) => {
                    if cfg.llm.timeout_secs > 0 {
                        c.timeout_secs = cfg.llm.timeout_secs;
                    }
                    if cfg.llm.concurrency > 0 {
                        c.concurrency = cfg.llm.concurrency;
                    }
                    if cfg.llm.max_tokens > 0 {
                        c.max_tokens = cfg.llm.max_tokens;
                    }
                    if cfg.llm.temperature > 0.0 {
                        c.temperature = cfg.llm.temperature;
                    }
                    c.retries = cfg.llm.retries;
                    c.json_mode = cfg.llm.json_mode;
                    Some(c)
                }
                None => {
                    self.status = format!(
                        "已勾选 LLM 拆条，但 {} 里 base_url / api_key / model 不完整。",
                        llmconfig_path().display()
                    );
                    return;
                }
            }
        } else {
            None
        };
        let Some(store) = self.store.take() else {
            return;
        };
        let progress = Arc::new(Mutex::new(
            if llm_cfg.is_some() {
                "后台导入 + LLM 拆条中，窗口应保持可点；请不要关程序。".to_string()
            } else {
                "后台导入中…".to_string()
            },
        ));
        let (tx, rx) = mpsc::channel();
        let progress_thread = progress.clone();
        let embedder = self.semantic_ready_embedder();
        thread::spawn(move || {
            let mut store = store;
            let splitter = llm_cfg.map(|c| LlmSplitter::with_progress(c, progress_thread.clone()));
            if let Ok(mut g) = progress_thread.lock() {
                *g = if splitter.is_some() {
                    "正在解析 Word 并用 LLM 拆条…".into()
                } else {
                    "正在解析并写入库…".into()
                };
            }
            let opts = ImportOpts {
                splitter: splitter.as_ref().map(|x| x as &dyn table_canon_core::EntrySplitter),
                reprocess: splitter.is_some(),
                embedder: embedder.as_deref(),
            };
            let result = store
                .import_with(&paths, opts)
                .map_err(|e| e.to_string());
            // 语义就绪时补算缺向量（覆盖本次未带向量导入的旧数据，§7.2）。
            // 结论必须带回主线程并入状态栏：这里返回的 (条数, 警告) 曾被直接丢弃，
            // 条目编码失败时用户只会看到「导入完成」和「语义 · 开」。
            let mut backfill = None;
            if result.is_ok() {
                if let Some(e) = embedder.as_deref() {
                    if let Ok(mut g) = progress_thread.lock() {
                        *g = "检查/补算语义向量…".into();
                    }
                    match store.backfill_embeddings(e, Some(&progress_thread)) {
                        Ok((n, notes)) => {
                            if n > 0 || !notes.is_empty() {
                                let mut s = if n > 0 {
                                    format!("语义向量补算 {n} 条")
                                } else {
                                    String::new()
                                };
                                if !notes.is_empty() {
                                    if !s.is_empty() {
                                        s.push('·');
                                    }
                                    s.push_str(&notes.join("；"));
                                }
                                backfill = Some(s);
                            }
                        }
                        Err(err) => backfill = Some(format!("语义向量补算失败：{err}")),
                    }
                }
            }
            let _ = tx.send(ImportOutcome {
                store,
                result,
                backfill,
            });
        });
        self.status = progress.lock().map(|g| g.clone()).unwrap_or_else(|_| "后台导入中…".into());
        self.import_job = Some(ImportJob { progress, rx });
    }

    fn poll_import_job(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.import_job else {
            return;
        };
        if let Ok(msg) = job.progress.lock() {
            if !msg.is_empty() && *msg != self.status {
                self.status = msg.clone();
            }
        }
        let recv = job.rx.try_recv();
        match recv {
            Ok(out) => {
                self.import_job = None;
                remember_store(&out.store.path);
                self.store = Some(out.store);
                let mut import_msg = match out.result {
                    Ok(r) => {
                        let mut s = format!(
                            "导入完成：成功 {} / 跳过 {} / 失败 {} / 条目 {} / 未挂修正 {}",
                            r.files_ok,
                            r.files_skip,
                            r.files_fail,
                            r.chunks,
                            r.unmatched_corrections
                        );
                        if !r.errors.is_empty() {
                            s.push_str(" · ");
                            s.push_str(&r.errors.join("；"));
                        }
                        if !r.warnings.is_empty() {
                            let mut w = r.warnings.join("；");
                            if w.chars().count() > 200 {
                                w = w.chars().take(200).collect::<String>() + "…";
                            }
                            s.push_str(" · ");
                            s.push_str(&w);
                        }
                        s
                    }
                    Err(e) => format!("导入失败: {e}"),
                };
                if let Some(b) = &out.backfill {
                    import_msg.push_str(" · ");
                    import_msg.push_str(b);
                }
                self.refresh_info();
                self.status = import_msg;
            }
            Err(mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(200));
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.import_job = None;
                self.status = "导入线程意外退出。请重新打开库后再试。".into();
            }
        }
    }

    fn open_reader(&mut self, id: i64) {
        self.reading_id = Some(id);
        self.selected = Some(id);
    }

    fn back_to_results(&mut self) {
        self.reading_id = None;
    }

    fn move_selection(&mut self, delta: i32) {
        let Some(hits) = self.last.as_ref().map(|l| &l.hits) else {
            return;
        };
        if hits.is_empty() {
            return;
        }
        let cur_id = self.reading_id.or(self.selected);
        let cur = cur_id
            .and_then(|id| hits.iter().position(|h| h.chunk.id == id))
            .unwrap_or(0);
        let next = (cur as i32 + delta).clamp(0, hits.len() as i32 - 1) as usize;
        let id = hits[next].chunk.id;
        self.selected = Some(id);
        if self.reading_id.is_some() {
            self.reading_id = Some(id);
        }
    }

    fn ui_reader(&mut self, ui: &mut egui::Ui, id: i64) {
        let loaded = {
            let Some(store) = self.store.as_ref() else {
                ui.label("库已关闭。");
                return;
            };
            match store.get_chunk(id) {
                Ok(d) => {
                    let prev_title = d
                        .prev_id
                        .and_then(|pid| store.get_chunk(pid).ok())
                        .map(|x| x.chunk.title);
                    let next_title = d
                        .next_id
                        .and_then(|nid| store.get_chunk(nid).ok())
                        .map(|x| x.chunk.title);
                    Some((d, prev_title, next_title))
                }
                Err(e) => {
                    ui.label(format!("读不到这条原文：{e}"));
                    None
                }
            }
        };
        let Some((detail, prev_title, next_title)) = loaded else {
            return;
        };
        let prev_id = detail.prev_id;
        let next_id = detail.next_id;
        let hits = self.last.as_ref().map(|l| l.hits.clone()).unwrap_or_default();
        let hit_i = hits.iter().position(|h| h.chunk.id == id);
        let hit_n = hits.len();
        let q = self.query.trim().to_string();
        let c = detail.chunk;

        ui.horizontal(|ui| {
            if ui.button("← 返回结果").clicked() {
                self.back_to_results();
            }
            if let Some(i) = hit_i {
                ui.label(RichText::new(format!("结果 {} / {}", i + 1, hit_n)).weak());
                if ui
                    .add_enabled(i > 0, egui::Button::new("上一条结果"))
                    .clicked()
                {
                    self.open_reader(hits[i - 1].chunk.id);
                }
                if ui
                    .add_enabled(i + 1 < hit_n, egui::Button::new("下一条结果"))
                    .clicked()
                {
                    self.open_reader(hits[i + 1].chunk.id);
                }
            }
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.heading(RichText::new(&c.title).color(Color32::from_rgb(232, 196, 104)));
            ui.label(
                RichText::new(&c.entity_type).color(Color32::from_rgb(160, 140, 90)),
            );
            if c.visibility & VIS_SECRET != 0 {
                ui.label(RichText::new("SECRET").color(Color32::from_rgb(200, 90, 80)));
            }
        });
        if !c.aliases.is_empty() {
            ui.label(format!("别名：{}", c.aliases.join(" / ")));
        }
        ui.label(
            RichText::new(format!("{} · {}", c.file_name, c.parent_path))
                .small()
                .weak(),
        );
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("复制公开").clicked() {
                self.copy(id, "player");
            }
            if ui.button("复制全文").clicked() {
                self.copy(id, "full");
            }
            if ui.button("复制来源").clicked() {
                self.copy(id, "source");
            }
            if ui.button("标为秘密").clicked() {
                if let Some(s) = &self.store {
                    let _ = s.update_chunk_meta(
                        id,
                        MetaPatch {
                            visibility: Some(c.visibility | VIS_SECRET | VIS_PUBLIC),
                            ..Default::default()
                        },
                    );
                }
            }
        });
        ui.add_space(8.0);
        egui::ScrollArea::vertical()
            .id_salt("reader-body")
            .show(ui, |ui| {
                ui.label(highlight_body(&c.body, &q, 17.0));
                ui.add_space(16.0);
                ui.separator();
                ui.label(RichText::new("原文位置（同一文档相邻条目）").weak());
                ui.horizontal(|ui| {
                    if let Some(t) = &prev_title {
                        if ui.button(format!("上文 · {t}")).clicked() {
                            if let Some(pid) = prev_id {
                                self.open_reader(pid);
                            }
                        }
                    } else {
                        ui.label(RichText::new("没有上文").weak());
                    }
                    if let Some(t) = &next_title {
                        if ui.button(format!("下文 · {t}")).clicked() {
                            if let Some(nid) = next_id {
                                self.open_reader(nid);
                            }
                        }
                    } else {
                        ui.label(RichText::new("没有下文").weak());
                    }
                });
            });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(job) = self.pending.take() {
            match job {
                Pending::Sample => self.load_sample(),
                Pending::Import(paths) => self.import_selected(paths),
            }
        }
        self.poll_import_job(ctx);
        self.poll_backfill_job(ctx);
        // 预热完成（Loading → Ready）后自动补算库内缺的向量（§7.2）。
        // 没有这一步，启动时打开的旧库永远是纯关键词，底栏却写着「语义 · 开」。
        if !self.semantic_ready_seen && matches!(self.semantic_state(), SemanticState::Ready(_)) {
            self.semantic_ready_seen = true;
            self.start_backfill();
        }

        let enter = ctx.input(|i| i.key_pressed(egui::Key::Enter));
        let escape = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        if escape && self.reading_id.is_some() {
            self.back_to_results();
        }
        if enter && !self.search_focused && self.store.is_some() {
            if let Some(id) = self.selected {
                if self.reading_id.is_some() {
                    self.copy(id, "full");
                } else {
                    self.open_reader(id);
                }
            }
        }
        if !self.search_focused {
            if ctx.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                self.move_selection(1);
            }
            if ctx.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                self.move_selection(-1);
            }
        }

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if self.reading_id.is_some() {
                    let back = ui.add(
                        egui::Button::new(
                            RichText::new("← 返回结果").color(Color32::from_rgb(40, 28, 8)),
                        )
                        .fill(Color32::from_rgb(232, 196, 104)),
                    );
                    if back.clicked() {
                        self.back_to_results();
                    }
                    ui.separator();
                }
                ui.heading(RichText::new("席间索").color(Color32::from_rgb(232, 196, 104)));
                ui.label(RichText::new("可用小样").color(Color32::GRAY));
                ui.separator();
                let sample = ui.add(
                    egui::Button::new(RichText::new("试用样例").color(Color32::from_rgb(40, 28, 8)))
                        .fill(Color32::from_rgb(232, 196, 104)),
                );
                if sample.clicked() {
                    self.status = "正在准备断桅港样例…".into();
                    self.pending = Some(Pending::Sample);
                    ctx.request_repaint();
                }
                if ui.button("新建库").clicked() {
                    if let Some(p) = rfd::FileDialog::new()
                        .add_filter("席间索库", &["tcs"])
                        .set_file_name("campaign.tcs")
                        .save_file()
                    {
                        match Store::create(&p, "未命名战役") {
                            Ok(s) => {
                                self.swap_store(s);
                                self.focus_search = true;
                            }
                            Err(e) => self.status = format!("新建失败: {e}"),
                        }
                    }
                }
                if ui.button("打开库").clicked() {
                    if let Some(p) = rfd::FileDialog::new()
                        .add_filter("席间索库", &["tcs"])
                        .pick_file()
                    {
                        match Store::open(&p) {
                            Ok(s) => {
                                self.swap_store(s);
                                self.focus_search = true;
                            }
                            Err(e) => self.status = format!("打开失败: {e}"),
                        }
                    }
                }
                ui.add_enabled_ui(self.store.is_some() && self.import_job.is_none(), |ui| {
                    if ui.button("导入文件夹").clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_folder() {
                            self.status = format!("正在导入 {} …", p.display());
                            self.pending = Some(Pending::Import(vec![p]));
                            ctx.request_repaint();
                        }
                    }
                    if ui.button("导入 Word").clicked() {
                        if let Some(files) = rfd::FileDialog::new()
                            .add_filter("Word 文档", &["docx"])
                            .pick_files()
                        {
                            self.status = format!("正在导入 {} 个 Word…", files.len());
                            self.pending = Some(Pending::Import(files));
                            ctx.request_repaint();
                        }
                    }
                    if ui
                        .checkbox(&mut self.llm_enabled, "导入时 LLM 拆条")
                        .on_hover_text("导入可变慢，查询仍在本地 5 秒内。密钥写在仓库根目录 llmconfig.toml")
                        .changed()
                    {
                        let mut c = load_app_config();
                        c.llm.enabled = self.llm_enabled;
                        save_app_config(&c);
                    }
                    if ui.button("导出便携库").clicked() {
                        if let Some(p) = rfd::FileDialog::new()
                            .add_filter("席间索库", &["tcs"])
                            .set_file_name("portable.tcs")
                            .save_file()
                        {
                            if let Some(s) = &self.store {
                                match s.export_snapshot(&p) {
                                    Ok((n, mode)) => {
                                        self.status = format!("已导出 {n} 字节 · journal={mode}")
                                    }
                                    Err(e) => self.status = format!("导出失败: {e}"),
                                }
                            }
                        }
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.checkbox(&mut self.always_on_top, "置顶").changed() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                            if self.always_on_top {
                                egui::WindowLevel::AlwaysOnTop
                            } else {
                                egui::WindowLevel::Normal
                            },
                        ));
                    }
                });
            });
            ui.add_space(4.0);
        });

        egui::TopBottomPanel::bottom("bot").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&self.status).color(Color32::from_rgb(180, 180, 168)));
                if !self.toast.is_empty() {
                    ui.separator();
                    ui.label(RichText::new(&self.toast).color(Color32::from_rgb(140, 210, 150)));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let sem = self.semantic_state();
                    let (label, color, hover) = match &sem {
                        SemanticState::Loading => (
                            "语义 · 准备中",
                            Color32::from_rgb(210, 190, 100),
                            "首次加载模型约数秒，期间检索仅用关键词".to_string(),
                        ),
                        SemanticState::Ready(_) => (
                            "语义 · 开",
                            Color32::from_rgb(140, 210, 150),
                            "语义检索已开启；导入时自动计算向量，结果卡片会标「语义」".to_string(),
                        ),
                        SemanticState::Failed(e) => (
                            "语义 · 不可用",
                            Color32::from_rgb(210, 130, 120),
                            e.clone(),
                        ),
                        SemanticState::Idle => (
                            "语义 · 未装模型",
                            Color32::from_rgb(150, 150, 145),
                            "把模型放到 models\\bge-small-zh-v1.5\\（model.onnx + tokenizer.json）后重启即可开启语义检索"
                                .to_string(),
                        ),
                    };
                    ui.label(RichText::new(label).small().color(color))
                        .on_hover_text(hover);
                });
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_enabled_ui(self.store.is_some(), |ui| {
                ui.horizontal(|ui| {
                    let edit = ui.add(
                        egui::TextEdit::singleline(&mut self.query)
                            .desired_width(ui.available_width() - 88.0)
                            .hint_text("粘贴玩家刚说的话，回车检索"),
                    );
                    if self.focus_search {
                        edit.request_focus();
                        self.focus_search = false;
                    }
                    self.search_focused = edit.has_focus();
                    if edit.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        self.do_search();
                        edit.surrender_focus();
                        self.search_focused = false;
                    }
                    if ui.button("检索").clicked() {
                        self.do_search();
                    }
                });
            });
            ui.add_space(8.0);

            if let Some(id) = self.reading_id {
                self.ui_reader(ui, id);
                return;
            }

            let hits: Vec<Hit> = self.last.as_ref().map(|l| l.hits.clone()).unwrap_or_default();
            let selected = self.selected;
            egui::ScrollArea::vertical().show(ui, |ui| {
                if self.import_job.is_some() {
                    ui.label(
                        RichText::new("正在后台导入 / LLM 拆条。窗口应仍可拖动；底栏会显示当前章节。")
                            .size(16.0),
                    );
                    ui.add_space(6.0);
                    ui.label(RichText::new(&self.status).weak());
                    return;
                }
                if self.store.is_none() {
                    ui.label(
                        RichText::new("这是开席用的设定检索小样：导入资料 → 对着玩家原话搜 → 复制公开版。")
                            .size(16.0),
                    );
                    ui.add_space(6.0);
                    ui.label(RichText::new("点左上角「试用样例」，会自动建库并导入 testdata/sample-campaign。").weak());
                    return;
                }
                if hits.is_empty() {
                    ui.label(
                        RichText::new("没有结果。样例可搜：独眼酒保 / gelimu / 老格 / 潮响号。")
                            .weak(),
                    );
                    return;
                }
                for h in &hits {
                    let on = selected == Some(h.chunk.id);
                    let mut frame = egui::Frame::group(ui.style());
                    if on {
                        frame = frame.stroke(egui::Stroke::new(1.0, Color32::from_rgb(232, 196, 104)));
                    }
                    let inner = frame.show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&h.chunk.title).strong().size(18.0));
                            ui.label(
                                RichText::new(&h.chunk.entity_type)
                                    .color(Color32::from_rgb(160, 140, 90)),
                            );
                            if h.chunk.visibility & VIS_SECRET != 0 {
                                ui.label(
                                    RichText::new("SECRET").color(Color32::from_rgb(200, 90, 80)),
                                );
                            }
                            let q = self.query.trim();
                            let is_entry = h.via.iter().any(|v| v == "index")
                                || (!q.is_empty() && h.chunk.title.contains(q));
                            ui.label(RichText::new(if is_entry { "条目" } else { "正文" }).color(
                                if is_entry {
                                    Color32::from_rgb(232, 196, 104)
                                } else {
                                    Color32::from_rgb(140, 140, 140)
                                },
                            ));
                            // §6.6：语义命中无词重叠时不高亮生造词，只在卡片角标标「语义」
                            if h.via.iter().any(|v| v == "semantic") && !is_entry {
                                ui.label(
                                    RichText::new("语义").color(Color32::from_rgb(120, 175, 250)),
                                );
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(RichText::new("点击查看原文").weak().small());
                            });
                        });
                        let (preview, _) = snippet_around(&h.chunk.body, self.query.trim(), 90);
                        ui.label(RichText::new(preview).weak().size(14.0));
                        ui.label(
                            RichText::new(format!("{} · {}", h.chunk.file_name, h.chunk.parent_path))
                                .small()
                                .weak(),
                        );
                    });
                    if inner.response.interact(egui::Sense::click()).clicked() {
                        self.open_reader(h.chunk.id);
                    }
                    ui.add_space(8.0);
                }
            });
        });
    }
}

fn highlight_body(body: &str, query: &str, size: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    let normal = TextFormat {
        font_id: FontId::proportional(size),
        color: Color32::from_rgb(228, 226, 214),
        ..Default::default()
    };
    let hit = TextFormat {
        font_id: FontId::proportional(size),
        color: Color32::from_rgb(40, 28, 8),
        background: Color32::from_rgb(232, 196, 104),
        ..Default::default()
    };
    let q = query.trim();
    if q.is_empty() {
        job.append(body, 0.0, normal);
        return job;
    }
    let mut rest = body;
    while let Some(i) = rest.find(q) {
        if i > 0 {
            job.append(&rest[..i], 0.0, normal.clone());
        }
        job.append(q, 0.0, hit.clone());
        rest = &rest[i + q.len()..];
    }
    if !rest.is_empty() {
        job.append(rest, 0.0, normal);
    }
    job
}

fn snippet_around(body: &str, query: &str, max_chars: usize) -> (String, bool) {
    let chars: Vec<char> = body.chars().collect();
    if chars.len() <= max_chars {
        return (body.to_string(), false);
    }
    let q = query.trim();
    let idx = if q.is_empty() {
        None
    } else {
        body.find(q)
    };
    let start_char = if let Some(byte_idx) = idx {
        body[..byte_idx].chars().count().saturating_sub(80)
    } else {
        0
    };
    let end_char = (start_char + max_chars).min(chars.len());
    let mut s: String = chars[start_char..end_char].iter().collect();
    if start_char > 0 {
        s = format!("…{s}");
    }
    if end_char < chars.len() {
        s.push('…');
    }
    (s, true)
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct AppConfig {
    #[serde(default)]
    llm: LlmSection,
}

#[derive(serde::Serialize, serde::Deserialize, Default, Clone)]
struct LlmSection {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    timeout_secs: u64,
    #[serde(default)]
    concurrency: u32,
    #[serde(default)]
    max_tokens: u32,
    /// 0 表示用内置默认 0.1。
    #[serde(default)]
    temperature: f32,
    /// 解析/调用失败后的追加重试次数（带错误回喂），0 表示一次定生死。
    #[serde(default)]
    retries: u32,
    /// 请求 response_format=json_object；代理不支持时自动回退。
    #[serde(default)]
    json_mode: bool,
}

fn demo_root() -> PathBuf {
    let from_crate = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    if from_crate.join("Cargo.toml").is_file() {
        return from_crate.canonicalize().unwrap_or(from_crate);
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn llmconfig_candidates() -> Vec<PathBuf> {
    let root = demo_root();
    let cwd = std::env::current_dir().unwrap_or_else(|_| root.clone());
    vec![
        root.join("llmconfig.toml"),
        root.join("llmconfig"),
        cwd.join("llmconfig.toml"),
        cwd.join("llmconfig"),
    ]
}

fn llmconfig_path() -> PathBuf {
    llmconfig_candidates()
        .into_iter()
        .find(|p| p.is_file())
        .unwrap_or_else(|| demo_root().join("llmconfig.toml"))
}

fn parse_llm_file(raw: &str) -> LlmSection {
    if let Ok(flat) = toml::from_str::<LlmSection>(raw) {
        if flat.enabled
            || !flat.base_url.trim().is_empty()
            || !flat.api_key.trim().is_empty()
            || !flat.model.trim().is_empty()
        {
            return flat;
        }
    }
    toml::from_str::<AppConfig>(raw)
        .map(|c| c.llm)
        .unwrap_or_default()
}

fn load_app_config() -> AppConfig {
    let p = llmconfig_path();
    match fs::read_to_string(&p) {
        Ok(raw) => AppConfig {
            llm: parse_llm_file(&raw),
        },
        Err(_) => {
            let cfg = AppConfig {
                llm: LlmSection {
                    enabled: false,
                    base_url: "http://127.0.0.1:8317/v1".into(),
                    api_key: String::new(),
                    model: "gpt-4o-mini".into(),
                    timeout_secs: 60,
                    concurrency: 3,
                    ..Default::default()
                },
            };
            save_app_config(&cfg);
            cfg
        }
    }
}

fn save_app_config(cfg: &AppConfig) {
    let path = llmconfig_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let llm = &cfg.llm;
    let timeout = if llm.timeout_secs == 0 {
        60
    } else {
        llm.timeout_secs
    };
    let conc = if llm.concurrency == 0 { 3 } else { llm.concurrency };
    let max_tokens = if llm.max_tokens == 0 { 2048 } else { llm.max_tokens };
    let temp = if llm.temperature <= 0.0 { 0.1 } else { llm.temperature };
    let text = format!(
        "# 席间索 AI 配置（仅导入拆条；查询走本地）\n# 拆条请用小模型（如 gpt-4o-mini），不要用 grok-4.6 这类推理模型。\nenabled = {}\nbase_url = \"{}\"\napi_key = \"{}\"\nmodel = \"{}\"\ntimeout_secs = {}\nconcurrency = {}\nmax_tokens = {}\ntemperature = {}\nretries = {}\njson_mode = {}\n",
        llm.enabled,
        llm.base_url.replace('\\', "\\\\").replace('"', "\\\""),
        llm.api_key.replace('\\', "\\\\").replace('"', "\\\""),
        llm.model.replace('\\', "\\\\").replace('"', "\\\""),
        timeout,
        conc,
        max_tokens,
        temp,
        llm.retries,
        llm.json_mode
    );
    let _ = fs::write(path, text);
}

fn appdata_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("table-canon")
}

fn last_store_file() -> PathBuf {
    appdata_dir().join("last-store.txt")
}

fn sample_store_path() -> PathBuf {
    appdata_dir().join("demo-sample.tcs")
}

fn remember_store(path: &Path) {
    let _ = fs::create_dir_all(appdata_dir());
    let _ = fs::write(last_store_file(), path.to_string_lossy().as_bytes());
}

fn load_last_store_path() -> Option<PathBuf> {
    let raw = fs::read_to_string(last_store_file()).ok()?;
    let p = PathBuf::from(raw.trim());
    p.exists().then_some(p)
}

fn sample_campaign_dir() -> Option<PathBuf> {
    let bundled = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/sample-campaign");
    let candidates = [
        bundled,
        PathBuf::from("testdata/sample-campaign"),
        PathBuf::from("../testdata/sample-campaign"),
    ];
    candidates.into_iter().find(|p| p.is_dir())
}

fn install_cjk_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let candidates = [
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\msyh.ttf",
        r"C:\Windows\Fonts\simhei.ttf",
        r"C:\Windows\Fonts\msyhbd.ttc",
    ];
    for p in candidates {
        if let Ok(bytes) = std::fs::read(p) {
            fonts.font_data.insert("cjk".into(), FontData::from_owned(bytes).into());
            fonts
                .families
                .entry(FontFamily::Proportional)
                .or_default()
                .insert(0, "cjk".into());
            fonts
                .families
                .entry(FontFamily::Monospace)
                .or_default()
                .insert(0, "cjk".into());
            ctx.set_fonts(fonts);
            return;
        }
    }
}
