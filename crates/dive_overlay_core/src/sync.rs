use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDateTime, Utc};

use crate::csv_data::{parse_duration_to_seconds, read_csv_datetime_columns, read_first_row_columns};
use crate::error::CoreError;
use crate::ffprobe::get_video_creation_time_utc;
use crate::model::ClipJob;

pub fn derive_output_path(video_path: &Path, output: Option<PathBuf>) -> PathBuf {
    match output {
        Some(p) => p,
        None => {
            let stem = video_path.file_stem().and_then(|s| s.to_str()).unwrap_or("output");
            video_path.with_file_name(format!("{stem}_overlay.mp4"))
        }
    }
}

/// Parses a `--clip` spec: `video_path|video_sync_sec|csv_sync_mmss[|output_path]`.
pub fn parse_clip_spec(spec: &str) -> Result<ClipJob, CoreError> {
    let parts: Vec<&str> = spec.split('|').map(|p| p.trim()).collect();
    if parts.len() != 3 && parts.len() != 4 {
        return Err(CoreError::InvalidClipSpec(
            "Invalid --clip format. Expected: video_path|video_sync_sec|csv_sync_mmss[|output_path]".to_string(),
        ));
    }

    let video_path = PathBuf::from(parts[0]);
    let video_sync_sec: f64 = parts[1]
        .parse()
        .map_err(|_| CoreError::InvalidClipSpec(format!("Invalid video_sync_sec in --clip: {}", parts[1])))?;
    let csv_sync_sec = parse_duration_to_seconds(parts[2])?;
    let output_path = if parts.len() == 4 {
        PathBuf::from(parts[3])
    } else {
        derive_output_path(&video_path, None)
    };

    Ok(ClipJob {
        video_path,
        output_path,
        video_sync_sec,
        csv_sync_sec,
        video_start_utc: None,
    })
}

fn parse_naive_utc(text: &str) -> Result<DateTime<Utc>, CoreError> {
    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(text, fmt) {
            return Ok(DateTime::from_naive_utc_and_offset(naive, Utc));
        }
    }
    Err(CoreError::Other(format!("Unknown date/time format: {text}")))
}

pub fn parse_datetime_utc(date_str: &str, time_str: &str) -> Result<DateTime<Utc>, CoreError> {
    parse_naive_utc(&format!("{} {}", date_str.trim(), time_str.trim()))
}

pub fn parse_datetime_text(value: &str) -> Result<DateTime<Utc>, CoreError> {
    let value = value.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Ok(dt.with_timezone(&Utc));
    }
    parse_naive_utc(value)
}

pub struct AutoSyncParams<'a> {
    pub base_clip: &'a Path,
    pub base_video_sync_sec: f64,
    pub base_csv_datetime: &'a str,
}

/// Auto-computes per-clip `csv_sync_sec` from each video's MP4
/// `creation_time` relative to one manually-synced base clip, so multi-clip
/// dive sessions with real-world gaps between clips sync automatically.
///
/// Preserves the original's exact (slightly surprising) behavior: every job
/// receives the *same* `video_sync_sec`, copied verbatim from
/// `base_video_sync_sec` -- only `csv_sync_sec` varies per clip via the
/// creation-time delta. This is intentional, not a bug to fix: the
/// original assumes every clip's manual sync point sits at the same video
/// second (e.g. "point the camera at the dive computer for the first few
/// seconds of every clip").
pub fn compute_auto_sync(csv_path: &Path, jobs: &mut [ClipJob], params: &AutoSyncParams) -> Result<(), CoreError> {
    let base_clip_resolved = params
        .base_clip
        .canonicalize()
        .unwrap_or_else(|_| params.base_clip.to_path_buf());
    let base_job_video_path = jobs
        .iter()
        .find(|j| {
            let resolved = j.video_path.canonicalize().unwrap_or_else(|_| j.video_path.clone());
            resolved == base_clip_resolved
        })
        .map(|j| j.video_path.clone())
        .ok_or_else(|| CoreError::Other("--base-clip must be one of the --clip paths".to_string()))?;

    let base_video_start = get_video_creation_time_utc(&base_job_video_path)?;
    let base_csv_dt = parse_datetime_text(params.base_csv_datetime)?;

    let (date_col, clock_col) = read_csv_datetime_columns(csv_path)?;
    let (date_col, clock_col) = match (date_col, clock_col) {
        (Some(d), Some(c)) => (d, c),
        _ => {
            return Err(CoreError::Other(
                "CSV needs date and time columns for auto-sync".to_string(),
            ))
        }
    };

    let first_row = read_first_row_columns(csv_path, &[&date_col, &clock_col])?
        .ok_or_else(|| CoreError::Other("CSV contains no rows".to_string()))?;
    let first_dt = parse_datetime_utc(&first_row[0], &first_row[1])?;
    let base_csv_dt_offset_sec = (base_csv_dt - first_dt).num_milliseconds() as f64 / 1000.0;

    for job in jobs.iter_mut() {
        let video_start = get_video_creation_time_utc(&job.video_path)?;
        let delta_sec = (video_start - base_video_start).num_milliseconds() as f64 / 1000.0;
        job.video_start_utc = Some(video_start);
        job.video_sync_sec = params.base_video_sync_sec;
        job.csv_sync_sec = (base_csv_dt_offset_sec + delta_sec).max(0.0);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn make_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("dive_overlay_sync_test").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn synth_clip_with_creation_time(dir: &Path, name: &str, creation_time: &str) -> PathBuf {
        let path = dir.join(name);
        let status = Command::new("ffmpeg")
            .args(["-y", "-f", "lavfi", "-i", "testsrc=size=64x64:rate=5:duration=1"])
            .args(["-metadata", &format!("creation_time={creation_time}")])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        path
    }

    #[test]
    fn parse_clip_spec_three_and_four_parts() {
        let job = parse_clip_spec("video.mp4|1.5|0:10").unwrap();
        assert_eq!(job.video_sync_sec, 1.5);
        assert_eq!(job.csv_sync_sec, 10.0);
        assert_eq!(job.output_path, PathBuf::from("video_overlay.mp4"));

        let job2 = parse_clip_spec("video.mp4|1.5|0:10|out.mp4").unwrap();
        assert_eq!(job2.output_path, PathBuf::from("out.mp4"));

        assert!(parse_clip_spec("only_one_part").is_err());
    }

    #[test]
    fn derive_output_path_uses_stem_suffix() {
        let path = derive_output_path(Path::new("clip1.mp4"), None);
        assert_eq!(path, PathBuf::from("clip1_overlay.mp4"));
        let explicit = derive_output_path(Path::new("clip1.mp4"), Some(PathBuf::from("custom.mp4")));
        assert_eq!(explicit, PathBuf::from("custom.mp4"));
    }

    #[test]
    fn parse_datetime_variants() {
        assert!(parse_datetime_utc("2025-07-05", "15:32:55").is_ok());
        assert!(parse_datetime_text("2025-07-05 15:32:55").is_ok());
        assert!(parse_datetime_text("2025-07-05T15:32:55Z").is_ok());
    }

    #[test]
    fn auto_sync_offsets_csv_sync_by_creation_time_delta_and_keeps_video_sync_uniform() {
        let dir = make_dir("auto_sync");
        let base_clip = synth_clip_with_creation_time(&dir, "base.mp4", "2025-07-05T10:00:00Z");
        let second_clip = synth_clip_with_creation_time(&dir, "second.mp4", "2025-07-05T10:05:00Z");

        let csv_path = dir.join("dive.csv");
        std::fs::write(
            &csv_path,
            "date,time,sample time (min),sample depth (m)\n2025-07-05,09:58:00,0:00,1.0\n2025-07-05,09:59:00,1:00,2.0\n",
        )
        .unwrap();

        let mut jobs = vec![
            ClipJob {
                video_path: base_clip.clone(),
                output_path: PathBuf::from("base_overlay.mp4"),
                video_sync_sec: 0.0,
                csv_sync_sec: 0.0,
                video_start_utc: None,
            },
            ClipJob {
                video_path: second_clip.clone(),
                output_path: PathBuf::from("second_overlay.mp4"),
                video_sync_sec: 0.0,
                csv_sync_sec: 0.0,
                video_start_utc: None,
            },
        ];

        let params = AutoSyncParams {
            base_clip: &base_clip,
            base_video_sync_sec: 2.0,
            base_csv_datetime: "2025-07-05 10:00:00",
        };

        compute_auto_sync(&csv_path, &mut jobs, &params).unwrap();

        // CSV's first row is 09:58:00; base sync point is 10:00:00 -> +120s offset.
        assert!((jobs[0].csv_sync_sec - 120.0).abs() < 1.0);
        // Second clip started recording 300s after base -> csv_sync_sec should be +300s more.
        assert!((jobs[1].csv_sync_sec - 420.0).abs() < 1.0);
        // video_sync_sec must be identical across jobs (copied from base), per original behavior.
        assert_eq!(jobs[0].video_sync_sec, 2.0);
        assert_eq!(jobs[1].video_sync_sec, 2.0);
    }
}
