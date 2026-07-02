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
        let lines = build_overlay_lines(fields, samples, times, dive_sec);

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

        let srt = build_srt(&[Field::Time, Field::Depth], &samples, &times, 0.0, 0.0, 2.5);

        assert_eq!(srt.matches(" --> ").count(), 3);
        assert!(srt.starts_with("1\n00:00:00,000 --> 00:00:01,000\n"));
        assert!(srt.contains("00:00:02,000 --> 00:00:02,500\n"));
        assert!(srt.contains("Tiefe: 2.0 m"));
    }

    #[test]
    fn shifts_cues_by_sync_offset() {
        let samples = vec![sample(10.0, 5.0)];
        let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();

        // video-second 0 corresponds to csv_sync_sec=10, so the single cue
        // should already show the sample instead of "Keine Daten".
        let srt = build_srt(&[Field::Depth], &samples, &times, 0.0, 10.0, 1.0);
        assert!(srt.contains("Tiefe: 5.0 m"));
        assert!(!srt.contains("Keine Daten"));
    }
}
