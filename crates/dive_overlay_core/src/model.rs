use std::path::PathBuf;

use chrono::{DateTime, Utc};

#[derive(Debug, Clone, PartialEq)]
pub struct DiveSample {
    pub elapsed_sec: f64,
    pub depth_m: Option<f64>,
    pub temp_c: Option<f64>,
    pub pressure_bar: Option<f64>,
    pub heart_rate: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct ClipJob {
    pub video_path: PathBuf,
    pub output_path: PathBuf,
    pub video_sync_sec: f64,
    pub csv_sync_sec: f64,
    pub video_start_utc: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Field {
    Time,
    Depth,
    Temp,
    Pressure,
    Hr,
}

/// Sorted to match the original Python's `sorted(allowed)` in error messages.
pub const ALLOWED_FIELD_NAMES: [&str; 5] = ["depth", "hr", "pressure", "temp", "time"];

impl Field {
    pub fn parse(s: &str) -> Option<Field> {
        match s {
            "time" => Some(Field::Time),
            "depth" => Some(Field::Depth),
            "temp" => Some(Field::Temp),
            "pressure" => Some(Field::Pressure),
            "hr" => Some(Field::Hr),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Field::Time => "time",
            Field::Depth => "depth",
            Field::Temp => "temp",
            Field::Pressure => "pressure",
            Field::Hr => "hr",
        }
    }
}

/// Formats a single field's value for display. Returns `None` when the
/// sample lacks that measurement, or for `Field::Time` (whose display line
/// is built by the caller from the dive-elapsed-second, not from a
/// per-sample value).
pub fn value_for_field(sample: &DiveSample, field: Field) -> Option<String> {
    match field {
        Field::Time => None,
        Field::Depth => sample.depth_m.map(|d| format!("Depth: {:.1} m", d)),
        Field::Temp => sample.temp_c.map(|t| format!("Temp: {:.1} C", t)),
        Field::Pressure => sample.pressure_bar.map(|p| format!("Pressure: {:.0} bar", p)),
        Field::Hr => sample.heart_rate.map(|h| format!("HR: {:.0} bpm", h)),
    }
}
