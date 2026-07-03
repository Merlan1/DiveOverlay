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
use crate::overlay::{build_overlay_lines, draw_depth_graph, draw_overlay, OverlayCache};
use crate::subtitle::build_srt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Auto,
    H264,
    H265,
    Mpeg4,
    Xvid,
    Mjpeg,
}

impl Codec {
    pub fn parse(s: &str) -> Codec {
        match s.trim().to_lowercase().as_str() {
            "avc1" | "h264" => Codec::H264,
            "hevc" | "h265" | "x265" => Codec::H265,
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
            Codec::H265 => "libx265",
            Codec::Mpeg4 => "mpeg4",
            Codec::Xvid => "libxvid",
            Codec::Mjpeg => "mjpeg",
        }
    }

    /// Only libx264/libx265 understand `-preset`; the other encoders (mpeg4,
    /// xvid, mjpeg) have no such concept, so the flag must be omitted for
    /// them rather than passed and ignored/rejected by ffmpeg.
    pub fn supports_preset(self) -> bool {
        matches!(self, Codec::Auto | Codec::H264 | Codec::H265)
    }
}

/// x264/x265 speed-vs-compression presets, passed straight through as
/// ffmpeg's `-preset` value. Faster presets trade off compression efficiency
/// (larger output for the same quality) for encoding speed; slower ones do
/// the opposite. Ignored for codecs where `Codec::supports_preset` is false.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    UltraFast,
    SuperFast,
    VeryFast,
    Faster,
    Fast,
    Medium,
    Slow,
    Slower,
    VerySlow,
    Placebo,
}

impl Preset {
    pub fn parse(s: &str) -> Option<Preset> {
        match s.trim().to_lowercase().as_str() {
            "ultrafast" => Some(Preset::UltraFast),
            "superfast" => Some(Preset::SuperFast),
            "veryfast" => Some(Preset::VeryFast),
            "faster" => Some(Preset::Faster),
            "fast" => Some(Preset::Fast),
            "medium" => Some(Preset::Medium),
            "slow" => Some(Preset::Slow),
            "slower" => Some(Preset::Slower),
            "veryslow" => Some(Preset::VerySlow),
            "placebo" => Some(Preset::Placebo),
            _ => None,
        }
    }

    fn ffmpeg_name(self) -> &'static str {
        match self {
            Preset::UltraFast => "ultrafast",
            Preset::SuperFast => "superfast",
            Preset::VeryFast => "veryfast",
            Preset::Faster => "faster",
            Preset::Fast => "fast",
            Preset::Medium => "medium",
            Preset::Slow => "slow",
            Preset::Slower => "slower",
            Preset::VerySlow => "veryslow",
            Preset::Placebo => "placebo",
        }
    }
}

impl Default for Preset {
    fn default() -> Self {
        Preset::VeryFast
    }
}

/// Hardware video encoders that auto-detection can consider, one per GPU
/// vendor's ffmpeg backend. Compiled-in support for an encoder (i.e. it
/// showing up in `ffmpeg -encoders`) does not mean the corresponding
/// hardware/driver is actually present on the running machine -- see
/// `probe_hw_encoder`.
///
/// `Nvenc`/`Amf` are unused for now (see `ENABLED_HW_CANDIDATES`) -- allowed
/// dead code rather than deleted, since the mapping in
/// `ffmpeg_encoder_name` below is already correct and ready to enable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum HwEncoder {
    Qsv,
    Nvenc,
    Amf,
}

impl HwEncoder {
    fn backend_label(self) -> &'static str {
        match self {
            HwEncoder::Qsv => "Intel Quick Sync (QSV)",
            HwEncoder::Nvenc => "NVIDIA NVENC",
            HwEncoder::Amf => "AMD AMF",
        }
    }

    /// Maps to the concrete ffmpeg encoder name for the requested codec
    /// family, or `None` if that family has no hardware path (mpeg4/xvid/
    /// mjpeg never do, regardless of backend).
    fn ffmpeg_encoder_name(self, codec: Codec) -> Option<&'static str> {
        match (self, codec) {
            (HwEncoder::Qsv, Codec::Auto | Codec::H264) => Some("h264_qsv"),
            (HwEncoder::Qsv, Codec::H265) => Some("hevc_qsv"),
            (HwEncoder::Nvenc, Codec::Auto | Codec::H264) => Some("h264_nvenc"),
            (HwEncoder::Nvenc, Codec::H265) => Some("hevc_nvenc"),
            (HwEncoder::Amf, Codec::Auto | Codec::H264) => Some("h264_amf"),
            (HwEncoder::Amf, Codec::H265) => Some("hevc_amf"),
            _ => None,
        }
    }
}

/// Hardware encoders auto-detection will actually probe/try, in priority
/// order. NVENC and AMF are fully implemented above (name mapping) and
/// below (`probe_hw_encoder` works identically for all three backends),
/// but are deliberately left out of this list until verified against real
/// Nvidia/AMD hardware -- the machine this was developed on only has an
/// Intel iGPU, so only the QSV path has been exercised end to end. Add
/// `HwEncoder::Nvenc`/`HwEncoder::Amf` here once confirmed on the
/// corresponding hardware.
const ENABLED_HW_CANDIDATES: &[HwEncoder] = &[HwEncoder::Qsv];

/// Describes which concrete ffmpeg video encoder ended up being used for a
/// job. Silently falling back from a requested hardware encoder to
/// software (or vice versa) is exactly the kind of thing a user needs
/// visibility into, so this is surfaced back out through `process_clip`'s
/// `on_encoder` callback rather than staying an internal implementation
/// detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncoderInfo {
    Hardware { backend: &'static str, ffmpeg_name: &'static str },
    Software { ffmpeg_name: &'static str, preset: Option<&'static str> },
    Remux,
}

impl EncoderInfo {
    pub fn describe(&self) -> String {
        match self {
            EncoderInfo::Hardware { backend, ffmpeg_name } => format!("Hardware: {backend} ({ffmpeg_name})"),
            EncoderInfo::Software { ffmpeg_name, preset: Some(p) } => {
                format!("Software: {ffmpeg_name} (Preset: {p})")
            }
            EncoderInfo::Software { ffmpeg_name, preset: None } => format!("Software: {ffmpeg_name}"),
            EncoderInfo::Remux => "Kein Re-Encode (Untertitel-Modus)".to_string(),
        }
    }
}

/// Probes whether `encoder_name` actually initializes on this machine by
/// attempting a trivial fraction-of-a-second encode into ffmpeg's null
/// muxer. This is the only reliable check: `ffmpeg -encoders` lists every
/// backend the binary was compiled with, not the ones whose driver/hardware
/// is actually present, and a missing/mismatched driver fails at encoder
/// init time rather than at compile time.
fn probe_hw_encoder(encoder_name: &str, pix_fmt: &str) -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", "nullsrc=size=320x240:rate=5:duration=0.2"])
        .args(["-frames:v", "1", "-c:v", encoder_name, "-pix_fmt", pix_fmt])
        .args(["-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Resolves the concrete `-c:v` value, pixel format, and any extra encoder
/// args to use for a job: tries `ENABLED_HW_CANDIDATES` first (if
/// `hw_accel` was requested and the codec has a hardware path), falling
/// back to the software codec/preset otherwise. Hardware encoders are given
/// a fixed quality target (`-global_quality`) rather than left on their
/// default constant-quantization mode, which otherwise produces much larger
/// files than the software encoders for comparable quality.
fn resolve_encoder(
    codec: Codec,
    preset: Preset,
    hw_accel: bool,
) -> (&'static str, &'static str, Vec<&'static str>, EncoderInfo) {
    if hw_accel {
        for hw in ENABLED_HW_CANDIDATES {
            if let Some(name) = hw.ffmpeg_encoder_name(codec) {
                if probe_hw_encoder(name, "nv12") {
                    return (
                        name,
                        "nv12",
                        vec!["-global_quality", "23"],
                        EncoderInfo::Hardware {
                            backend: hw.backend_label(),
                            ffmpeg_name: name,
                        },
                    );
                }
            }
        }
    }

    let name = codec.ffmpeg_codec_name();
    let preset_name = codec.supports_preset().then(|| preset.ffmpeg_name());
    let extra_args = match preset_name {
        Some(p) => vec!["-preset", p],
        None => Vec::new(),
    };
    (
        name,
        "yuv420p",
        extra_args,
        EncoderInfo::Software {
            ffmpeg_name: name,
            preset: preset_name,
        },
    )
}

/// Selects how dive telemetry is attached to the output video. `Overlay`
/// burns it into the pixels (the original behavior); `Subtitles` writes it
/// as a soft subtitle track instead, so the video is re-muxed losslessly
/// (`-c copy`, no decode/re-encode) and a player can toggle the info on and
/// off after the fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Overlay,
    Subtitles,
}

impl OutputMode {
    pub fn parse(s: &str) -> Option<OutputMode> {
        match s.trim().to_lowercase().as_str() {
            "overlay" => Some(OutputMode::Overlay),
            "subtitles" | "subtitle" => Some(OutputMode::Subtitles),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProcessingOptions {
    pub fields: Vec<Field>,
    pub codec: Codec,
    pub preset: Preset,
    pub hw_accel: bool,
    pub show_graph: bool,
    pub mode: OutputMode,
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
    info: EncoderInfo,
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
    preset: Preset,
    hw_accel: bool,
) -> Result<EncodeProcess, CoreError> {
    if let Some(parent) = output_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let size_arg = format!("{width}x{height}");
    let fps_arg = format!("{fps}");
    let (encoder_name, pix_fmt, extra_args, info) = resolve_encoder(codec, preset, hw_accel);

    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-y", "-f", "rawvideo", "-pix_fmt", "rgb24"])
        .args(["-s", &size_arg, "-r", &fps_arg, "-i", "pipe:0"])
        .arg("-i")
        .arg(original_input)
        .args(["-map", "0:v:0", "-map", "1:a:0?"])
        .args(["-c:v", encoder_name])
        .args(&extra_args);
    let mut child = cmd
        .args(["-pix_fmt", pix_fmt])
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
        info,
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
    mut on_encoder: impl FnMut(&EncoderInfo),
) -> Result<bool, CoreError> {
    if options.mode == OutputMode::Subtitles {
        on_encoder(&EncoderInfo::Remux);
        return process_clip_subtitles(job, samples, times, options, stop_flag, progress);
    }

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
        options.preset,
        options.hw_accel,
    )?;
    on_encoder(&encoder.info);

    let frame_size = info.width as usize * info.height as usize * 3;
    let total_estimate = info.estimated_frames.unwrap_or(0);

    let mut buf = vec![0u8; frame_size];
    let mut frame_idx: u64 = 0;
    let mut cancelled = false;
    let mut overlay_cache = OverlayCache::new();
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
        draw_overlay(&mut img, &lines, &mut overlay_cache);
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

/// Writes dive telemetry as a soft subtitle track instead of burning it into
/// the pixels: probes the clip's duration, renders one SRT cue per second via
/// `build_srt`, and re-muxes it into the output alongside a lossless
/// `-c copy` of the original streams (no decode/encode loop needed, since no
/// pixel touches the frames). A sidecar `.srt` is written next to the output
/// too, since embedded-subtitle toggle support varies by player/container.
pub fn process_clip_subtitles(
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
    let video_duration_sec = info
        .duration_sec
        .or_else(|| info.estimated_frames.map(|frames| frames as f64 / info.fps))
        .unwrap_or(0.0);

    let srt = build_srt(
        &options.fields,
        samples,
        times,
        job.video_sync_sec,
        job.csv_sync_sec,
        video_duration_sec,
    );

    if let Some(parent) = job.output_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let srt_path = job.output_path.with_extension("srt");
    std::fs::write(&srt_path, &srt)?;

    progress(0, 1);

    let mut child = Command::new("ffmpeg")
        .args(["-y", "-nostdin", "-v", "error", "-i"])
        .arg(&job.video_path)
        .arg("-i")
        .arg(&srt_path)
        .args(["-map", "0:v:0", "-map", "0:a:0?", "-map", "1:0"])
        .args(["-c:v", "copy", "-c:a", "copy", "-c:s", "mov_text"])
        .arg(&job.output_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CoreError::Ffmpeg(format!("Remux konnte nicht gestartet werden: {e}")))?;

    if let Some(stdout) = child.stdout.take() {
        spawn_stderr_drain(stdout, "ffmpeg-subtitle-stdout");
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_stderr_drain(stderr, "ffmpeg-subtitle");
    }

    let mut cancelled = false;
    loop {
        match child
            .try_wait()
            .map_err(|e| CoreError::Ffmpeg(format!("Fehler beim Warten auf ffmpeg: {e}")))?
        {
            Some(status) => {
                if !status.success() {
                    return Err(CoreError::Ffmpeg(format!("Remux beendete mit Fehler: {status}")));
                }
                break;
            }
            None => {
                if stop_flag.load(Ordering::Relaxed) {
                    let _ = child.kill();
                    let _ = child.wait();
                    cancelled = true;
                    break;
                }
                thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }

    if cancelled {
        let _ = std::fs::remove_file(&job.output_path);
        let _ = std::fs::remove_file(&srt_path);
        progress(0, 1);
        return Ok(false);
    }

    progress(1, 1);
    Ok(true)
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

    #[test]
    fn codec_parse_recognizes_h265_aliases() {
        assert_eq!(Codec::parse("hevc"), Codec::H265);
        assert_eq!(Codec::parse("h265"), Codec::H265);
        assert_eq!(Codec::parse("x265"), Codec::H265);
        assert_eq!(Codec::parse("HEVC"), Codec::H265);
        assert_eq!(Codec::parse(""), Codec::Auto);
    }

    #[test]
    fn codec_supports_preset_matches_x26x_only() {
        assert!(Codec::Auto.supports_preset());
        assert!(Codec::H264.supports_preset());
        assert!(Codec::H265.supports_preset());
        assert!(!Codec::Mpeg4.supports_preset());
        assert!(!Codec::Xvid.supports_preset());
        assert!(!Codec::Mjpeg.supports_preset());
    }

    #[test]
    fn preset_parse_round_trips_all_known_values() {
        let names = [
            "ultrafast", "superfast", "veryfast", "faster", "fast", "medium", "slow", "slower", "veryslow", "placebo",
        ];
        for name in names {
            assert_eq!(Preset::parse(name).unwrap().ffmpeg_name(), name);
        }
        assert_eq!(Preset::parse("bogus"), None);
    }

    #[test]
    fn probe_hw_encoder_rejects_bogus_encoder_name() {
        assert!(!probe_hw_encoder("definitely_not_a_real_encoder", "nv12"));
    }

    #[test]
    fn resolve_encoder_uses_software_when_hw_accel_disabled() {
        let (name, pix_fmt, args, info) = resolve_encoder(Codec::H264, Preset::Fast, false);
        assert_eq!(name, "libx264");
        assert_eq!(pix_fmt, "yuv420p");
        assert_eq!(args, vec!["-preset", "fast"]);
        assert!(matches!(
            info,
            EncoderInfo::Software {
                ffmpeg_name: "libx264",
                preset: Some("fast")
            }
        ));
    }

    #[test]
    fn resolve_encoder_ignores_hw_accel_for_codecs_without_a_hardware_path() {
        // mpeg4/xvid/mjpeg have no hardware encoder in any backend, so
        // hw_accel=true must still resolve to the software encoder.
        let (name, pix_fmt, args, info) = resolve_encoder(Codec::Mpeg4, Preset::VeryFast, true);
        assert_eq!(name, "mpeg4");
        assert_eq!(pix_fmt, "yuv420p");
        assert!(args.is_empty());
        assert!(matches!(
            info,
            EncoderInfo::Software {
                ffmpeg_name: "mpeg4",
                preset: None
            }
        ));
    }

    #[test]
    fn encoder_info_describe_covers_all_variants() {
        assert_eq!(
            EncoderInfo::Hardware {
                backend: "Intel Quick Sync (QSV)",
                ffmpeg_name: "h264_qsv"
            }
            .describe(),
            "Hardware: Intel Quick Sync (QSV) (h264_qsv)"
        );
        assert_eq!(
            EncoderInfo::Software {
                ffmpeg_name: "libx264",
                preset: Some("veryfast")
            }
            .describe(),
            "Software: libx264 (Preset: veryfast)"
        );
        assert_eq!(
            EncoderInfo::Software {
                ffmpeg_name: "mpeg4",
                preset: None
            }
            .describe(),
            "Software: mpeg4"
        );
        assert_eq!(EncoderInfo::Remux.describe(), "Kein Re-Encode (Untertitel-Modus)");
    }

    /// Opportunistic: exercises the real hardware-acceleration path when
    /// this machine has a working hardware encoder (verified separately to
    /// have Intel Quick Sync during development), and degrades to a no-op
    /// elsewhere (e.g. CI runners without QSV/NVENC/AMF) rather than
    /// failing on hardware this crate cannot assume is present.
    #[test]
    fn processes_synthetic_clip_with_hw_accel_when_available() {
        if !ENABLED_HW_CANDIDATES
            .iter()
            .any(|hw| hw.ffmpeg_encoder_name(Codec::Auto).is_some_and(|name| probe_hw_encoder(name, "nv12")))
        {
            eprintln!("skipping processes_synthetic_clip_with_hw_accel_when_available: no working hw encoder here");
            return;
        }

        let dir = make_test_dir("hw_accel");
        let clip = synth_clip(&dir, "input.mp4", 2, 5);
        let output = dir.join("output.mp4");

        let job = ClipJob {
            video_path: clip,
            output_path: output.clone(),
            video_sync_sec: 0.0,
            csv_sync_sec: 0.0,
            video_start_utc: None,
        };
        let samples = vec![sample(0.0, 1.0), sample(1.0, 5.0)];
        let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();
        let options = ProcessingOptions {
            fields: vec![Field::Depth],
            codec: Codec::Auto,
            preset: Preset::VeryFast,
            hw_accel: true,
            show_graph: false,
            mode: OutputMode::Overlay,
        };
        let stop_flag = Arc::new(AtomicBool::new(false));

        let mut encoder_info = None;
        let completed = process_clip(&job, &samples, &times, &options, &stop_flag, |_, _| {}, |info| {
            encoder_info = Some(info.clone());
        })
        .unwrap();

        assert!(completed);
        assert!(output.exists());
        assert!(matches!(encoder_info, Some(EncoderInfo::Hardware { .. })));
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
            preset: Preset::VeryFast,
            hw_accel: false,
            show_graph: true,
            mode: OutputMode::Overlay,
        };
        let stop_flag = Arc::new(AtomicBool::new(false));

        let mut progress_calls = Vec::new();
        let mut encoder_info = None;
        let completed = process_clip(
            &job,
            &samples,
            &times,
            &options,
            &stop_flag,
            |done, total| {
                progress_calls.push((done, total));
            },
            |info| encoder_info = Some(info.clone()),
        )
        .unwrap();
        assert!(matches!(encoder_info, Some(EncoderInfo::Software { ffmpeg_name: "libx264", .. })));

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
            preset: Preset::VeryFast,
            hw_accel: false,
            show_graph: false,
            mode: OutputMode::Overlay,
        };
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_flag_for_progress = stop_flag.clone();

        let completed = process_clip(
            &job,
            &samples,
            &times,
            &options,
            &stop_flag,
            move |done, _total| {
                if done >= 5 {
                    stop_flag_for_progress.store(true, Ordering::Relaxed);
                }
            },
            |_| {},
        )
        .unwrap();

        assert!(!completed);
        assert!(output.exists());
    }

    #[test]
    fn processes_synthetic_clip_with_h265_and_ultrafast_preset() {
        let dir = make_test_dir("h265");
        let clip = synth_clip(&dir, "input.mp4", 2, 5);
        let output = dir.join("output.mp4");

        let job = ClipJob {
            video_path: clip,
            output_path: output.clone(),
            video_sync_sec: 0.0,
            csv_sync_sec: 0.0,
            video_start_utc: None,
        };
        let samples = vec![sample(0.0, 1.0), sample(1.0, 5.0)];
        let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();
        let options = ProcessingOptions {
            fields: vec![Field::Depth],
            codec: Codec::H265,
            preset: Preset::UltraFast,
            hw_accel: false,
            show_graph: false,
            mode: OutputMode::Overlay,
        };
        let stop_flag = Arc::new(AtomicBool::new(false));

        let completed = process_clip(&job, &samples, &times, &options, &stop_flag, |_, _| {}, |_| {}).unwrap();
        assert!(completed);
        assert!(output.exists());

        let ffprobe_out = Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "v", "-show_entries", "stream=codec_name"])
            .args(["-of", "csv=p=0"])
            .arg(&output)
            .output()
            .unwrap();
        let codec_name = String::from_utf8_lossy(&ffprobe_out.stdout);
        assert!(codec_name.trim().contains("hevc"), "expected hevc codec, got: {codec_name}");
    }

    #[test]
    fn processes_synthetic_clip_in_subtitle_mode_without_reencoding() {
        let dir = make_test_dir("subtitle_mode");
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
            fields: vec![Field::Time, Field::Depth],
            codec: Codec::Auto,
            preset: Preset::VeryFast,
            hw_accel: false,
            show_graph: false,
            mode: OutputMode::Subtitles,
        };
        let stop_flag = Arc::new(AtomicBool::new(false));

        let completed = process_clip(&job, &samples, &times, &options, &stop_flag, |_, _| {}, |_| {}).unwrap();

        assert!(completed);
        assert!(output.exists());

        let srt_path = output.with_extension("srt");
        assert!(srt_path.exists());
        let srt_text = std::fs::read_to_string(&srt_path).unwrap();
        assert!(srt_text.contains("Tiefe: 1.0 m"));

        let info = probe_video(&output).unwrap();
        assert_eq!(info.width, 160);
        assert_eq!(info.height, 120);

        let ffprobe_out = Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "s", "-show_entries", "stream=codec_type"])
            .args(["-of", "csv=p=0"])
            .arg(&output)
            .output()
            .unwrap();
        let has_subtitle = String::from_utf8_lossy(&ffprobe_out.stdout).contains("subtitle");
        assert!(has_subtitle, "expected a subtitle stream in the muxed output");
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
