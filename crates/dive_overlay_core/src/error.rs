use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("IO-Fehler: {0}")]
    Io(#[from] std::io::Error),
    #[error("CSV-Fehler: {0}")]
    Csv(#[from] csv::Error),
    #[error("JSON-Fehler: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Ungültiger Zeitwert: {0}")]
    InvalidDuration(String),
    #[error("CSV hat keine Kopfzeile")]
    NoHeader,
    #[error("CSV enthält keine verwertbaren Samples")]
    NoSamples,
    #[error("Keine Zeitspalte gefunden (z. B. 'sample time (min)')")]
    MissingTimeColumn,
    #[error("Keine Tiefenspalte gefunden (z. B. 'sample depth (m)')")]
    MissingDepthColumn,
    #[error("Spalte '{0}' nicht gefunden")]
    ColumnNotFound(String),
    #[error("{0}")]
    InvalidColumnMap(String),
    #[error("{0}")]
    InvalidFields(String),
    #[error("{0}")]
    InvalidClipSpec(String),
    #[error("Video nicht gefunden: {0}")]
    VideoNotFound(PathBuf),
    #[error("CSV nicht gefunden: {0}")]
    CsvNotFound(PathBuf),
    #[error("ffprobe-Fehler: {0}")]
    Ffprobe(String),
    #[error("ffmpeg-Fehler: {0}")]
    Ffmpeg(String),
    #[error("{0}")]
    Other(String),
}

pub type CoreResult<T> = Result<T, CoreError>;
