use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::error::CoreError;
use crate::model::{DiveSample, Field, ALLOWED_FIELD_NAMES};

const COLUMN_MAP_KEYS: [&str; 7] = ["time", "depth", "temp", "pressure", "hr", "date", "clock"];

fn strip_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)
}

fn normalize_header(h: &str) -> String {
    h.trim().trim_matches('"').to_string()
}

pub fn parse_duration_to_seconds(value: &str) -> Result<f64, CoreError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(CoreError::InvalidDuration("Empty time value".to_string()));
    }

    // Rust's f64 parser accepts "nan"/"inf"; neither is a usable duration
    // (and NaN would break sorting of the sample list), so reject them here.
    let finite = |v: f64| if v.is_finite() { Some(v) } else { None };

    if !value.contains(':') {
        return value
            .parse::<f64>()
            .ok()
            .and_then(finite)
            .ok_or_else(|| CoreError::InvalidDuration(value.to_string()));
    }

    let parts: Vec<&str> = value.split(':').collect();
    let err = || CoreError::InvalidDuration(format!("Unknown time format: {value}"));
    match parts.len() {
        2 => {
            let minutes: i64 = parts[0].parse().map_err(|_| err())?;
            let seconds: f64 = parts[1].parse().ok().and_then(finite).ok_or_else(err)?;
            Ok(minutes as f64 * 60.0 + seconds)
        }
        3 => {
            let hours: i64 = parts[0].parse().map_err(|_| err())?;
            let minutes: i64 = parts[1].parse().map_err(|_| err())?;
            let seconds: f64 = parts[2].parse().ok().and_then(finite).ok_or_else(err)?;
            Ok(hours as f64 * 3600.0 + minutes as f64 * 60.0 + seconds)
        }
        _ => Err(err()),
    }
}

/// Formats to whole seconds (`mm:ss` / `hh:mm:ss`), rounding -- what the
/// overlay's "Dive time" line shows.
pub fn format_duration(seconds: f64) -> String {
    let total = seconds.round().max(0.0) as i64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let sec = total % 60;
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{sec:02}")
    } else {
        format!("{minutes:02}:{sec:02}")
    }
}

/// Like `format_duration` but keeps fractional seconds (to the millisecond,
/// trailing zeros trimmed, e.g. `00:10.5`), so a value round-trips through
/// `parse_duration_to_seconds` without being rounded to a whole second.
/// Used where the string is *stored* (the GUI's per-clip CSV sync field)
/// rather than displayed.
pub fn format_duration_precise(seconds: f64) -> String {
    let millis_total = (seconds.max(0.0) * 1000.0).round() as i64;
    let millis = millis_total % 1000;
    if millis == 0 {
        return format_duration((millis_total / 1000) as f64);
    }
    let total = millis_total / 1000;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let sec = total % 60;
    let frac = format!("{millis:03}");
    let frac = frac.trim_end_matches('0');
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{sec:02}.{frac}")
    } else {
        format!("{minutes:02}:{sec:02}.{frac}")
    }
}

pub fn parse_optional_float(value: Option<&str>) -> Option<f64> {
    let text = value?.trim().trim_matches('"');
    if text.is_empty() {
        return None;
    }
    text.replace(',', ".").parse::<f64>().ok()
}

/// Two-phase column resolution, ported verbatim from the original's
/// `find_column`: an exact-match pass over `candidates` in order first,
/// then a substring pass iterating headers x candidates. Both phases and
/// their iteration order are load-bearing for ambiguous-header resolution.
pub fn find_column_index(headers: &[String], candidates: &[&str]) -> Option<usize> {
    let normalized: Vec<String> = headers.iter().map(|h| h.to_lowercase().trim().to_string()).collect();

    for candidate in candidates {
        if let Some(idx) = normalized.iter().position(|key| key == candidate) {
            return Some(idx);
        }
    }

    for (idx, key) in normalized.iter().enumerate() {
        for candidate in candidates {
            if key.contains(candidate) {
                return Some(idx);
            }
        }
    }

    None
}

pub fn read_csv_headers(csv_path: &Path) -> Result<Vec<String>, CoreError> {
    let bytes = fs::read(csv_path)?;
    let bytes = strip_bom(&bytes);
    let mut reader = csv::ReaderBuilder::new().flexible(true).from_reader(bytes);
    let headers = reader.headers()?.iter().map(normalize_header).collect::<Vec<_>>();
    if headers.is_empty() {
        return Err(CoreError::NoHeader);
    }
    Ok(headers)
}

/// `find_column_index` with one header index masked out, so a column already
/// claimed for another purpose can't be matched again by the substring pass.
fn find_column_index_excluding(headers: &[String], candidates: &[&str], exclude: Option<usize>) -> Option<usize> {
    match exclude {
        None => find_column_index(headers, candidates),
        Some(skip) => {
            let masked: Vec<String> = headers
                .iter()
                .enumerate()
                .map(|(i, h)| if i == skip { String::new() } else { h.clone() })
                .collect();
            find_column_index(&masked, candidates)
        }
    }
}

const ELAPSED_TIME_CANDIDATES: [&str; 3] = ["sample time (min)", "sample time", "time"];

/// Resolves the CSV's date and wall-clock time columns (for auto-sync),
/// honoring `date=`/`clock=` overrides from `--column-map` first. Without
/// an override, the wall-clock heuristic deliberately skips whichever
/// column holds the *elapsed* dive time: with no exact `time` header, the
/// substring pass would otherwise happily pick `sample time (min)` as the
/// clock column and every datetime parse would fail.
pub fn read_csv_datetime_columns(
    csv_path: &Path,
    column_map: &HashMap<String, String>,
) -> Result<(Option<String>, Option<String>), CoreError> {
    let headers = read_csv_headers(csv_path)?;
    let elapsed_idx = resolve_from_map(column_map, &headers, "time")?
        .or_else(|| find_column_index(&headers, &ELAPSED_TIME_CANDIDATES));
    let date_idx = resolve_from_map(column_map, &headers, "date")?
        .or_else(|| find_column_index(&headers, &["date", "datum", "sample date"]));
    let clock_idx = resolve_from_map(column_map, &headers, "clock")?
        .or_else(|| find_column_index_excluding(&headers, &["time", "zeit", "clock time", "clock"], elapsed_idx));
    Ok((
        date_idx.map(|i| headers[i].clone()),
        clock_idx.map(|i| headers[i].clone()),
    ))
}

/// Reads the first data row's values for the given column names (by
/// normalized header match), used by auto-sync to read the CSV's initial
/// date/clock values without loading the full sample set.
pub fn read_first_row_columns(csv_path: &Path, columns: &[&str]) -> Result<Option<Vec<String>>, CoreError> {
    let bytes = fs::read(csv_path)?;
    let bytes = strip_bom(&bytes);
    let mut reader = csv::ReaderBuilder::new().flexible(true).from_reader(bytes.as_ref());
    let headers = reader.headers()?.iter().map(normalize_header).collect::<Vec<_>>();
    if headers.is_empty() {
        return Err(CoreError::NoHeader);
    }

    let indices: Vec<Option<usize>> = columns
        .iter()
        .map(|col| {
            let col_lower = col.to_lowercase();
            headers.iter().position(|h| h.to_lowercase() == col_lower)
        })
        .collect();

    let Some(record) = reader.records().next() else {
        return Ok(None);
    };
    let record = record?;

    let values = indices
        .into_iter()
        .map(|idx| {
            idx.and_then(|i| record.get(i))
                .unwrap_or("")
                .trim()
                .trim_matches('"')
                .to_string()
        })
        .collect();
    Ok(Some(values))
}

pub fn parse_fields(value: &str) -> Result<Vec<Field>, CoreError> {
    let raw_fields: Vec<String> = value
        .split(',')
        .map(|v| v.trim().to_lowercase())
        .filter(|v| !v.is_empty())
        .collect();
    if raw_fields.is_empty() {
        return Err(CoreError::InvalidFields("--fields must not be empty".to_string()));
    }

    let mut fields = Vec::new();
    let mut unknown = Vec::new();
    for raw in &raw_fields {
        match Field::parse(raw) {
            Some(field) => fields.push(field),
            None => unknown.push(raw.clone()),
        }
    }

    if !unknown.is_empty() {
        return Err(CoreError::InvalidFields(format!(
            "Unknown fields in --fields: {}. Allowed: {}",
            unknown.join(", "),
            ALLOWED_FIELD_NAMES.join(", ")
        )));
    }

    Ok(fields)
}

pub fn parse_column_map(value: &str) -> Result<HashMap<String, String>, CoreError> {
    let mut mapping = HashMap::new();
    let value = value.trim();
    if value.is_empty() {
        return Ok(mapping);
    }

    for part in value.split(',').map(|p| p.trim()).filter(|p| !p.is_empty()) {
        let Some((key, col)) = part.split_once('=') else {
            return Err(CoreError::InvalidColumnMap(
                "--column-map format: key=ColumnName, e.g. time=TIME,depth=Depth".to_string(),
            ));
        };
        let key = key.trim();
        let col = col.trim();
        if !COLUMN_MAP_KEYS.contains(&key) {
            return Err(CoreError::InvalidColumnMap(format!("Unknown column-map key: {key}")));
        }
        if col.is_empty() {
            return Err(CoreError::InvalidColumnMap(
                "Column name in --column-map must not be empty".to_string(),
            ));
        }
        mapping.insert(key.to_string(), col.to_string());
    }

    Ok(mapping)
}

fn resolve_from_map(
    column_map: &HashMap<String, String>,
    headers: &[String],
    key: &str,
) -> Result<Option<usize>, CoreError> {
    let Some(mapped) = column_map.get(key).map(|s| s.trim()).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let mapped_lower = mapped.to_lowercase();
    match headers.iter().position(|h| h.to_lowercase() == mapped_lower) {
        Some(idx) => Ok(Some(idx)),
        None => Err(CoreError::ColumnNotFound(mapped.to_string())),
    }
}

pub fn load_samples(csv_path: &Path, column_map: &HashMap<String, String>) -> Result<Vec<DiveSample>, CoreError> {
    let bytes = fs::read(csv_path)?;
    let bytes = strip_bom(&bytes);
    let mut reader = csv::ReaderBuilder::new().flexible(true).from_reader(bytes);
    let headers = reader.headers()?.iter().map(normalize_header).collect::<Vec<_>>();
    if headers.is_empty() {
        return Err(CoreError::NoHeader);
    }

    let time_idx = resolve_from_map(column_map, &headers, "time")?
        .or_else(|| find_column_index(&headers, &ELAPSED_TIME_CANDIDATES));
    let depth_idx = resolve_from_map(column_map, &headers, "depth")?
        .or_else(|| find_column_index(&headers, &["sample depth (m)", "sample depth", "depth"]));
    let temp_idx = resolve_from_map(column_map, &headers, "temp")?.or_else(|| {
        find_column_index(
            &headers,
            &["sample temperature (c)", "sample temperature", "temperature"],
        )
    });
    let pressure_idx = resolve_from_map(column_map, &headers, "pressure")?
        .or_else(|| find_column_index(&headers, &["sample pressure (bar)", "sample pressure", "pressure"]));
    let hr_idx = resolve_from_map(column_map, &headers, "hr")?.or_else(|| {
        find_column_index(
            &headers,
            &["sample heartrate", "sample heart rate", "heartrate", "heart rate"],
        )
    });

    let time_idx = time_idx.ok_or(CoreError::MissingTimeColumn)?;
    let depth_idx = depth_idx.ok_or(CoreError::MissingDepthColumn)?;

    let mut samples = Vec::new();
    for result in reader.records() {
        let record = result?;
        let time_raw = record.get(time_idx).unwrap_or("").trim().trim_matches('"');
        if time_raw.is_empty() {
            continue;
        }

        let elapsed_sec = parse_duration_to_seconds(time_raw)?;
        samples.push(DiveSample {
            elapsed_sec,
            depth_m: parse_optional_float(record.get(depth_idx)),
            temp_c: temp_idx.and_then(|i| parse_optional_float(record.get(i))),
            pressure_bar: pressure_idx.and_then(|i| parse_optional_float(record.get(i))),
            heart_rate: hr_idx.and_then(|i| parse_optional_float(record.get(i))),
        });
    }

    samples.sort_by(|a, b| a.elapsed_sec.total_cmp(&b.elapsed_sec));
    if samples.is_empty() {
        return Err(CoreError::NoSamples);
    }
    Ok(samples)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dive_csv_path() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("dive.csv")
    }

    #[test]
    fn parses_mmss_and_hhmmss() {
        assert_eq!(parse_duration_to_seconds("0:10").unwrap(), 10.0);
        assert_eq!(parse_duration_to_seconds("1:30").unwrap(), 90.0);
        assert_eq!(parse_duration_to_seconds("1:00:00").unwrap(), 3600.0);
        assert_eq!(parse_duration_to_seconds("5.5").unwrap(), 5.5);
    }

    #[test]
    fn rejects_empty_duration() {
        assert!(parse_duration_to_seconds("").is_err());
        assert!(parse_duration_to_seconds("   ").is_err());
    }

    #[test]
    fn rejects_non_finite_durations() {
        assert!(parse_duration_to_seconds("nan").is_err());
        assert!(parse_duration_to_seconds("inf").is_err());
        assert!(parse_duration_to_seconds("1:NaN").is_err());
    }

    #[test]
    fn load_samples_does_not_panic_on_nan_time_value() {
        let dir = std::env::temp_dir().join("dive_overlay_csv_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("nan.csv");
        std::fs::write(&path, "sample time (min),sample depth (m)\n0:10,1.0\nnan,2.0\n").unwrap();
        assert!(load_samples(&path, &HashMap::new()).is_err());
    }

    #[test]
    fn format_duration_round_trip() {
        assert_eq!(format_duration(10.0), "00:10");
        assert_eq!(format_duration(90.0), "01:30");
        assert_eq!(format_duration(3600.0), "01:00:00");
        assert_eq!(format_duration(-5.0), "00:00");
    }

    #[test]
    fn format_duration_precise_keeps_fractional_seconds_and_round_trips() {
        assert_eq!(format_duration_precise(10.0), "00:10");
        assert_eq!(format_duration_precise(10.5), "00:10.5");
        assert_eq!(format_duration_precise(9.5), "00:09.5");
        assert_eq!(format_duration_precise(3661.25), "01:01:01.25");
        assert_eq!(format_duration_precise(9.9999), "00:10");
        for v in [0.0, 0.5, 9.5, 10.5, 59.5, 61.25, 3600.75] {
            let s = format_duration_precise(v);
            assert!((parse_duration_to_seconds(&s).unwrap() - v).abs() < 1e-9, "{v} -> {s}");
        }
    }

    #[test]
    fn datetime_columns_skip_elapsed_time_column_and_honor_column_map() {
        let dir = std::env::temp_dir().join("dive_overlay_csv_test");
        std::fs::create_dir_all(&dir).unwrap();

        // No exact "time" header: the clock heuristic must not grab the
        // elapsed "sample time (min)" column.
        let path = dir.join("no_clock.csv");
        std::fs::write(&path, "date,sample time (min),sample depth (m)\n2025-07-05,0:10,1.0\n").unwrap();
        let (date, clock) = read_csv_datetime_columns(&path, &HashMap::new()).unwrap();
        assert_eq!(date.as_deref(), Some("date"));
        assert_eq!(clock, None);

        // ...but a clock column with an unusual name still matches by substring.
        let path = dir.join("time_of_day.csv");
        std::fs::write(
            &path,
            "date,sample time (min),Time of day,sample depth (m)\n2025-07-05,0:10,15:32:55,1.0\n",
        )
        .unwrap();
        let (_, clock) = read_csv_datetime_columns(&path, &HashMap::new()).unwrap();
        assert_eq!(clock.as_deref(), Some("Time of day"));

        // Explicit overrides win over every heuristic.
        let path = dir.join("mapped.csv");
        std::fs::write(
            &path,
            "D,C,sample time (min),sample depth (m)\n2025-07-05,15:32:55,0:10,1.0\n",
        )
        .unwrap();
        let mut map = HashMap::new();
        map.insert("date".to_string(), "D".to_string());
        map.insert("clock".to_string(), "c".to_string());
        let (date, clock) = read_csv_datetime_columns(&path, &map).unwrap();
        assert_eq!(date.as_deref(), Some("D"));
        assert_eq!(clock.as_deref(), Some("C"));

        map.insert("clock".to_string(), "missing".to_string());
        assert!(read_csv_datetime_columns(&path, &map).is_err());
    }

    #[test]
    fn optional_float_handles_comma_decimals_and_blanks() {
        assert_eq!(parse_optional_float(Some("24,0")), Some(24.0));
        assert_eq!(parse_optional_float(Some("")), None);
        assert_eq!(parse_optional_float(Some("  ")), None);
        assert_eq!(parse_optional_float(None), None);
        assert_eq!(parse_optional_float(Some("\"1.1\"")), Some(1.1));
    }

    #[test]
    fn find_column_index_exact_then_substring() {
        let headers = vec!["Sample Depth (m)".to_string(), "sample time (min)".to_string()];
        assert_eq!(find_column_index(&headers, &["sample depth (m)", "depth"]), Some(0));
        assert_eq!(find_column_index(&headers, &["depth"]), Some(0));
        assert_eq!(find_column_index(&headers, &["nonexistent"]), None);
    }

    #[test]
    fn parse_fields_rejects_unknown() {
        assert!(parse_fields("time,depth").is_ok());
        assert!(parse_fields("").is_err());
        assert!(parse_fields("time,bogus").is_err());
    }

    #[test]
    fn parse_column_map_rejects_unknown_key() {
        let map = parse_column_map("time=TIME,depth=Depth").unwrap();
        assert_eq!(map.get("time").unwrap(), "TIME");
        assert!(parse_column_map("bogus=Foo").is_err());
        assert!(parse_column_map("time=").is_err());
    }

    #[test]
    fn loads_real_dive_csv_fixture() {
        let samples = load_samples(&dive_csv_path(), &HashMap::new()).unwrap();
        assert!(!samples.is_empty());
        // sorted ascending by elapsed_sec
        for pair in samples.windows(2) {
            assert!(pair[0].elapsed_sec <= pair[1].elapsed_sec);
        }
        // first sample per dive.csv: 0:10 -> 10s, depth 1.1m, no temp/pressure
        assert_eq!(samples[0].elapsed_sec, 10.0);
        assert_eq!(samples[0].depth_m, Some(1.1));
    }

    #[test]
    fn column_map_override_takes_precedence() {
        let mut map = HashMap::new();
        map.insert("depth".to_string(), "sample depth (m)".to_string());
        let samples = load_samples(&dive_csv_path(), &map).unwrap();
        assert!(!samples.is_empty());
    }
}
