//! Planar YUV 4:2:0 frames, the format the decode/encode pipeline moves
//! between its two ffmpeg subprocesses.
//!
//! The pipeline used to carry `rgb24`, which cost two full swscale passes per
//! frame (video codecs are natively YUV, so the picture was converted out of
//! the format both ends already spoke and straight back into it) and twice
//! the bytes on both pipes: 3 bytes per pixel against 1.5. It was also
//! quietly lossy, since `yuv420p -> rgb24 -> yuv420p` upsamples the chroma to
//! full resolution and re-subsamples it every frame.
//!
//! The overlay is therefore composited directly into these planes. See
//! `overlay::composite_tile_yuv` for the blending, and `ColorRange` below for
//! the one piece of metadata that has to be right for it to look correct.

/// Whether luma/chroma samples use the full 0..255 range or the
/// broadcast-legal range (Y 16..235, chroma 16..240).
///
/// The overlay's black and white points must match the picture's, or the
/// text renders either as clipped super-white or as dull grey. Both cases
/// are real: GoPro footage is full range (`yuvj420p`), while most delivery
/// encodes are limited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorRange {
    Limited,
    Full,
}

impl ColorRange {
    pub fn from_full_range_flag(full_range: bool) -> Self {
        if full_range {
            ColorRange::Full
        } else {
            ColorRange::Limited
        }
    }

    /// The ffmpeg pixel-format name carrying this range for 8-bit planar
    /// 4:2:0. Both have identical plane layout -- `yuvj420p` differs only in
    /// declaring full range -- so `Yuv420Frame` treats them interchangeably.
    ///
    /// Asking the decoder for the source's own range means it hands the
    /// planes over untouched, with no swscale range conversion and no loss.
    pub fn raw_pixel_format(self) -> &'static str {
        match self {
            ColorRange::Limited => "yuv420p",
            ColorRange::Full => "yuvj420p",
        }
    }

    /// `(black, white)` luma endpoints.
    fn luma_endpoints(self) -> (f32, f32) {
        match self {
            ColorRange::Limited => (16.0, 235.0),
            ColorRange::Full => (0.0, 255.0),
        }
    }

    /// Converts a neutral (R==G==B) 0..255 sRGB level to a luma sample.
    ///
    /// Restricted to greys on purpose: for a neutral colour every YUV matrix
    /// agrees (BT.601 and BT.709 both reduce to Y = level, chroma = 128), so
    /// the overlay never has to know which matrix the footage uses, and a
    /// mis-detected `color_space` cannot tint it. That is why the overlay
    /// palette is all greys -- see `overlay.rs`.
    pub fn luma_for_grey(self, level: u8) -> u8 {
        let (black, white) = self.luma_endpoints();
        (black + (level as f32 / 255.0) * (white - black))
            .round()
            .clamp(0.0, 255.0) as u8
    }
}

/// The chroma sample meaning "no colour". Identical in every YUV matrix and
/// in both ranges, which is what makes an all-grey overlay palette safe.
pub const NEUTRAL_CHROMA: u8 = 128;

/// A planar YUV 4:2:0 frame: a full-resolution Y plane followed by
/// half-resolution U and V planes, exactly the byte layout ffmpeg's
/// `rawvideo` muxer reads and writes.
///
/// Both dimensions must be even -- 4:2:0 stores one chroma sample per 2x2
/// luma block, so an odd width or height has no representation here. The
/// pipeline guarantees this before constructing one.
pub struct Yuv420Frame {
    data: Vec<u8>,
    width: u32,
    height: u32,
}

impl Yuv420Frame {
    /// Bytes one `width` x `height` frame occupies: `w*h` luma plus two
    /// `(w/2)*(h/2)` chroma planes, i.e. 1.5 bytes per pixel against
    /// `rgb24`'s 3.
    pub const fn buffer_size(width: u32, height: u32) -> usize {
        let luma = width as usize * height as usize;
        luma + 2 * ((width as usize / 2) * (height as usize / 2))
    }

    /// Wraps a raw buffer read from the decoder. Returns `None` if the
    /// dimensions are odd or the buffer is the wrong length -- a silent
    /// mismatch here would shear the picture rather than fail, the same
    /// hazard as a decoder/encoder frame-size disagreement.
    pub fn from_raw(width: u32, height: u32, data: Vec<u8>) -> Option<Self> {
        if !width.is_multiple_of(2) || !height.is_multiple_of(2) || width == 0 || height == 0 {
            return None;
        }
        if data.len() != Self::buffer_size(width, height) {
            return None;
        }
        Some(Self { data, width, height })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Chroma-plane dimensions: half the luma resolution on both axes.
    pub fn chroma_dimensions(&self) -> (u32, u32) {
        (self.width / 2, self.height / 2)
    }

    pub fn as_raw(&self) -> &[u8] {
        &self.data
    }

    pub fn into_raw(self) -> Vec<u8> {
        self.data
    }

    fn luma_len(&self) -> usize {
        self.width as usize * self.height as usize
    }

    fn chroma_len(&self) -> usize {
        (self.width as usize / 2) * (self.height as usize / 2)
    }

    pub fn y_plane_mut(&mut self) -> &mut [u8] {
        let end = self.luma_len();
        &mut self.data[..end]
    }

    /// The U and V planes together, so the borrow checker allows writing both
    /// in one pass over the chroma grid (they are always touched as a pair).
    pub fn chroma_planes_mut(&mut self) -> (&mut [u8], &mut [u8]) {
        let luma = self.luma_len();
        let chroma = self.chroma_len();
        let (_, rest) = self.data.split_at_mut(luma);
        let (u, v) = rest.split_at_mut(chroma);
        (u, &mut v[..chroma])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_size_is_one_and_a_half_bytes_per_pixel() {
        assert_eq!(Yuv420Frame::buffer_size(1920, 1080), 1920 * 1080 * 3 / 2);
        // The sizes this project actually runs at, against the rgb24 cost
        // they replace.
        assert_eq!(Yuv420Frame::buffer_size(5312, 2988), 23_808_384);
        assert_eq!(Yuv420Frame::buffer_size(3840, 2160), 12_441_600);
    }

    #[test]
    fn from_raw_rejects_odd_dimensions_and_wrong_lengths() {
        assert!(Yuv420Frame::from_raw(4, 4, vec![0; 24]).is_some());
        assert!(Yuv420Frame::from_raw(5, 4, vec![0; 30]).is_none(), "odd width");
        assert!(Yuv420Frame::from_raw(4, 5, vec![0; 30]).is_none(), "odd height");
        assert!(Yuv420Frame::from_raw(0, 4, vec![]).is_none(), "zero width");
        assert!(Yuv420Frame::from_raw(4, 4, vec![0; 23]).is_none(), "short buffer");
        assert!(Yuv420Frame::from_raw(4, 4, vec![0; 25]).is_none(), "long buffer");
    }

    #[test]
    fn planes_partition_the_buffer_without_overlapping() {
        let mut frame = Yuv420Frame::from_raw(4, 4, vec![0; 24]).expect("valid frame");
        frame.y_plane_mut().fill(1);
        let (u, v) = frame.chroma_planes_mut();
        u.fill(2);
        v.fill(3);

        let raw = frame.as_raw();
        assert_eq!(&raw[..16], &[1; 16], "luma plane");
        assert_eq!(&raw[16..20], &[2; 4], "U plane");
        assert_eq!(&raw[20..24], &[3; 4], "V plane");
    }

    /// Greys are the whole reason the overlay palette is neutral: the mapping
    /// depends only on the range, never on the BT.601/BT.709 matrix.
    #[test]
    fn grey_luma_respects_the_color_range() {
        assert_eq!(ColorRange::Full.luma_for_grey(255), 255);
        assert_eq!(ColorRange::Full.luma_for_grey(0), 0);
        assert_eq!(ColorRange::Limited.luma_for_grey(255), 235);
        assert_eq!(ColorRange::Limited.luma_for_grey(0), 16);
        // Mid-grey sits proportionally between the endpoints in both.
        assert_eq!(ColorRange::Full.luma_for_grey(128), 128);
        assert_eq!(ColorRange::Limited.luma_for_grey(128), 126);
    }

    #[test]
    fn raw_pixel_format_matches_the_range() {
        assert_eq!(ColorRange::Limited.raw_pixel_format(), "yuv420p");
        assert_eq!(ColorRange::Full.raw_pixel_format(), "yuvj420p");
        assert_eq!(ColorRange::from_full_range_flag(true), ColorRange::Full);
        assert_eq!(ColorRange::from_full_range_flag(false), ColorRange::Limited);
    }
}
