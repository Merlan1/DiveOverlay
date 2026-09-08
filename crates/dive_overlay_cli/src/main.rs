use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;

use dive_overlay_core::csv_data::{
    format_duration, format_duration_precise, load_samples, parse_column_map, parse_duration_to_seconds,
    parse_fields,
};
use dive_overlay_core::ffprobe::ensure_ffmpeg_available;
use dive_overlay_core::merge::{finish_merge, plan_merge};
use dive_overlay_core::pipeline::{process_clip, Codec, OutputMode, Preset, ProcessingOptions};
use dive_overlay_core::sync::{compute_auto_sync, derive_output_path, parse_clip_spec, AutoSyncParams};
use dive_overlay_core::ClipJob;

/// Overlays dive telemetry from a CSV onto a video.
#[derive(Parser, Debug)]
#[command(about = "Overlays dive telemetry from a CSV onto a video")]
struct Args {
    /// Path to the CSV file
    #[arg(long)]
    csv: PathBuf,

    /// Path to the video file (single-clip mode)
    #[arg(long)]
    video: Option<PathBuf>,

    /// Output file (default: <video_stem>_overlay.mp4)
    #[arg(long)]
    output: Option<PathBuf>,

    /// Second in the video at which the CSV sync time applies
    #[arg(long, default_value_t = 0.0)]
    video_sync_sec: f64,

    /// Dive time at the sync point (format mm:ss or hh:mm:ss)
    #[arg(long, default_value = "0:00")]
    csv_sync_mmss: String,

    /// Fields to display: time,depth,temp,pressure,hr
    #[arg(long, default_value = "time,depth,temp,pressure,hr")]
    fields: String,

    /// CSV column mapping: time=...,depth=...,temp=...,pressure=...,hr=...,date=...,clock=...
    #[arg(long, default_value = "")]
    column_map: String,

    /// Video codec: auto, avc1, H264, hevc/H265, mp4v, XVID, MJPG
    #[arg(long, default_value = "auto")]
    codec: String,

    /// Encoder preset for H264/H265 (speed vs. compression):
    /// ultrafast, superfast, veryfast, faster, fast, medium, slow, slower,
    /// veryslow, placebo. Ignored for other codecs.
    #[arg(long, default_value = "veryfast")]
    preset: String,

    /// Tries to use hardware acceleration (Intel Quick Sync, NVIDIA NVENC,
    /// or AMD AMF) for H264/H265; falls back to software automatically if no
    /// matching hardware is found. Ignored for other codecs.
    #[arg(long)]
    hw_accel: bool,

    /// Shows a small depth profile (graph)
    #[arg(long)]
    show_graph: bool,

    /// Smoothly interpolates field values between samples (cubic spline)
    /// instead of carrying the last known reading forward
    #[arg(long)]
    interpolate: bool,

    /// Output mode: overlay (burned into pixels) or subtitles (soft
    /// subtitle track, toggleable on/off in the player, instead of overlay)
    #[arg(long, default_value = "overlay")]
    mode: String,

    /// Derives every clip's CSV sync from one manually-synced base clip and
    /// how much later each clip started recording (from its start timecode,
    /// falling back to the MP4 creation time)
    #[arg(long)]
    auto_sync: bool,

    /// Clip path for auto-sync (must be one of the --clip paths). Its own
    /// csv_sync_mmss is the sync point every other clip is offset from.
    #[arg(long, default_value = "")]
    base_clip: String,

    /// Video second of the manual sync point (auto-sync only)
    #[arg(long, default_value_t = 0.0)]
    base_video_sync_sec: f64,

    /// Process multiple clips. Format: video_path|video_sync_sec|csv_sync_mmss[|output_path].
    /// Can be used multiple times.
    #[arg(long = "clip")]
    clip: Vec<String>,

    /// Combine every clip into a single dive video at this path. The clips
    /// are sorted by dive time and joined back-to-back (no filler for the
    /// gaps between them), and only the combined file is kept -- the
    /// per-clip outputs become scratch files and are deleted afterwards.
    #[arg(long)]
    merge_output: Option<PathBuf>,
}

fn build_jobs(args: &Args) -> Result<Vec<ClipJob>> {
    if !args.clip.is_empty() {
        return args.clip.iter().map(|s| parse_clip_spec(s).map_err(Into::into)).collect();
    }

    let video = args
        .video
        .clone()
        .ok_or_else(|| anyhow!("Please specify --video or at least one --clip"))?;
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
        bail!("CSV not found: {}", args.csv.display());
    }

    let fields = parse_fields(&args.fields)?;
    let column_map = parse_column_map(&args.column_map)?;
    let mut jobs = build_jobs(&args)?;

    for job in &jobs {
        if !job.video_path.exists() {
            bail!("Video not found: {}", job.video_path.display());
        }
    }

    let samples = load_samples(&args.csv, &column_map)?;
    let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();

    if args.auto_sync {
        if args.clip.is_empty() {
            bail!("Auto-sync requires --clip entries");
        }
        if args.base_clip.is_empty() {
            bail!("Auto-sync requires --base-clip");
        }

        let base_clip = PathBuf::from(&args.base_clip);
        let params = AutoSyncParams {
            base_clip: &base_clip,
            base_video_sync_sec: args.base_video_sync_sec,
        };
        let report = compute_auto_sync(&mut jobs, &times, &params)?;
        println!("Auto-sync: clips placed by {}.", report.source.label());
        for job in &jobs {
            println!(
                "  {} -> CSV {}",
                job.video_path.file_name().unwrap_or_default().to_string_lossy(),
                format_duration_precise(job.csv_sync_sec),
            );
        }
        for warning in &report.warnings {
            eprintln!("Warning: {warning}");
        }
    }

    let mode = OutputMode::parse(&args.mode)
        .ok_or_else(|| anyhow!("Invalid --mode value: {} (expected: overlay, subtitles)", args.mode))?;
    let preset = Preset::parse(&args.preset).ok_or_else(|| {
        anyhow!(
            "Invalid --preset value: {} (expected: ultrafast, superfast, veryfast, faster, fast, medium, slow, slower, veryslow, placebo)",
            args.preset
        )
    })?;

    let codec = Codec::parse(&args.codec).ok_or_else(|| {
        anyhow!(
            "Invalid --codec value: {} (expected: auto, avc1, H264, hevc, H265, mp4v, XVID, MJPG)",
            args.codec
        )
    })?;

    let options = ProcessingOptions {
        fields,
        codec,
        preset,
        hw_accel: args.hw_accel,
        show_graph: args.show_graph,
        mode,
        interpolate: args.interpolate,
    };
    let stop_flag = Arc::new(AtomicBool::new(false));

    for job in jobs.iter_mut() {
        job.output_path = job.output_path.with_extension("mp4");
    }

    // Redirects every job to a scratch part file and puts the jobs in dive
    // order, so the loop below already writes the parts in merge order.
    let merge_output = args.merge_output.as_ref().map(|path| path.with_extension("mp4"));
    let plan = match &merge_output {
        Some(path) => Some(plan_merge(&mut jobs, path)?),
        None => None,
    };

    let total = jobs.len();
    for (i, job) in jobs.iter_mut().enumerate() {
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
                    print!("\r[{}/{}] Frame {}/{} (done)   ", i + 1, total, done, total_frames);
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
        .map_err(|e| match &plan {
            // The finished parts are deliberately left behind on failure so
            // the encodes done so far aren't thrown away.
            Some(plan) => anyhow!("{e}\nPartial results kept in: {}", plan.parts_dir.display()),
            None => e.into(),
        })
        .with_context(|| format!("Processing failed: {}", job.video_path.display()))?;

        if printed_progress {
            println!();
        }
        println!("[{}/{}] Done: {}", i + 1, total, job.output_path.display());
    }

    if let (Some(plan), Some(merge_output)) = (&plan, &merge_output) {
        println!("Combining {total} clip(s) into {}...", merge_output.display());
        let mut last_print = Instant::now();
        let mut printed_progress = false;
        finish_merge(plan, merge_output, mode, &options, &stop_flag, |done_sec, total_sec| {
            // Throttled like the per-clip counter above: the merge polls ffmpeg
            // ten times a second, which is far more often than a terminal line
            // needs rewriting.
            if total_sec <= 0.0 || (last_print.elapsed().as_secs_f64() < 0.2 && done_sec < total_sec) {
                return;
            }
            let percent = (done_sec * 100.0 / total_sec).min(100.0);
            print!(
                "\rCombining {} / {} ({percent:.0}%)   ",
                format_duration(done_sec.min(total_sec)),
                format_duration(total_sec)
            );
            let _ = std::io::stdout().flush();
            printed_progress = true;
            last_print = Instant::now();
        })
        .with_context(|| format!("Combining clips failed (parts kept in {})", plan.parts_dir.display()))?;
        if printed_progress {
            println!();
        }
        println!("Done: {}", merge_output.display());
    }

    Ok(())
}
