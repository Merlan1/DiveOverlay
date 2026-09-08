use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDateTime, Utc};

use crate::csv_data::parse_duration_to_seconds;
use crate::error::CoreError;
use crate::ffprobe::{probe_video, VideoInfo};
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
    for fmt in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
    ] {
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

/// Which of a video file's two clocks auto-sync measured the clips against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipStartSource {
    /// The `tmcd` start timecode -- time of day at record start, stamped
    /// from the camera's real-time clock at frame resolution. Preferred,
    /// because `creation_time` is quantized to whole seconds and so can be
    /// most of a second off per clip, which a dive overlay shows.
    Timecode,
    /// The container's `creation_time` tag, used when any clip lacks a
    /// usable timecode.
    CreationTime,
}

impl ClipStartSource {
    pub fn label(&self) -> &'static str {
        match self {
            ClipStartSource::Timecode => "start timecode",
            ClipStartSource::CreationTime => "creation time",
        }
    }
}

pub struct AutoSyncParams<'a> {
    pub base_clip: &'a Path,
}

/// Outcome of `compute_auto_sync`: which clock supplied the deltas, plus any
/// clip whose derived position looks wrong -- most likely because it belongs
/// to a different dive than the CSV.
#[derive(Debug)]
pub struct AutoSyncReport {
    pub source: ClipStartSource,
    pub warnings: Vec<String>,
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

/// Elapsed seconds between two times of day, assuming both clips belong to
/// one dive and so are far less than 12 hours apart. A start timecode
/// carries no date, so a dive spanning midnight would otherwise read as a
/// ~24 hour jump backwards.
fn timecode_delta(from: f64, to: f64) -> f64 {
    const DAY: f64 = 86_400.0;
    let delta = to - from;
    if delta < -DAY / 2.0 {
        delta + DAY
    } else if delta > DAY / 2.0 {
        delta - DAY
    } else {
        delta
    }
}

/// `m:ss` for warning text, keeping the sign for clips landing before the
/// dive log starts.
fn format_signed_duration(seconds: f64) -> String {
    let sign = if seconds < 0.0 { "-" } else { "" };
    let total = seconds.abs().round() as u64;
    format!("{sign}{}:{:02}", total / 60, total % 60)
}

/// Auto-computes every clip's `csv_sync_sec` from how much later it started
/// recording than one manually-synced base clip.
///
/// The base clip's own `csv_sync_sec` -- entered by hand, exactly as in
/// single-clip mode -- is the anchor, so neither the user nor the CSV has to
/// supply a wall-clock date/time: only the *delta* between two clips of the
/// same camera is read from the files, and a delta needs no shared epoch.
/// (An earlier version took an absolute CSV datetime and converted it to
/// dive-elapsed via the CSV's date/clock columns. That was redundant with
/// the base clip's manual sync, and locked auto-sync out of dive computers
/// that export elapsed time only.)
///
/// Deltas come from the `tmcd` start timecode when *every* clip has one, and
/// from `creation_time` otherwise. The choice is all-or-nothing on purpose:
/// the two are independent clocks that can sit tens of seconds apart within
/// a single camera (~45 s on the HERO11 footage this was built against), so
/// falling back per-clip would silently inject that gap into one clip's
/// sync.
///
/// Preserves the original's exact (slightly surprising) behavior: every job
/// receives the *same* `video_sync_sec`, copied verbatim from the base
/// clip's own -- only `csv_sync_sec` varies per clip. This is
/// intentional, not a bug to fix: it assumes every clip's manual sync point
/// sits at the same video second (e.g. "point the camera at the dive
/// computer for the first few seconds of every clip").
///
/// `dive_times` is the CSV's elapsed-second column, used only to flag clips
/// landing outside the dive; pass an empty slice to skip that check.
pub fn compute_auto_sync(
    jobs: &mut [ClipJob],
    dive_times: &[f64],
    params: &AutoSyncParams,
) -> Result<AutoSyncReport, CoreError> {
    let base_clip_resolved = params
        .base_clip
        .canonicalize()
        .unwrap_or_else(|_| params.base_clip.to_path_buf());
    let base_index = jobs
        .iter()
        .position(|j| {
            let resolved = j.video_path.canonicalize().unwrap_or_else(|_| j.video_path.clone());
            resolved == base_clip_resolved
        })
        .ok_or_else(|| CoreError::Other("--base-clip must be one of the --clip paths".to_string()))?;

    let infos: Vec<VideoInfo> = jobs
        .iter()
        .map(|job| probe_video(&job.video_path))
        .collect::<Result<_, _>>()?;

    let source = if infos.iter().all(|i| i.timecode_sec.is_some()) {
        ClipStartSource::Timecode
    } else {
        ClipStartSource::CreationTime
    };

    let base_timecode = infos[base_index].timecode_sec;
    let base_creation = infos[base_index].creation_time;
    // The base clip's hand-entered sync point is the anchor every other clip
    // is offset from: the CSV time the diver read, and the video second they
    // read it at. Both halves live on the base clip's own job, so neither is
    // passed in alongside it -- a second copy could disagree with the one the
    // caller shows the user.
    let base_csv_sync_sec = jobs[base_index].csv_sync_sec;
    let base_video_sync_sec = jobs[base_index].video_sync_sec;

    let mut warnings = Vec::new();
    let dive_range = dive_times.first().copied().zip(dive_times.last().copied());

    for (idx, job) in jobs.iter_mut().enumerate() {
        let info = &infos[idx];
        let delta_sec = match source {
            // `source` is Timecode only when every clip has one.
            ClipStartSource::Timecode => timecode_delta(base_timecode.unwrap(), info.timecode_sec.unwrap()),
            ClipStartSource::CreationTime => {
                let start = info.creation_time.ok_or_else(|| {
                    CoreError::Ffprobe(format!(
                        "No start timecode and no creation_time in {} -- auto-sync needs one of them to place the clip",
                        file_label(&job.video_path)
                    ))
                })?;
                let base_start = base_creation.ok_or_else(|| {
                    CoreError::Ffprobe(format!(
                        "No start timecode and no creation_time in base clip {}",
                        file_label(params.base_clip)
                    ))
                })?;
                (start - base_start).num_milliseconds() as f64 / 1000.0
            }
        };

        let raw_csv_sync_sec = base_csv_sync_sec + delta_sec;
        let clip_start = raw_csv_sync_sec - base_video_sync_sec;

        // Checked before the clamp below, which would otherwise hide a clip
        // that landed hours away because it belongs to a different dive.
        if let Some((first, last)) = dive_range {
            let clip_end = clip_start + info.duration_sec.unwrap_or(0.0);
            if clip_end < first || clip_start > last {
                warnings.push(format!(
                    "{} lands at dive time {}, outside the logged dive ({} to {}). Is it from a different dive?",
                    file_label(&job.video_path),
                    format_signed_duration(clip_start),
                    format_signed_duration(first),
                    format_signed_duration(last),
                ));
            }
        }
        if raw_csv_sync_sec < 0.0 {
            warnings.push(format!(
                "{} starts before dive time 0:00; its CSV sync was clamped to 0:00.",
                file_label(&job.video_path)
            ));
        }

        job.video_sync_sec = base_video_sync_sec;
        job.csv_sync_sec = raw_csv_sync_sec.max(0.0);
        // Recorded whichever clock supplied the delta: `plan_merge` uses it
        // only to break ties between equal dive times.
        job.video_start_utc = info.creation_time;
    }

    Ok(AutoSyncReport { source, warnings })
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

    /// `timecode` writes a `tmcd` track the way a real camera does; passing
    /// `None` produces a clip with only a `creation_time`, which is what the
    /// fallback path has to cope with.
    fn synth_clip(dir: &Path, name: &str, creation_time: &str, timecode: Option<&str>) -> PathBuf {
        let path = dir.join(name);
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-y", "-f", "lavfi", "-i", "testsrc=size=64x64:rate=30:duration=2"])
            .args(["-metadata", &format!("creation_time={creation_time}")]);
        if let Some(tc) = timecode {
            cmd.args(["-timecode", tc]);
        }
        let status = cmd
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        path
    }

    fn job(video: &Path, csv_sync_sec: f64) -> ClipJob {
        ClipJob {
            video_path: video.to_path_buf(),
            output_path: derive_output_path(video, None),
            video_sync_sec: 0.0,
            csv_sync_sec,
            video_start_utc: None,
        }
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
    fn timecode_delta_wraps_across_midnight() {
        // 23:59:00 -> 00:01:00 is two minutes forward, not 23h58m backward.
        assert_eq!(timecode_delta(86_340.0, 60.0), 120.0);
        // ...and the reverse stays negative rather than jumping a day ahead.
        assert_eq!(timecode_delta(60.0, 86_340.0), -120.0);
        assert_eq!(timecode_delta(100.0, 400.0), 300.0);
    }

    #[test]
    fn auto_sync_offsets_from_the_base_clips_own_csv_sync() {
        let dir = make_dir("auto_sync_timecode");
        let base = synth_clip(&dir, "base.mp4", "2025-07-05T10:00:00Z", Some("10:00:00:00"));
        let second = synth_clip(&dir, "second.mp4", "2025-07-05T10:05:00Z", Some("10:05:00:00"));

        let mut jobs = vec![job(&base, 120.0), job(&second, 0.0)];
        // The base clip carries the video second the sync point was read at;
        // the other clip's own value is meaningless and must be overwritten.
        jobs[0].video_sync_sec = 2.0;
        jobs[1].video_sync_sec = 9.0;
        let params = AutoSyncParams { base_clip: &base };
        // A long dive, so neither clip trips the out-of-range warning.
        let dive_times: Vec<f64> = (0..3600).map(|s| s as f64).collect();
        let report = compute_auto_sync(&mut jobs, &dive_times, &params).unwrap();

        assert_eq!(report.source, ClipStartSource::Timecode);
        assert!(report.warnings.is_empty(), "unexpected warnings: {:?}", report.warnings);
        // The base clip keeps the sync point it was given by hand...
        assert!((jobs[0].csv_sync_sec - 120.0).abs() < 0.1);
        // ...and the second, recorded 300 s later, is offset by exactly that.
        assert!((jobs[1].csv_sync_sec - 420.0).abs() < 0.1);
        // video_sync_sec must be identical across jobs, taken from the base
        // clip's own field rather than from anything passed in beside it.
        assert_eq!(jobs[0].video_sync_sec, 2.0);
        assert_eq!(jobs[1].video_sync_sec, 2.0);
    }

    #[test]
    fn auto_sync_falls_back_to_creation_time_when_a_clip_has_no_timecode() {
        let dir = make_dir("auto_sync_fallback");
        // The base has a timecode two hours off its creation_time, mimicking
        // a camera whose RTC and UTC clock disagree. Mixing the two sources
        // would place the second clip 2 h away; using creation_time for both
        // keeps it at +300 s.
        let base = synth_clip(&dir, "base.mp4", "2025-07-05T10:00:00Z", Some("12:00:00:00"));
        let second = synth_clip(&dir, "second.mp4", "2025-07-05T10:05:00Z", None);

        let mut jobs = vec![job(&base, 120.0), job(&second, 0.0)];
        let params = AutoSyncParams { base_clip: &base };
        let report = compute_auto_sync(&mut jobs, &[], &params).unwrap();

        assert_eq!(report.source, ClipStartSource::CreationTime);
        assert!((jobs[1].csv_sync_sec - 420.0).abs() < 0.1);
    }

    #[test]
    fn auto_sync_warns_when_a_clip_lands_outside_the_dive() {
        let dir = make_dir("auto_sync_wrong_dive");
        let base = synth_clip(&dir, "base.mp4", "2025-07-05T10:00:00Z", Some("10:00:00:00"));
        // Four hours later: a different dive that happened to be selected.
        let other = synth_clip(&dir, "other.mp4", "2025-07-05T14:00:00Z", Some("14:00:00:00"));

        let mut jobs = vec![job(&base, 0.0), job(&other, 0.0)];
        let params = AutoSyncParams { base_clip: &base };
        let dive_times: Vec<f64> = (0..1800).map(|s| s as f64).collect();
        let report = compute_auto_sync(&mut jobs, &dive_times, &params).unwrap();

        assert_eq!(report.warnings.len(), 1, "warnings: {:?}", report.warnings);
        assert!(
            report.warnings[0].contains("other.mp4"),
            "warnings: {:?}",
            report.warnings
        );
        assert!(
            report.warnings[0].contains("different dive"),
            "warnings: {:?}",
            report.warnings
        );
    }

    #[test]
    fn auto_sync_rejects_a_base_clip_outside_the_job_list() {
        let mut jobs = vec![job(Path::new("a.mp4"), 0.0)];
        let params = AutoSyncParams {
            base_clip: Path::new("not_in_list.mp4"),
        };
        let err = compute_auto_sync(&mut jobs, &[], &params).unwrap_err();
        assert!(err.to_string().contains("must be one of"), "unexpected error: {err}");
    }
}
