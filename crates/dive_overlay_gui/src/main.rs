use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

use dive_overlay_core::csv_data::{
    format_duration, load_samples, parse_column_map, parse_duration_to_seconds, parse_fields,
};
use dive_overlay_core::ffprobe::probe_video;
use dive_overlay_core::model::Field;
use dive_overlay_core::overlay::{build_overlay_lines, draw_depth_graph, draw_overlay, OverlayCache};
use dive_overlay_core::pipeline::{
    extract_frame_at, process_clip, Codec, EncoderInfo, OutputMode, Preset, ProcessingOptions,
};
use dive_overlay_core::ClipJob;

mod update_check;
use update_check::UpdateStatus;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1000.0, 700.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Dive Data Overlay",
        options,
        Box::new(|cc| {
            let mut app = App::default();
            let (tx, rx) = std::sync::mpsc::channel();
            app.update_rx = Some(rx);
            update_check::spawn_check(tx, cc.egui_ctx.clone());
            Ok(Box::new(app))
        }),
    )
}

#[derive(Clone)]
struct ClipEntry {
    video_path: PathBuf,
    video_sync_sec: f64,
    csv_sync_mmss: String,
    output_path: PathBuf,
}

enum WorkerEvent {
    Log(String),
    Progress(f32),
    Fps(f64),
    Encoder(String),
    Done(Result<(), String>),
}

struct ClipDialogState {
    editing_index: Option<usize>,
    video: String,
    video_sync: String,
    csv_sync: String,
    output: String,
    error: Option<String>,
}

impl ClipDialogState {
    fn new_add() -> Self {
        Self {
            editing_index: None,
            video: String::new(),
            video_sync: "0.0".to_string(),
            csv_sync: "0:00".to_string(),
            output: String::new(),
            error: None,
        }
    }

    fn new_edit(idx: usize, entry: &ClipEntry) -> Self {
        Self {
            editing_index: Some(idx),
            video: entry.video_path.display().to_string(),
            video_sync: format!("{}", entry.video_sync_sec),
            csv_sync: entry.csv_sync_mmss.clone(),
            output: entry.output_path.display().to_string(),
            error: None,
        }
    }

    fn validate(&self) -> Result<ClipEntry, String> {
        let video = self.video.trim();
        if video.is_empty() {
            return Err("Please select a video file.".to_string());
        }
        let output = self.output.trim();
        if output.is_empty() {
            return Err("Please specify an output path.".to_string());
        }
        let video_sync_sec: f64 = self
            .video_sync
            .trim()
            .parse()
            .map_err(|_| "Video sync must be a number.".to_string())?;
        parse_duration_to_seconds(self.csv_sync.trim()).map_err(|e| format!("CSV sync invalid: {e}"))?;

        Ok(ClipEntry {
            video_path: PathBuf::from(video),
            video_sync_sec,
            csv_sync_mmss: self.csv_sync.trim().to_string(),
            output_path: PathBuf::from(output),
        })
    }
}

struct PreviewState {
    clip_index: usize,
    texture: egui::TextureHandle,
    size: egui::Vec2,
    lines: Vec<String>,
}

struct App {
    csv_path: String,
    fields: String,
    codec: String,
    preset: String,
    hw_accel: bool,
    column_map: String,
    show_graph: bool,
    interpolate: bool,
    mode: OutputMode,
    entries: Vec<ClipEntry>,
    selected: Option<usize>,
    status: String,
    progress: f32,
    fps: f64,
    encoder_info: String,
    log_lines: Vec<String>,
    running: bool,
    cancel_flag: Arc<AtomicBool>,
    worker_rx: Option<Receiver<WorkerEvent>>,
    worker_handle: Option<JoinHandle<()>>,
    dialog: Option<ClipDialogState>,
    preview: Option<PreviewState>,
    update_rx: Option<Receiver<UpdateStatus>>,
    update_available: Option<(String, String)>,
}

impl Default for App {
    fn default() -> Self {
        Self {
            csv_path: String::new(),
            fields: "time,depth,temp,pressure,hr".to_string(),
            codec: "auto".to_string(),
            preset: "veryfast".to_string(),
            hw_accel: true,
            column_map: String::new(),
            show_graph: false,
            interpolate: false,
            mode: OutputMode::Overlay,
            entries: Vec::new(),
            selected: None,
            status: "Ready".to_string(),
            progress: 0.0,
            fps: 0.0,
            encoder_info: String::new(),
            log_lines: Vec::new(),
            running: false,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            worker_rx: None,
            worker_handle: None,
            dialog: None,
            preview: None,
            update_rx: None,
            update_available: None,
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_worker(&ctx);
        self.poll_update_check();

        egui::Panel::top("general").show(ui, |ui| {
            self.ui_update_banner(ui);
            self.ui_general(ui);
        });
        egui::Panel::bottom("execution").show(ui, |ui| {
            self.ui_execution(ui, &ctx);
        });
        egui::CentralPanel::default().show(ui, |ui| {
            self.ui_clip_table(ui);
        });

        self.ui_clip_dialog(&ctx);
        self.ui_preview_window(&ctx);

        if self.running {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
}

impl App {
    fn poll_worker(&mut self, ctx: &egui::Context) {
        let mut done = None;
        if let Some(rx) = &self.worker_rx {
            while let Ok(event) = rx.try_recv() {
                match event {
                    WorkerEvent::Log(line) => self.log_lines.push(line),
                    WorkerEvent::Progress(p) => self.progress = p,
                    WorkerEvent::Fps(f) => self.fps = f,
                    WorkerEvent::Encoder(desc) => self.encoder_info = desc,
                    WorkerEvent::Done(result) => done = Some(result),
                }
            }
        }
        if let Some(result) = done {
            self.running = false;
            self.worker_rx = None;
            if let Some(handle) = self.worker_handle.take() {
                let _ = handle.join();
            }
            match result {
                Ok(()) => {
                    self.status = "Done".to_string();
                    self.progress = 100.0;
                }
                Err(e) => {
                    self.status = "Error".to_string();
                    self.log_lines.push(format!("Error: {e}"));
                }
            }
            ctx.request_repaint();
        }
    }

    fn poll_update_check(&mut self) {
        let Some(rx) = &self.update_rx else { return };
        let Ok(status) = rx.try_recv() else { return };
        self.update_rx = None;
        match status {
            UpdateStatus::Available { version, url } => {
                self.update_available = Some((version, url));
            }
            UpdateStatus::UpToDate => {}
            UpdateStatus::Error(e) => {
                self.log_lines.push(format!("Update check failed: {e}"));
            }
        }
    }

    fn ui_update_banner(&mut self, ui: &mut egui::Ui) {
        let Some((version, url)) = &self.update_available else { return };
        ui.horizontal(|ui| {
            ui.colored_label(egui::Color32::from_rgb(184, 92, 0), format!("New version available: {version}"));
            ui.hyperlink_to("Download", url);
        });
        ui.separator();
    }

    fn ui_general(&mut self, ui: &mut egui::Ui) {
        ui.heading("Dive Data Overlay");
        ui.horizontal(|ui| {
            ui.label("CSV:");
            ui.text_edit_singleline(&mut self.csv_path);
            if ui.button("Browse").clicked() {
                if let Some(path) = rfd::FileDialog::new().add_filter("CSV", &["csv"]).pick_file() {
                    self.csv_path = path.display().to_string();
                }
            }
            ui.checkbox(&mut self.interpolate, "Interpolate between samples");
        });
        ui.horizontal(|ui| {
            ui.label("Fields (time,depth,temp,pressure,hr):");
            ui.text_edit_singleline(&mut self.fields);
        });
        ui.horizontal(|ui| {
            ui.label("Column mapping (e.g. time=TIME,depth=Depth):");
            ui.text_edit_singleline(&mut self.column_map);
        });
        ui.horizontal(|ui| {
            ui.label("Mode:");
            egui::ComboBox::from_id_salt("mode")
                .selected_text(match self.mode {
                    OutputMode::Overlay => "Overlay (burned-in)",
                    OutputMode::Subtitles => "Subtitles (toggle on/off)",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.mode, OutputMode::Overlay, "Overlay (burned-in)");
                    ui.selectable_value(&mut self.mode, OutputMode::Subtitles, "Subtitles (toggle on/off)");
                });

            ui.add_enabled_ui(self.mode == OutputMode::Overlay, |ui| {
                ui.checkbox(&mut self.show_graph, "Show depth profile");
            });
        });
        let subtitle_mode = self.mode == OutputMode::Subtitles;
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!subtitle_mode, |ui| {
                ui.label("Codec:");
                egui::ComboBox::from_id_salt("codec")
                    .selected_text(self.codec.clone())
                    .show_ui(ui, |ui| {
                        for opt in ["auto", "avc1", "H264", "hevc", "mp4v", "XVID", "MJPG"] {
                            ui.selectable_value(&mut self.codec, opt.to_string(), opt);
                        }
                    });

                let hw_applies = Codec::parse(&self.codec).supports_preset();
                ui.add_enabled_ui(hw_applies, |ui| {
                    ui.label("Preset:");
                    egui::ComboBox::from_id_salt("preset")
                        .selected_text(self.preset.clone())
                        .show_ui(ui, |ui| {
                            for opt in [
                                "ultrafast", "superfast", "veryfast", "faster", "fast", "medium", "slow", "slower",
                                "veryslow", "placebo",
                            ] {
                                ui.selectable_value(&mut self.preset, opt.to_string(), opt);
                            }
                        });
                });
            });
        });
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!subtitle_mode, |ui| {
                let hw_applies = Codec::parse(&self.codec).supports_preset();
                ui.add_enabled_ui(hw_applies, |ui| {
                    ui.checkbox(&mut self.hw_accel, "Hardware acceleration (if available)");
                });
            });
        });
        if subtitle_mode {
            ui.label("Note: subtitle mode copies video/audio losslessly and additionally writes a .srt file next to the output.");
        }
        if !self.encoder_info.is_empty() {
            ui.label(format!("Encoder: {}", self.encoder_info));
        }
    }

    fn ui_clip_table(&mut self, ui: &mut egui::Ui) {
        ui.heading("Clips");
        ui.horizontal(|ui| {
            if ui.button("Add clip").clicked() {
                self.dialog = Some(ClipDialogState::new_add());
            }
            if ui.button("Edit clip").clicked() {
                if let Some(idx) = self.selected {
                    self.dialog = Some(ClipDialogState::new_edit(idx, &self.entries[idx]));
                }
            }
            if ui.button("Remove clip").clicked() {
                if let Some(idx) = self.selected {
                    self.entries.remove(idx);
                    self.selected = None;
                }
            }
            if ui.button("Sync preview").clicked() {
                if let Some(idx) = self.selected {
                    let ctx = ui.ctx().clone();
                    self.render_preview(idx, &ctx);
                }
            }
        });

        let entries_snapshot: Vec<(String, f64, String, String)> = self
            .entries
            .iter()
            .map(|e| {
                (
                    e.video_path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    e.video_sync_sec,
                    e.csv_sync_mmss.clone(),
                    e.output_path.display().to_string(),
                )
            })
            .collect();
        let selected = self.selected;
        let mut clicked_index: Option<usize> = None;

        egui_extras::TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .column(egui_extras::Column::initial(260.0))
            .column(egui_extras::Column::initial(110.0))
            .column(egui_extras::Column::initial(90.0))
            .column(egui_extras::Column::remainder())
            .header(20.0, |mut header| {
                header.col(|ui| {
                    ui.strong("Video");
                });
                header.col(|ui| {
                    ui.strong("Video Sync (s)");
                });
                header.col(|ui| {
                    ui.strong("CSV Sync");
                });
                header.col(|ui| {
                    ui.strong("Output");
                });
            })
            .body(|mut body| {
                for (i, (name, video_sync, csv_sync, output)) in entries_snapshot.iter().enumerate() {
                    body.row(20.0, |mut row| {
                        row.col(|ui| {
                            if ui.selectable_label(selected == Some(i), name).clicked() {
                                clicked_index = Some(i);
                            }
                        });
                        row.col(|ui| {
                            ui.label(format!("{video_sync:.2}"));
                        });
                        row.col(|ui| {
                            ui.label(csv_sync);
                        });
                        row.col(|ui| {
                            ui.label(output);
                        });
                    });
                }
            });

        if let Some(i) = clicked_index {
            self.selected = Some(i);
        }
    }

    fn ui_clip_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.dialog else { return };
        let mut open = true;
        let mut submit = false;
        let mut cancel = false;
        let title = if dialog.editing_index.is_some() {
            "Edit clip"
        } else {
            "Add clip"
        };

        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Video:");
                    ui.text_edit_singleline(&mut dialog.video);
                    if ui.button("Browse").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("Video", &["mp4", "mov", "avi", "mkv"])
                            .pick_file()
                        {
                            dialog.video = path.display().to_string();
                            if dialog.output.trim().is_empty() {
                                let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("output");
                                dialog.output = path.with_file_name(format!("{stem}_overlay.mp4")).display().to_string();
                            }
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Video sync (seconds):");
                    ui.text_edit_singleline(&mut dialog.video_sync);
                });
                ui.horizontal(|ui| {
                    ui.label("CSV sync (mm:ss or hh:mm:ss):");
                    ui.text_edit_singleline(&mut dialog.csv_sync);
                });
                ui.horizontal(|ui| {
                    ui.label("Output:");
                    ui.text_edit_singleline(&mut dialog.output);
                    if ui.button("Save as").clicked() {
                        if let Some(path) = rfd::FileDialog::new().add_filter("MP4", &["mp4"]).save_file() {
                            dialog.output = path.display().to_string();
                        }
                    }
                });
                if let Some(err) = &dialog.error {
                    ui.colored_label(egui::Color32::RED, err);
                }
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                    if ui.button("OK").clicked() {
                        submit = true;
                    }
                });
            });

        if submit {
            match dialog.validate() {
                Ok(entry) => {
                    let editing_index = dialog.editing_index;
                    self.dialog = None;
                    match editing_index {
                        Some(idx) => self.entries[idx] = entry,
                        None => self.entries.push(entry),
                    }
                }
                Err(e) => dialog.error = Some(e),
            }
        } else if cancel || !open {
            self.dialog = None;
        }
    }

    fn ui_preview_window(&mut self, ctx: &egui::Context) {
        if self.preview.is_none() {
            return;
        }
        let clip_index = self.preview.as_ref().unwrap().clip_index;
        let mut open = true;
        let mut adjust: Option<f64> = None;
        let mut reload = false;

        egui::Window::new("Sync preview")
            .open(&mut open)
            .resizable(true)
            .show(ctx, |ui| {
                if let Some(entry) = self.entries.get(clip_index) {
                    ui.label(format!(
                        "Video: {} | Video sync: {:.2}s | CSV sync: {}",
                        entry
                            .video_path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default(),
                        entry.video_sync_sec,
                        entry.csv_sync_mmss,
                    ));
                }

                ui.horizontal(|ui| {
                    for (label, delta) in [
                        ("-1 min", -60.0),
                        ("-30 s", -30.0),
                        ("-5 s", -5.0),
                        ("-0.5 s", -0.5),
                        ("+0.5 s", 0.5),
                        ("+5 s", 5.0),
                        ("+30 s", 30.0),
                        ("+1 min", 60.0),
                    ] {
                        if ui.button(label).clicked() {
                            adjust = Some(delta);
                        }
                    }
                    if ui.button("Reload").clicked() {
                        reload = true;
                    }
                });

                if let Some(preview) = &self.preview {
                    let available = ui.available_size();
                    let scale = (available.x / preview.size.x).min(1.0).max(0.05);
                    let display_size = preview.size * scale;
                    let sized = egui::load::SizedTexture::new(preview.texture.id(), display_size);
                    ui.add(egui::Image::from_texture(sized));
                    ui.label(preview.lines.join(" | "));
                }
            });

        if !open {
            self.preview = None;
            return;
        }

        if let Some(delta) = adjust {
            if let Some(entry) = self.entries.get_mut(clip_index) {
                let current = parse_duration_to_seconds(&entry.csv_sync_mmss).unwrap_or(0.0);
                entry.csv_sync_mmss = format_duration((current + delta).max(0.0));
            }
            self.render_preview(clip_index, ctx);
        } else if reload {
            self.render_preview(clip_index, ctx);
        }
    }

    fn render_preview(&mut self, idx: usize, ctx: &egui::Context) {
        match self.try_render_preview(idx, ctx) {
            Ok(preview) => self.preview = Some(preview),
            Err(e) => self.log_lines.push(format!("Preview failed: {e}")),
        }
    }

    fn try_render_preview(&self, idx: usize, ctx: &egui::Context) -> anyhow::Result<PreviewState> {
        let entry = self
            .entries
            .get(idx)
            .ok_or_else(|| anyhow::anyhow!("Invalid clip index"))?;
        if !entry.video_path.exists() {
            anyhow::bail!("Video not found: {}", entry.video_path.display());
        }

        let csv_path = PathBuf::from(self.csv_path.trim());
        if self.csv_path.trim().is_empty() || !csv_path.exists() {
            anyhow::bail!("Please select a valid CSV file first.");
        }

        let fields = parse_fields(&self.fields)?;
        let column_map = parse_column_map(&self.column_map)?;
        let csv_sync_sec = parse_duration_to_seconds(&entry.csv_sync_mmss)?;
        let samples = load_samples(&csv_path, &column_map)?;
        let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();

        let mut frame = extract_frame_at(&entry.video_path, entry.video_sync_sec)?;
        let lines = build_overlay_lines(&fields, &samples, &times, csv_sync_sec, self.interpolate);
        draw_overlay(&mut frame, &lines, &mut OverlayCache::new());
        if self.show_graph {
            draw_depth_graph(&mut frame, &samples, &times, csv_sync_sec, 600.0);
        }

        let (w, h) = frame.dimensions();
        let color_image = egui::ColorImage::from_rgb([w as usize, h as usize], frame.as_raw());
        let size = egui::vec2(w as f32, h as f32);
        let texture = ctx.load_texture(format!("preview-{idx}"), color_image, egui::TextureOptions::default());

        Ok(PreviewState {
            clip_index: idx,
            texture,
            size,
            lines,
        })
    }

    fn ui_execution(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!self.running, egui::Button::new("Start processing"))
                .clicked()
            {
                self.start_processing(ctx);
            }
            if ui.add_enabled(self.running, egui::Button::new("Cancel")).clicked() {
                self.cancel_flag.store(true, Ordering::Relaxed);
                self.log_lines.push("Cancel requested...".to_string());
                self.status = "Cancelling...".to_string();
            }
            ui.label(&self.status);
        });

        let bar_text = if self.running && self.fps > 0.0 {
            format!("{}% ({:.1} fps)", self.progress as i32, self.fps)
        } else {
            format!("{}%", self.progress as i32)
        };
        ui.add(egui::ProgressBar::new(self.progress / 100.0).text(bar_text));

        egui::ScrollArea::vertical().max_height(150.0).stick_to_bottom(true).show(ui, |ui| {
            for line in &self.log_lines {
                ui.label(line);
            }
        });
    }

    fn start_processing(&mut self, ctx: &egui::Context) {
        if self.running {
            return;
        }

        let csv_path = PathBuf::from(self.csv_path.trim());
        if self.csv_path.trim().is_empty() {
            self.log_lines.push("Error: please select a CSV file.".to_string());
            return;
        }
        if !csv_path.exists() {
            self.log_lines.push(format!("Error: CSV not found: {}", csv_path.display()));
            return;
        }
        if self.entries.is_empty() {
            self.log_lines.push("Error: please add at least one clip.".to_string());
            return;
        }

        let fields = match parse_fields(&self.fields) {
            Ok(f) => f,
            Err(e) => {
                self.log_lines.push(format!("Error: invalid field list: {e}"));
                return;
            }
        };
        let column_map = match parse_column_map(&self.column_map) {
            Ok(m) => m,
            Err(e) => {
                self.log_lines.push(format!("Error: invalid column mapping: {e}"));
                return;
            }
        };

        for entry in &self.entries {
            if !entry.video_path.exists() {
                self.log_lines
                    .push(format!("Error: video not found: {}", entry.video_path.display()));
                return;
            }
        }

        let codec = Codec::parse(&self.codec);
        let preset = Preset::parse(&self.preset).unwrap_or_default();
        let hw_accel = self.hw_accel;
        let show_graph = self.show_graph;
        let interpolate = self.interpolate;
        let mode = self.mode;
        let entries = self.entries.clone();

        self.cancel_flag = Arc::new(AtomicBool::new(false));
        let cancel_flag = self.cancel_flag.clone();

        let (tx, rx) = std::sync::mpsc::channel();
        self.worker_rx = Some(rx);
        self.running = true;
        self.status = "Processing...".to_string();
        self.progress = 0.0;
        self.fps = 0.0;
        self.encoder_info = String::new();
        self.log_lines.push("Starting processing...".to_string());

        let worker_ctx = ctx.clone();
        let handle = std::thread::spawn(move || {
            let result = run_worker(
                csv_path,
                fields,
                column_map,
                entries,
                codec,
                preset,
                hw_accel,
                show_graph,
                interpolate,
                mode,
                &cancel_flag,
                &tx,
                &worker_ctx,
            );
            let _ = tx.send(WorkerEvent::Done(result));
            worker_ctx.request_repaint();
        });
        self.worker_handle = Some(handle);
    }
}

#[allow(clippy::too_many_arguments)]
fn run_worker(
    csv_path: PathBuf,
    fields: Vec<Field>,
    column_map: HashMap<String, String>,
    entries: Vec<ClipEntry>,
    codec: Codec,
    preset: Preset,
    hw_accel: bool,
    show_graph: bool,
    interpolate: bool,
    mode: OutputMode,
    cancel_flag: &Arc<AtomicBool>,
    tx: &Sender<WorkerEvent>,
    ctx: &egui::Context,
) -> Result<(), String> {
    let samples = load_samples(&csv_path, &column_map).map_err(|e| e.to_string())?;
    let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();
    let total = entries.len();

    let mut clip_frame_totals = Vec::with_capacity(total);
    for entry in &entries {
        let info = probe_video(&entry.video_path).map_err(|e| e.to_string())?;
        clip_frame_totals.push(info.estimated_frames.unwrap_or(0).max(1));
    }
    let total_frames_all: u64 = clip_frame_totals.iter().sum::<u64>().max(total as u64).max(1);

    let mut base_done_frames: u64 = 0;

    for (idx, entry) in entries.iter().enumerate() {
        let _ = tx.send(WorkerEvent::Log(format!(
            "[{}/{}] {} -> {}",
            idx + 1,
            total,
            entry
                .video_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            entry
                .output_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
        )));
        ctx.request_repaint();

        let csv_sync_sec = parse_duration_to_seconds(&entry.csv_sync_mmss).map_err(|e| e.to_string())?;
        let job = ClipJob {
            video_path: entry.video_path.clone(),
            output_path: entry.output_path.with_extension("mp4"),
            video_sync_sec: entry.video_sync_sec,
            csv_sync_sec,
            video_start_utc: None,
        };
        let clip_total = clip_frame_totals[idx];
        let options = ProcessingOptions {
            fields: fields.clone(),
            codec,
            preset,
            hw_accel,
            show_graph,
            mode,
            interpolate,
        };

        let tx_progress = tx.clone();
        let ctx_progress = ctx.clone();
        let tx_encoder = tx.clone();
        let mut last_instant = std::time::Instant::now();
        let mut last_done: u64 = 0;
        let completed = process_clip(
            &job,
            &samples,
            &times,
            &options,
            cancel_flag,
            move |done, total_reported| {
                let effective_total = if total_reported > 0 { total_reported } else { clip_total };
                let effective_done = done.min(effective_total);
                let global_done = base_done_frames + effective_done;
                let percent = (global_done as f64 * 100.0 / total_frames_all as f64) as f32;
                let _ = tx_progress.send(WorkerEvent::Progress(percent));

                // Skip the final call: it fires after the encoder has been
                // awaited (mp4 finalization), so its elapsed time includes that
                // wait rather than just frame processing, which would read as a
                // bogus last-moment slowdown.
                let is_final_call = total_reported > 0 && done >= total_reported;
                let elapsed = last_instant.elapsed().as_secs_f64();
                if !is_final_call && elapsed >= 0.1 {
                    let fps = done.saturating_sub(last_done) as f64 / elapsed;
                    let _ = tx_progress.send(WorkerEvent::Fps(fps));
                    last_instant = std::time::Instant::now();
                    last_done = done;
                }

                ctx_progress.request_repaint();
            },
            move |info: &EncoderInfo| {
                let _ = tx_encoder.send(WorkerEvent::Encoder(info.describe()));
            },
        )
        .map_err(|e| e.to_string())?;

        if !completed {
            let _ = tx.send(WorkerEvent::Log("Cancelled: processing stopped.".to_string()));
            return Ok(());
        }

        base_done_frames += clip_total;
        let percent = (base_done_frames as f64 * 100.0 / total_frames_all as f64) as f32;
        let _ = tx.send(WorkerEvent::Progress(percent));
        let _ = tx.send(WorkerEvent::Log(format!(
            "[{}/{}] Done: {}",
            idx + 1,
            total,
            job.output_path.display()
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    fn make_test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("dive_overlay_gui_worker_test").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn synth_clip(dir: &Path, name: &str, duration_secs: u32, fps: u32) -> PathBuf {
        let path = dir.join(name);
        let video_src = format!("testsrc=size=160x120:rate={fps}:duration={duration_secs}");
        let status = Command::new("ffmpeg")
            .args(["-y", "-f", "lavfi", "-i", &video_src])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .arg(&path)
            .status()
            .expect("failed to run ffmpeg");
        assert!(status.success());
        path
    }

    /// Exercises the exact function `start_processing` spawns on its
    /// background thread, end to end, without going through the eframe UI.
    /// This is what a click-through of "Start processing" would
    /// ultimately trigger -- calling it directly gives deterministic proof
    /// that the worker/channel wiring produces progress + a finished output,
    /// which a screenshot of a dialog box would not.
    #[test]
    fn run_worker_processes_clip_and_reports_progress_and_completion() {
        let dir = make_test_dir("basic");
        let clip = synth_clip(&dir, "input.mp4", 1, 5);
        let csv_path = dir.join("dive.csv");
        std::fs::write(&csv_path, "sample time (min),sample depth (m)\n0:00,1.0\n0:01,2.0\n").unwrap();
        let output = dir.join("out.mp4");

        let entry = ClipEntry {
            video_path: clip,
            video_sync_sec: 0.0,
            csv_sync_mmss: "0:00".to_string(),
            output_path: output.clone(),
        };

        let ctx = egui::Context::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel_flag = Arc::new(AtomicBool::new(false));

        let result = run_worker(
            csv_path,
            vec![Field::Time, Field::Depth],
            HashMap::new(),
            vec![entry],
            Codec::Auto,
            Preset::VeryFast,
            false,
            false,
            false,
            OutputMode::Overlay,
            &cancel_flag,
            &tx,
            &ctx,
        );

        assert!(result.is_ok(), "run_worker failed: {result:?}");
        assert!(output.exists());

        let events: Vec<WorkerEvent> = rx.try_iter().collect();
        assert!(
            events.iter().any(|e| matches!(e, WorkerEvent::Progress(p) if *p > 0.0)),
            "expected at least one progress event"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, WorkerEvent::Log(l) if l.contains("Done"))),
            "expected a completion log line"
        );
    }

    #[test]
    fn run_worker_stops_early_when_cancel_flag_is_set_before_start() {
        let dir = make_test_dir("cancelled");
        let clip = synth_clip(&dir, "input.mp4", 2, 10);
        let csv_path = dir.join("dive.csv");
        std::fs::write(&csv_path, "sample time (min),sample depth (m)\n0:00,1.0\n").unwrap();
        let output = dir.join("out.mp4");

        let entry = ClipEntry {
            video_path: clip,
            video_sync_sec: 0.0,
            csv_sync_mmss: "0:00".to_string(),
            output_path: output.clone(),
        };

        let ctx = egui::Context::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel_flag = Arc::new(AtomicBool::new(true));

        let result = run_worker(
            csv_path,
            vec![Field::Depth],
            HashMap::new(),
            vec![entry],
            Codec::Auto,
            Preset::VeryFast,
            false,
            false,
            false,
            OutputMode::Overlay,
            &cancel_flag,
            &tx,
            &ctx,
        );

        assert!(result.is_ok());
        let events: Vec<WorkerEvent> = rx.try_iter().collect();
        assert!(events
            .iter()
            .any(|e| matches!(e, WorkerEvent::Log(l) if l.contains("Cancelled"))));
    }
}
