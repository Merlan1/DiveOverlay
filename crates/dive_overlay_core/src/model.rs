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

/// Raw numeric measurement of `sample` for `field`, or `None` if that
/// sample doesn't have it (or for `Field::Time`, which has no per-sample
/// value). Shared by `value_for_field` and the interpolated lookup in
/// `overlay.rs`, so both paths read the same underlying number.
pub fn field_raw_value(sample: &DiveSample, field: Field) -> Option<f64> {
    match field {
        Field::Time => None,
        Field::Depth => sample.depth_m,
        Field::Temp => sample.temp_c,
        Field::Pressure => sample.pressure_bar,
        Field::Hr => sample.heart_rate,
    }
}

/// Formats an already-resolved numeric `value` for `field` display. Split
/// out from `value_for_field` so an interpolated (not-directly-logged)
/// value can be formatted identically to a carried-forward one.
pub fn format_field_value(field: Field, value: f64) -> Option<String> {
    match field {
        Field::Time => None,
        Field::Depth => Some(format!("Depth: {:.1} m", value)),
        Field::Temp => Some(format!("Temp: {:.1} C", value)),
        Field::Pressure => Some(format!("Pressure: {:.0} bar", value)),
        Field::Hr => Some(format!("HR: {:.0} bpm", value)),
    }
}

/// Formats a single field's value for display. Returns `None` when the
/// sample lacks that measurement, or for `Field::Time` (whose display line
/// is built by the caller from the dive-elapsed-second, not from a
/// per-sample value).
pub fn value_for_field(sample: &DiveSample, field: Field) -> Option<String> {
    field_raw_value(sample, field).and_then(|v| format_field_value(field, v))
}
