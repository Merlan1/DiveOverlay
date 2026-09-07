pub mod csv_data;
pub mod error;
pub mod ffprobe;
pub mod lookup;
pub mod merge;
pub mod model;
pub mod overlay;
pub mod pipeline;
pub mod subtitle;
pub mod sync;

pub use error::{CoreError, CoreResult};
pub use model::{ClipJob, DiveSample, Field};

/// Re-exported so frontends can hold a decoded frame (from `extract_frame_at`)
/// without depending on `image` themselves -- and, more importantly, without
/// risking a second copy of the crate at a different version, which would make
/// that frame a different type than the drawing functions accept.
pub use image::RgbImage;
