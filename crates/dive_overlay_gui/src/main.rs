use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

use dive_overlay_core::csv_data::{
    format_duration, format_duration_precise, load_samples, parse_column_map, parse_duration_to_seconds, parse_fields,
};
use dive_overlay_core::ffprobe::probe_video;
use dive_overlay_core::merge::{finish_merge, plan_merge, MergePlan};
use dive_overlay_core::overlay::{build_overlay_lines, draw_depth_graph, draw_overlay, OverlayCache};
use dive_overlay_core::pipeline::{
    extract_frame_at, process_clip, Codec, EncoderInfo, OutputMode, OutputResolution, Preset, ProcessingOptions,
    OUTPUT_RESOLUTIONS,
};
use dive_overlay_core::sync::{compute_auto_sync, derive_output_path, AutoSyncParams};
use dive_overlay_core::{ClipJob, DiveSample, RgbImage};

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

/// Auto-sync settings: only the base clip is synced by hand, and every
/// other clip's CSV sync point is derived from how much later it started
/// recording (see `dive_overlay_core::sync::compute_auto_sync`). The base
/// clip's own CSV sync comes from its row in the clip table, so there is
/// nothing to carry here beyond which clip it is.
#[derive(Clone)]
struct AutoSyncConfig {
    base_clip: PathBuf,
    base_video_sync_sec: f64,
}

/// Everything the background worker needs for one run, bundled up so a new
/// setting doesn't mean threading another positional argument through the
/// spawn site.
struct WorkerConfig {
    csv_path: PathBuf,
    column_map: HashMap<String, String>,
    entries: Vec<ClipEntry>,
    options: ProcessingOptions,
    auto_sync: Option<AutoSyncConfig>,
    /// When set, the clips are joined into this one file and their
    /// individual outputs are demoted to scratch parts.
    merge_output: Option<PathBuf>,
}

enum WorkerEvent {
    Log(String),
    Progress(f32),
    /// Replaces the label next to the Start button. Used for phases that
    /// are not per-frame processing (combining clips), which would
    /// otherwise leave the UI reading "Processing..." with a still bar.
    Status(String),
    Fps(f64),
    Encoder(String),
    /// `Ok(true)` = every clip finished, `Ok(false)` = stopped early via the
    /// cancel flag (the partial output is still a valid file), `Err` = failed.
    Done(Result<bool, String>),
}

/// The per-clip editor. Clips are only ever *added* in bulk (see
/// `add_clips_bulk`), so this dialog exists solely to edit one existing
/// entry -- hence a plain index rather than an optional one.
struct ClipDialogState {
    editing_index: usize,
    video: String,
    video_sync: String,
    csv_sync: String,
    output: String,
    error: Option<String>,
}

impl ClipDialogState {
    fn new_edit(idx: usize, entry: &ClipEntry) -> Self {
        Self {
            editing_index: idx,
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
    /// The decoded frame *without* the overlay drawn on it. Typing in the
    /// CSV sync field redraws the info box straight from this, so only
    /// moving the video position pays for another ffmpeg seek and decode.
    frame: RgbImage,
    texture: egui::TextureHandle,
    size: egui::Vec2,
    lines: Vec<String>,
    /// Clip length, so the scrub buttons can't walk off the end of the
    /// video into an opaque "could not read a frame" error.
    duration_sec: f64,
    /// Cached alongside the frame for the same reason: redrawing on every
    /// keystroke must not re-parse the whole CSV.
    samples: Vec<DiveSample>,
    times: Vec<f64>,
    /// Edit buffer for the CSV sync field. Half-typed values like `1:` are
    /// held here and only written back to the clip once they parse.
    csv_sync_edit: String,
    csv_sync_error: Option<String>,
}

/// Applies one scrub step to a video sync point, keeping it inside the clip.
/// `duration_sec` of 0 means ffprobe didn't report one, in which case only
/// the lower bound is enforced.
fn scrubbed_video_sync(current: f64, delta: f64, duration_sec: f64) -> f64 {
    let moved = current + delta;
    if duration_sec > 0.0 {
        // A hair short of the end: seeking to exactly the duration lands
        // past the last frame and decodes nothing.
        moved.clamp(0.0, (duration_sec - 0.05).max(0.0))
    } else {
        moved.max(0.0)
    }
}

struct App {
    csv_path: String,
    fields: String,
    codec: String,
    preset: String,
    hw_accel: bool,
    column_map: String,
    resolution: OutputResolution,
    show_graph: bool,
    interpolate: bool,
    mode: OutputMode,
    merge_enabled: bool,
    merge_output: String,
    auto_sync: bool,
    base_clip_index: usize,
    base_video_sync: String,
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
            resolution: OutputResolution::Original,
            show_graph: false,
            interpolate: false,
            mode: OutputMode::Overlay,
            merge_enabled: false,
            merge_output: String::new(),
            auto_sync: false,
            base_clip_index: 0,
            base_video_sync: "0.0".to_string(),
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
            self.ui_multi_clip(ui);
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
                    WorkerEvent::Status(text) => self.status = text,
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
                Ok(true) => {
                    self.status = "Done".to_string();
                    self.progress = 100.0;
                }
                Ok(false) => {
                    self.status = "Cancelled".to_string();
                    self.fps = 0.0;
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
        let Some((version, url)) = &self.update_available else {
            return;
        };
        ui.horizontal(|ui| {
            ui.colored_label(
                egui::Color32::from_rgb(184, 92, 0),
                format!("New version available: {version}"),
            );
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
            // Subtitle mode re-muxes losslessly, so there is nothing to
            // resize -- greyed out for the same reason as codec and preset.
            ui.add_enabled_ui(!subtitle_mode, |ui| {
                ui.label("Resolution:");
                egui::ComboBox::from_id_salt("resolution")
                    .selected_text(self.resolution.label())
                    .show_ui(ui, |ui| {
                        for opt in OUTPUT_RESOLUTIONS {
                            ui.selectable_value(&mut self.resolution, opt, opt.label());
                        }
                    });
            })
            .response
            .on_hover_text(
                "Downscales the output. Footage already at or below the chosen size is left                  untouched, and the aspect ratio is always preserved. Smaller frames encode                  much faster, and can bring a hardware encoder into range that the original                  resolution was too large for.",
            );
        });
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

                let hw_applies = Codec::parse(&self.codec).is_some_and(Codec::supports_preset);
                ui.add_enabled_ui(hw_applies, |ui| {
                    ui.label("Preset:");
                    egui::ComboBox::from_id_salt("preset")
                        .selected_text(self.preset.clone())
                        .show_ui(ui, |ui| {
                            for opt in [
                                "ultrafast",
                                "superfast",
                                "veryfast",
                                "faster",
                                "fast",
                                "medium",
                                "slow",
                                "slower",
                                "veryslow",
                                "placebo",
                            ] {
                                ui.selectable_value(&mut self.preset, opt.to_string(), opt);
                            }
                        });
                });
            });
        });
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!subtitle_mode, |ui| {
                let hw_applies = Codec::parse(&self.codec).is_some_and(Codec::supports_preset);
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

    /// The settings that only mean anything across several clips of one
    /// dive: joining them into a single file, and deriving their sync
    /// points from one another instead of entering each by hand.
    fn ui_multi_clip(&mut self, ui: &mut egui::Ui) {
        ui.separator();

        ui.horizontal(|ui| {
            ui.checkbox(&mut self.merge_enabled, "Combine all clips into one dive video");
            ui.add_enabled_ui(self.merge_enabled, |ui| {
                ui.label("Output:");
                ui.text_edit_singleline(&mut self.merge_output);
                if ui.button("Save as").clicked() {
                    if let Some(path) = rfd::FileDialog::new().add_filter("MP4", &["mp4"]).save_file() {
                        self.merge_output = path.display().to_string();
                    }
                }
            });
        });
        if self.merge_enabled {
            ui.weak(
                "Sorted by dive time, joined back-to-back with no filler for the gaps. Only the combined file is kept.",
            );
        }

        ui.checkbox(&mut self.auto_sync, "Auto-sync clips from their recording timestamps");
        if self.auto_sync {
            let names: Vec<String> = self
                .entries
                .iter()
                .map(|e| {
                    e.video_path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default()
                })
                .collect();
            let mut base_clip_index = self.base_clip_index;

            ui.horizontal(|ui| {
                ui.label("Base clip:");
                let selected_text = names
                    .get(base_clip_index)
                    .cloned()
                    .unwrap_or_else(|| "<none>".to_string());
                egui::ComboBox::from_id_salt("base_clip")
                    .selected_text(selected_text)
                    .show_ui(ui, |ui| {
                        for (i, name) in names.iter().enumerate() {
                            ui.selectable_value(&mut base_clip_index, i, name);
                        }
                    });
                ui.label("Video sync (s):");
                ui.add(egui::TextEdit::singleline(&mut self.base_video_sync).desired_width(60.0));
                ui.label("CSV sync:");
                // Read-only: the anchor is the base clip's own CSV sync, so
                // it is edited in the clip table (or the sync preview) like
                // any other clip's, and only shown here to make clear which
                // value the rest are derived from.
                let base_csv_sync = self
                    .entries
                    .get(base_clip_index)
                    .map(|e| e.csv_sync_mmss.clone())
                    .unwrap_or_else(|| "-".to_string());
                ui.strong(base_csv_sync);
                ui.weak("(from the clip table)");
            });

            self.base_clip_index = base_clip_index;
            ui.weak("Only the base clip is synced by hand; every other clip's CSV sync is derived from how much later it started recording, read from the clips' start timecode (or their creation time if any clip has none).");
        }
    }

    /// Adds several clips at once, in the order they were recorded.
    ///
    /// A dive is normally one camera's worth of consecutive chapters, so the
    /// per-clip dialog's sync fields are pure repetition here: every entry
    /// starts at 0 and the user syncs only the base clip, letting auto-sync
    /// place the rest. Sorting uses `creation_time` rather than the start
    /// timecode auto-sync prefers, because it carries a date and so cannot
    /// wrap at midnight; ordering is cosmetic either way, since `plan_merge`
    /// re-sorts by dive time before joining.
    fn add_clips_bulk(&mut self) {
        let Some(paths) = rfd::FileDialog::new()
            .add_filter("Video", &["mp4", "MP4", "mov", "MOV"])
            .pick_files()
        else {
            return;
        };

        let mut probed: Vec<(Option<i64>, String, PathBuf)> = paths
            .into_iter()
            .map(|path| {
                let recorded_at = probe_video(&path)
                    .ok()
                    .and_then(|info| info.creation_time)
                    .map(|t| t.timestamp_millis());
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                (recorded_at, name, path)
            })
            .collect();
        // Files whose timestamp could not be read fall back to name order
        // rather than being scattered through the list.
        probed.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

        let mut added = 0;
        for (_, _, video_path) in probed {
            if self.entries.iter().any(|e| e.video_path == video_path) {
                continue;
            }
            self.entries.push(ClipEntry {
                output_path: derive_output_path(&video_path, None),
                video_path,
                video_sync_sec: 0.0,
                csv_sync_mmss: "0:00".to_string(),
            });
            added += 1;
        }
        if added > 0 {
            self.log_lines.push(format!("Added {added} clip(s)."));
        }
    }

    fn ui_clip_table(&mut self, ui: &mut egui::Ui) {
        ui.heading("Clips");
        ui.horizontal(|ui| {
            if ui.button("Add clips...").clicked() {
                self.add_clips_bulk();
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
                    // Keep an open preview pointing at the same clip (or close
                    // it if that clip is the one just removed) -- otherwise its
                    // +-buttons would silently edit whichever entry shifted
                    // into the removed slot.
                    if let Some(preview) = &mut self.preview {
                        if preview.clip_index == idx {
                            self.preview = None;
                        } else if preview.clip_index > idx {
                            preview.clip_index -= 1;
                        }
                    }
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
        let Some(dialog) = &mut self.dialog else {
            return;
        };
        let mut open = true;
        let mut submit = false;
        let mut cancel = false;
        egui::Window::new("Edit clip")
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
                                dialog.output =
                                    path.with_file_name(format!("{stem}_overlay.mp4")).display().to_string();
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
                    // The clip can have been removed while the dialog was
                    // open; writing back into a shifted slot would edit the
                    // wrong clip, so a stale index just drops the edit.
                    if let Some(slot) = self.entries.get_mut(editing_index) {
                        *slot = entry;
                    }
                }
                Err(e) => dialog.error = Some(e),
            }
        } else if cancel || !open {
            self.dialog = None;
        }
    }

    /// Two steps, in the order a diver actually works: scrub the video until
    /// the dive computer is readable in frame (which moves the *video* sync
    /// point), then type the dive time it shows (the CSV sync point).
    fn ui_preview_window(&mut self, ctx: &egui::Context) {
        if self.preview.is_none() {
            return;
        }
        let clip_index = self.preview.as_ref().unwrap().clip_index;
        let mut scrub: Option<f64> = None;
        let mut reload = false;
        let mut csv_sync_edited = false;
        let mut open = true;

        egui::Window::new("Sync preview")
            .open(&mut open)
            .resizable(true)
            .show(ctx, |ui| {
                if let Some(entry) = self.entries.get(clip_index) {
                    ui.label(format!(
                        "Video: {}",
                        entry
                            .video_path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default(),
                    ));
                }

                ui.weak("1. Scrub to the frame where the dive computer is readable.");
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
                            scrub = Some(delta);
                        }
                    }
                    if ui.button("Reload").clicked() {
                        reload = true;
                    }
                });
                ui.horizontal(|ui| {
                    let position = self.entries.get(clip_index).map(|e| e.video_sync_sec).unwrap_or(0.0);
                    let duration = self.preview.as_ref().map(|p| p.duration_sec).unwrap_or(0.0);
                    if duration > 0.0 {
                        ui.label(format!("Video sync: {position:.2} s of {duration:.2} s"));
                    } else {
                        ui.label(format!("Video sync: {position:.2} s"));
                    }
                });

                ui.separator();

                ui.weak("2. Enter the dive time the computer reads in this frame.");
                ui.horizontal(|ui| {
                    ui.label("CSV sync (mm:ss or hh:mm:ss):");
                    if let Some(preview) = self.preview.as_mut() {
                        let response =
                            ui.add(egui::TextEdit::singleline(&mut preview.csv_sync_edit).desired_width(90.0));
                        if response.changed() {
                            csv_sync_edited = true;
                        }
                    }
                });
                if let Some(error) = self.preview.as_ref().and_then(|p| p.csv_sync_error.as_ref()) {
                    ui.colored_label(egui::Color32::RED, error);
                }

                ui.separator();

                if let Some(preview) = &self.preview {
                    let available = ui.available_size();
                    let scale = (available.x / preview.size.x).clamp(0.05, 1.0);
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

        if let Some(delta) = scrub {
            let duration = self.preview.as_ref().map(|p| p.duration_sec).unwrap_or(0.0);
            if let Some(entry) = self.entries.get_mut(clip_index) {
                entry.video_sync_sec = scrubbed_video_sync(entry.video_sync_sec, delta, duration);
            }
            self.refresh_preview_frame(ctx);
        } else if reload {
            self.render_preview(clip_index, ctx);
        } else if csv_sync_edited {
            let text = self
                .preview
                .as_ref()
                .map(|p| p.csv_sync_edit.clone())
                .unwrap_or_default();
            match parse_duration_to_seconds(&text) {
                Ok(_) => {
                    if let Some(entry) = self.entries.get_mut(clip_index) {
                        entry.csv_sync_mmss = text;
                    }
                    if let Some(preview) = self.preview.as_mut() {
                        preview.csv_sync_error = None;
                    }
                    self.redraw_preview_overlay(ctx);
                }
                // Half-typed input is normal while someone is still typing,
                // so this only marks the field -- the clip keeps its last
                // value rather than being clobbered with garbage.
                Err(e) => {
                    if let Some(preview) = self.preview.as_mut() {
                        preview.csv_sync_error = Some(e.to_string());
                    }
                }
            }
        }
    }

    /// Opens (or fully reloads) the preview: re-reads the CSV, re-probes the
    /// clip and seeks to the current video sync point.
    fn render_preview(&mut self, idx: usize, ctx: &egui::Context) {
        match self.try_render_preview(idx, ctx) {
            Ok(preview) => self.preview = Some(preview),
            Err(e) => self.log_lines.push(format!("Preview failed: {e}")),
        }
    }

    /// Re-seeks after the scrub buttons moved the video sync point, keeping
    /// the CSV samples already loaded for this preview.
    fn refresh_preview_frame(&mut self, ctx: &egui::Context) {
        let Some(preview) = self.preview.as_ref() else { return };
        let idx = preview.clip_index;
        let Some(entry) = self.entries.get(idx) else { return };
        let (video_path, second) = (entry.video_path.clone(), entry.video_sync_sec);

        let frame = match extract_frame_at(&video_path, second) {
            Ok(frame) => frame,
            Err(e) => {
                self.log_lines.push(format!("Preview failed: {e}"));
                return;
            }
        };

        let preview = self.preview.as_ref().expect("checked above");
        let drawn = self.draw_preview_frame(idx, &frame, &preview.samples, &preview.times, ctx);
        match drawn {
            Ok((texture, size, lines)) => {
                let preview = self.preview.as_mut().expect("checked above");
                preview.frame = frame;
                preview.texture = texture;
                preview.size = size;
                preview.lines = lines;
            }
            Err(e) => self.log_lines.push(format!("Preview failed: {e}")),
        }
    }

    /// Redraws the info box on the frame already on screen -- what a
    /// keystroke in the CSV sync field triggers, with no ffmpeg call at all.
    fn redraw_preview_overlay(&mut self, ctx: &egui::Context) {
        let Some(preview) = self.preview.as_ref() else { return };
        let idx = preview.clip_index;
        let drawn = self.draw_preview_frame(idx, &preview.frame, &preview.samples, &preview.times, ctx);
        match drawn {
            Ok((texture, size, lines)) => {
                let preview = self.preview.as_mut().expect("checked above");
                preview.texture = texture;
                preview.size = size;
                preview.lines = lines;
            }
            Err(e) => self.log_lines.push(format!("Preview failed: {e}")),
        }
    }

    /// Draws the clip's current CSV sync onto a copy of `frame` and uploads
    /// it as a texture. `frame` itself stays clean so it can be redrawn
    /// again with a different sync point.
    fn draw_preview_frame(
        &self,
        idx: usize,
        frame: &RgbImage,
        samples: &[DiveSample],
        times: &[f64],
        ctx: &egui::Context,
    ) -> anyhow::Result<(egui::TextureHandle, egui::Vec2, Vec<String>)> {
        let entry = self
            .entries
            .get(idx)
            .ok_or_else(|| anyhow::anyhow!("Invalid clip index"))?;
        let fields = parse_fields(&self.fields)?;
        let csv_sync_sec = parse_duration_to_seconds(&entry.csv_sync_mmss)?;

        let mut frame = frame.clone();
        let lines = build_overlay_lines(&fields, samples, times, csv_sync_sec, self.interpolate);
        draw_overlay(&mut frame, &lines, &mut OverlayCache::new());
        if self.show_graph {
            draw_depth_graph(&mut frame, samples, times, csv_sync_sec, &mut OverlayCache::new());
        }

        let (w, h) = frame.dimensions();
        let color_image = egui::ColorImage::from_rgb([w as usize, h as usize], frame.as_raw());
        let texture = ctx.load_texture(format!("preview-{idx}"), color_image, egui::TextureOptions::default());
        Ok((texture, egui::vec2(w as f32, h as f32), lines))
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

        let column_map = parse_column_map(&self.column_map)?;
        let samples = load_samples(&csv_path, &column_map)?;
        let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();
        let duration_sec = probe_video(&entry.video_path)?.duration_sec.unwrap_or(0.0);

        let frame = extract_frame_at(&entry.video_path, entry.video_sync_sec)?;
        let (texture, size, lines) = self.draw_preview_frame(idx, &frame, &samples, &times, ctx)?;

        Ok(PreviewState {
            clip_index: idx,
            frame,
            texture,
            size,
            lines,
            duration_sec,
            samples,
            times,
            csv_sync_edit: entry.csv_sync_mmss.clone(),
            csv_sync_error: None,
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

        egui::ScrollArea::vertical()
            .max_height(150.0)
            .stick_to_bottom(true)
            .show(ui, |ui| {
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
            self.log_lines
                .push(format!("Error: CSV not found: {}", csv_path.display()));
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

        let codec = match Codec::parse(&self.codec) {
            Some(c) => c,
            None => {
                self.log_lines.push(format!("Error: unknown codec: {}", self.codec));
                return;
            }
        };
        let preset = Preset::parse(&self.preset).unwrap_or_default();

        let merge_output = if self.merge_enabled {
            let trimmed = self.merge_output.trim();
            if trimmed.is_empty() {
                self.log_lines
                    .push("Error: please specify an output path for the combined video.".to_string());
                return;
            }
            Some(PathBuf::from(trimmed).with_extension("mp4"))
        } else {
            None
        };

        let auto_sync = if self.auto_sync {
            let Some(entry) = self.entries.get(self.base_clip_index) else {
                self.log_lines
                    .push("Error: please select a base clip for auto-sync.".to_string());
                return;
            };
            let Ok(base_video_sync_sec) = self.base_video_sync.trim().parse::<f64>() else {
                self.log_lines
                    .push("Error: the base clip's video sync must be a number.".to_string());
                return;
            };
            Some(AutoSyncConfig {
                base_clip: entry.video_path.clone(),
                base_video_sync_sec,
            })
        } else {
            None
        };

        let config = WorkerConfig {
            csv_path,
            column_map,
            entries: self.entries.clone(),
            options: ProcessingOptions {
                fields,
                codec,
                preset,
                hw_accel: self.hw_accel,
                show_graph: self.show_graph,
                resolution: self.resolution,
                mode: self.mode,
                interpolate: self.interpolate,
            },
            auto_sync,
            merge_output,
        };

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
            let result = run_worker(config, &cancel_flag, &tx, &worker_ctx);
            let _ = tx.send(WorkerEvent::Done(result));
            worker_ctx.request_repaint();
        });
        self.worker_handle = Some(handle);
    }
}

fn run_worker(
    config: WorkerConfig,
    cancel_flag: &Arc<AtomicBool>,
    tx: &Sender<WorkerEvent>,
    ctx: &egui::Context,
) -> Result<bool, String> {
    let WorkerConfig {
        csv_path,
        column_map,
        entries,
        options,
        auto_sync,
        merge_output,
    } = config;

    let samples = load_samples(&csv_path, &column_map).map_err(|e| e.to_string())?;
    let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();

    let mut jobs = entries
        .iter()
        .map(|entry| {
            Ok(ClipJob {
                video_path: entry.video_path.clone(),
                output_path: entry.output_path.with_extension("mp4"),
                video_sync_sec: entry.video_sync_sec,
                csv_sync_sec: parse_duration_to_seconds(&entry.csv_sync_mmss).map_err(|e| e.to_string())?,
                video_start_utc: None,
            })
        })
        .collect::<Result<Vec<ClipJob>, String>>()?;

    if let Some(auto) = &auto_sync {
        let params = AutoSyncParams {
            base_clip: &auto.base_clip,
            base_video_sync_sec: auto.base_video_sync_sec,
        };
        let report = compute_auto_sync(&mut jobs, &times, &params).map_err(|e| e.to_string())?;
        let _ = tx.send(WorkerEvent::Log(format!(
            "Auto-sync: clips placed by {}.",
            report.source.label()
        )));
        for job in &jobs {
            let _ = tx.send(WorkerEvent::Log(format!(
                "Auto-sync: {} -> CSV {}",
                job.video_path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
                format_duration_precise(job.csv_sync_sec),
            )));
        }
        for warning in &report.warnings {
            let _ = tx.send(WorkerEvent::Log(format!("Warning: {warning}")));
        }
        ctx.request_repaint();
    }

    // Planning the merge both orders the jobs by dive time and redirects
    // them to scratch part files, so the loop below already writes the
    // parts in the order they will be joined.
    let plan = match &merge_output {
        Some(path) => Some(plan_merge(&mut jobs, path).map_err(|e| e.to_string())?),
        None => None,
    };
    // A failed or cancelled merged run keeps its finished parts rather than
    // deleting them, so the message has to say where they ended up.
    let parts_note = |plan: Option<&MergePlan>| match plan {
        Some(plan) => format!("\nPartial results kept in: {}", plan.parts_dir.display()),
        None => String::new(),
    };

    let total = jobs.len();
    let mut clip_frame_totals = Vec::with_capacity(total);
    for job in &jobs {
        let info = probe_video(&job.video_path).map_err(|e| e.to_string())?;
        clip_frame_totals.push(info.estimated_frames.unwrap_or(0).max(1));
    }
    let total_frames_all: u64 = clip_frame_totals.iter().sum::<u64>().max(total as u64).max(1);

    let mut base_done_frames: u64 = 0;

    for (idx, job) in jobs.iter().enumerate() {
        let _ = tx.send(WorkerEvent::Log(format!(
            "[{}/{}] {} -> {}",
            idx + 1,
            total,
            job.video_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            job.output_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
        )));
        ctx.request_repaint();

        let clip_total = clip_frame_totals[idx];

        let tx_progress = tx.clone();
        let ctx_progress = ctx.clone();
        let tx_encoder = tx.clone();
        let mut last_instant = std::time::Instant::now();
        let mut last_done: u64 = 0;
        let completed = process_clip(
            job,
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
        .map_err(|e| format!("{e}{}", parts_note(plan.as_ref())))?;

        if !completed {
            let _ = tx.send(WorkerEvent::Log(format!(
                "Cancelled: processing stopped.{}",
                parts_note(plan.as_ref())
            )));
            return Ok(false);
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

    if let (Some(plan), Some(merge_output)) = (&plan, &merge_output) {
        let _ = tx.send(WorkerEvent::Log(format!(
            "Combining {total} clip(s) into {}...",
            merge_output.display()
        )));
        let _ = tx.send(WorkerEvent::Fps(0.0));
        let _ = tx.send(WorkerEvent::Progress(0.0));
        let _ = tx.send(WorkerEvent::Status("Combining clips...".to_string()));
        ctx.request_repaint();

        let tx_merge = tx.clone();
        let ctx_merge = ctx.clone();
        let mut last_sent = std::time::Instant::now();
        // The merge reports its position ten times a second; repainting the
        // UI that often for a bar that moves slowly is wasted work.
        let merged = finish_merge(
            plan,
            merge_output,
            options.mode,
            &options,
            cancel_flag,
            move |done, total| {
                if total <= 0.0 || (last_sent.elapsed().as_secs_f64() < 0.25 && done < total) {
                    return;
                }
                let _ = tx_merge.send(WorkerEvent::Progress((done * 100.0 / total).min(100.0) as f32));
                let _ = tx_merge.send(WorkerEvent::Status(format!(
                    "Combining clips... {} / {}",
                    format_duration(done.min(total)),
                    format_duration(total)
                )));
                ctx_merge.request_repaint();
                last_sent = std::time::Instant::now();
            },
        )
        .map_err(|e| format!("{e}{}", parts_note(Some(plan))))?;
        if !merged {
            let _ = tx.send(WorkerEvent::Log(format!(
                "Cancelled: combining stopped.{}",
                parts_note(Some(plan))
            )));
            return Ok(false);
        }
        let _ = tx.send(WorkerEvent::Log(format!("Done: {}", merge_output.display())));
    }

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dive_overlay_core::model::Field;
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

    fn worker_config(csv_path: PathBuf, entries: Vec<ClipEntry>, fields: Vec<Field>) -> WorkerConfig {
        WorkerConfig {
            csv_path,
            column_map: HashMap::new(),
            entries,
            options: ProcessingOptions {
                fields,
                codec: Codec::Auto,
                preset: Preset::VeryFast,
                hw_accel: false,
                show_graph: false,
                resolution: OutputResolution::Original,
                mode: OutputMode::Overlay,
                interpolate: false,
            },
            auto_sync: None,
            merge_output: None,
        }
    }

    fn entry(video_path: PathBuf, csv_sync_mmss: &str, output_path: PathBuf) -> ClipEntry {
        ClipEntry {
            video_path,
            video_sync_sec: 0.0,
            csv_sync_mmss: csv_sync_mmss.to_string(),
            output_path,
        }
    }

    #[test]
    fn scrubbing_stays_inside_the_clip() {
        // Ordinary steps in both directions.
        assert_eq!(scrubbed_video_sync(10.0, 5.0, 60.0), 15.0);
        assert_eq!(scrubbed_video_sync(10.0, -5.0, 60.0), 5.0);
        // Never before the first frame...
        assert_eq!(scrubbed_video_sync(2.0, -60.0, 60.0), 0.0);
        // ...and never past the last one, where the seek would decode nothing.
        assert_eq!(scrubbed_video_sync(59.0, 60.0, 60.0), 59.95);
        // No duration reported: only the lower bound can be enforced.
        assert_eq!(scrubbed_video_sync(10.0, 600.0, 0.0), 610.0);
        assert_eq!(scrubbed_video_sync(10.0, -600.0, 0.0), 0.0);
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

        let config = worker_config(
            csv_path,
            vec![entry(clip, "0:00", output.clone())],
            vec![Field::Time, Field::Depth],
        );

        let ctx = egui::Context::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel_flag = Arc::new(AtomicBool::new(false));

        let result = run_worker(config, &cancel_flag, &tx, &ctx);

        assert_eq!(result, Ok(true), "run_worker failed: {result:?}");
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

        let config = worker_config(csv_path, vec![entry(clip, "0:00", output.clone())], vec![Field::Depth]);

        let ctx = egui::Context::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel_flag = Arc::new(AtomicBool::new(true));

        let result = run_worker(config, &cancel_flag, &tx, &ctx);

        assert_eq!(result, Ok(false), "a cancelled run must not report success");
        let events: Vec<WorkerEvent> = rx.try_iter().collect();
        assert!(events
            .iter()
            .any(|e| matches!(e, WorkerEvent::Log(l) if l.contains("Cancelled"))));
    }

    /// The combine path as the GUI drives it: clips handed over in the wrong
    /// order, joined into one file, with the scratch parts cleaned up.
    #[test]
    fn run_worker_combines_clips_into_one_file_in_dive_order() {
        let dir = make_test_dir("merged");
        let first = synth_clip(&dir, "first.mp4", 1, 10);
        let second = synth_clip(&dir, "second.mp4", 1, 10);
        let csv_path = dir.join("dive.csv");
        std::fs::write(&csv_path, "sample time (min),sample depth (m)\n0:00,1.0\n5:00,20.0\n").unwrap();
        let merged = dir.join("dive_full.mp4");

        // Listed later-clip-first on purpose: the merge has to reorder them.
        let mut config = worker_config(
            csv_path,
            vec![
                entry(second.clone(), "5:00", dir.join("second_overlay.mp4")),
                entry(first.clone(), "0:00", dir.join("first_overlay.mp4")),
            ],
            vec![Field::Time, Field::Depth],
        );
        config.merge_output = Some(merged.clone());

        let ctx = egui::Context::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel_flag = Arc::new(AtomicBool::new(false));

        let result = run_worker(config, &cancel_flag, &tx, &ctx);
        assert_eq!(result, Ok(true), "run_worker failed: {result:?}");

        assert!(merged.exists());
        // Only the combined file survives: no per-clip outputs, no scratch dir.
        assert!(!dir.join("first_overlay.mp4").exists());
        assert!(!dir.join("second_overlay.mp4").exists());
        assert!(!dir.join("dive_full_parts").exists());

        let info = probe_video(&merged).unwrap();
        let joined = info.duration_sec.unwrap_or(0.0);
        assert!(joined > 1.5, "expected both clips in the merged file, got {joined}s");

        let logged: Vec<String> = rx
            .try_iter()
            .filter_map(|e| match e {
                WorkerEvent::Log(line) => Some(line),
                _ => None,
            })
            .collect();
        let processed_first = logged.iter().position(|l| l.contains("first.mp4"));
        let processed_second = logged.iter().position(|l| l.contains("second.mp4"));
        assert!(
            processed_first < processed_second,
            "clips were not reordered by dive time: {logged:?}"
        );
        assert!(logged.iter().any(|l| l.contains("Combining 2 clip(s)")), "{logged:?}");
    }
}
