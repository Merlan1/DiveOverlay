use crate::error::CoreError;
use crate::model::{DiveSample, Field};
use crate::overlay::build_overlay_lines;

fn format_srt_timestamp(total_sec: f64) -> String {
    let millis_total = (total_sec.max(0.0) * 1000.0).round() as u64;
    let hours = millis_total / 3_600_000;
    let minutes = (millis_total % 3_600_000) / 60_000;
    let seconds = (millis_total % 60_000) / 1000;
    let millis = millis_total % 1000;
    format!("{hours:02}:{minutes:02}:{seconds:02},{millis:03}")
}

/// Builds an SRT subtitle track spanning `video_duration_sec`, with one cue
/// per whole second showing the same info-box text the burned-in overlay
/// would draw at that instant (reusing `build_overlay_lines` keeps both
/// modes' text identical). Kept as a soft subtitle stream rather than pixels
/// so a player can toggle it on/off after the fact.
pub fn build_srt(
    fields: &[Field],
    samples: &[DiveSample],
    times: &[f64],
    video_sync_sec: f64,
    csv_sync_sec: f64,
    video_duration_sec: f64,
    interpolate: bool,
) -> String {
    let mut out = String::new();
    let total_seconds = video_duration_sec.max(0.0).ceil() as u64;

    for sec in 0..total_seconds {
        let start = sec as f64;
        let end = ((sec + 1) as f64).min(video_duration_sec);
        if end <= start {
            break;
        }

        let dive_sec = csv_sync_sec + (start - video_sync_sec);
        let lines = build_overlay_lines(fields, samples, times, dive_sec, interpolate);

        out.push_str(&(sec + 1).to_string());
        out.push('\n');
        out.push_str(&format_srt_timestamp(start));
        out.push_str(" --> ");
        out.push_str(&format_srt_timestamp(end));
        out.push('\n');
        out.push_str(&lines.join("\n"));
        out.push_str("\n\n");
    }

    out
}

/// Parses an SRT timestamp (`hh:mm:ss,mmm`, also tolerating a `.` decimal
/// separator) back into seconds. The inverse of `format_srt_timestamp`,
/// needed to shift an already-rendered per-clip track onto a merged
/// timeline.
fn parse_srt_timestamp(text: &str) -> Option<f64> {
    let text = text.trim();
    let (hms, millis) = text.rsplit_once(',').or_else(|| text.rsplit_once('.'))?;
    let mut parts = hms.split(':');
    let hours: f64 = parts.next()?.trim().parse().ok()?;
    let minutes: f64 = parts.next()?.trim().parse().ok()?;
    let seconds: f64 = parts.next()?.trim().parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let millis: f64 = millis.trim().parse().ok()?;
    Some(hours * 3600.0 + minutes * 60.0 + seconds + millis / 1000.0)
}

/// One clip's already-rendered subtitle track, plus where that clip starts
/// on the merged timeline.
pub struct SrtPart<'a> {
    pub srt: &'a str,
    pub offset_sec: f64,
}

/// Joins per-clip SRT documents into one track for a merged dive video:
/// every cue is shifted by its part's `offset_sec` and the result is
/// renumbered from 1. Concatenating the clips carries their embedded
/// `mov_text` streams over for free, but the sidecar `.srt` files are each
/// written on their own clip's timeline, so they have to be re-based here.
pub fn concat_srt(parts: &[SrtPart<'_>]) -> Result<String, CoreError> {
    let mut out = String::new();
    let mut cue_number: u64 = 0;

    for part in parts {
        for block in part.srt.replace("\r\n", "\n").split("\n\n") {
            let block = block.trim();
            if block.is_empty() {
                continue;
            }

            let mut timing: Option<&str> = None;
            let mut text_lines: Vec<&str> = Vec::new();
            for line in block.lines() {
                match timing {
                    // Anything before the timing line is the part's own cue
                    // number, which is dropped: the merged track is
                    // renumbered so the indices stay strictly increasing.
                    None if line.contains("-->") => timing = Some(line),
                    None => {}
                    Some(_) => text_lines.push(line),
                }
            }

            let timing =
                timing.ok_or_else(|| CoreError::Other(format!("Malformed SRT block (no timing line): {block}")))?;
            let (start, end) = timing
                .split_once("-->")
                .ok_or_else(|| CoreError::Other(format!("Malformed SRT timing line: {timing}")))?;
            let bad = || CoreError::Other(format!("Invalid SRT timestamp: {}", timing.trim()));
            let start = parse_srt_timestamp(start).ok_or_else(bad)?;
            let end = parse_srt_timestamp(end).ok_or_else(bad)?;

            cue_number += 1;
            out.push_str(&cue_number.to_string());
            out.push('\n');
            out.push_str(&format_srt_timestamp(start + part.offset_sec));
            out.push_str(" --> ");
            out.push_str(&format_srt_timestamp(end + part.offset_sec));
            out.push('\n');
            out.push_str(&text_lines.join("\n"));
            out.push_str("\n\n");
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(elapsed_sec: f64, depth_m: f64) -> DiveSample {
        DiveSample {
            elapsed_sec,
            depth_m: Some(depth_m),
            temp_c: None,
            pressure_bar: None,
            heart_rate: None,
        }
    }

    #[test]
    fn formats_srt_timestamps() {
        assert_eq!(format_srt_timestamp(0.0), "00:00:00,000");
        assert_eq!(format_srt_timestamp(61.5), "00:01:01,500");
        assert_eq!(format_srt_timestamp(3661.25), "01:01:01,250");
    }

    #[test]
    fn builds_one_cue_per_second_covering_the_full_duration() {
        let samples = vec![sample(0.0, 1.0), sample(1.0, 2.0), sample(2.0, 3.0)];
        let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();

        let srt = build_srt(&[Field::Time, Field::Depth], &samples, &times, 0.0, 0.0, 2.5, false);

        assert_eq!(srt.matches(" --> ").count(), 3);
        assert!(srt.starts_with("1\n00:00:00,000 --> 00:00:01,000\n"));
        assert!(srt.contains("00:00:02,000 --> 00:00:02,500\n"));
        assert!(srt.contains("Depth: 2.0 m"));
    }

    #[test]
    fn shifts_cues_by_sync_offset() {
        let samples = vec![sample(10.0, 5.0)];
        let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();

        // video-second 0 corresponds to csv_sync_sec=10, so the single cue
        // should already show the sample instead of "No data".
        let srt = build_srt(&[Field::Depth], &samples, &times, 0.0, 10.0, 1.0, false);
        assert!(srt.contains("Depth: 5.0 m"));
        assert!(!srt.contains("No data"));
    }

    #[test]
    fn parses_srt_timestamps_back_into_seconds() {
        assert_eq!(parse_srt_timestamp("00:00:00,000"), Some(0.0));
        assert_eq!(parse_srt_timestamp("00:01:01,500"), Some(61.5));
        assert_eq!(parse_srt_timestamp("01:01:01.250"), Some(3661.25));
        assert_eq!(parse_srt_timestamp("nope"), None);
        assert_eq!(parse_srt_timestamp("00:00,000"), None);
    }

    #[test]
    fn round_trips_every_timestamp_a_built_track_contains() {
        let samples = vec![sample(0.0, 1.0), sample(1.0, 2.0)];
        let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();
        let srt = build_srt(&[Field::Depth], &samples, &times, 0.0, 0.0, 2.0, false);

        for line in srt.lines().filter(|l| l.contains(" --> ")) {
            let (start, end) = line.split_once(" --> ").unwrap();
            assert_eq!(format_srt_timestamp(parse_srt_timestamp(start).unwrap()), start);
            assert_eq!(format_srt_timestamp(parse_srt_timestamp(end).unwrap()), end);
        }
    }

    #[test]
    fn concat_srt_shifts_and_renumbers_cues() {
        let first = "1\n00:00:00,000 --> 00:00:01,000\nDepth: 1.0 m\n\n\
                     2\n00:00:01,000 --> 00:00:02,000\nDepth: 2.0 m\n\n";
        let second = "1\n00:00:00,000 --> 00:00:01,000\nDepth: 9.0 m\n\n";

        let merged = concat_srt(&[
            SrtPart {
                srt: first,
                offset_sec: 0.0,
            },
            SrtPart {
                srt: second,
                offset_sec: 2.0,
            },
        ])
        .unwrap();

        assert_eq!(merged.matches(" --> ").count(), 3);
        // Cue numbers keep climbing across the seam rather than restarting.
        assert!(
            merged.contains("3\n00:00:02,000 --> 00:00:03,000\nDepth: 9.0 m"),
            "{merged}"
        );
        assert!(merged.starts_with("1\n00:00:00,000 --> 00:00:01,000\nDepth: 1.0 m"));
    }

    #[test]
    fn concat_srt_keeps_multi_line_cues_and_tolerates_crlf() {
        let part = "1\r\n00:00:00,000 --> 00:00:01,000\r\nDive: 0:10\r\nDepth: 3.0 m\r\n\r\n";
        let merged = concat_srt(&[SrtPart {
            srt: part,
            offset_sec: 90.0,
        }])
        .unwrap();
        assert_eq!(merged, "1\n00:01:30,000 --> 00:01:31,000\nDive: 0:10\nDepth: 3.0 m\n\n");
    }

    #[test]
    fn concat_srt_accepts_an_empty_part_and_rejects_a_malformed_one() {
        let ok = concat_srt(&[
            SrtPart {
                srt: "",
                offset_sec: 0.0,
            },
            SrtPart {
                srt: "1\n00:00:00,000 --> 00:00:01,000\nDepth: 1.0 m\n\n",
                offset_sec: 5.0,
            },
        ])
        .unwrap();
        assert_eq!(ok, "1\n00:00:05,000 --> 00:00:06,000\nDepth: 1.0 m\n\n");

        let err = concat_srt(&[SrtPart {
            srt: "1\n00:00:00,000 -> 00:00:01,000\nDepth: 1.0 m\n\n",
            offset_sec: 0.0,
        }])
        .unwrap_err();
        assert!(
            err.to_string().contains("Malformed SRT block"),
            "unexpected error: {err}"
        );
    }
}
