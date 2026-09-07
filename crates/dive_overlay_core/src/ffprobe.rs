use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use chrono::{DateTime, NaiveDateTime, Utc};
use serde::Deserialize;

use crate::error::CoreError;

#[derive(Debug, Deserialize)]
struct FfprobeOutput {
    #[serde(default)]
    streams: Vec<StreamInfo>,
    format: Option<FormatInfo>,
}

#[derive(Debug, Deserialize)]
struct StreamInfo {
    codec_type: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    r_frame_rate: Option<String>,
    avg_frame_rate: Option<String>,
    nb_frames: Option<String>,
    duration: Option<String>,
    #[serde(default)]
    side_data_list: Vec<SideData>,
}

/// Only the "Display Matrix" entry is of interest: its `rotation` (in
/// degrees, e.g. 90/-90/180) tells us that ffmpeg's decoder will auto-rotate
/// the frames, so their pixel dimensions differ from the stream's coded
/// `width`/`height` for +-90 degree rotations. ffprobe reports `rotation`
/// as a number; it is read as f64 so both `90` and `-90.0` deserialize.
#[derive(Debug, Deserialize)]
struct SideData {
    side_data_type: Option<String>,
    rotation: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct FormatInfo {
    duration: Option<String>,
    #[serde(default)]
    tags: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoInfo {
    /// Pixel dimensions of the frames ffmpeg's decoder will actually emit,
    /// i.e. already swapped for clips carrying a +-90 degree rotation tag
    /// (phone/action-cam portrait footage), which ffmpeg auto-rotates on
    /// decode. The container's coded `width`/`height` would be wrong for
    /// sizing raw frame buffers in that case.
    pub width: u32,
    pub height: u32,
    /// Rotation in degrees from the stream's display matrix (0 when absent).
    pub rotation_deg: i32,
    /// Frame rate the decode/encode pipeline runs at. Prefers ffprobe's
    /// `avg_frame_rate` (frames / duration) over `r_frame_rate`, which for
    /// variable-frame-rate sources is the timebase-derived *maximum* rate
    /// and can be far above the real average.
    pub fps: f64,
    /// Estimate only (from `nb_frames`, falling back to duration*fps) -- an
    /// unreliable value for mp4/mov, same as the original's tolerance of
    /// `CAP_PROP_FRAME_COUNT`. Must only be used for progress-bar display,
    /// never as a decode loop's termination condition.
    pub estimated_frames: Option<u64>,
    pub creation_time: Option<DateTime<Utc>>,
    /// Container/stream duration in seconds, when ffprobe reports one.
    /// Independent of `estimated_frames` (which derives from `nb_frames`
    /// first) -- used for sizing subtitle-cue generation, where we need the
    /// actual runtime rather than a frame-count estimate.
    pub duration_sec: Option<f64>,
    /// Whether the file carries at least one audio stream. Merging needs it:
    /// clips with and without audio cannot be joined by a stream copy, and
    /// the re-encode path has to synthesize silence for the silent ones.
    pub has_audio: bool,
}

/// Fails fast with a clear message if ffmpeg/ffprobe aren't on PATH, instead
/// of letting every downstream `Command::spawn` fail with an opaque ENOENT.
pub fn ensure_ffmpeg_available() -> Result<(), CoreError> {
    if which::which("ffmpeg").is_err() {
        return Err(CoreError::Ffmpeg(
            "ffmpeg was not found. Please install it and add it to PATH.".to_string(),
        ));
    }
    if which::which("ffprobe").is_err() {
        return Err(CoreError::Ffprobe(
            "ffprobe was not found. Please install it and add it to PATH.".to_string(),
        ));
    }
    Ok(())
}

fn parse_frame_rate(value: &str) -> Option<f64> {
    let (num, den) = value.split_once('/')?;
    let num: f64 = num.parse().ok()?;
    let den: f64 = den.parse().ok()?;
    if den == 0.0 {
        return None;
    }
    Some(num / den)
}

/// Tries a handful of datetime formats since ffprobe's `creation_time`
/// format varies slightly by camera/firmware (fractional seconds
/// present/absent, `Z` vs explicit offset).
pub fn parse_creation_time(text: &str) -> Result<DateTime<Utc>, CoreError> {
    let text = text.trim();

    if let Ok(dt) = DateTime::parse_from_rfc3339(text) {
        return Ok(dt.with_timezone(&Utc));
    }

    let normalized = text.replace('Z', "+00:00");
    if normalized != text {
        if let Ok(dt) = DateTime::parse_from_rfc3339(&normalized) {
            return Ok(dt.with_timezone(&Utc));
        }
    }

    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(text, fmt) {
            return Ok(DateTime::from_naive_utc_and_offset(naive, Utc));
        }
    }

    Err(CoreError::Ffprobe(format!("Unknown creation_time format: {text}")))
}

fn parse_ffprobe_json(bytes: &[u8]) -> Result<VideoInfo, CoreError> {
    let parsed: FfprobeOutput = serde_json::from_slice(bytes)?;

    let video_stream = parsed
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("video"))
        .ok_or_else(|| CoreError::Ffprobe("No video stream found".to_string()))?;

    let coded_width = video_stream.width.unwrap_or(0);
    let coded_height = video_stream.height.unwrap_or(0);

    let rotation_deg = video_stream
        .side_data_list
        .iter()
        .find(|sd| sd.side_data_type.as_deref() == Some("Display Matrix"))
        .and_then(|sd| sd.rotation)
        .map(|r| r.round() as i32)
        .unwrap_or(0);
    let swaps_dimensions = (rotation_deg.rem_euclid(180)) == 90;
    let (width, height) = if swaps_dimensions {
        (coded_height, coded_width)
    } else {
        (coded_width, coded_height)
    };

    let valid_rate = |s: &str| parse_frame_rate(s).filter(|f| f.is_finite() && *f > 0.0);
    let fps = video_stream
        .avg_frame_rate
        .as_deref()
        .and_then(valid_rate)
        .or_else(|| video_stream.r_frame_rate.as_deref().and_then(valid_rate))
        .unwrap_or(30.0);

    let estimated_frames = video_stream
        .nb_frames
        .as_deref()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .or_else(|| {
            let duration = video_stream
                .duration
                .as_deref()
                .or_else(|| parsed.format.as_ref().and_then(|f| f.duration.as_deref()))
                .and_then(|s| s.parse::<f64>().ok())?;
            Some((duration * fps).round() as u64)
        });

    let creation_time = parsed
        .format
        .as_ref()
        .and_then(|f| f.tags.get("creation_time"))
        .and_then(|s| parse_creation_time(s).ok());

    let duration_sec = video_stream
        .duration
        .as_deref()
        .or_else(|| parsed.format.as_ref().and_then(|f| f.duration.as_deref()))
        .and_then(|s| s.parse::<f64>().ok());

    let has_audio = parsed
        .streams
        .iter()
        .any(|s| s.codec_type.as_deref() == Some("audio"));

    Ok(VideoInfo {
        width,
        height,
        rotation_deg,
        fps,
        estimated_frames,
        creation_time,
        duration_sec,
        has_audio,
    })
}

pub fn probe_video(video_path: &Path) -> Result<VideoInfo, CoreError> {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-print_format", "json", "-show_streams", "-show_format"])
        .arg(video_path)
        .output()
        .map_err(|e| CoreError::Ffprobe(format!("Failed to start ffprobe: {e}")))?;

    if !output.status.success() {
        return Err(CoreError::Ffprobe(String::from_utf8_lossy(&output.stderr).to_string()));
    }

    parse_ffprobe_json(&output.stdout)
}

pub fn get_video_creation_time_utc(video_path: &Path) -> Result<DateTime<Utc>, CoreError> {
    let info = probe_video(video_path)?;
    info.creation_time
        .ok_or_else(|| CoreError::Ffprobe(format!("No creation_time in MP4: {}", video_path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_JSON: &str = r#"
    {
        "streams": [
            {
                "codec_type": "video",
                "width": 1280,
                "height": 720,
                "r_frame_rate": "30/1",
                "nb_frames": "150",
                "duration": "5.000000"
            },
            {
                "codec_type": "audio"
            }
        ],
        "format": {
            "duration": "5.000000",
            "tags": {
                "creation_time": "2025-07-05T15:30:00.000000Z"
            }
        }
    }
    "#;

    #[test]
    fn parses_fixture_json() {
        let info = parse_ffprobe_json(FIXTURE_JSON.as_bytes()).unwrap();
        assert_eq!(info.width, 1280);
        assert_eq!(info.height, 720);
        assert_eq!(info.fps, 30.0);
        assert_eq!(info.estimated_frames, Some(150));
        assert!(info.creation_time.is_some());
        assert!(info.has_audio);
    }

    #[test]
    fn falls_back_to_duration_times_fps_when_nb_frames_missing() {
        let json = r#"{
            "streams": [{"codec_type":"video","width":640,"height":480,"r_frame_rate":"25/1","duration":"2.000000"}],
            "format": {"tags": {}}
        }"#;
        let info = parse_ffprobe_json(json.as_bytes()).unwrap();
        assert_eq!(info.estimated_frames, Some(50));
        assert!(info.creation_time.is_none());
        assert!(!info.has_audio);
    }

    #[test]
    fn swaps_dimensions_for_90_degree_rotation_tag() {
        let json = r#"{
            "streams": [{
                "codec_type":"video","width":1920,"height":1080,"r_frame_rate":"30/1",
                "side_data_list":[{"side_data_type":"Display Matrix","displaymatrix":"...","rotation":-90}]
            }],
            "format": {}
        }"#;
        let info = parse_ffprobe_json(json.as_bytes()).unwrap();
        assert_eq!((info.width, info.height), (1080, 1920));
        assert_eq!(info.rotation_deg, -90);

        let json_180 = json.replace("\"rotation\":-90", "\"rotation\":180");
        let info = parse_ffprobe_json(json_180.as_bytes()).unwrap();
        assert_eq!((info.width, info.height), (1920, 1080));
        assert_eq!(info.rotation_deg, 180);
    }

    #[test]
    fn prefers_avg_frame_rate_over_r_frame_rate() {
        // VFR-style source: r_frame_rate is the timebase-derived maximum,
        // avg_frame_rate the real frames/duration.
        let json = r#"{
            "streams": [{"codec_type":"video","width":64,"height":48,"r_frame_rate":"30/1","avg_frame_rate":"14/1"}],
            "format": {}
        }"#;
        let info = parse_ffprobe_json(json.as_bytes()).unwrap();
        assert_eq!(info.fps, 14.0);

        let json_bad_avg = json.replace("\"avg_frame_rate\":\"14/1\"", "\"avg_frame_rate\":\"0/0\"");
        let info = parse_ffprobe_json(json_bad_avg.as_bytes()).unwrap();
        assert_eq!(info.fps, 30.0);
    }

    #[test]
    fn probes_rotated_synthetic_clip_with_decoder_dimensions() {
        let dir = std::env::temp_dir().join("dive_overlay_ffprobe_test");
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("probe_rotated.mp4");

        let status = Command::new("ffmpeg")
            .args(["-y", "-f", "lavfi", "-i", "testsrc=size=64x48:rate=10:duration=1"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .arg(&clip)
            .status()
            .expect("failed to run ffmpeg");
        assert!(status.success());
        let rotated = dir.join("probe_rotated_90.mp4");
        let status = Command::new("ffmpeg")
            .args(["-y", "-display_rotation", "90", "-i"])
            .arg(&clip)
            .args(["-c", "copy"])
            .arg(&rotated)
            .status()
            .expect("failed to run ffmpeg");
        assert!(status.success());

        let info = probe_video(&rotated).unwrap();
        assert_eq!((info.width, info.height), (48, 64));
        assert_eq!(info.rotation_deg.abs(), 90);
    }

    #[test]
    fn parses_tolerant_creation_time_variants() {
        assert!(parse_creation_time("2025-07-05T15:30:00.000000Z").is_ok());
        assert!(parse_creation_time("2025-07-05T15:30:00+00:00").is_ok());
        assert!(parse_creation_time("2025-07-05 15:30:00").is_ok());
        assert!(parse_creation_time("not-a-date").is_err());
    }

    #[test]
    fn ffmpeg_and_ffprobe_are_available() {
        // This machine has ffmpeg installed specifically to support this port;
        // fail loudly (not skip) if that regresses.
        ensure_ffmpeg_available().expect("ffmpeg/ffprobe should be installed and on PATH");
    }

    #[test]
    fn probes_a_real_synthetic_clip() {
        let dir = std::env::temp_dir().join("dive_overlay_ffprobe_test");
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("probe_test.mp4");

        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x64:rate=10:duration=1",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&clip)
            .status()
            .expect("failed to run ffmpeg");
        assert!(status.success());

        let info = probe_video(&clip).unwrap();
        assert_eq!(info.width, 64);
        assert_eq!(info.height, 64);
        assert!((info.fps - 10.0).abs() < 0.01);
    }
}
