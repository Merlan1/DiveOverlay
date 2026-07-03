use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;

use dive_overlay_core::csv_data::{load_samples, parse_column_map, parse_duration_to_seconds, parse_fields};
use dive_overlay_core::ffprobe::ensure_ffmpeg_available;
use dive_overlay_core::pipeline::{process_clip, Codec, OutputMode, Preset, ProcessingOptions};
use dive_overlay_core::sync::{compute_auto_sync, derive_output_path, parse_clip_spec, AutoSyncParams};
use dive_overlay_core::ClipJob;

/// Blendet Tauchdaten aus CSV über ein Video ein.
#[derive(Parser, Debug)]
#[command(about = "Blendet Tauchdaten aus CSV über ein Video ein")]
struct Args {
    /// Pfad zur CSV-Datei
    #[arg(long)]
    csv: PathBuf,

    /// Pfad zur Video-Datei (Single-Clip Modus)
    #[arg(long)]
    video: Option<PathBuf>,

    /// Ausgabedatei (Standard: <video_stem>_overlay.mp4)
    #[arg(long)]
    output: Option<PathBuf>,

    /// Sekunde im Video, bei der die CSV-Sync-Zeit gilt
    #[arg(long, default_value_t = 0.0)]
    video_sync_sec: f64,

    /// Tauchzeit am Sync-Punkt (Format mm:ss oder hh:mm:ss)
    #[arg(long, default_value = "0:00")]
    csv_sync_mmss: String,

    /// Anzuzeigende Felder: time,depth,temp,pressure,hr
    #[arg(long, default_value = "time,depth,temp,pressure,hr")]
    fields: String,

    /// CSV-Spaltenzuordnung: time=...,depth=...,temp=...,pressure=...,hr=...,date=...,clock=...
    #[arg(long, default_value = "")]
    column_map: String,

    /// Video-Codec: auto, avc1, H264, hevc/H265, mp4v, XVID, MJPG
    #[arg(long, default_value = "auto")]
    codec: String,

    /// Encoder-Preset fuer H264/H265 (Geschwindigkeit vs. Kompression):
    /// ultrafast, superfast, veryfast, faster, fast, medium, slow, slower,
    /// veryslow, placebo. Wird fuer andere Codecs ignoriert.
    #[arg(long, default_value = "veryfast")]
    preset: String,

    /// Versucht Hardware-Beschleunigung (z. B. Intel Quick Sync) fuer
    /// H264/H265 zu nutzen; faellt automatisch auf Software zurueck, falls
    /// keine passende Hardware gefunden wird. Wird fuer andere Codecs
    /// ignoriert.
    #[arg(long)]
    hw_accel: bool,

    /// Zeigt kleines Tiefenprofil (Graph) an
    #[arg(long)]
    show_graph: bool,

    /// Ausgabe-Modus: overlay (in Pixel eingebrannt) oder subtitles (weiche,
    /// im Player an/aus schaltbare Untertitelspur statt Overlay)
    #[arg(long, default_value = "overlay")]
    mode: String,

    /// Automatisches Sync basierend auf MP4 CreationTime + CSV Datum/Uhrzeit
    #[arg(long)]
    auto_sync: bool,

    /// Clip-Pfad fuer Auto-Sync (muss in --clip enthalten sein)
    #[arg(long, default_value = "")]
    base_clip: String,

    /// Video-Sekunde des manuellen Sync-Punkts (nur Auto-Sync)
    #[arg(long, default_value_t = 0.0)]
    base_video_sync_sec: f64,

    /// CSV Datum/Uhrzeit am Sync-Punkt (ISO: YYYY-MM-DD HH:MM:SS)
    #[arg(long, default_value = "")]
    base_csv_datetime: String,

    /// Mehrere Clips verarbeiten. Format: video_path|video_sync_sec|csv_sync_mmss[|output_path].
    /// Kann mehrfach angegeben werden.
    #[arg(long = "clip")]
    clip: Vec<String>,
}

fn build_jobs(args: &Args) -> Result<Vec<ClipJob>> {
    if !args.clip.is_empty() {
        return args.clip.iter().map(|s| parse_clip_spec(s).map_err(Into::into)).collect();
    }

    let video = args
        .video
        .clone()
        .ok_or_else(|| anyhow!("Bitte --video angeben oder mindestens ein --clip verwenden"))?;
    let output = derive_output_path(&video, args.output.clone());
    let csv_sync_sec = parse_duration_to_seconds(&args.csv_sync_mmss)?;

    Ok(vec![ClipJob {
        video_path: video,
        output_path: output,
        video_sync_sec: args.video_sync_sec,
        csv_sync_sec,
        video_start_utc: None,
    }])
}

fn main() -> Result<()> {
    let args = Args::parse();
    ensure_ffmpeg_available()?;

    if !args.csv.exists() {
        bail!("CSV nicht gefunden: {}", args.csv.display());
    }

    let fields = parse_fields(&args.fields)?;
    let column_map = parse_column_map(&args.column_map)?;
    let mut jobs = build_jobs(&args)?;

    for job in &jobs {
        if !job.video_path.exists() {
            bail!("Video nicht gefunden: {}", job.video_path.display());
        }
    }

    let samples = load_samples(&args.csv, &column_map)?;
    let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();

    if args.auto_sync {
        if args.clip.is_empty() {
            bail!("Auto-Sync benötigt --clip Angaben");
        }
        if args.base_clip.is_empty() {
            bail!("Auto-Sync benötigt --base-clip");
        }
        if args.base_csv_datetime.is_empty() {
            bail!("Auto-Sync benötigt --base-csv-datetime");
        }

        let base_clip = PathBuf::from(&args.base_clip);
        let params = AutoSyncParams {
            base_clip: &base_clip,
            base_video_sync_sec: args.base_video_sync_sec,
            base_csv_datetime: &args.base_csv_datetime,
        };
        compute_auto_sync(&args.csv, &mut jobs, &params)?;
    }

    let mode = OutputMode::parse(&args.mode)
        .ok_or_else(|| anyhow!("Ungültiger --mode Wert: {} (erwartet: overlay, subtitles)", args.mode))?;
    let preset = Preset::parse(&args.preset).ok_or_else(|| {
        anyhow!(
            "Ungültiger --preset Wert: {} (erwartet: ultrafast, superfast, veryfast, faster, fast, medium, slow, slower, veryslow, placebo)",
            args.preset
        )
    })?;

    let options = ProcessingOptions {
        fields,
        codec: Codec::parse(&args.codec),
        preset,
        hw_accel: args.hw_accel,
        show_graph: args.show_graph,
        mode,
    };
    let stop_flag = Arc::new(AtomicBool::new(false));

    let total = jobs.len();
    for (i, job) in jobs.iter_mut().enumerate() {
        job.output_path = job.output_path.with_extension("mp4");

        let mut last_instant = Instant::now();
        let mut last_done: u64 = 0;
        let mut printed_progress = false;

        process_clip(
            job,
            &samples,
            &times,
            &options,
            &stop_flag,
            |done, total_frames| {
                // The final progress call happens after the encoder has been
                // awaited (mp4 finalization/moov write), so its elapsed time
                // includes that wait, not just frame processing -- computing an
                // fps from it would read as a bogus last-moment slowdown.
                if total_frames > 0 && done >= total_frames {
                    print!("\r[{}/{}] Frame {}/{} (fertig)   ", i + 1, total, done, total_frames);
                    let _ = std::io::stdout().flush();
                    printed_progress = true;
                    return;
                }

                let elapsed = last_instant.elapsed().as_secs_f64();
                if elapsed >= 0.1 {
                    let fps = done.saturating_sub(last_done) as f64 / elapsed;
                    if total_frames > 0 {
                        print!("\r[{}/{}] Frame {}/{} ({:.1} fps)   ", i + 1, total, done, total_frames, fps);
                    } else {
                        print!("\r[{}/{}] Frame {} ({:.1} fps)   ", i + 1, total, done, fps);
                    }
                    let _ = std::io::stdout().flush();
                    printed_progress = true;
                    last_instant = Instant::now();
                    last_done = done;
                }
            },
            |info| println!("[{}/{}] Encoder: {}", i + 1, total, info.describe()),
        )
        .with_context(|| format!("Verarbeitung fehlgeschlagen: {}", job.video_path.display()))?;

        if printed_progress {
            println!();
        }
        println!("[{}/{}] Fertig: {}", i + 1, total, job.output_path.display());
    }

    Ok(())
}
