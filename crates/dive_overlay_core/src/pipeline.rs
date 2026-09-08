use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};

use image::RgbImage;

use crate::error::CoreError;
use crate::ffprobe::probe_video;
use crate::model::{ClipJob, DiveSample, Field};
use crate::overlay::{build_overlay_lines, draw_depth_graph_yuv, draw_overlay_yuv, OverlayCache};
use crate::subtitle::build_srt;
use crate::yuv::{ColorRange, Yuv420Frame};

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
    /// Returns `None` for unrecognized names so callers can report a typo
    /// instead of silently encoding with the default (libx264).
    pub fn parse(s: &str) -> Option<Codec> {
        match s.trim().to_lowercase().as_str() {
            "" | "auto" => Some(Codec::Auto),
            "avc1" | "h264" => Some(Codec::H264),
            "hevc" | "h265" | "x265" => Some(Codec::H265),
            "mp4v" => Some(Codec::Mpeg4),
            "xvid" => Some(Codec::Xvid),
            "mjpg" | "mjpeg" => Some(Codec::Mjpeg),
            _ => None,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Preset {
    UltraFast,
    SuperFast,
    #[default]
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

/// Hardware video encoders that auto-detection can consider, one per GPU
/// vendor's ffmpeg backend. Compiled-in support for an encoder (i.e. it
/// showing up in `ffmpeg -encoders`) does not mean the corresponding
/// hardware/driver is actually present on the running machine -- see
/// `probe_hw_encoder`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

    /// Extra `-c:v` args that pin the encoder to a fixed-quality target
    /// instead of its default mode (which otherwise produces much larger
    /// files than the software encoders for comparable quality). The knob
    /// differs per backend: QSV/NVENC honor `-global_quality`, while AMF
    /// ignores it and needs its own constant-quantization rate control.
    fn quality_args(self) -> Vec<&'static str> {
        match self {
            HwEncoder::Qsv | HwEncoder::Nvenc => vec!["-global_quality", "23"],
            HwEncoder::Amf => vec!["-rc", "cqp", "-qp_i", "23", "-qp_p", "23"],
        }
    }
}

/// Hardware encoders auto-detection will actually probe/try, in priority
/// order. NVENC has been confirmed end to end on real Nvidia hardware
/// (GeForce RTX 2070); AMF (`h264_amf`/`hevc_amf`) has been confirmed on an
/// AMD iGPU. `probe_hw_encoder` runs the same runtime init check for every
/// backend, so a machine without the matching GPU simply fails the probe
/// and falls back to software.
const ENABLED_HW_CANDIDATES: &[HwEncoder] = &[HwEncoder::Qsv, HwEncoder::Nvenc, HwEncoder::Amf];

/// Describes which concrete ffmpeg video encoder ended up being used for a
/// job. Silently falling back from a requested hardware encoder to
/// software (or vice versa) is exactly the kind of thing a user needs
/// visibility into, so this is surfaced back out through `process_clip`'s
/// `on_encoder` callback rather than staying an internal implementation
/// detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncoderInfo {
    Hardware {
        backend: &'static str,
        ffmpeg_name: &'static str,
    },
    Software {
        ffmpeg_name: &'static str,
        preset: Option<&'static str>,
    },
    Remux,
}

impl EncoderInfo {
    pub fn describe(&self) -> String {
        match self {
            EncoderInfo::Hardware { backend, ffmpeg_name } => format!("Hardware: {backend} ({ffmpeg_name})"),
            EncoderInfo::Software {
                ffmpeg_name,
                preset: Some(p),
            } => {
                format!("Software: {ffmpeg_name} (Preset: {p})")
            }
            EncoderInfo::Software {
                ffmpeg_name,
                preset: None,
            } => format!("Software: {ffmpeg_name}"),
            EncoderInfo::Remux => "No re-encode (subtitle mode)".to_string(),
        }
    }
}

/// Memoizes `run_hw_encoder_probe` per encoder *and* frame size. A
/// multi-clip run would otherwise spawn the same probe for every job, and
/// the answer genuinely differs per resolution (see below).
type HwProbeCache = OnceLock<Mutex<HashMap<(&'static str, u32, u32), bool>>>;
static HW_PROBE_CACHE: HwProbeCache = OnceLock::new();

/// Probes whether `encoder_name` actually initializes on this machine by
/// attempting a trivial fraction-of-a-second encode into ffmpeg's null
/// muxer. This is the only reliable check: `ffmpeg -encoders` lists every
/// backend the binary was compiled with, not the ones whose driver/hardware
/// is actually present, and a missing/mismatched driver fails at encoder
/// init time rather than at compile time.
///
/// The probe MUST run at the frame size the job will really encode. A
/// hardware encoder's maximum resolution is a property of the silicon, not
/// of the ffmpeg build, and the limit is a clamp on each *dimension*
/// independently rather than a budget of total pixels: the AMD iGPU here
/// encodes 4096x4096 (16.8 MP) with `h264_amf` but refuses 4608x2592
/// (11.9 MP) with `encoder->Init() failed with error 5`, because 4608
/// exceeds its per-axis maximum of 4096. Probing at a fixed small size
/// therefore reported "available" and the real encode then died at init,
/// failing the whole job -- for exactly the 5.3K GoPro footage this tool is
/// pointed at, which busts the limit on width alone. Only a probe at the
/// job's own width and height can see that, and it turns the failure into a
/// silent, correct fallback to software.
fn probe_hw_encoder(encoder_name: &'static str, pix_fmt: &str, width: u32, height: u32) -> bool {
    let cache = HW_PROBE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (encoder_name, width, height);
    if let Ok(cache) = cache.lock() {
        if let Some(&known) = cache.get(&key) {
            return known;
        }
    }

    let available = run_hw_encoder_probe(encoder_name, pix_fmt, width, height);
    if let Ok(mut cache) = cache.lock() {
        cache.insert(key, available);
    }
    available
}

fn run_hw_encoder_probe(encoder_name: &str, pix_fmt: &str, width: u32, height: u32) -> bool {
    if width == 0 || height == 0 {
        return false;
    }
    Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args([
            "-f",
            "lavfi",
            "-i",
            &format!("nullsrc=size={width}x{height}:rate=5:duration=0.2"),
        ])
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
/// a fixed quality target (see `HwEncoder::quality_args`) rather than left
/// on their default rate-control mode, which otherwise produces much larger
/// files than the software encoders for comparable quality.
///
/// `width`/`height` are the frame size the job will encode, and are passed
/// straight to the probe -- a hardware encoder that can't do this size is
/// not "available" for this job, however well it does 1080p.
pub(crate) fn resolve_encoder(
    codec: Codec,
    preset: Preset,
    hw_accel: bool,
    width: u32,
    height: u32,
) -> (&'static str, &'static str, Vec<&'static str>, EncoderInfo) {
    if hw_accel {
        for hw in ENABLED_HW_CANDIDATES {
            if let Some(name) = hw.ffmpeg_encoder_name(codec) {
                if probe_hw_encoder(name, "nv12", width, height) {
                    return (
                        name,
                        "nv12",
                        hw.quality_args(),
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

/// Frame size of the burned-in overlay output. `Original` keeps whatever
/// the decoder produces; the fixed rungs scale the picture *down* to fit a
/// box of that size, never up, so picking 4K for 1080p footage is a no-op
/// rather than a blurry upscale.
///
/// The scale is applied in the decoder (see `spawn_decoder`), not the
/// encoder, and that placement is the whole point of the feature: it shrinks
/// the encode, the pipe traffic (23.8 MB -> 12.4 MB per planar frame going
/// from 5.3K to 4K) and the overlay's draw area all at once, where an
/// encoder-side filter would shrink only the encode.
///
/// It also drops the frame size below the hardware encoders' per-axis limits
/// -- `resolve_encoder` probes at the *output* size, so a 5.3K job that could
/// only ever use software silently gains the AMF path at 4K. Measured on the
/// test clip, that is the difference between 4.52 and 14.60 fps.
///
/// Note 4:2:0 has no way to represent an odd width or height; `process_clip`
/// rejects such a frame size rather than letting the raw stream go out of
/// step with the buffer sized for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputResolution {
    Original,
    Uhd4k,
    Fhd1080p,
    Hd720p,
}

/// Every selectable value, in menu order -- shared by the CLI's help text
/// and the GUI's combo box so the two can't drift apart.
pub const OUTPUT_RESOLUTIONS: [OutputResolution; 4] = [
    OutputResolution::Original,
    OutputResolution::Uhd4k,
    OutputResolution::Fhd1080p,
    OutputResolution::Hd720p,
];

impl OutputResolution {
    /// Returns `None` for unrecognized names so callers can report a typo
    /// instead of silently keeping the original size.
    pub fn parse(s: &str) -> Option<OutputResolution> {
        match s.trim().to_lowercase().as_str() {
            "" | "original" | "source" | "native" => Some(OutputResolution::Original),
            "4k" | "uhd" | "2160p" | "2160" => Some(OutputResolution::Uhd4k),
            "1080p" | "1080" | "fhd" => Some(OutputResolution::Fhd1080p),
            "720p" | "720" | "hd" => Some(OutputResolution::Hd720p),
            _ => None,
        }
    }

    /// The canonical name, i.e. the one `parse` round-trips.
    pub fn as_str(self) -> &'static str {
        match self {
            OutputResolution::Original => "original",
            OutputResolution::Uhd4k => "4k",
            OutputResolution::Fhd1080p => "1080p",
            OutputResolution::Hd720p => "720p",
        }
    }

    /// Label for the GUI combo box.
    pub fn label(self) -> &'static str {
        match self {
            OutputResolution::Original => "Original",
            OutputResolution::Uhd4k => "4K (3840x2160)",
            OutputResolution::Fhd1080p => "1080p (1920x1080)",
            OutputResolution::Hd720p => "720p (1280x720)",
        }
    }

    /// The target box as (long side, short side), or `None` for `Original`.
    fn box_sides(self) -> Option<(u32, u32)> {
        match self {
            OutputResolution::Original => None,
            OutputResolution::Uhd4k => Some((3840, 2160)),
            OutputResolution::Fhd1080p => Some((1920, 1080)),
            OutputResolution::Hd720p => Some((1280, 720)),
        }
    }

    /// The frame size a `width`x`height` source becomes under this setting.
    ///
    /// The box is oriented to match the source, so portrait footage is
    /// limited by its own short side (1080p portrait is 1080x1920) instead
    /// of being squeezed into a landscape box. The aspect ratio is always
    /// preserved -- the picture is fitted inside the box, never stretched or
    /// cropped to it -- and both dimensions are rounded to even numbers,
    /// which yuv420p chroma subsampling requires.
    pub fn target_dimensions(self, width: u32, height: u32) -> (u32, u32) {
        let Some((long, short)) = self.box_sides() else {
            return (width, height);
        };
        if width == 0 || height == 0 {
            return (width, height);
        }
        let (box_w, box_h) = if width >= height { (long, short) } else { (short, long) };
        if width <= box_w && height <= box_h {
            return (width, height);
        }
        let factor = (box_w as f64 / width as f64).min(box_h as f64 / height as f64);
        (
            round_to_even(width as f64 * factor),
            round_to_even(height as f64 * factor),
        )
    }
}

/// Rounds a scaled dimension to an even number of pixels, with a floor of 2:
/// yuv420p halves both axes for the chroma planes, so an odd width or height
/// is rejected by the encoder outright.
fn round_to_even(value: f64) -> u32 {
    let rounded = value.round().max(2.0) as u32;
    let even = if rounded.is_multiple_of(2) {
        rounded
    } else {
        rounded - 1
    };
    even.max(2)
}

#[derive(Debug, Clone)]
pub struct ProcessingOptions {
    pub fields: Vec<Field>,
    pub codec: Codec,
    pub preset: Preset,
    pub hw_accel: bool,
    pub show_graph: bool,
    pub mode: OutputMode,
    pub interpolate: bool,
    /// Ignored in `OutputMode::Subtitles`, which re-muxes losslessly and so
    /// cannot resize without defeating its own purpose.
    pub resolution: OutputResolution,
}

/// Reads an ffmpeg child's stderr to EOF on a background thread (so the
/// child never blocks on a full stderr pipe) and hands the text back via
/// the join handle, so it can be attached to the error message if the
/// process fails -- "exit code: 1" on its own tells a GUI user nothing.
pub(crate) fn spawn_stderr_capture(mut pipe: impl Read + Send + 'static) -> JoinHandle<String> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).trim().to_string()
    })
}

/// Collects a captured stderr. On success any leftover text (warnings) is
/// echoed to our own stderr as before; on failure it is returned so the
/// caller can fold it into the `CoreError` instead.
pub(crate) fn finish_stderr(handle: JoinHandle<String>, label: &str, succeeded: bool) -> String {
    let text = handle.join().unwrap_or_default();
    if succeeded && !text.is_empty() {
        eprintln!("[{label}] {text}");
    }
    text
}

pub(crate) fn ffmpeg_failure(what: &str, status: std::process::ExitStatus, stderr_text: &str) -> CoreError {
    if stderr_text.is_empty() {
        CoreError::Ffmpeg(format!("{what} exited with error: {status}"))
    } else {
        CoreError::Ffmpeg(format!("{what} exited with error: {status}\n{stderr_text}"))
    }
}

/// Best-effort absolute form of `path` for equality comparison: the real
/// canonical path if it exists, otherwise the canonical parent joined with
/// the file name (the output file usually doesn't exist yet).
pub(crate) fn resolved_for_compare(path: &Path) -> PathBuf {
    if let Ok(p) = path.canonicalize() {
        return p;
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let parent = parent.canonicalize().unwrap_or(parent);
    match path.file_name() {
        Some(name) => parent.join(name),
        None => path.to_path_buf(),
    }
}

/// ffmpeg refuses an output whose path string is byte-identical to an
/// input, but a differently spelled path (relative vs absolute, other case
/// on Windows) slips through and the encoder then truncates the very file
/// the decoder is still reading. Catch that before spawning anything.
pub(crate) fn ensure_output_differs_from_input(video_path: &Path, output_path: &Path) -> Result<(), CoreError> {
    let a = resolved_for_compare(video_path);
    let b = resolved_for_compare(output_path);
    let same = if cfg!(windows) {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    } else {
        a == b
    };
    if same {
        return Err(CoreError::Other(format!(
            "Output path must differ from the input video: {}",
            output_path.display()
        )));
    }
    Ok(())
}

struct DecodeProcess {
    child: Child,
    stdout: ChildStdout,
    stderr: JoinHandle<String>,
}

/// Spawns an ffmpeg process that decodes `video_path` to a raw planar 4:2:0
/// stream on stdout at a constant `fps`. Constant-frame-rate output is essential:
/// the pipeline derives each frame's timestamp as `frame_idx / fps` and the
/// encoder is told the same `-r`, so with `passthrough` a variable-frame-rate
/// source (typical phone footage) would emit fewer/more frames than
/// `duration * fps`, making the overlay clock drift and `-shortest` truncate
/// the output against the audio. `-nostdin` is safe here because this
/// process's stdin is unused -- do not use it on the encoder, whose stdin
/// carries real frame data.
///
/// `scale_to` downsizes the picture here, in the decoder, rather than in the
/// encoder: every later stage (the pipe, the overlay drawing, the encode)
/// then works on the smaller frame. The target is computed by
/// `OutputResolution::target_dimensions` and passed in explicitly rather
/// than expressed as an ffmpeg filter expression, so this process and
/// ffmpeg cannot disagree about the frame size -- the read buffer in
/// `process_clip` is sized from the same pair of numbers.
///
/// Frames cross the pipe as planar 4:2:0, in the source's *own* colour
/// range (`range.raw_pixel_format()`), which is what video codecs decode to
/// natively. Asking for the native format means ffmpeg hands the planes over
/// with no swscale pass at all. The previous `rgb24` cost two full
/// conversions per frame -- out of the format both ends already spoke and
/// straight back into it -- twice the bytes on both pipes, and a lossy
/// chroma upsample/re-subsample round-trip on every single frame.
fn spawn_decoder(
    video_path: &Path,
    fps: f64,
    scale_to: Option<(u32, u32)>,
    range: ColorRange,
) -> Result<DecodeProcess, CoreError> {
    let fps_arg = format!("{fps}");
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-nostdin", "-v", "error", "-i"]).arg(video_path).args([
        "-an",
        "-f",
        "rawvideo",
        "-pix_fmt",
        range.raw_pixel_format(),
    ]);
    if let Some((width, height)) = scale_to {
        cmd.args(["-vf", &format!("scale={width}:{height}")]);
    }
    let mut child = cmd
        .args(["-fps_mode", "cfr", "-r", &fps_arg, "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CoreError::Ffmpeg(format!("Failed to start decoder: {e}")))?;

    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = spawn_stderr_capture(child.stderr.take().expect("stderr was piped"));

    Ok(DecodeProcess { child, stdout, stderr })
}

/// Largest single write handed to the encoder's stdin pipe.
///
/// Windows fails `WriteFile` on a pipe with `ERROR_NO_SYSTEM_RESOURCES`
/// (os error 1450) when one write is large enough to exhaust the kernel's
/// non-paged pool -- a full 5.3K frame is tens of megabytes (~48 MB back when
/// the pipe carried rgb24, ~24 MB now that it carries planar 4:2:0), and a
/// full-length dive eventually trips it even though the first frames go
/// through. Splitting each frame into small writes keeps every request well
/// under the limit. Halving the frame size halved the number of chunks too,
/// but the hazard is unchanged, so the retry stays.
const PIPE_CHUNK: usize = 256 * 1024;

/// Writes one raw frame to the encoder in `PIPE_CHUNK`-sized pieces,
/// retrying a chunk that hits the transient Windows resource error above
/// (the pool frees up as ffmpeg drains the pipe).
fn write_frame(stdin: &mut ChildStdin, frame: &[u8]) -> std::io::Result<()> {
    for chunk in frame.chunks(PIPE_CHUNK) {
        let mut attempt = 0u64;
        loop {
            match stdin.write_all(chunk) {
                Ok(()) => break,
                Err(e) if is_transient_pipe_error(&e) && attempt < 10 => {
                    attempt += 1;
                    thread::sleep(std::time::Duration::from_millis(50 * attempt));
                }
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

/// `ERROR_NO_SYSTEM_RESOURCES` (1450) and `EINTR` both mean "try again",
/// not "the encoder died".
fn is_transient_pipe_error(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::Interrupted || e.raw_os_error() == Some(1450)
}

struct EncodeProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    stderr: JoinHandle<String>,
    info: EncoderInfo,
}

/// Spawns an ffmpeg process that reads raw planar 4:2:0 frames on stdin, muxes in
/// the original file's audio track (mapped optionally via `1:a:0?` so
/// audio-less clips don't fail the job), and writes the final mp4. `-y`
/// (not `-nostdin`) belongs here since stdin carries real data -- `-y`
/// alone prevents ffmpeg from trying to read an interactive overwrite
/// confirmation off that same pipe.
#[allow(clippy::too_many_arguments)]
fn spawn_encoder(
    output_path: &Path,
    original_input: &Path,
    width: u32,
    height: u32,
    fps: f64,
    codec: Codec,
    preset: Preset,
    hw_accel: bool,
    range: ColorRange,
) -> Result<EncodeProcess, CoreError> {
    if let Some(parent) = output_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let size_arg = format!("{width}x{height}");
    let fps_arg = format!("{fps}");
    let (encoder_name, pix_fmt, extra_args, info) = resolve_encoder(codec, preset, hw_accel, width, height);

    // The input pix_fmt matches what the decoder emits; the output one
    // (further down, from `resolve_encoder`) is unchanged, so full-range
    // sources still get the same single range conversion at encode time that
    // they always did -- via rgb24 before, directly now.
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-y",
        "-v",
        "error",
        "-f",
        "rawvideo",
        "-pix_fmt",
        range.raw_pixel_format(),
    ])
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
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CoreError::Ffmpeg(format!("Failed to start encoder: {e}")))?;

    let stdin = child.stdin.take().expect("stdin was piped");
    let stderr = spawn_stderr_capture(child.stderr.take().expect("stderr was piped"));

    Ok(EncodeProcess {
        child,
        stdin: Some(stdin),
        stderr,
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
    ensure_output_differs_from_input(&job.video_path, &job.output_path)?;

    let info = probe_video(&job.video_path)?;
    if info.width == 0 || info.height == 0 {
        return Err(CoreError::Ffprobe(format!(
            "Could not determine video resolution: {}",
            job.video_path.display()
        )));
    }

    // Everything downstream of the decoder works at the *output* size: the
    // pipe buffer, the overlay drawing, and -- via `resolve_encoder` -- the
    // hardware-encoder probe, which is why downscaling can turn a job that
    // had no hardware path at its native size into one that does.
    let (out_width, out_height) = options.resolution.target_dimensions(info.width, info.height);
    let scale_to = ((out_width, out_height) != (info.width, info.height)).then_some((out_width, out_height));

    // 4:2:0 stores one chroma sample per 2x2 luma block, so it has no way to
    // represent an odd width or height. `target_dimensions` guarantees even
    // sizes for the fixed rungs, but `Original` passes the source through --
    // and an odd-sized source would otherwise desynchronize the raw stream
    // rather than fail, so reject it here with a message that says why.
    if !out_width.is_multiple_of(2) || !out_height.is_multiple_of(2) {
        return Err(CoreError::Ffmpeg(format!(
            "Frame size {out_width}x{out_height} has an odd dimension, which planar 4:2:0 video cannot represent. \
             Choose an explicit output resolution (e.g. --resolution 1080p) to scale it to an even size."
        )));
    }

    let range = ColorRange::from_full_range_flag(info.full_range);

    let mut decoder = spawn_decoder(&job.video_path, info.fps, scale_to, range)?;
    let mut encoder = spawn_encoder(
        &job.output_path,
        &job.video_path,
        out_width,
        out_height,
        info.fps,
        options.codec,
        options.preset,
        options.hw_accel,
        range,
    )?;
    on_encoder(&encoder.info);

    let frame_size = Yuv420Frame::buffer_size(out_width, out_height);
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
            Err(e) => {
                let _ = decoder.child.kill();
                let _ = encoder.child.kill();
                return Err(CoreError::Ffmpeg(format!("Error reading frames: {e}")));
            }
        }

        let mut img = Yuv420Frame::from_raw(out_width, out_height, std::mem::take(&mut buf))
            .ok_or_else(|| CoreError::Ffmpeg("Invalid frame size".to_string()))?;

        let video_sec = frame_idx as f64 / info.fps;
        let dive_sec = job.dive_start_sec + video_sec;

        let lines = build_overlay_lines(&options.fields, samples, times, dive_sec, options.interpolate);
        draw_overlay_yuv(&mut img, &lines, &mut overlay_cache, range);
        if options.show_graph {
            draw_depth_graph_yuv(&mut img, samples, times, dive_sec, range, &mut overlay_cache);
        }

        if let Some(stdin) = encoder.stdin.as_mut() {
            if let Err(e) = write_frame(stdin, img.as_raw()) {
                // Almost always a broken pipe because the encoder died (bad
                // codec, unwritable output, ...): its stderr holds the real
                // reason, so surface that instead of just "broken pipe".
                let _ = decoder.child.kill();
                let _ = decoder.child.wait();
                encoder.stdin.take();
                let status = encoder.child.wait();
                let stderr_text = finish_stderr(encoder.stderr, "ffmpeg-encode", false);
                return Err(match status {
                    Ok(status) if !status.success() => ffmpeg_failure("Encoder", status, &stderr_text),
                    _ if !stderr_text.is_empty() => {
                        CoreError::Ffmpeg(format!("Error writing frames: {e}\n{stderr_text}"))
                    }
                    _ => CoreError::Ffmpeg(format!("Error writing frames: {e}")),
                });
            }
        }

        buf = img.into_raw();
        frame_idx += 1;
        if frame_idx.is_multiple_of(10) {
            progress(frame_idx, total_estimate);
        }
    }

    // The decode process should already be at EOF in the normal case; kill
    // defensively on early cancellation.
    let _ = decoder.child.kill();
    let decoder_status = decoder.child.wait();
    let decoder_ok = cancelled || decoder_status.as_ref().map(|s| s.success()).unwrap_or(false);
    let decoder_stderr = finish_stderr(decoder.stderr, "ffmpeg-decode", decoder_ok);

    // Dropping the encoder's stdin lets ffmpeg see EOF and finalize the mp4
    // (moov atom etc.) -- keeping the handle alive here is a common hang cause.
    encoder.stdin.take();
    let status = encoder
        .child
        .wait()
        .map_err(|e| CoreError::Ffmpeg(format!("Encoder process failed: {e}")))?;
    let encoder_stderr = finish_stderr(encoder.stderr, "ffmpeg-encode", status.success());
    if !status.success() {
        return Err(ffmpeg_failure("Encoder", status, &encoder_stderr));
    }
    if !decoder_ok {
        if let Ok(decoder_status) = decoder_status {
            return Err(ffmpeg_failure("Decoder", decoder_status, &decoder_stderr));
        }
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
    ensure_output_differs_from_input(&job.video_path, &job.output_path)?;

    let info = probe_video(&job.video_path)?;
    let video_duration_sec = info
        .duration_sec
        .or_else(|| info.estimated_frames.map(|frames| frames as f64 / info.fps))
        .unwrap_or(0.0);

    let srt = build_srt(
        &options.fields,
        samples,
        times,
        job.dive_start_sec,
        video_duration_sec,
        options.interpolate,
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
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CoreError::Ffmpeg(format!("Failed to start remux: {e}")))?;

    let stderr = spawn_stderr_capture(child.stderr.take().expect("stderr was piped"));

    let mut cancelled = false;
    loop {
        match child
            .try_wait()
            .map_err(|e| CoreError::Ffmpeg(format!("Error waiting for ffmpeg: {e}")))?
        {
            Some(status) => {
                let stderr_text = finish_stderr(stderr, "ffmpeg-subtitle", status.success());
                if !status.success() {
                    return Err(ffmpeg_failure("Remux", status, &stderr_text));
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
            "Could not determine video resolution: {}",
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
            .map_err(|e| CoreError::Ffmpeg(format!("Failed to start ffmpeg: {e}")))?;

        if output.stdout.len() >= frame_size {
            Ok(Some(output.stdout))
        } else {
            Ok(None)
        }
    };

    let bytes = match try_decode(true)? {
        Some(bytes) => bytes,
        None => try_decode(false)?
            .ok_or_else(|| CoreError::Ffmpeg("Could not read a frame at the sync point".to_string()))?,
    };

    RgbImage::from_raw(info.width, info.height, bytes[..frame_size].to_vec())
        .ok_or_else(|| CoreError::Ffmpeg("Invalid frame size".to_string()))
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
        assert_eq!(Codec::parse("hevc"), Some(Codec::H265));
        assert_eq!(Codec::parse("h265"), Some(Codec::H265));
        assert_eq!(Codec::parse("x265"), Some(Codec::H265));
        assert_eq!(Codec::parse("HEVC"), Some(Codec::H265));
        assert_eq!(Codec::parse(""), Some(Codec::Auto));
        assert_eq!(Codec::parse("auto"), Some(Codec::Auto));
        assert_eq!(Codec::parse("h.264"), None);
    }

    #[test]
    fn rejects_output_equal_to_input_even_when_spelled_differently() {
        let dir = make_test_dir("same_path");
        let clip = synth_clip(&dir, "input.mp4", 1, 5);
        let relative_spelling = dir.join("sub").join("..").join("INPUT.MP4");
        std::fs::create_dir_all(dir.join("sub")).unwrap();

        assert!(ensure_output_differs_from_input(&clip, &clip).is_err());
        if cfg!(windows) {
            assert!(ensure_output_differs_from_input(&clip, &relative_spelling).is_err());
        }
        assert!(ensure_output_differs_from_input(&clip, &dir.join("other.mp4")).is_ok());

        let job = ClipJob {
            video_path: clip.clone(),
            output_path: clip.clone(),
            dive_start_sec: 0.0,
            video_start_utc: None,
        };
        let options = ProcessingOptions {
            fields: vec![Field::Depth],
            codec: Codec::Auto,
            preset: Preset::VeryFast,
            hw_accel: false,
            show_graph: false,
            mode: OutputMode::Overlay,
            interpolate: false,
            resolution: OutputResolution::Original,
        };
        let err = process_clip(
            &job,
            &[],
            &[],
            &options,
            &Arc::new(AtomicBool::new(false)),
            |_, _| {},
            |_| {},
        )
        .unwrap_err();
        assert!(err.to_string().contains("must differ"), "unexpected error: {err}");
        // The input must be untouched.
        assert!(probe_video(&clip).is_ok());
    }

    #[test]
    fn encoder_failure_reports_ffmpeg_stderr() {
        let dir = make_test_dir("encoder_error");
        let clip = synth_clip(&dir, "input.mp4", 1, 5);
        // A directory as the output path makes the encoder fail at open time.
        let output_dir = dir.join("output_is_a_dir.mp4");
        std::fs::create_dir_all(&output_dir).unwrap();

        let job = ClipJob {
            video_path: clip,
            output_path: output_dir,
            dive_start_sec: 0.0,
            video_start_utc: None,
        };
        let options = ProcessingOptions {
            fields: vec![Field::Depth],
            codec: Codec::Auto,
            preset: Preset::VeryFast,
            hw_accel: false,
            show_graph: false,
            mode: OutputMode::Overlay,
            interpolate: false,
            resolution: OutputResolution::Original,
        };
        let err = process_clip(
            &job,
            &[],
            &[],
            &options,
            &Arc::new(AtomicBool::new(false)),
            |_, _| {},
            |_| {},
        )
        .unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains('\n') && text.lines().count() >= 2,
            "expected ffmpeg's stderr to be appended to the error, got: {text}"
        );
    }

    /// A 90-degree rotation tag makes ffmpeg's decoder emit portrait frames;
    /// the pipeline must size buffers/encoder for those, not for the coded
    /// landscape dimensions ffprobe reports.
    #[test]
    fn processes_rotated_clip_with_swapped_dimensions() {
        let dir = make_test_dir("rotated");
        let landscape = synth_clip(&dir, "landscape.mp4", 1, 5);
        let rotated = dir.join("rotated.mp4");
        let status = Command::new("ffmpeg")
            .args(["-y", "-display_rotation", "90", "-i"])
            .arg(&landscape)
            .args(["-c", "copy"])
            .arg(&rotated)
            .status()
            .unwrap();
        assert!(status.success());
        let output = dir.join("output.mp4");

        let job = ClipJob {
            video_path: rotated,
            output_path: output.clone(),
            dive_start_sec: 0.0,
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
            interpolate: false,
            resolution: OutputResolution::Original,
        };
        let completed = process_clip(
            &job,
            &samples,
            &times,
            &options,
            &Arc::new(AtomicBool::new(false)),
            |_, _| {},
            |_| {},
        )
        .unwrap();
        assert!(completed);

        let info = probe_video(&output).unwrap();
        assert_eq!((info.width, info.height), (120, 160));
        assert_eq!(
            info.rotation_deg, 0,
            "output pixels are already upright; no rotation tag expected"
        );
    }

    /// Variable-frame-rate input: the decoder must emit exactly
    /// `duration * fps` frames so the overlay clock and the muxed audio stay
    /// aligned instead of the video being squeezed and truncated.
    #[test]
    fn vfr_clip_keeps_full_duration() {
        let dir = make_test_dir("vfr");
        let vfr = dir.join("vfr.mp4");
        let status = Command::new("ffmpeg")
            .args(["-y", "-f", "lavfi", "-i", "testsrc=size=160x120:rate=30:duration=2"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=2"])
            .args(["-vf", "select='not(mod(n\\,3))+not(mod(n\\,5))'", "-fps_mode", "vfr"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"])
            .arg(&vfr)
            .status()
            .unwrap();
        assert!(status.success());
        let output = dir.join("output.mp4");

        let job = ClipJob {
            video_path: vfr,
            output_path: output.clone(),
            dive_start_sec: 0.0,
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
            interpolate: false,
            resolution: OutputResolution::Original,
        };
        process_clip(
            &job,
            &samples,
            &times,
            &options,
            &Arc::new(AtomicBool::new(false)),
            |_, _| {},
            |_| {},
        )
        .unwrap();

        let info = probe_video(&output).unwrap();
        let duration = info.duration_sec.expect("output duration");
        assert!(
            (duration - 1.9).abs() < 0.25,
            "expected ~1.9s of video, got {duration}s"
        );
    }

    #[test]
    fn output_resolution_parse_round_trips_and_rejects_typos() {
        for res in OUTPUT_RESOLUTIONS {
            assert_eq!(OutputResolution::parse(res.as_str()), Some(res));
        }
        assert_eq!(OutputResolution::parse(""), Some(OutputResolution::Original));
        assert_eq!(OutputResolution::parse("2160P"), Some(OutputResolution::Uhd4k));
        assert_eq!(OutputResolution::parse("4K"), Some(OutputResolution::Uhd4k));
        assert_eq!(OutputResolution::parse("1440p"), None);
        assert_eq!(OutputResolution::parse("huge"), None);
    }

    /// The whole point of the 4K rung on this project's target footage: 5.3K
    /// GoPro video is 16:9, so it lands exactly on 3840x2160 -- and that is
    /// under every hardware encoder's per-axis limit, which 5312 is not.
    #[test]
    fn output_resolution_scales_5k_gopro_footage_to_exactly_16_by_9() {
        assert_eq!(OutputResolution::Uhd4k.target_dimensions(5312, 2988), (3840, 2160));
        assert_eq!(OutputResolution::Fhd1080p.target_dimensions(5312, 2988), (1920, 1080));
        assert_eq!(OutputResolution::Hd720p.target_dimensions(5312, 2988), (1280, 720));
        assert_eq!(OutputResolution::Original.target_dimensions(5312, 2988), (5312, 2988));
    }

    /// Selecting a rung larger than the source must not upscale: a blurry
    /// stretch is never what "output at 4K" is asking for.
    #[test]
    fn output_resolution_never_upscales() {
        assert_eq!(OutputResolution::Uhd4k.target_dimensions(1920, 1080), (1920, 1080));
        assert_eq!(OutputResolution::Fhd1080p.target_dimensions(1280, 720), (1280, 720));
        assert_eq!(
            OutputResolution::Fhd1080p.target_dimensions(1920, 1080),
            (1920, 1080),
            "a source exactly at the target is already the target"
        );
    }

    /// The target box is oriented like the source, so "1080p" on portrait
    /// footage means 1080 across, not 1080 tall (which would throw away
    /// three quarters of the picture's detail).
    #[test]
    fn output_resolution_orients_the_box_to_portrait_sources() {
        assert_eq!(OutputResolution::Fhd1080p.target_dimensions(2988, 5312), (1080, 1920));
        assert_eq!(OutputResolution::Uhd4k.target_dimensions(2988, 5312), (2160, 3840));
    }

    /// Aspect ratios that don't match the box are fitted inside it, never
    /// stretched or cropped, and both dimensions stay even for yuv420p.
    #[test]
    fn output_resolution_preserves_aspect_ratio_with_even_dimensions() {
        // 4:3 fits on width; 1440x1080 keeps 4:3 exactly.
        assert_eq!(OutputResolution::Fhd1080p.target_dimensions(4000, 3000), (1440, 1080));
        // 21:9 fits on width, and the odd result is rounded down to even.
        let (w, h) = OutputResolution::Fhd1080p.target_dimensions(5120, 2145);
        assert_eq!(w, 1920);
        assert_eq!(h % 2, 0, "odd heights are rejected by yuv420p: got {h}");
        let source_ratio = 5120.0 / 2145.0;
        let scaled_ratio = w as f64 / h as f64;
        assert!(
            (source_ratio - scaled_ratio).abs() < 0.01,
            "aspect ratio drifted: {source_ratio} -> {scaled_ratio}"
        );
    }

    /// A zero dimension means ffprobe told us nothing; `process_clip` rejects
    /// that separately, so scaling must not divide by it here.
    #[test]
    fn output_resolution_passes_through_a_zero_frame_size() {
        assert_eq!(OutputResolution::Uhd4k.target_dimensions(0, 2988), (0, 2988));
        assert_eq!(OutputResolution::Uhd4k.target_dimensions(5312, 0), (5312, 0));
    }

    /// End-to-end proof that the decoder-side scale actually lands: the
    /// frame buffer, the `-s` given to the encoder and ffmpeg's own filter
    /// all have to agree, and a mismatch would either tear the picture or
    /// desynchronize the raw stream rather than fail loudly.
    #[test]
    fn downscales_the_output_to_the_selected_resolution() {
        let dir = make_test_dir("resolution_downscale");
        let clip = synth_clip_res(&dir, "input.mp4", 1920, 1080, 2, 5);
        let output = dir.join("out.mp4");

        let samples = vec![sample(0.0, 1.0), sample(10.0, 2.0)];
        let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();
        let job = ClipJob {
            video_path: clip,
            output_path: output.clone(),
            dive_start_sec: 0.0,
            video_start_utc: None,
        };
        let options = ProcessingOptions {
            fields: vec![Field::Depth],
            codec: Codec::Auto,
            preset: Preset::UltraFast,
            hw_accel: false,
            show_graph: false,
            mode: OutputMode::Overlay,
            interpolate: false,
            resolution: OutputResolution::Hd720p,
        };

        let stop_flag = Arc::new(AtomicBool::new(false));
        let completed = process_clip(&job, &samples, &times, &options, &stop_flag, |_, _| {}, |_| {})
            .expect("processing a downscaled clip should succeed");
        assert!(completed);

        let probed = probe_video(&output).expect("probing the downscaled output");
        assert_eq!((probed.width, probed.height), (1280, 720));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `Original` must leave the frame size exactly alone -- no scale filter,
    /// no even-rounding, no surprise resample of odd-sized footage.
    #[test]
    fn original_resolution_leaves_the_frame_size_untouched() {
        let dir = make_test_dir("resolution_original");
        let clip = synth_clip_res(&dir, "input.mp4", 640, 480, 2, 5);
        let output = dir.join("out.mp4");

        let samples = vec![sample(0.0, 1.0)];
        let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();
        let job = ClipJob {
            video_path: clip,
            output_path: output.clone(),
            dive_start_sec: 0.0,
            video_start_utc: None,
        };
        let options = ProcessingOptions {
            fields: vec![Field::Depth],
            codec: Codec::Auto,
            preset: Preset::UltraFast,
            hw_accel: false,
            show_graph: false,
            mode: OutputMode::Overlay,
            interpolate: false,
            resolution: OutputResolution::Original,
        };

        let stop_flag = Arc::new(AtomicBool::new(false));
        process_clip(&job, &samples, &times, &options, &stop_flag, |_, _| {}, |_| {})
            .expect("processing at the original resolution should succeed");

        let probed = probe_video(&output).expect("probing the output");
        assert_eq!((probed.width, probed.height), (640, 480));

        let _ = std::fs::remove_dir_all(&dir);
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
        ];
        for name in names {
            assert_eq!(Preset::parse(name).unwrap().ffmpeg_name(), name);
        }
        assert_eq!(Preset::parse("bogus"), None);
    }

    #[test]
    fn probe_hw_encoder_rejects_bogus_encoder_name() {
        assert!(!probe_hw_encoder("definitely_not_a_real_encoder", "nv12", 320, 240));
    }

    /// The regression this guards: the probe used to run at a fixed 320x240
    /// no matter what the job was, so a hardware encoder that tops out below
    /// the clip's resolution was reported as available and then failed at
    /// encoder init, taking the whole job with it. No consumer H264/HEVC
    /// hardware encoder initializes at 16384x16384, so on any machine --
    /// with or without a GPU -- this must resolve to software.
    #[test]
    fn resolve_encoder_falls_back_to_software_for_a_frame_size_no_hardware_supports() {
        let (name, pix_fmt, _args, info) = resolve_encoder(Codec::H264, Preset::Fast, true, 16384, 16384);
        assert_eq!(name, "libx264");
        assert_eq!(pix_fmt, "yuv420p");
        assert!(
            matches!(info, EncoderInfo::Software { .. }),
            "hardware encoder accepted an impossible frame size: {info:?}"
        );
    }

    /// A zero frame size means ffprobe told us nothing useful; probing it
    /// would just spawn an ffmpeg that fails, so it is rejected outright.
    #[test]
    fn probe_hw_encoder_rejects_a_zero_frame_size_without_spawning_ffmpeg() {
        assert!(!probe_hw_encoder("libx264", "yuv420p", 0, 240));
        assert!(!probe_hw_encoder("libx264", "yuv420p", 320, 0));
    }

    #[test]
    fn resolve_encoder_uses_software_when_hw_accel_disabled() {
        let (name, pix_fmt, args, info) = resolve_encoder(Codec::H264, Preset::Fast, false, 1920, 1080);
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
        let (name, pix_fmt, args, info) = resolve_encoder(Codec::Mpeg4, Preset::VeryFast, true, 1920, 1080);
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
    fn hw_encoder_maps_amf_names_and_quality_args() {
        assert_eq!(HwEncoder::Amf.ffmpeg_encoder_name(Codec::Auto), Some("h264_amf"));
        assert_eq!(HwEncoder::Amf.ffmpeg_encoder_name(Codec::H265), Some("hevc_amf"));
        // AMF ignores -global_quality; it needs its own CQP rate control.
        assert_eq!(
            HwEncoder::Amf.quality_args(),
            vec!["-rc", "cqp", "-qp_i", "23", "-qp_p", "23"]
        );
        assert_eq!(HwEncoder::Qsv.quality_args(), vec!["-global_quality", "23"]);
        assert!(ENABLED_HW_CANDIDATES.contains(&HwEncoder::Amf));
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
        assert_eq!(EncoderInfo::Remux.describe(), "No re-encode (subtitle mode)");
    }

    /// Opportunistic: exercises the real hardware-acceleration path when
    /// this machine has a working hardware encoder (verified separately to
    /// have Intel Quick Sync during development), and degrades to a no-op
    /// elsewhere (e.g. CI runners without QSV/NVENC/AMF) rather than
    /// failing on hardware this crate cannot assume is present.
    #[test]
    fn processes_synthetic_clip_with_hw_accel_when_available() {
        // 320x240, not the usual 160x120: AMF rejects very small frames at
        // encoder init (error 5). The probe below uses the same size as the
        // fixture, exactly as a real job does.
        const W: u32 = 320;
        const H: u32 = 240;

        if !ENABLED_HW_CANDIDATES.iter().any(|hw| {
            hw.ffmpeg_encoder_name(Codec::Auto)
                .is_some_and(|name| probe_hw_encoder(name, "nv12", W, H))
        }) {
            eprintln!("skipping processes_synthetic_clip_with_hw_accel_when_available: no working hw encoder here");
            return;
        }

        let dir = make_test_dir("hw_accel");
        let clip = synth_clip_res(&dir, "input.mp4", W, H, 2, 5);
        let output = dir.join("output.mp4");

        let job = ClipJob {
            video_path: clip,
            output_path: output.clone(),
            dive_start_sec: 0.0,
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
            interpolate: false,
            resolution: OutputResolution::Original,
        };
        let stop_flag = Arc::new(AtomicBool::new(false));

        let mut encoder_info = None;
        let completed = process_clip(
            &job,
            &samples,
            &times,
            &options,
            &stop_flag,
            |_, _| {},
            |info| {
                encoder_info = Some(info.clone());
            },
        )
        .unwrap();

        assert!(completed);
        assert!(output.exists());
        assert!(matches!(encoder_info, Some(EncoderInfo::Hardware { .. })));
    }

    fn synth_clip(dir: &Path, name: &str, duration_secs: u32, fps: u32) -> PathBuf {
        synth_clip_res(dir, name, 160, 120, duration_secs, fps)
    }

    fn synth_clip_res(dir: &Path, name: &str, w: u32, h: u32, duration_secs: u32, fps: u32) -> PathBuf {
        let path = dir.join(name);
        let video_src = format!("testsrc=size={w}x{h}:rate={fps}:duration={duration_secs}");
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
            dive_start_sec: 0.0,
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
            interpolate: false,
            resolution: OutputResolution::Original,
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
        assert!(matches!(
            encoder_info,
            Some(EncoderInfo::Software {
                ffmpeg_name: "libx264",
                ..
            })
        ));

        assert!(completed);
        assert!(output.exists());

        let info = probe_video(&output).unwrap();
        assert_eq!(info.width, 160);
        assert_eq!(info.height, 120);

        // Verify audio survived the mux.
        let ffprobe_out = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "a",
                "-show_entries",
                "stream=codec_type",
            ])
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
            dive_start_sec: 0.0,
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
            interpolate: false,
            resolution: OutputResolution::Original,
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
            dive_start_sec: 0.0,
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
            interpolate: false,
            resolution: OutputResolution::Original,
        };
        let stop_flag = Arc::new(AtomicBool::new(false));

        let completed = process_clip(&job, &samples, &times, &options, &stop_flag, |_, _| {}, |_| {}).unwrap();
        assert!(completed);
        assert!(output.exists());

        let ffprobe_out = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v",
                "-show_entries",
                "stream=codec_name",
            ])
            .args(["-of", "csv=p=0"])
            .arg(&output)
            .output()
            .unwrap();
        let codec_name = String::from_utf8_lossy(&ffprobe_out.stdout);
        assert!(
            codec_name.trim().contains("hevc"),
            "expected hevc codec, got: {codec_name}"
        );
    }

    #[test]
    fn processes_synthetic_clip_in_subtitle_mode_without_reencoding() {
        let dir = make_test_dir("subtitle_mode");
        let clip = synth_clip(&dir, "input.mp4", 2, 5);
        let output = dir.join("output.mp4");

        let job = ClipJob {
            video_path: clip,
            output_path: output.clone(),
            dive_start_sec: 0.0,
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
            interpolate: false,
            resolution: OutputResolution::Original,
        };
        let stop_flag = Arc::new(AtomicBool::new(false));

        let completed = process_clip(&job, &samples, &times, &options, &stop_flag, |_, _| {}, |_| {}).unwrap();

        assert!(completed);
        assert!(output.exists());

        let srt_path = output.with_extension("srt");
        assert!(srt_path.exists());
        let srt_text = std::fs::read_to_string(&srt_path).unwrap();
        assert!(srt_text.contains("Depth: 1.0 m"));

        let info = probe_video(&output).unwrap();
        assert_eq!(info.width, 160);
        assert_eq!(info.height, 120);

        let ffprobe_out = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "s",
                "-show_entries",
                "stream=codec_type",
            ])
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
