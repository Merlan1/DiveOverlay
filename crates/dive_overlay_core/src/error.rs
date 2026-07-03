use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("CSV error: {0}")]
    Csv(#[from] csv::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Invalid time value: {0}")]
    InvalidDuration(String),
    #[error("CSV has no header row")]
    NoHeader,
    #[error("CSV contains no usable samples")]
    NoSamples,
    #[error("No time column found (e.g. 'sample time (min)')")]
    MissingTimeColumn,
    #[error("No depth column found (e.g. 'sample depth (m)')")]
    MissingDepthColumn,
    #[error("Column '{0}' not found")]
    ColumnNotFound(String),
    #[error("{0}")]
    InvalidColumnMap(String),
    #[error("{0}")]
    InvalidFields(String),
    #[error("{0}")]
    InvalidClipSpec(String),
    #[error("Video not found: {0}")]
    VideoNotFound(PathBuf),
    #[error("CSV not found: {0}")]
    CsvNotFound(PathBuf),
    #[error("ffprobe error: {0}")]
    Ffprobe(String),
    #[error("ffmpeg error: {0}")]
    Ffmpeg(String),
    #[error("{0}")]
    Other(String),
}

pub type CoreResult<T> = Result<T, CoreError>;
