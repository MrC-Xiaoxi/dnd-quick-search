use eframe::egui::{self, Color32, FontData, FontDefinitions, FontFamily, RichText, ViewportBuilder};
use table_canon_core::{Hit, MetaPatch, SearchResult, Store, StoreInfo, VIS_PUBLIC, VIS_SECRET};

fn main() -> eframe::Result<()> {
    let viewport = ViewportBuilder::default()
        .with_inner_size([920.0, 640.0])
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

struct App {
    store: Option<Store>,
    info: Option<StoreInfo>,
    query: String,
    last: Option<SearchResult>,
    status: String,
    toast: String,
    always_on_top: bool,
    selected: Option<i64>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_cjk_fonts(&cc.egui_ctx);
        let mut style = (*cc.egui_ctx.style()).clone();
        style.visuals.dark_mode = true;
        cc.egui_ctx.set_style(style);
        Self {
            store: None,
            info: None,
            query: String::new(),
            last: None,
            status: "新建或打开一个 .tcs 战役库，然后导入模组文件夹。".into(),
            toast: String::new(),
            always_on_top: true,
            selected: None,
        }
    }

    fn refresh_info(&mut self) {
        if let Some(s) = &self.store {
            match s.info() {
                Ok(i) => {
                    self.status = format!(
                        "战役「{}」· {} 条 · 语义未启用（M1）",
                        i.campaign_name, i.chunk_count
                    );
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
        if let Some(s) = &self.store {
            match s.search(&q) {
                Ok(r) => {
                    self.status = format!(
                        "{} ms · 抽词 {} · 命中 {}",
                        r.latency_ms,
                        r.terms.join(" / "),
                        r.hits.len()
                    );
                    self.selected = r.hits.first().map(|h| h.chunk.id);
                    self.last = Some(r);
                }
                Err(e) => self.status = format!("检索失败: {e}"),
            }
        }
    }

    fn copy(&mut self, id: i64, key: &str) {
        if let Some(s) = &self.store {
            match s.copy_payload(id, key) {
                Ok(text) => match arboard::Clipboard::new() {
                    Ok(mut c) => {
                        if c.set_text(text.clone()).is_ok() {
                            self.toast = format!("已复制「{key}」{} 字", text.chars().count());
                        }
                    }
                    Err(e) => self.status = format!("剪贴板: {e}"),
                },
                Err(e) => self.status = format!("复制失败: {e}"),
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if ctx.input(|i| i.key_pressed(egui::Key::Enter)) && !self.query.is_empty() {
            // 避免在文本框里误触发多次：仅当搜索框焦点时
        }

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading(RichText::new("席间索").color(Color32::from_rgb(232, 196, 104)));
                ui.label(RichText::new("M1 demo").color(Color32::GRAY));
                ui.separator();
                if ui.button("新建库").clicked() {
                    if let Some(p) = rfd::FileDialog::new()
                        .add_filter("席间索库", &["tcs"])
                        .set_file_name("campaign.tcs")
                        .save_file()
                    {
                        match Store::create(&p, "未命名战役") {
                            Ok(s) => {
                                self.store = Some(s);
                                self.refresh_info();
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
                                self.store = Some(s);
                                self.refresh_info();
                            }
                            Err(e) => self.status = format!("打开失败: {e}"),
                        }
                    }
                }
                ui.add_enabled_ui(self.store.is_some(), |ui| {
                    if ui.button("导入文件夹").clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_folder() {
                            if let Some(s) = self.store.as_mut() {
                                match s.import_paths(&[p]) {
                                    Ok(r) => {
                                        self.status = format!(
                                            "导入完成：成功 {} / 跳过 {} / 失败 {} / 条目 {} / 未挂修正 {}",
                                            r.files_ok,
                                            r.files_skip,
                                            r.files_fail,
                                            r.chunks,
                                            r.unmatched_corrections
                                        );
                                        if !r.errors.is_empty() {
                                            self.status.push_str(" · ");
                                            self.status.push_str(&r.errors.join("；"));
                                        }
                                    }
                                    Err(e) => self.status = format!("导入失败: {e}"),
                                }
                            }
                            self.refresh_info();
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
                    if ui
                        .checkbox(&mut self.always_on_top, "置顶")
                        .changed()
                    {
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
                    if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        self.do_search();
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
                if hits.is_empty() {
                    ui.label(RichText::new("没有结果。先导入 testdata/sample-campaign 再搜「独眼酒保」。").weak());
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
                            ui.label(RichText::new(&h.chunk.entity_type).color(Color32::from_rgb(160, 140, 90)));
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
            fonts.font_data.insert(
                "cjk".into(),
                FontData::from_owned(bytes).into(),
            );
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
