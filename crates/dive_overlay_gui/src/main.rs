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
use dive_overlay_core::overlay::{build_overlay_lines, draw_depth_graph, draw_overlay};
use dive_overlay_core::pipeline::{extract_frame_at, process_clip, Codec, ProcessingOptions};
use dive_overlay_core::ClipJob;

mod update_check;
use update_check::UpdateStatus;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1000.0, 700.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Tauchdaten Overlay",
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
            return Err("Bitte eine Videodatei wählen.".to_string());
        }
        let output = self.output.trim();
        if output.is_empty() {
            return Err("Bitte einen Output-Pfad angeben.".to_string());
        }
        let video_sync_sec: f64 = self
            .video_sync
            .trim()
            .parse()
            .map_err(|_| "Video Sync muss eine Zahl sein.".to_string())?;
        parse_duration_to_seconds(self.csv_sync.trim()).map_err(|e| format!("CSV Sync ungültig: {e}"))?;

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
    column_map: String,
    show_graph: bool,
    entries: Vec<ClipEntry>,
    selected: Option<usize>,
    status: String,
    progress: f32,
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
            column_map: String::new(),
            show_graph: false,
            entries: Vec::new(),
            selected: None,
            status: "Bereit".to_string(),
            progress: 0.0,
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
                    self.status = "Fertig".to_string();
                    self.progress = 100.0;
                }
                Err(e) => {
                    self.status = "Fehler".to_string();
                    self.log_lines.push(format!("Fehler: {e}"));
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
                self.log_lines.push(format!("Update-Prüfung fehlgeschlagen: {e}"));
            }
        }
    }

    fn ui_update_banner(&mut self, ui: &mut egui::Ui) {
        let Some((version, url)) = &self.update_available else { return };
        ui.horizontal(|ui| {
            ui.colored_label(egui::Color32::YELLOW, format!("Neue Version verfügbar: {version}"));
            ui.hyperlink_to("Download", url);
        });
        ui.separator();
    }

    fn ui_general(&mut self, ui: &mut egui::Ui) {
        ui.heading("Tauchdaten Overlay");
        ui.horizontal(|ui| {
            ui.label("CSV:");
            ui.text_edit_singleline(&mut self.csv_path);
            if ui.button("Durchsuchen").clicked() {
                if let Some(path) = rfd::FileDialog::new().add_filter("CSV", &["csv"]).pick_file() {
                    self.csv_path = path.display().to_string();
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label("Felder (time,depth,temp,pressure,hr):");
            ui.text_edit_singleline(&mut self.fields);
        });
        ui.horizontal(|ui| {
            ui.label("Spaltenzuordnung (z.B. time=TIME,depth=Depth):");
            ui.text_edit_singleline(&mut self.column_map);
        });
        ui.horizontal(|ui| {
            ui.label("Codec:");
            egui::ComboBox::from_id_salt("codec")
                .selected_text(self.codec.clone())
                .show_ui(ui, |ui| {
                    for opt in ["auto", "avc1", "H264", "mp4v", "XVID", "MJPG"] {
                        ui.selectable_value(&mut self.codec, opt.to_string(), opt);
                    }
                });
            ui.checkbox(&mut self.show_graph, "Tiefenprofil anzeigen");
        });
    }

    fn ui_clip_table(&mut self, ui: &mut egui::Ui) {
        ui.heading("Clips");
        ui.horizontal(|ui| {
            if ui.button("Clip hinzufügen").clicked() {
                self.dialog = Some(ClipDialogState::new_add());
            }
            if ui.button("Clip bearbeiten").clicked() {
                if let Some(idx) = self.selected {
                    self.dialog = Some(ClipDialogState::new_edit(idx, &self.entries[idx]));
                }
            }
            if ui.button("Clip entfernen").clicked() {
                if let Some(idx) = self.selected {
                    self.entries.remove(idx);
                    self.selected = None;
                }
            }
            if ui.button("Sync Vorschau").clicked() {
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
            "Clip bearbeiten"
        } else {
            "Clip hinzufügen"
        };

        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Video:");
                    ui.text_edit_singleline(&mut dialog.video);
                    if ui.button("Durchsuchen").clicked() {
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
                    ui.label("Video Sync (Sekunden):");
                    ui.text_edit_singleline(&mut dialog.video_sync);
                });
                ui.horizontal(|ui| {
                    ui.label("CSV Sync (mm:ss oder hh:mm:ss):");
                    ui.text_edit_singleline(&mut dialog.csv_sync);
                });
                ui.horizontal(|ui| {
                    ui.label("Output:");
                    ui.text_edit_singleline(&mut dialog.output);
                    if ui.button("Speichern unter").clicked() {
                        if let Some(path) = rfd::FileDialog::new().add_filter("MP4", &["mp4"]).save_file() {
                            dialog.output = path.display().to_string();
                        }
                    }
                });
                if let Some(err) = &dialog.error {
                    ui.colored_label(egui::Color32::RED, err);
                }
                ui.horizontal(|ui| {
                    if ui.button("Abbrechen").clicked() {
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

        egui::Window::new("Sync Vorschau")
            .open(&mut open)
            .resizable(true)
            .show(ctx, |ui| {
                if let Some(entry) = self.entries.get(clip_index) {
                    ui.label(format!(
                        "Video: {} | Video Sync: {:.2}s | CSV Sync: {}",
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
                    if ui.button("Neu laden").clicked() {
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
            Err(e) => self.log_lines.push(format!("Vorschau fehlgeschlagen: {e}")),
        }
    }

    fn try_render_preview(&self, idx: usize, ctx: &egui::Context) -> anyhow::Result<PreviewState> {
        let entry = self
            .entries
            .get(idx)
            .ok_or_else(|| anyhow::anyhow!("Ungültiger Clip-Index"))?;
        if !entry.video_path.exists() {
            anyhow::bail!("Video nicht gefunden: {}", entry.video_path.display());
        }

        let csv_path = PathBuf::from(self.csv_path.trim());
        if self.csv_path.trim().is_empty() || !csv_path.exists() {
            anyhow::bail!("Bitte erst eine gültige CSV-Datei auswählen.");
        }

        let fields = parse_fields(&self.fields)?;
        let column_map = parse_column_map(&self.column_map)?;
        let csv_sync_sec = parse_duration_to_seconds(&entry.csv_sync_mmss)?;
        let samples = load_samples(&csv_path, &column_map)?;
        let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();

        let mut frame = extract_frame_at(&entry.video_path, entry.video_sync_sec)?;
        let lines = build_overlay_lines(&fields, &samples, &times, csv_sync_sec);
        draw_overlay(&mut frame, &lines);
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
                .add_enabled(!self.running, egui::Button::new("Verarbeitung starten"))
                .clicked()
            {
                self.start_processing(ctx);
            }
            if ui.add_enabled(self.running, egui::Button::new("Abbruch")).clicked() {
                self.cancel_flag.store(true, Ordering::Relaxed);
                self.log_lines.push("Abbruch angefordert...".to_string());
                self.status = "Abbruch...".to_string();
            }
            ui.label(&self.status);
        });

        ui.add(egui::ProgressBar::new(self.progress / 100.0).text(format!("{}%", self.progress as i32)));

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
            self.log_lines.push("Fehler: Bitte CSV-Datei auswählen.".to_string());
            return;
        }
        if !csv_path.exists() {
            self.log_lines.push(format!("Fehler: CSV nicht gefunden: {}", csv_path.display()));
            return;
        }
        if self.entries.is_empty() {
            self.log_lines.push("Fehler: Bitte mindestens einen Clip hinzufügen.".to_string());
            return;
        }

        let fields = match parse_fields(&self.fields) {
            Ok(f) => f,
            Err(e) => {
                self.log_lines.push(format!("Fehler: Feldliste ungültig: {e}"));
                return;
            }
        };
        let column_map = match parse_column_map(&self.column_map) {
            Ok(m) => m,
            Err(e) => {
                self.log_lines.push(format!("Fehler: Spaltenzuordnung ungültig: {e}"));
                return;
            }
        };

        for entry in &self.entries {
            if !entry.video_path.exists() {
                self.log_lines
                    .push(format!("Fehler: Video nicht gefunden: {}", entry.video_path.display()));
                return;
            }
        }

        let codec = Codec::parse(&self.codec);
        let show_graph = self.show_graph;
        let entries = self.entries.clone();

        self.cancel_flag = Arc::new(AtomicBool::new(false));
        let cancel_flag = self.cancel_flag.clone();

        let (tx, rx) = std::sync::mpsc::channel();
        self.worker_rx = Some(rx);
        self.running = true;
        self.status = "Verarbeite...".to_string();
        self.progress = 0.0;
        self.log_lines.push("Starte Verarbeitung...".to_string());

        let worker_ctx = ctx.clone();
        let handle = std::thread::spawn(move || {
            let result = run_worker(
                csv_path,
                fields,
                column_map,
                entries,
                codec,
                show_graph,
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
    show_graph: bool,
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
            show_graph,
        };

        let tx_progress = tx.clone();
        let ctx_progress = ctx.clone();
        let completed = process_clip(&job, &samples, &times, &options, cancel_flag, move |done, total_reported| {
            let effective_total = if total_reported > 0 { total_reported } else { clip_total };
            let effective_done = done.min(effective_total);
            let global_done = base_done_frames + effective_done;
            let percent = (global_done as f64 * 100.0 / total_frames_all as f64) as f32;
            let _ = tx_progress.send(WorkerEvent::Progress(percent));
            ctx_progress.request_repaint();
        })
        .map_err(|e| e.to_string())?;

        if !completed {
            let _ = tx.send(WorkerEvent::Log("Abbruch: Verarbeitung gestoppt.".to_string()));
            return Ok(());
        }

        base_done_frames += clip_total;
        let percent = (base_done_frames as f64 * 100.0 / total_frames_all as f64) as f32;
        let _ = tx.send(WorkerEvent::Progress(percent));
        let _ = tx.send(WorkerEvent::Log(format!(
            "[{}/{}] Fertig: {}",
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
    /// This is what a click-through of "Verarbeitung starten" would
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
            false,
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
                .any(|e| matches!(e, WorkerEvent::Log(l) if l.contains("Fertig"))),
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
            false,
            &cancel_flag,
            &tx,
            &ctx,
        );

        assert!(result.is_ok());
        let events: Vec<WorkerEvent> = rx.try_iter().collect();
        assert!(events
            .iter()
            .any(|e| matches!(e, WorkerEvent::Log(l) if l.contains("Abbruch"))));
    }
}
