use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use image::RgbImage;

use crate::error::CoreError;
use crate::ffprobe::probe_video;
use crate::model::{ClipJob, DiveSample, Field};
use crate::overlay::{build_overlay_lines, draw_depth_graph, draw_overlay};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Auto,
    H264,
    Mpeg4,
    Xvid,
    Mjpeg,
}

impl Codec {
    pub fn parse(s: &str) -> Codec {
        match s.trim().to_lowercase().as_str() {
            "avc1" | "h264" => Codec::H264,
            "mp4v" => Codec::Mpeg4,
            "xvid" => Codec::Xvid,
            "mjpg" | "mjpeg" => Codec::Mjpeg,
            _ => Codec::Auto,
        }
    }

    /// With ffmpeg doing the encoding directly, there is no more need for the
    /// original's runtime codec-availability probing loop (`avc1` ->
    /// `H264` -> `mp4v` fallback) -- ffmpeg + libx264 is a fixed, known-good
    /// dependency, so each option maps straight to an `-c:v` value.
    fn ffmpeg_codec_name(self) -> &'static str {
        match self {
            Codec::Auto | Codec::H264 => "libx264",
            Codec::Mpeg4 => "mpeg4",
            Codec::Xvid => "libxvid",
            Codec::Mjpeg => "mjpeg",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProcessingOptions {
    pub fields: Vec<Field>,
    pub codec: Codec,
    pub show_graph: bool,
}

fn spawn_stderr_drain(mut pipe: impl Read + Send + 'static, label: &'static str) {
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        let text = String::from_utf8_lossy(&buf);
        if !text.trim().is_empty() {
            eprintln!("[{label}] {text}");
        }
    });
}

struct DecodeProcess {
    child: Child,
    stdout: ChildStdout,
}

/// Spawns an ffmpeg process that decodes `video_path` to a raw rgb24 stream
/// on stdout. `-nostdin` is safe here because this process's stdin is
/// unused -- do not use it on the encoder, whose stdin carries real frame
/// data.
fn spawn_decoder(video_path: &Path) -> Result<DecodeProcess, CoreError> {
    let mut child = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(video_path)
        .args(["-an", "-f", "rawvideo", "-pix_fmt", "rgb24", "-vsync", "0", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CoreError::Ffmpeg(format!("Decoder konnte nicht gestartet werden: {e}")))?;

    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    spawn_stderr_drain(stderr, "ffmpeg-decode");

    Ok(DecodeProcess { child, stdout })
}

struct EncodeProcess {
    child: Child,
    stdin: Option<ChildStdin>,
}

/// Spawns an ffmpeg process that reads raw rgb24 frames on stdin, muxes in
/// the original file's audio track (mapped optionally via `1:a:0?` so
/// audio-less clips don't fail the job), and writes the final mp4. `-y`
/// (not `-nostdin`) belongs here since stdin carries real data -- `-y`
/// alone prevents ffmpeg from trying to read an interactive overwrite
/// confirmation off that same pipe.
fn spawn_encoder(
    output_path: &Path,
    original_input: &Path,
    width: u32,
    height: u32,
    fps: f64,
    codec: Codec,
) -> Result<EncodeProcess, CoreError> {
    if let Some(parent) = output_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let size_arg = format!("{width}x{height}");
    let fps_arg = format!("{fps}");

    let mut child = Command::new("ffmpeg")
        .args(["-y", "-f", "rawvideo", "-pix_fmt", "rgb24"])
        .args(["-s", &size_arg, "-r", &fps_arg, "-i", "pipe:0"])
        .arg("-i")
        .arg(original_input)
        .args(["-map", "0:v:0", "-map", "1:a:0?"])
        .args(["-c:v", codec.ffmpeg_codec_name(), "-pix_fmt", "yuv420p"])
        .args(["-c:a", "aac", "-b:a", "192k", "-shortest"])
        .arg(output_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CoreError::Ffmpeg(format!("Encoder konnte nicht gestartet werden: {e}")))?;

    let stdin = child.stdin.take().expect("stdin was piped");
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    spawn_stderr_drain(stdout, "ffmpeg-encode-stdout");
    spawn_stderr_drain(stderr, "ffmpeg-encode");

    Ok(EncodeProcess {
        child,
        stdin: Some(stdin),
    })
}

/// Decodes every frame of `job.video_path`, overlays dive telemetry looked
/// up at that frame's dive-elapsed-second, and encodes the result (with the
/// original audio muxed back in) to `job.output_path`. Returns `Ok(true)` if
/// all frames were processed, `Ok(false)` if `stop_flag` triggered an early
/// stop (the output up to that point is still finalized as a valid mp4).
pub fn process_clip(
    job: &ClipJob,
    samples: &[DiveSample],
    times: &[f64],
    options: &ProcessingOptions,
    stop_flag: &Arc<AtomicBool>,
    mut progress: impl FnMut(u64, u64),
) -> Result<bool, CoreError> {
    if !job.video_path.exists() {
        return Err(CoreError::VideoNotFound(job.video_path.clone()));
    }

    let info = probe_video(&job.video_path)?;
    if info.width == 0 || info.height == 0 {
        return Err(CoreError::Ffprobe(format!(
            "Konnte Videoauflösung nicht bestimmen: {}",
            job.video_path.display()
        )));
    }

    let mut decoder = spawn_decoder(&job.video_path)?;
    let mut encoder = spawn_encoder(
        &job.output_path,
        &job.video_path,
        info.width,
        info.height,
        info.fps,
        options.codec,
    )?;

    let frame_size = info.width as usize * info.height as usize * 3;
    let total_estimate = info.estimated_frames.unwrap_or(0);

    let mut buf = vec![0u8; frame_size];
    let mut frame_idx: u64 = 0;
    let mut cancelled = false;
    progress(0, total_estimate);

    loop {
        if stop_flag.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }

        match decoder.stdout.read_exact(&mut buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(CoreError::Ffmpeg(format!("Fehler beim Lesen der Frames: {e}"))),
        }

        let mut img = RgbImage::from_raw(info.width, info.height, std::mem::take(&mut buf))
            .ok_or_else(|| CoreError::Ffmpeg("Ungültige Frame-Größe".to_string()))?;

        let video_sec = frame_idx as f64 / info.fps;
        let dive_sec = job.csv_sync_sec + (video_sec - job.video_sync_sec);

        let lines = build_overlay_lines(&options.fields, samples, times, dive_sec);
        draw_overlay(&mut img, &lines);
        if options.show_graph {
            draw_depth_graph(&mut img, samples, times, dive_sec, 600.0);
        }

        if let Some(stdin) = encoder.stdin.as_mut() {
            stdin
                .write_all(img.as_raw())
                .map_err(|e| CoreError::Ffmpeg(format!("Fehler beim Schreiben der Frames: {e}")))?;
        }

        buf = img.into_raw();
        frame_idx += 1;
        if frame_idx % 10 == 0 {
            progress(frame_idx, total_estimate);
        }
    }

    // The decode process should already be at EOF in the normal case; kill
    // defensively on early cancellation.
    let _ = decoder.child.kill();
    let _ = decoder.child.wait();

    // Dropping the encoder's stdin lets ffmpeg see EOF and finalize the mp4
    // (moov atom etc.) -- keeping the handle alive here is a common hang cause.
    encoder.stdin.take();
    let status = encoder
        .child
        .wait()
        .map_err(|e| CoreError::Ffmpeg(format!("Encoder-Prozess fehlgeschlagen: {e}")))?;
    if !status.success() {
        return Err(CoreError::Ffmpeg(format!("Encoder beendete mit Fehler: {status}")));
    }

    progress(frame_idx, total_estimate.max(frame_idx));
    Ok(!cancelled)
}

/// Extracts a single frame at `second` for sync preview, mirroring the
/// original's two-tier seek: a fast input-side `-ss` first, falling back to
/// a frame-accurate output-side `-ss` if that yields nothing.
pub fn extract_frame_at(video_path: &Path, second: f64) -> Result<RgbImage, CoreError> {
    let info = probe_video(video_path)?;
    if info.width == 0 || info.height == 0 {
        return Err(CoreError::Ffprobe(format!(
            "Konnte Videoauflösung nicht bestimmen: {}",
            video_path.display()
        )));
    }
    let frame_size = info.width as usize * info.height as usize * 3;
    let second = second.max(0.0);
    let seek_arg = format!("{second}");

    let try_decode = |input_side_seek: bool| -> Result<Option<Vec<u8>>, CoreError> {
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-nostdin", "-v", "error"]);
        if input_side_seek {
            cmd.args(["-ss", &seek_arg]);
        }
        cmd.arg("-i").arg(video_path);
        if !input_side_seek {
            cmd.args(["-ss", &seek_arg]);
        }
        cmd.args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "pipe:1"]);

        let output = cmd
            .stdin(Stdio::null())
            .output()
            .map_err(|e| CoreError::Ffmpeg(format!("ffmpeg konnte nicht gestartet werden: {e}")))?;

        if output.stdout.len() >= frame_size {
            Ok(Some(output.stdout))
        } else {
            Ok(None)
        }
    };

    let bytes = match try_decode(true)? {
        Some(bytes) => bytes,
        None => try_decode(false)?
            .ok_or_else(|| CoreError::Ffmpeg("Konnte keinen Frame an der Sync-Stelle lesen".to_string()))?,
    };

    RgbImage::from_raw(info.width, info.height, bytes[..frame_size].to_vec())
        .ok_or_else(|| CoreError::Ffmpeg("Ungültige Frame-Größe".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("dive_overlay_pipeline_test").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn synth_clip(dir: &Path, name: &str, duration_secs: u32, fps: u32) -> PathBuf {
        let path = dir.join(name);
        let video_src = format!("testsrc=size=160x120:rate={fps}:duration={duration_secs}");
        let audio_src = format!("sine=frequency=440:duration={duration_secs}");
        let status = Command::new("ffmpeg")
            .args(["-y", "-f", "lavfi", "-i", &video_src, "-f", "lavfi", "-i", &audio_src])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"])
            .arg(&path)
            .status()
            .expect("failed to run ffmpeg");
        assert!(status.success());
        path
    }

    fn sample(elapsed_sec: f64, depth_m: f64) -> DiveSample {
        DiveSample {
            elapsed_sec,
            depth_m: Some(depth_m),
            temp_c: Some(20.0),
            pressure_bar: None,
            heart_rate: None,
        }
    }

    #[test]
    fn processes_synthetic_clip_end_to_end_with_audio() {
        let dir = make_test_dir("end_to_end");
        let clip = synth_clip(&dir, "input.mp4", 2, 5);
        let output = dir.join("output.mp4");

        let job = ClipJob {
            video_path: clip,
            output_path: output.clone(),
            video_sync_sec: 0.0,
            csv_sync_sec: 0.0,
            video_start_utc: None,
        };
        let samples = vec![sample(0.0, 1.0), sample(1.0, 5.0), sample(2.0, 3.0)];
        let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();
        let options = ProcessingOptions {
            fields: vec![Field::Time, Field::Depth, Field::Temp],
            codec: Codec::Auto,
            show_graph: true,
        };
        let stop_flag = Arc::new(AtomicBool::new(false));

        let mut progress_calls = Vec::new();
        let completed = process_clip(&job, &samples, &times, &options, &stop_flag, |done, total| {
            progress_calls.push((done, total));
        })
        .unwrap();

        assert!(completed);
        assert!(output.exists());

        let info = probe_video(&output).unwrap();
        assert_eq!(info.width, 160);
        assert_eq!(info.height, 120);

        // Verify audio survived the mux.
        let ffprobe_out = Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "a", "-show_entries", "stream=codec_type"])
            .args(["-of", "csv=p=0"])
            .arg(&output)
            .output()
            .unwrap();
        let has_audio = String::from_utf8_lossy(&ffprobe_out.stdout).contains("audio");
        assert!(has_audio, "expected an audio stream in the muxed output");
    }

    #[test]
    fn stop_flag_halts_processing_early_and_still_finalizes_output() {
        let dir = make_test_dir("cancel");
        let clip = synth_clip(&dir, "input.mp4", 3, 10);
        let output = dir.join("output.mp4");

        let job = ClipJob {
            video_path: clip,
            output_path: output.clone(),
            video_sync_sec: 0.0,
            csv_sync_sec: 0.0,
            video_start_utc: None,
        };
        let samples = vec![sample(0.0, 1.0)];
        let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();
        let options = ProcessingOptions {
            fields: vec![Field::Depth],
            codec: Codec::Auto,
            show_graph: false,
        };
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_flag_for_progress = stop_flag.clone();

        let completed = process_clip(&job, &samples, &times, &options, &stop_flag, move |done, _total| {
            if done >= 5 {
                stop_flag_for_progress.store(true, Ordering::Relaxed);
            }
        })
        .unwrap();

        assert!(!completed);
        assert!(output.exists());
    }

    #[test]
    fn extract_frame_at_matches_video_dimensions() {
        let dir = make_test_dir("extract_frame");
        let clip = synth_clip(&dir, "input.mp4", 2, 5);
        let img = extract_frame_at(&clip, 0.5).unwrap();
        assert_eq!(img.width(), 160);
        assert_eq!(img.height(), 120);
    }
}
