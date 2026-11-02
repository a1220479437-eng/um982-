#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(test)]
mod memory_tests;
mod model;
mod protocol;
mod recording;
mod serial;
mod statistics;
#[cfg(test)]
mod test_date;
mod theme;

use eframe::egui::{self, FontId, Pos2, RichText, Sense, Stroke, Vec2};
use model::Model;
use protocol::{Decoder, Record};
use std::{
    path::PathBuf,
    sync::mpsc::{self, Receiver, TryRecvError},
    time::{Duration, Instant},
};
use theme::{Palette, Theme};

const LIVE_ARCHIVE_BYTES: usize = 8 * 1024 * 1024;
const LIVE_ARCHIVE_FRAMES: usize = 200_000;

fn archive_live(records: &mut Vec<Record>, bytes: &mut usize, record: Record) -> bool {
    let size = record.retained_bytes();
    if records.len() >= LIVE_ARCHIVE_FRAMES || size > LIVE_ARCHIVE_BYTES - *bytes {
        return false;
    }
    *bytes += size;
    records.push(record);
    true
}

const CHART_WINDOW_SECONDS: f64 = 60.0;

#[cfg(debug_assertions)]
const REPLAY_TITLE: &str = "离线测试（仅开发验证）";
#[cfg(not(debug_assertions))]
const REPLAY_TITLE: &str = "数据回放";
#[cfg(debug_assertions)]
const REPLAY_DIR: &str = "data";
#[cfg(not(debug_assertions))]
const REPLAY_DIR: &str = "logs";

fn replay_files(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_file()
                || !path
                    .extension()
                    .is_some_and(|s| s.eq_ignore_ascii_case("dat"))
            {
                return None;
            }
            let modified = entry.metadata().ok().and_then(|m| m.modified().ok());
            Some((path, modified))
        })
        .collect();
    files.sort_by(|(a, at), (b, bt)| bt.cmp(at).then_with(|| b.cmp(a)));
    files.into_iter().map(|(path, _)| path).collect()
}

fn main() -> eframe::Result {
    eframe::run_native(
        "北斗 · UM982 导航监测",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([1320.0, 900.0])
                .with_min_inner_size([1000.0, 700.0]),
            ..Default::default()
        },
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

struct App {
    model: Model,
    statistics: statistics::Statistics,
    decoder: Decoder,
    records: Vec<Record>,
    times: Vec<f64>,
    cursor: usize,
    clock: f64,
    playing: bool,
    speed: f64,
    last_tick: Instant,
    connection: Option<serial::Connection>,
    load_rx: Option<Receiver<(PathBuf, Result<LoadedReplay, String>)>>,
    live_start: Instant,
    ports: Vec<String>,
    port: String,
    baud: u32,
    command: String,
    files: Vec<PathBuf>,
    selected: Option<PathBuf>,
    source: String,
    notice: String,
    tab: usize,
    theme: Theme,
    filter: String,
    follow: bool,
    live_limit_warned: bool,
    archive_bytes: usize,
    automatic_reset: bool,
}

struct LoadedReplay {
    records: Vec<Record>,
    stats: protocol::Stats,
    times: Vec<f64>,
    statistics: statistics::Statistics,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut fonts = egui::FontDefinitions::default();
        // Use system CJK fonts rather than shipping a large proprietary font.
        for path in [
            "C:/Windows/Fonts/msyh.ttc",
            "C:/Windows/Fonts/simhei.ttf",
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/System/Library/Fonts/PingFang.ttc",
        ] {
            if let Ok(bytes) = std::fs::read(path) {
                fonts
                    .font_data
                    .insert("cjk".into(), egui::FontData::from_owned(bytes).into());
                for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                    fonts
                        .families
                        .entry(family)
                        .or_default()
                        .insert(0, "cjk".into());
                }
                break;
            }
        }
        cc.egui_ctx.set_fonts(fonts);
        Theme::default().apply(&cc.egui_ctx);
        let mut style = (*cc.egui_ctx.style()).clone();
        style.spacing.item_spacing = Vec2::new(10.0, 9.0);
        cc.egui_ctx.set_style(style);
        let files = replay_files(std::path::Path::new(REPLAY_DIR));
        let mut app = Self {
            model: Model::default(),
            statistics: statistics::Statistics::default(),
            decoder: Decoder::default(),
            records: vec![],
            times: vec![],
            cursor: 0,
            clock: 0.0,
            playing: false,
            speed: 1.0,
            last_tick: Instant::now(),
            connection: None,
            load_rx: None,
            live_start: Instant::now(),
            ports: vec![],
            port: "COM13".into(),
            baud: 115200,
            command: "BESTNAVA 0.2".into(),
            files,
            selected: None,
            source: "等待串口输入".into(),
            notice: "请选择串口并连接（8N1，无流控）".into(),
            tab: 0,
            theme: Theme::default(),
            filter: String::new(),
            follow: true,
            live_limit_warned: false,
            archive_bytes: 0,
            automatic_reset: true,
        };
        app.refresh_ports();
        // File input is opt-in; normal startup waits for a serial connection.
        let mut args = std::env::args_os().skip(1);
        if args.next().is_some_and(|arg| arg == "--replay")
            && let Some(path) = args.next()
        {
            app.load(PathBuf::from(path));
        }
        app
    }
    fn refresh_ports(&mut self) {
        match serialport::available_ports() {
            Ok(ports) => {
                self.ports = ports.into_iter().map(|p| p.port_name).collect();
                self.ports.sort();
                if self.connection.is_none() && !self.ports.contains(&self.port) {
                    self.port = self.ports.first().cloned().unwrap_or_default();
                }
                self.notice = format!("已刷新：发现 {} 个串口", self.ports.len());
            }
            Err(e) => self.notice = format!("枚举串口失败：{e}"),
        }
    }
    fn reset(&mut self) {
        self.statistics = statistics::Statistics::default();
        self.model = Model::default();
        self.decoder = Decoder::default();
        self.records = Vec::new();
        self.times = Vec::new();
        self.archive_bytes = 0;
        self.cursor = 0;
        self.clock = 0.0;
        self.playing = false;
        self.live_limit_warned = false;
    }
    fn load(&mut self, path: PathBuf) {
        if self.load_rx.is_some() {
            self.notice = "正在读取日志，请稍候".into();
            return;
        }
        let (tx, rx) = mpsc::channel();
        let worker_path = path.clone();
        match std::thread::Builder::new()
            .name("replay-loader".into())
            .spawn(move || {
                let result = std::fs::read(&worker_path)
                    .map_err(|e| e.to_string())
                    .map(|bytes| {
                        let (records, stats) = protocol::decode_log(&bytes);
                        let times = model::timeline(&records);
                        let statistics = statistics::Statistics::from_records(&records, &times);
                        LoadedReplay {
                            records,
                            stats,
                            times,
                            statistics,
                        }
                    });
                let _ = tx.send((worker_path, result));
            }) {
            Ok(_) => {
                self.load_rx = Some(rx);
                self.notice = format!("正在读取并解析 {}", path.display());
            }
            Err(e) => self.notice = format!("无法启动日志读取线程：{e}"),
        }
    }

    fn finish_load(&mut self) {
        let Some(rx) = self.load_rx.take() else {
            return;
        };
        match rx.try_recv() {
            Ok((_path, Ok(loaded))) if loaded.records.is_empty() => {
                self.notice = "没有可解析的 UM982 帧，当前数据保持不变".into();
            }
            Ok((path, Ok(loaded))) => {
                self.connection = None;
                self.reset();
                self.decoder.stats = loaded.stats;
                self.records = loaded.records;
                self.times = loaded.times;
                self.statistics = loaded.statistics;
                self.selected = Some(path.clone());
                self.source = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                self.seek(0.0);
                self.notice = format!(
                    "已加载 {} 帧；可播放、拖动进度或导出全部日志",
                    self.records.len()
                );
            }
            Ok((path, Err(e))) => self.notice = format!("读取 {} 失败：{e}", path.display()),
            Err(TryRecvError::Empty) => self.load_rx = Some(rx),
            Err(TryRecvError::Disconnected) => self.notice = "日志读取线程异常退出".into(),
        }
    }
    fn seek(&mut self, t: f64) {
        self.model = Model::default();
        self.cursor = 0;
        self.clock = t;
        while self.cursor < self.records.len() && self.times[self.cursor] <= t {
            self.model
                .apply(&self.records[self.cursor], self.times[self.cursor]);
            self.cursor += 1;
        }
        self.last_tick = Instant::now();
    }
    fn tick(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f64();
        self.last_tick = now;
        if self.connection.is_some() {
            self.clock = self.live_start.elapsed().as_secs_f64();
            let mut error = None;
            for _ in 0..256 {
                let event = self
                    .connection
                    .as_ref()
                    .and_then(|c| c.events.try_recv().ok());
                match event {
                    Some(serial::Event::Bytes(bytes)) => {
                        for r in self.decoder.feed(&bytes) {
                            self.model.apply(&r, self.clock);
                            self.statistics.apply(&r, self.clock);
                            // Once full, preserve a contiguous prefix for replay/export.
                            if !self.live_limit_warned
                                && !archive_live(&mut self.records, &mut self.archive_bytes, r)
                            {
                                self.notice =
                                    "会话归档已达 8 MiB / 200000 帧上限：继续采集，完整数据见录制日志；CSV 仅含已归档帧".into();
                                self.live_limit_warned = true;
                            }
                        }
                    }
                    Some(serial::Event::Reset(path)) => {
                        self.reset();
                        self.statistics = statistics::Statistics::live();
                        self.live_start = Instant::now();
                        self.selected = None;
                        if let Some(connection) = self.connection.as_mut() {
                            connection.log_path = path.clone();
                        }
                        self.files = replay_files(std::path::Path::new(REPLAY_DIR));
                        self.notice = format!("数据已重置；录制至 {}", path.display());
                    }
                    Some(serial::Event::Error(e)) => {
                        error = Some(e);
                        break;
                    }
                    None => break,
                }
            }
            self.statistics.advance(self.clock);
            if let Some(e) = error {
                self.disconnect();
                self.notice = format!("串口断开：{e}");
            }
        } else if self.playing {
            self.clock =
                (self.clock + dt * self.speed).min(self.times.last().copied().unwrap_or(0.0));
            while self.cursor < self.records.len() && self.times[self.cursor] <= self.clock {
                self.model
                    .apply(&self.records[self.cursor], self.times[self.cursor]);
                self.cursor += 1;
            }
            if self.cursor >= self.records.len() {
                self.playing = false;
            }
        }
    }
    fn disconnect(&mut self) {
        self.connection = None;
        self.files = replay_files(std::path::Path::new(REPLAY_DIR));
        self.decoder.finish();
        self.times = model::timeline(&self.records);
        let end = self.times.last().copied().unwrap_or(0.0);
        self.seek(end);
    }
    fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(12.0);
        ui.heading(RichText::new("北斗 / BEIDOU").color(Palette::for_ui(ui).accent));
        ui.label(
            RichText::new("UM982  NAVIGATION CONSOLE")
                .size(10.0)
                .color(Palette::for_ui(ui).muted),
        );
        ui.add_space(16.0);
        let previous_theme = self.theme;
        egui::ComboBox::from_label("显示主题")
            .selected_text(self.theme.label())
            .show_ui(ui, |ui| {
                for theme in [Theme::Dark, Theme::Light] {
                    ui.selectable_value(&mut self.theme, theme, theme.label());
                }
            });
        if self.theme != previous_theme {
            self.theme.apply(ui.ctx());
            ui.ctx().request_repaint();
        }
        ui.separator();
        ui.heading("串口采集");
        ui.label("串口端口");
        ui.add_enabled_ui(self.connection.is_none(), |ui| {
            egui::ComboBox::from_id_salt("ports")
                .width(205.0)
                .selected_text(if self.port.is_empty() {
                    "未发现串口，请刷新"
                } else {
                    &self.port
                })
                .show_ui(ui, |ui| {
                    for port in &self.ports {
                        ui.selectable_value(&mut self.port, port.clone(), port);
                    }
                });
        });
        if ui.small_button("刷新端口").clicked() {
            self.refresh_ports();
        }
        egui::ComboBox::from_label("波特率")
            .selected_text(self.baud.to_string())
            .show_ui(ui, |ui| {
                for b in [9600, 19200, 38400, 57600, 115200, 230400, 460800, 921600] {
                    ui.selectable_value(&mut self.baud, b, b.to_string());
                }
            });
        if self.connection.is_some() {
            if ui.button("断开串口").clicked() {
                self.disconnect();
                self.notice = "已断开，可回放本次会话".into();
            }
        } else if ui
            .add_enabled(!self.port.is_empty(), egui::Button::new("连接串口"))
            .clicked()
        {
            match serial::connect(&self.port, self.baud, self.automatic_reset) {
                Ok(c) => {
                    self.reset();
                    self.statistics = statistics::Statistics::live();
                    self.selected = None;
                    self.notice = format!("实时接收中；原始数据录制至 {}", c.log_path.display());
                    self.connection = Some(c);
                    self.live_start = Instant::now();
                    self.source = format!("{} · {} bps", self.port, self.baud);
                }
                Err(e) => self.notice = format!("连接失败：{e}"),
            }
        }
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label("重置模式");
            let (rect, response) = ui.allocate_exact_size(Vec2::new(36.0, 20.0), Sense::click());
            if response.clicked() {
                self.automatic_reset = !self.automatic_reset;
                if let Some(c) = &self.connection
                    && c.commands
                        .send(serial::Command::SetAutomatic(self.automatic_reset))
                        .is_err()
                {
                    self.notice = "切换模式失败：串口线程已停止".into();
                }
            }
            response.widget_info(|| {
                egui::WidgetInfo::selected(
                    egui::WidgetType::Checkbox,
                    true,
                    self.automatic_reset,
                    "自动重置",
                )
            });
            let color = if self.automatic_reset { CYAN } else { MUTED };
            ui.painter().rect_filled(rect, 10.0, color);
            let x = if self.automatic_reset {
                rect.right() - 10.0
            } else {
                rect.left() + 10.0
            };
            ui.painter()
                .circle_filled(Pos2::new(x, rect.center().y), 7.0, Color32::WHITE);
            ui.label(if self.automatic_reset {
                "自动"
            } else {
                "手动"
            });
        });
        if self.automatic_reset {
            ui.label(
                RichText::new("断流 ≥ 2 秒后，下一完整帧触发重置及文件滚动")
                    .size(11.0)
                    .color(MUTED),
            );
        } else if ui
            .add_enabled(
                self.connection.is_some(),
                egui::Button::new("重置数据并滚动文件"),
            )
            .clicked()
            && let Some(c) = &self.connection
        {
            self.notice = match c.commands.send(serial::Command::Reset) {
                Ok(()) => "正在重置数据并滚动文件…".into(),
                Err(e) => format!("重置失败：{e}"),
            };
        }
        ui.add_space(10.0);
        ui.label("手动指令（自动添加 CRLF）");
        ui.add(egui::TextEdit::singleline(&mut self.command).desired_width(210.0));
        if ui
            .add_enabled(self.connection.is_some(), egui::Button::new("发送指令"))
            .clicked()
        {
            if !self.command.contains(['\r', '\n']) && !self.command.trim().is_empty() {
                if let Some(c) = &self.connection {
                    self.notice = match c.commands.send(serial::Command::Send(self.command.clone()))
                    {
                        Ok(()) => format!("已发送：{}", self.command),
                        Err(e) => format!("发送失败：{e}"),
                    };
                }
            } else {
                self.notice = "指令不能为空或包含换行".into();
            }
        }
        ui.add_space(10.0);
        ui.separator();
        if ui
            .add_enabled(
                !self.records.is_empty(),
                egui::Button::new("导出整个会话 CSV…"),
            )
            .clicked()
            && let Some(path) = rfd::FileDialog::new()
                .set_file_name("um982.csv")
                .save_file()
        {
            self.notice = match model::export(&path, &self.records) {
                Ok(()) => format!("已导出 {} 帧至 {}", self.records.len(), path.display()),
                Err(e) => format!("导出失败：{e}"),
            };
        }
        ui.add_space(10.0);
        ui.add_enabled_ui(self.connection.is_none(), |ui| {
            ui.collapsing(REPLAY_TITLE, |ui| {
                #[cfg(debug_assertions)]
                let hint = "本地日志仅用于验证解析与显示，实际采集使用串口。";
                #[cfg(not(debug_assertions))]
                let hint = "logs/ 录制文件（最新优先），选择后可播放。";
                ui.label(
                    RichText::new(hint)
                        .size(12.0)
                        .color(Palette::for_ui(ui).muted),
                );
                if ui.button("刷新文件列表").clicked() {
                    self.files = replay_files(std::path::Path::new(REPLAY_DIR));
                }
                if ui.button("打开 DAT / LOG…").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .set_directory(REPLAY_DIR)
                        .add_filter("串口日志", &["DAT", "dat", "log", "txt"])
                        .pick_file()
                {
                    self.load(path);
                }
                if self.files.is_empty() {
                    ui.label(format!("{REPLAY_DIR}/ 中暂无 DAT 文件"));
                }
                let mut load = None;
                for file in &self.files {
                    let name = file.file_name().unwrap_or_default().to_string_lossy();
                    if ui
                        .selectable_label(self.selected.as_ref() == Some(file), name)
                        .clicked()
                    {
                        load = Some(file.clone());
                    }
                }
                if let Some(path) = load {
                    self.load(path);
                }
            });
        });
        ui.label(RichText::new("协议：BESTNAVA / UNIHEADINGA / RECTIMEA\nCRC32 · 跨包拼帧 · 2 秒超时\n每类曲线保留最近 10000 个测量点\n无在线地图依赖").size(12.0).color(Palette::for_ui(ui).muted));
    }
    fn playback(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(&self.source).strong());
            ui.label(if self.connection.is_some() {
                RichText::new("● LIVE").color(Palette::for_ui(ui).valid)
            } else if self.selected.is_some() {
                RichText::new(format!("● {REPLAY_TITLE}")).color(Palette::for_ui(ui).accent)
            } else if !self.records.is_empty() {
                RichText::new("○ 串口已断开").color(Palette::for_ui(ui).muted)
            } else {
                RichText::new("○ 串口未连接").color(Palette::for_ui(ui).muted)
            });
        });
        if self.connection.is_none() && !self.records.is_empty() {
            ui.horizontal(|ui| {
                if ui
                    .button(if self.playing { "暂停" } else { "播放" })
                    .clicked()
                {
                    if self.cursor >= self.records.len() {
                        self.seek(0.0);
                    }
                    self.playing = !self.playing;
                }
                if ui.button("重置").clicked() {
                    self.playing = false;
                    self.seek(0.0);
                }
                egui::ComboBox::from_id_salt("speed")
                    .selected_text(format!("{}×", self.speed))
                    .show_ui(ui, |ui| {
                        for s in [0.25, 0.5, 1.0, 2.0, 5.0, 10.0] {
                            ui.selectable_value(&mut self.speed, s, format!("{s}×"));
                        }
                    });
                let end = self.times.last().copied().unwrap_or(0.0);
                let mut t = self.clock;
                if ui
                    .add(egui::Slider::new(&mut t, 0.0..=end).suffix(" s"))
                    .changed()
                {
                    self.seek(t);
                }
                ui.label(format!(
                    "/ {end:.1} s   {} / {} 帧",
                    self.cursor,
                    self.records.len()
                ));
            });
        }
    }
    fn heading_failure_analysis(&self, ui: &mut egui::Ui) {
        ui.heading("航向无效分段与成因线索");
        let segments = &self.statistics.heading_segments;
        if segments.is_empty() {
            ui.label("暂无航向无效帧。");
            return;
        }
        ui.label(format!(
            "共 {} 段无效，保留最近 {} 段；按连续无效航向帧合并，不因失效原因变化而拆分。",
            segments.len() + self.statistics.discarded_segments,
            segments.len()
        ));
        ui.label("时间范围为首个至最后一个无效帧（不是恢复时刻）；仅含报文无效段，纯超时见下方区间明细。dev 为航向标准差；卫星数为 UNIHEADINGA 双频卫星数。");
        // Compute before entering the horizontal scroll area, whose content
        // width is unconstrained. Reserve room for the other five columns.
        let time_column_width = (ui.available_width() - 440.0).clamp(220.0, 500.0);
        egui::ScrollArea::horizontal()
            .id_salt("heading_segments_table")
            .show(ui, |ui| {
                egui::Grid::new("heading_segments")
                    .striped(true)
                    .show(ui, |ui| {
                        for title in [
                            "段",
                            "时间区间",
                            "帧数",
                            "dev / °",
                            "双频卫星",
                            "俯仰角 / °",
                        ] {
                            if title == "时间区间" {
                                ui.add_sized(
                                    [time_column_width, 24.0],
                                    egui::Label::new(RichText::new(title).strong())
                                        .halign(egui::Align::Center),
                                );
                            } else {
                                ui.strong(title);
                            }
                        }
                        ui.end_row();
                        for (i, segment) in segments.iter().enumerate() {
                            ui.label((i + 1).to_string());
                            ui.add_sized(
                                [time_column_width, 0.0],
                                egui::Label::new(format!(
                                    "{} ～ {}（{:.3} ～ {:.3} s）",
                                    self.statistics.format_time(segment.start_time),
                                    self.statistics.format_time(segment.end_time),
                                    segment.start,
                                    segment.end
                                ))
                                .wrap()
                                .halign(egui::Align::Center),
                            );
                            ui.label(segment.frames.to_string());
                            ui.label(format!(
                                "{:.3} → {:.3}\n范围 {:.3}–{:.3}",
                                segment.first.std,
                                segment.last.std,
                                segment.std_range[0],
                                segment.std_range[1]
                            ));
                            ui.label(format!(
                                "{} → {}\n范围 {}–{}",
                                segment.first.dual,
                                segment.last.dual,
                                segment.satellite_range[0],
                                segment.satellite_range[1]
                            ));
                            ui.label(format!(
                                "{:+.2} → {:+.2}\n范围 {:+.2}–{:+.2}",
                                segment.first.pitch,
                                segment.last.pitch,
                                segment.pitch_range[0],
                                segment.pitch_range[1]
                            ));
                            ui.end_row();
                        }
                    });
            });
        ui.add_space(8.0);
        ui.strong("各段解状态变化与直接失效原因");
        for (i, segment) in segments.iter().enumerate() {
            ui.group(|ui| {
                ui.label(format!("第 {} 段：{}", i + 1, segment.solution_chain()));
                ui.label(format!(
                    "直接触发条件：{}",
                    segment
                        .reasons
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("；")
                ));
                let mut evidence = Vec::new();
                if segment
                    .before
                    .as_ref()
                    .is_some_and(|h| matches!(h.kind.as_str(), "NARROW_INT" | "L1_INT"))
                    && segment
                        .kinds
                        .iter()
                        .any(|k| matches!(k.as_str(), "NARROW_FLOAT" | "L1_FLOAT"))
                {
                    evidence.push("整数解退化为浮点解");
                }
                if segment
                    .before
                    .as_ref()
                    .is_some_and(|h| segment.satellite_range[0] < h.dual)
                {
                    evidence.push("双频卫星数相对前序有效帧减少");
                }
                if segment.std_range[1] >= 1.0 {
                    evidence.push("航向标准差达到或超过 1° 有效性门限");
                }
                if !evidence.is_empty() {
                    ui.label(format!("观测到：{}。", evidence.join("；")));
                }
                ui.label(format!(
                    "段内俯仰变化范围 {:.2}°；基线 {:.4} → {:.4} m。",
                    segment.pitch_range[1] - segment.pitch_range[0],
                    segment.first.baseline,
                    segment.last.baseline
                ));
            });
        }
        if let [Some(valid), Some(invalid)] = self.statistics.baseline_means() {
            ui.label(format!(
                "基线长度均值对比：无效帧 {invalid:.4} m / 有效帧 {valid:.4} m，差值 {:.1} mm。",
                (invalid - valid).abs() * 1000.0
            ));
        }
        ui.colored_label(Palette::for_ui(ui).muted, "成因提示：解类型退化、卫星数下降、俯仰变化与标准差超限可作为关联线索，但仅凭这些报文不能确认转动、遮挡或失锁，也不能仅凭基线均值排除硬件故障。");
    }
    fn statistics_page(&self, ui: &mut egui::Ui) {
        ui.heading("失效统计");
        ui.label(format!(
            "数据源：{}    统计范围：0.000 – {:.3} s",
            self.source, self.statistics.end
        ));
        ui.label(format!(
            "区间时间基准：{}；开始、结束显示报文日期时间（毫秒）。",
            self.statistics.time_basis()
        ));
        ui.label("串口实时累计；文件统计整个日志，不随播放或拖动改变。未恢复区间的结束时间随统计终点更新。");
        ui.label(format!("实时统计明细最多保留 {} 条区间 / 航向分段；累计计数和时长不截断。段内解类型变化及原因最多各保留 32 项，完整数据见录制日志。", statistics::LIVE_DETAILS));
        ui.label("各类报文从首次收到后独立统计；连续相同原因合并，原因改变则拆分。末尾未恢复区间截至统计终点。");
        ui.label("CRC / 格式错误仅计入底部错误总数，不推断损坏帧的类型和失效时间。未收到过的类型不推断失效。");
        ui.label("持续时长仍按会话时间计算；文件负跳变或超过 10 秒的跳变压缩为 0.2 秒，因此可能与日期时间差不同。超时边界按上一报文时间推算。");
        ui.separator();
        self.heading_failure_analysis(ui);
        ui.separator();
        ui.heading("全部类型失效区间");
        egui::Grid::new("failure_summary")
            .striped(true)
            .show(ui, |ui| {
                for title in ["类型", "报文总数", "无效报文", "失效区间数", "失效总时长"]
                {
                    ui.strong(title);
                }
                ui.end_row();
                for (channel, name) in statistics::NAMES.iter().enumerate() {
                    let (total, invalid) = self.statistics.counts(channel);
                    let (intervals, duration) = self.statistics.interval_summary(channel);
                    ui.label(*name);
                    ui.label(total.to_string());
                    ui.label(invalid.to_string());
                    ui.label(intervals.to_string());
                    ui.label(format!("{duration:.3} s"));
                    ui.end_row();
                }
            });
        ui.separator();
        if self.statistics.intervals.is_empty() {
            ui.label("暂无失效区间");
            return;
        }
        ui.label("区间明细（按发生顺序；各类型可能重叠，总时长不可跨类型相加）");
        egui::ScrollArea::both()
            .id_salt("failure_intervals")
            .min_scrolled_height(420.0)
            .max_height(720.0)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let mut order: Vec<_> = (0..self.statistics.intervals.len()).collect();
                order.sort_by(|&a, &b| {
                    self.statistics.intervals[a]
                        .start
                        .total_cmp(&self.statistics.intervals[b].start)
                });
                egui::Grid::new("failure_details")
                    .striped(true)
                    .show(ui, |ui| {
                        for title in [
                            "类型",
                            "开始时间",
                            "结束时间",
                            "会话持续 / s",
                            "状态",
                            "原因",
                        ] {
                            let width = match title {
                                "开始时间" | "结束时间" => 230.0,
                                "原因" => 320.0,
                                _ => 100.0,
                            };
                            ui.add_sized(
                                [width, 28.0],
                                egui::Label::new(RichText::new(title).strong()),
                            );
                        }
                        ui.end_row();
                        for i in order {
                            let interval = &self.statistics.intervals[i];
                            ui.label(statistics::NAMES[interval.channel]);
                            ui.add_sized(
                                [230.0, 48.0],
                                egui::Label::new(self.statistics.format_time(interval.start_time))
                                    .wrap()
                                    .halign(egui::Align::Center),
                            );
                            ui.add_sized(
                                [230.0, 48.0],
                                egui::Label::new(self.statistics.format_time(interval.end_time))
                                    .wrap()
                                    .halign(egui::Align::Center),
                            );
                            ui.label(format!("{:.3}", interval.end - interval.start));
                            ui.label(if self.statistics.ongoing(i) {
                                "截至终点未恢复"
                            } else {
                                "已结束"
                            });
                            ui.add_sized([320.0, 48.0], egui::Label::new(&interval.reason).wrap());
                            ui.end_row();
                        }
                    });
            });
    }
    fn dashboard(&self, ui: &mut egui::Ui) {
        let pos = self.model.position.as_ref();
        let heading = self.model.heading.as_ref();
        let pv = pos.is_some_and(|(p, t)| p.valid() && self.clock - t <= 2.0);
        let hv = heading.is_some_and(|(h, t)| h.valid() && self.clock - t <= 2.0);
        ui.columns(4, |cols| {
            metric(
                &mut cols[0],
                "定位解 / POSITION",
                pos.map(|(p, _)| p.kind.as_str()).unwrap_or("--"),
                pv,
            );
            metric(
                &mut cols[1],
                "使用 / 跟踪卫星",
                &pos.map(|(p, _)| format!("{} / {}", p.used, p.tracked))
                    .unwrap_or("--".into()),
                pv,
            );
            metric(
                &mut cols[2],
                "航向角 / HEADING",
                &heading
                    .map(|(h, _)| format!("{:.2}°", h.angle))
                    .unwrap_or("--".into()),
                hv,
            );
            metric(
                &mut cols[3],
                "地速 / SPEED",
                &pos.map(|(p, _)| format!("{:.3} m/s", p.speed))
                    .unwrap_or("--".into()),
                pv,
            );
        });
        ui.add_space(8.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.strong("双天线定向 · 真北为 0°");
            ui.columns(2, |cols| {
                cols[0].vertical(|ui| {
                    compass(ui, self.model.compass_angle(), hv);
                    if let Some((h, _)) = heading {
                        ui.label(format!("{} · σ {:.3}° · 双频 {} 星", h.kind, h.std, h.dual));
                        ui.label(format!("基线 {:.4} m    俯仰 {:.2}°", h.baseline, h.pitch));
                        ui.colored_label(
                            if hv {
                                Palette::for_ui(ui).valid
                            } else {
                                Palette::for_ui(ui).muted
                            },
                            if hv {
                                "航向有效"
                            } else {
                                "航向无效 / 超时（仍显示当前收到的角度）"
                            },
                        );
                    } else {
                        ui.label("等待 UNIHEADINGA…");
                    }
                });
                cols[1].vertical(|ui| {
                    ui.strong("当前俯仰角");
                    pitch_ruler(ui, heading.map(|(h, _)| h.pitch), hv);
                    ui.add_space(8.0);
                    ui.strong("俯仰角变化");
                    ui.label(
                        RichText::new(
                            "有效俯仰（绿）/ 无效俯仰（红）· 北斗时间（UTC），最近 60 秒",
                        )
                        .size(12.0)
                        .color(Palette::for_ui(ui).muted),
                    );
                    let pitches = self
                        .model
                        .pitches
                        .iter()
                        .filter_map(|p| self.model.chart_time(p[0]).map(|x| [x, p[1], p[2]]));
                    pitch_chart(ui, pitches, 180.0, self.model.chart_now(self.clock));
                });
            });
        });
        ui.add_space(8.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.strong("从首帧完整数据到有效");
            egui::Grid::new("acquisition_delays")
                .num_columns(2)
                .show(ui, |ui| {
                    for (label, index) in [("时间有效", 0), ("定位有效", 1), ("航向有效", 2)]
                    {
                        ui.label(RichText::new(label).color(Palette::for_ui(ui).muted));
                        ui.label(
                            self.model
                                .acquisition_delay(index)
                                .map(format_duration)
                                .unwrap_or_else(|| {
                                    self.model
                                        .first_frame_at
                                        .map(|first| {
                                            format!(
                                                "尚未有效 · 已等待 {}",
                                                format_duration((self.clock - first).max(0.0))
                                            )
                                        })
                                        .unwrap_or_else(|| "等待首帧完整数据".into())
                                }),
                        );
                        ui.end_row();
                    }
                });
        });
        ui.add_space(8.0);
        ui.columns(2, |cols| {
            if let Some((p, _)) = pos {
                egui::Grid::new("position").num_columns(2).show(&mut cols[0], |ui| {
                    for (k, v) in [
                        ("纬度", format!("{:.11}°", p.lat)),
                        ("经度", format!("{:.11}°", p.lon)),
                        ("高程（原始字段）", format!("{:.4} m", p.height)),
                        ("位置标准差 纬/经/高", format!("{:.3} / {:.3} / {:.3} m", p.std[0], p.std[1], p.std[2])),
                        ("地面航迹 / 垂向速度", format!("{:.2}° / {:.3} m/s", p.course, p.up)),
                    ] {
                        ui.label(RichText::new(k).color(Palette::for_ui(ui).muted));
                        ui.label(v);
                        ui.end_row();
                    }
                });
            }
            cols[1].label(RichText::new("接收机时间").color(Palette::for_ui(&cols[1]).muted));
            if let Some((Some(t), last)) = &self.model.time {
                cols[1].label(format!("UTC  {}", t.format("%Y-%m-%d %H:%M:%S%.3f")));
                cols[1].label(format!("北京 {}", (*t + chrono::Duration::hours(8)).format("%Y-%m-%d %H:%M:%S%.3f")));
                cols[1].colored_label(
                    if self.clock - last <= 2.0 { Palette::for_ui(&cols[1]).valid } else { Palette::for_ui(&cols[1]).warning },
                    if self.clock - last <= 2.0 { "时间有效" } else { "时间超时" },
                );
            } else {
                cols[1].label("时间无效 / 尚未接收");
            }
            cols[1].label(
                RichText::new("有效航向使用环形 EMA（α=0.3）；无效航向仍绘制当前原始角度并灰显。\n定位和定向独立判定。")
                    .size(12.0)
                    .color(Palette::for_ui(&cols[1]).muted),
            );
        });
    }
}
fn format_duration(seconds: f64) -> String {
    if seconds < 60.0 {
        format!("{seconds:.2} s")
    } else {
        let minutes = (seconds / 60.0).floor() as u64;
        let remaining = seconds - minutes as f64 * 60.0;
        format!("{minutes} 分 {remaining:.2} s")
    }
}
impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.finish_load();
        self.tick();
        ctx.request_repaint_after(Duration::from_millis(33));
        let dropped = ctx.input(|i| i.raw.dropped_files.first().and_then(|f| f.path.clone()));
        if let Some(path) = dropped {
            if self.connection.is_none() {
                self.load(path);
            } else {
                self.notice = format!("串口采集中，不能切换为{REPLAY_TITLE}；请先断开串口");
            }
        }
        egui::SidePanel::left("sidebar")
            .exact_width(255.0)
            .resizable(false)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.sidebar(ui));
            });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&self.notice).color(Palette::for_ui(ui).muted));
                ui.separator();
                let s = &self.decoder.stats;
                ui.label(format!(
                    "校验通过 {}   CRC错误 {}   格式/残帧 {}   其他 {}",
                    s.valid, s.crc_errors, s.malformed, s.ignored
                ));
            });
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            self.playback(ui);
            ui.separator();
            ui.horizontal(|ui| {
                for (i, name) in ["导航总览", "趋势曲线", "原始报文", "失效统计"]
                    .iter()
                    .enumerate()
                {
                    ui.selectable_value(&mut self.tab, i, *name);
                }
            });
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| match self.tab {
                0 => self.dashboard(ui),
                1 => {
                    ui.label("高程（m） / 北斗时间（UTC），有效绿色 / 无效红色，最近 60 秒");
                    let heights = self
                        .model
                        .heights
                        .iter()
                        .filter_map(|p| self.model.chart_time(p[0]).map(|x| [x, p[1], p[2]]));
                    chart(
                        ui,
                        heights,
                        180.0,
                        false,
                        self.model.chart_now(self.clock),
                    );
                    ui.label(
                        "航向角（°，0/360°跨越处保留原始跳变） / 北斗时间（UTC），有效绿色 / 无效红色，最近 60 秒",
                    );
                    let angles = self
                        .model
                        .angles
                        .iter()
                        .filter_map(|p| self.model.chart_time(p[0]).map(|x| [x, p[1], p[2]]));
                    chart(
                        ui,
                        angles,
                        180.0,
                        false,
                        self.model.chart_now(self.clock),
                    );
                    ui.label("地速（m/s） / 北斗时间（UTC），有效绿色 / 无效红色，最近 60 秒");
                    let speeds = self
                        .model
                        .speeds
                        .iter()
                        .filter_map(|p| self.model.chart_time(p[0]).map(|x| [x, p[1], p[2]]));
                    chart(
                        ui,
                        speeds,
                        150.0,
                        false,
                        self.model.chart_now(self.clock),
                    );
                    ui.label("参与定位卫星数 / 北斗时间（UTC），有效绿色 / 无效红色");
                    let satellites = self
                        .model
                        .satellites
                        .iter()
                        .filter_map(|p| self.model.chart_time(p[0]).map(|x| [x, p[1], p[2]]));
                    chart(
                        ui,
                        satellites,
                        150.0,
                        false,
                        self.model.chart_now(self.clock),
                    );
                }
                3 => self.statistics_page(ui),
                _ => {
                    ui.horizontal(|ui| {
                        ui.label("过滤");
                        ui.text_edit_singleline(&mut self.filter);
                        ui.checkbox(&mut self.follow, "跟随最新");
                    });
                    ui.label("最近 300 条 CRC 校验通过的报文（不显示损坏帧）");
                    egui::ScrollArea::both()
                        .id_salt("raw")
                        .max_height(600.0)
                        .stick_to_bottom(self.follow)
                        .show(ui, |ui| {
                            for line in &self.model.logs {
                                if line.contains(&self.filter) {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(line).monospace().size(13.0).color(Palette::for_ui(ui).muted),
                                        )
                                        .wrap_mode(egui::TextWrapMode::Extend),
                                    );
                                }
                            }
                        });
                }
            });
        });
    }
}
fn metric(ui: &mut egui::Ui, title: &str, value: &str, valid: bool) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_min_width((ui.available_width() - 12.0).max(0.0));
        ui.label(
            RichText::new(title)
                .size(12.0)
                .color(Palette::for_ui(ui).muted),
        );
        ui.label(RichText::new(value).size(23.0).color(if valid {
            Palette::for_ui(ui).accent
        } else {
            Palette::for_ui(ui).muted
        }));
        ui.label(
            RichText::new(if valid {
                "● 有效"
            } else {
                "○ 无效 / 超时"
            })
            .size(11.0)
            .color(if valid {
                Palette::for_ui(ui).valid
            } else {
                Palette::for_ui(ui).muted
            }),
        );
    });
}
fn compass(ui: &mut egui::Ui, angle: Option<f64>, valid: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 210.0), Sense::hover());
    let painter = ui.painter();
    let center = rect.center();
    let radius = 79.0;
    let colors = Palette::for_ui(ui);
    painter.circle_stroke(center, radius, Stroke::new(2.0_f32, colors.muted));
    for (label, dx, dy) in [
        ("N", 0.0, -1.0),
        ("E", 1.0, 0.0),
        ("S", 0.0, 1.0),
        ("W", -1.0, 0.0),
    ] {
        painter.text(
            center + Vec2::new(dx, dy) * (radius + 15.0),
            egui::Align2::CENTER_CENTER,
            label,
            FontId::proportional(16.0),
            colors.text,
        );
    }
    for i in 0..36 {
        let a = i as f32 * std::f32::consts::TAU / 36.0;
        let dir = Vec2::new(a.sin(), -a.cos());
        painter.line_segment(
            [center + dir * (radius - 5.0), center + dir * radius],
            Stroke::new(1.5_f32, colors.muted),
        );
    }
    if let Some(angle) = angle {
        let a = angle.to_radians() as f32;
        let dir = Vec2::new(a.sin(), -a.cos());
        painter.arrow(
            center - dir * 20.0,
            dir * 85.0,
            Stroke::new(4.0_f32, if valid { colors.accent } else { colors.muted }),
        );
    }
    painter.circle_filled(center, 4.0, colors.text);
}
fn pitch_ruler(ui: &mut egui::Ui, pitch: Option<f64>, valid: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 150.0), Sense::hover());
    let painter = ui.painter();
    let colors = Palette::for_ui(ui);
    let color = if valid { colors.accent } else { colors.muted };
    if let Some(pitch) = pitch {
        painter.text(
            Pos2::new(rect.center().x, rect.top() + 18.0),
            egui::Align2::CENTER_CENTER,
            format!("{pitch:.2}°"),
            FontId::proportional(24.0),
            color,
        );
    } else {
        painter.text(
            Pos2::new(rect.center().x, rect.top() + 18.0),
            egui::Align2::CENTER_CENTER,
            "--",
            FontId::proportional(24.0),
            colors.muted,
        );
    }

    let left = rect.left() + 16.0;
    let right = rect.right() - 16.0;
    let y = rect.top() + 82.0;
    painter.line_segment(
        [Pos2::new(left, y), Pos2::new(right, y)],
        Stroke::new(2.0_f32, colors.muted),
    );
    for degree in (-90..=90).step_by(10) {
        let x = egui::remap(degree as f32, -90.0..=90.0, left..=right);
        let major = degree % 30 == 0;
        let tick_end = y + if major { 17.0 } else { 9.0 };
        painter.line_segment(
            [Pos2::new(x, y), Pos2::new(x, tick_end)],
            Stroke::new(1.5_f32, colors.muted),
        );
        if major {
            painter.text(
                Pos2::new(x, tick_end + 11.0),
                egui::Align2::CENTER_CENTER,
                format!("{degree}°"),
                FontId::monospace(12.0),
                colors.text,
            );
        }
    }
    if let Some(pitch) = pitch {
        let x = egui::remap(pitch.clamp(-90.0, 90.0) as f32, -90.0..=90.0, left..=right);
        painter.line_segment(
            [Pos2::new(x, y - 10.0), Pos2::new(x, y + 22.0)],
            Stroke::new(3.0_f32, color),
        );
        painter.circle_filled(Pos2::new(x, y - 12.0), 5.0, color);
    }
}
fn pitch_chart(
    ui: &mut egui::Ui,
    points: impl Iterator<Item = [f64; 3]> + Clone,
    height: f32,
    now: Option<f64>,
) {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
    let painter = ui.painter();
    let colors = Palette::for_ui(ui);
    painter.rect_filled(rect, 4.0, colors.chart);
    let area = rect.shrink2(Vec2::new(40.0, 24.0));
    let Some(now) = now else {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "等待北斗时间有效",
            FontId::proportional(16.0),
            colors.muted,
        );
        return;
    };
    let axis_now = points.clone().last().map_or(now, |point| now.max(point[0]));
    let (x_min, x_max) = chart_window_range_iter(axis_now, points.clone());
    let project = |point: [f64; 2]| {
        Pos2::new(
            area.left() + ((point[0] - x_min) / (x_max - x_min)) as f32 * area.width(),
            area.bottom() - ((point[1] + 90.0) / 180.0) as f32 * area.height(),
        )
    };

    for i in 0..=4 {
        let fraction = i as f32 / 4.0;
        let x = area.left() + area.width() * fraction;
        let y = area.bottom() - area.height() * fraction;
        let grid = Stroke::new(1.0_f32, colors.grid);
        painter.line_segment(
            [Pos2::new(x, area.top()), Pos2::new(x, area.bottom())],
            grid,
        );
        painter.line_segment(
            [Pos2::new(area.left(), y), Pos2::new(area.right(), y)],
            grid,
        );
        painter.text(
            Pos2::new(x, area.bottom() + 5.0),
            egui::Align2::CENTER_TOP,
            format_gnss_axis(x_min + (x_max - x_min) * fraction as f64),
            FontId::monospace(12.0),
            colors.text,
        );
        painter.text(
            Pos2::new(area.left() - 4.0, y),
            egui::Align2::RIGHT_CENTER,
            format!("{:.0}", -90.0 + 180.0 * fraction as f64),
            FontId::monospace(12.0),
            colors.text,
        );
    }

    let visible = |point: &[f64; 3]| point[0] >= x_min && point[0] <= x_max;
    let mut visible_points = points.clone().filter(visible);
    if let Some(mut previous) = visible_points.next() {
        let color = if previous[2] > 0.5 {
            colors.valid
        } else {
            colors.invalid
        };
        painter.circle_filled(project([previous[0], previous[1]]), 2.5, color);
        for point in visible_points {
            painter.line_segment(
                [
                    project([previous[0], previous[1]]),
                    project([point[0], point[1]]),
                ],
                Stroke::new(
                    3.0_f32,
                    if point[2] > 0.5 {
                        colors.valid
                    } else {
                        colors.invalid
                    },
                ),
            );
            painter.circle_filled(
                project([point[0], point[1]]),
                3.5,
                if point[2] > 0.5 {
                    colors.valid
                } else {
                    colors.invalid
                },
            );
            previous = point;
        }
    } else {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "等待俯仰数据",
            FontId::proportional(16.0),
            colors.muted,
        );
    }
    if let Some(mouse) = response.hover_pos()
        && area.contains(mouse)
    {
        let hovered_time =
            x_min + ((mouse.x - area.left()) / area.width()) as f64 * (x_max - x_min);
        painter.text(
            mouse + Vec2::new(8.0, -8.0),
            egui::Align2::LEFT_BOTTOM,
            format_gnss_axis(hovered_time),
            FontId::monospace(12.0),
            colors.text,
        );
    }
}
fn chart(
    ui: &mut egui::Ui,
    points: impl Iterator<Item = [f64; 3]> + Clone,
    height: f32,
    equal: bool,
    now: Option<f64>,
) {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
    let p = ui.painter();
    let colors = Palette::for_ui(ui);
    p.rect_filled(rect, 4.0, colors.chart);
    let area = rect.shrink2(Vec2::new(40.0, 24.0));
    let Some(now) = now else {
        p.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "等待北斗时间有效",
            FontId::proportional(16.0),
            colors.muted,
        );
        return;
    };
    let axis_now = points.clone().last().map_or(now, |point| now.max(point[0]));
    let (x_min, x_max) = chart_window_range_iter(axis_now, points.clone());
    let x_span = (x_max - x_min).max(0.001);
    let mut min = [0.0, f64::INFINITY];
    let mut max = [x_span, f64::NEG_INFINITY];
    let visible = |point: &[f64; 3]| point[0] >= x_min && point[0] <= x_max;
    let mut has_visible_points = false;
    for point in points.clone().filter(visible) {
        has_visible_points = true;
        min[1] = min[1].min(point[1]);
        max[1] = max[1].max(point[1]);
    }
    if !has_visible_points {
        min[1] = -1.0;
        max[1] = 1.0;
    }
    let pad = ((max[1] - min[1]) * 0.08).max(if equal { 0.1 } else { 0.01 });
    min[1] -= pad;
    max[1] += pad;
    if equal {
        let scale =
            ((max[0] - min[0]) / area.width() as f64).max((max[1] - min[1]) / area.height() as f64);
        for i in 0..2 {
            let center = (min[i] + max[i]) / 2.0;
            let span = scale * if i == 0 { area.width() } else { area.height() } as f64;
            min[i] = center - span / 2.0;
            max[i] = center + span / 2.0;
        }
    }
    let project = |v: [f64; 2]| {
        Pos2::new(
            area.left() + ((v[0] - x_min) / x_span) as f32 * area.width(),
            area.bottom() - ((v[1] - min[1]) / (max[1] - min[1])) as f32 * area.height(),
        )
    };
    for i in 0..=4 {
        let f = i as f32 / 4.0;
        let x = area.left() + area.width() * f;
        let y = area.bottom() - area.height() * f;
        let grid = Stroke::new(1.0_f32, colors.grid);
        p.line_segment(
            [Pos2::new(x, area.top()), Pos2::new(x, area.bottom())],
            grid,
        );
        p.line_segment(
            [Pos2::new(area.left(), y), Pos2::new(area.right(), y)],
            grid,
        );
        p.text(
            Pos2::new(x, area.bottom() + 5.0),
            egui::Align2::CENTER_TOP,
            format_gnss_axis(x_min + x_span * f as f64),
            FontId::monospace(12.0),
            colors.text,
        );
        p.text(
            Pos2::new(area.left() - 4.0, y),
            egui::Align2::RIGHT_CENTER,
            format!("{:.2}", min[1] + (max[1] - min[1]) * f as f64),
            FontId::monospace(12.0),
            colors.text,
        );
    }
    let mut visible_points = points.clone().filter(visible);
    if let Some(mut previous) = visible_points.next() {
        p.circle_filled(
            project([previous[0], previous[1]]),
            3.5,
            if previous[2] > 0.5 {
                colors.valid
            } else {
                colors.invalid
            },
        );
        for point in visible_points {
            p.line_segment(
                [
                    project([previous[0], previous[1]]),
                    project([point[0], point[1]]),
                ],
                Stroke::new(
                    3.0_f32,
                    if point[2] > 0.5 {
                        colors.valid
                    } else {
                        colors.invalid
                    },
                ),
            );
            p.circle_filled(
                project([point[0], point[1]]),
                3.5,
                if point[2] > 0.5 {
                    colors.valid
                } else {
                    colors.invalid
                },
            );
            previous = point;
        }
    } else {
        p.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "等待曲线数据",
            FontId::proportional(16.0),
            colors.muted,
        );
    }
    if let Some(mouse) = response.hover_pos()
        && let Some(v) = points.filter(visible).min_by(|a, b| {
            project([a[0], a[1]])
                .distance_sq(mouse)
                .total_cmp(&project([b[0], b[1]]).distance_sq(mouse))
        })
    {
        p.circle_stroke(
            project([v[0], v[1]]),
            5.0,
            Stroke::new(2.0_f32, colors.text),
        );
        response.on_hover_text(format!(
            "时间: {}\n数值: {:.4}",
            format_gnss_axis(v[0]),
            v[1]
        ));
    }
}
fn format_gnss_axis(timestamp: f64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis((timestamp * 1000.0).round() as i64)
        .map(|time| time.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "--:--:--".into())
}
#[cfg(test)]
fn chart_window_range(now: f64, points: &[[f64; 3]]) -> (f64, f64) {
    chart_window_range_iter(now, points.iter().copied())
}

fn chart_window_range_iter(now: f64, points: impl Iterator<Item = [f64; 3]> + Clone) -> (f64, f64) {
    let x_max = points.clone().last().map_or(now, |point| now.max(point[0]));
    let window_start = x_max - CHART_WINDOW_SECONDS;
    let x_min = points
        .clone()
        .find(|point| point[0] >= window_start)
        .map_or(window_start, |point| point[0]);
    if x_max - x_min < 0.001 {
        (x_min - 0.5, x_max + 0.5)
    } else {
        (x_min, x_max)
    }
}
