use eframe::egui::{self, Color32, FontData, FontDefinitions, FontFamily, RichText, ViewportBuilder};
use std::fs;
use std::path::{Path, PathBuf};
use table_canon_core::{Hit, MetaPatch, SearchResult, Store, StoreInfo, VIS_PUBLIC, VIS_SECRET};

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
        };
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
        self.refresh_info();
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

    fn do_search(&mut self) {
        let q = self.query.trim().to_string();
        if q.is_empty() {
            return;
        }
        let Some(s) = self.store.as_ref() else {
            self.status = "还没有打开库。请先点「试用样例」。".into();
            return;
        };
        match s.search(&q) {
            Ok(r) => {
                let terms: String = r
                    .terms
                    .iter()
                    .take(8)
                    .map(|t| t.trim_matches('"'))
                    .collect::<Vec<_>>()
                    .join(" / ");
                self.status = format!("{} ms · 抽词 {} · 命中 {}", r.latency_ms, terms, r.hits.len());
                self.selected = r.hits.first().map(|h| h.chunk.id);
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
        match store.import_paths(&[src]) {
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
    }

    fn import_selected(&mut self, paths: Vec<PathBuf>) {
        let Some(s) = self.store.as_mut() else {
            self.status = "请先新建或打开库，再导入。".into();
            return;
        };
        match s.import_paths(&paths) {
            Ok(r) => {
                self.status = format!(
                    "导入完成：成功 {} / 跳过 {} / 失败 {} / 条目 {} / 未挂修正 {}",
                    r.files_ok, r.files_skip, r.files_fail, r.chunks, r.unmatched_corrections
                );
                if !r.errors.is_empty() {
                    self.status.push_str(" · ");
                    self.status.push_str(&r.errors.join("；"));
                }
            }
            Err(e) => self.status = format!("导入失败: {e}"),
        }
        self.refresh_info();
    }

    fn move_selection(&mut self, delta: i32) {
        let Some(hits) = self.last.as_ref().map(|l| &l.hits) else { return };
        if hits.is_empty() {
            return;
        }
        let cur = self
            .selected
            .and_then(|id| hits.iter().position(|h| h.chunk.id == id))
            .unwrap_or(0);
        let next = (cur as i32 + delta).clamp(0, hits.len() as i32 - 1) as usize;
        self.selected = Some(hits[next].chunk.id);
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

        let enter = ctx.input(|i| i.key_pressed(egui::Key::Enter));
        if enter && !self.search_focused && self.store.is_some() {
            if let Some(id) = self.selected {
                self.copy(id, "player");
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
                ui.add_enabled_ui(self.store.is_some(), |ui| {
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

            let hits: Vec<Hit> = self.last.as_ref().map(|l| l.hits.clone()).unwrap_or_default();
            let selected = self.selected;
            egui::ScrollArea::vertical().show(ui, |ui| {
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
                    frame.show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if ui
                                .selectable_label(on, RichText::new(&h.chunk.title).strong())
                                .clicked()
                            {
                                self.selected = Some(h.chunk.id);
                            }
                            ui.label(
                                RichText::new(&h.chunk.entity_type).color(Color32::from_rgb(160, 140, 90)),
                            );
                            if h.chunk.visibility & VIS_SECRET != 0 {
                                ui.label(RichText::new("SECRET").color(Color32::from_rgb(200, 90, 80)));
                            }
                            ui.label(RichText::new(format!("via {}", h.via.join("+"))).weak());
                        });
                        if !h.chunk.aliases.is_empty() {
                            ui.label(format!("别名：{}", h.chunk.aliases.join(" / ")));
                        }
                        ui.label(RichText::new(&h.chunk.body).size(16.0));
                        ui.label(
                            RichText::new(format!("{} · {}", h.chunk.file_name, h.chunk.parent_path))
                                .small()
                                .weak(),
                        );
                        ui.horizontal(|ui| {
                            if ui.button("复制公开").clicked() {
                                self.copy(h.chunk.id, "player");
                            }
                            if ui.button("复制全文").clicked() {
                                self.copy(h.chunk.id, "full");
                            }
                            if ui.button("复制来源").clicked() {
                                self.copy(h.chunk.id, "source");
                            }
                            if ui.button("标为秘密").clicked() {
                                if let Some(s) = &self.store {
                                    let _ = s.update_chunk_meta(
                                        h.chunk.id,
                                        MetaPatch {
                                            visibility: Some(h.chunk.visibility | VIS_SECRET | VIS_PUBLIC),
                                            ..Default::default()
                                        },
                                    );
                                }
                                self.do_search();
                            }
                        });
                    });
                    ui.add_space(8.0);
                }
            });
        });
    }
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
