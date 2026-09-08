use std::sync::OnceLock;

use ab_glyph::{FontRef, PxScale};
use image::{Rgb, RgbImage, Rgba, RgbaImage};
use imageproc::drawing::{draw_hollow_rect_mut, draw_line_segment_mut, draw_text_mut, text_size};
use imageproc::rect::Rect;

use crate::csv_data::format_duration;
use crate::lookup::choose_sample_index;
use crate::model::{field_raw_value, format_field_value, value_for_field, DiveSample, Field};
use crate::yuv::{ColorRange, Yuv420Frame, NEUTRAL_CHROMA};

const FONT_BYTES: &[u8] = include_bytes!("../assets/fonts/DejaVuSans.ttf");

static FONT: OnceLock<FontRef<'static>> = OnceLock::new();

/// Parses the bundled font once and caches it -- `draw_overlay`/`draw_depth_graph`
/// call this every frame, and re-parsing the same static bytes on every call
/// was pure repeated work.
pub fn font() -> &'static FontRef<'static> {
    FONT.get_or_init(|| FontRef::try_from_slice(FONT_BYTES).expect("bundled DejaVuSans.ttf must be a valid font"))
}

/// Builds the display lines for the info box at `dive_sec`: the elapsed dive
/// time (if requested) plus, for every other requested field, either the
/// latest known value (carried forward) or a value linearly interpolated
/// between the nearest bracketing samples (if `interpolate` is set),
/// falling back to "No data" if nothing is available yet. Centralizing this
/// (the original duplicated it between the CLI's frame loop and the GUI's
/// preview code) keeps CLI/GUI rendering identical.
pub fn build_overlay_lines(
    fields: &[Field],
    samples: &[DiveSample],
    times: &[f64],
    dive_sec: f64,
    interpolate: bool,
) -> Vec<String> {
    let mut lines = Vec::new();
    if fields.contains(&Field::Time) {
        lines.push(format!("Dive time: {}", format_duration(dive_sec)));
    }

    if let Some(idx) = choose_sample_index(times, dive_sec) {
        for &field in fields {
            if field == Field::Time {
                continue;
            }
            let value = if interpolate {
                interpolated_value(samples, times, dive_sec, idx, field)
            } else {
                last_known_value(&samples[..=idx], field)
            };
            if let Some(value) = value {
                lines.push(value);
            }
        }
    }

    if lines.is_empty() {
        lines.push("No data".to_string());
    }
    lines
}

/// Fields like temperature aren't logged every sample, so walk backward from
/// the current sample to the most recent one that actually has this field.
fn last_known_value(samples_up_to_now: &[DiveSample], field: Field) -> Option<String> {
    samples_up_to_now
        .iter()
        .rev()
        .find_map(|sample| value_for_field(sample, field))
}

/// Interpolates `field` at `dive_sec` between the nearest samples at-or-before
/// and after `idx` that actually carry this field (skipping over sparse gaps
/// the same way `last_known_value` does), using a cubic Hermite spline so the
/// curve isn't kinked at each sample like linear interpolation. Never
/// extrapolates: before the first logged value there's nothing to show, and
/// after the last one this carries it forward, same as the non-interpolated
/// path.
fn interpolated_value(
    samples: &[DiveSample],
    times: &[f64],
    dive_sec: f64,
    idx: usize,
    field: Field,
) -> Option<String> {
    let before_idx = (0..=idx)
        .rev()
        .find(|&j| field_raw_value(&samples[j], field).is_some())?;
    let before = (times[before_idx], field_raw_value(&samples[before_idx], field).unwrap());
    let after_idx = (idx + 1..times.len()).find(|&j| field_raw_value(&samples[j], field).is_some());

    let value = match after_idx {
        Some(after_idx) if times[after_idx] > before.0 => {
            let after = (times[after_idx], field_raw_value(&samples[after_idx], field).unwrap());
            let prev = (0..before_idx)
                .rev()
                .find_map(|j| field_raw_value(&samples[j], field).map(|v| (times[j], v)));
            let next =
                (after_idx + 1..times.len()).find_map(|j| field_raw_value(&samples[j], field).map(|v| (times[j], v)));
            cubic_hermite(dive_sec, prev, before, after, next)
        }
        _ => before.1,
    };
    format_field_value(field, value)
}

/// Cubic Hermite interpolation between `p0` and `p1` at time `t`, with
/// tangents estimated Catmull-Rom style from each endpoint's other neighbor
/// (`prev`/`next`) over the *actual* elapsed time to that neighbor -- this
/// keeps tangents sane when samples are irregularly spaced, unlike a
/// standard uniform-spacing Catmull-Rom spline. Falls back to the `p0`-`p1`
/// secant slope at whichever end has no neighbor (start/end of the series
/// for this field), which degrades to plain linear interpolation there.
fn cubic_hermite(t: f64, prev: Option<(f64, f64)>, p0: (f64, f64), p1: (f64, f64), next: Option<(f64, f64)>) -> f64 {
    let (t0, v0) = p0;
    let (t1, v1) = p1;
    let dt = t1 - t0;
    let secant = (v1 - v0) / dt;

    let m0 = match prev {
        Some((tp, vp)) if t1 > tp => (v1 - vp) / (t1 - tp),
        _ => secant,
    };
    let m1 = match next {
        Some((tn, vn)) if tn > t0 => (vn - v0) / (tn - t0),
        _ => secant,
    };

    let s = (t - t0) / dt;
    let s2 = s * s;
    let s3 = s2 * s;
    let h00 = 2.0 * s3 - 3.0 * s2 + 1.0;
    let h10 = s3 - 2.0 * s2 + s;
    let h01 = -2.0 * s3 + 3.0 * s2;
    let h11 = s3 - s2;

    h00 * v0 + h10 * dt * m0 + h01 * v1 + h11 * dt * m1
}

/// Caches the rendered info-box tile (translucent background + text) across
/// frames, keyed on the exact lines shown. Given 1-10s dive-computer logging
/// intervals, the same lines are typically shown for many consecutive
/// frames, so re-rendering only on an actual content change turns most
/// frames' overlay cost into a cheap alpha-composite instead of glyph
/// rasterization.
#[derive(Default)]
pub struct OverlayCache {
    tile: Option<CachedTile>,
    graph: Option<CachedGraph>,
}

struct CachedTile {
    lines: Vec<String>,
    x: i32,
    y: i32,
    /// Stands in for the whole `OverlayMetrics` in the cache key, since the
    /// rest is derived from it: a tile rendered for one frame size must not
    /// be reused at another.
    line_height: i32,
    image: RgbaImage,
    /// The same tile converted for planar 4:2:0 compositing, built on first
    /// use and kept for as long as `image` is. This is what makes the YUV
    /// path cheap: the text changes about once a second, so the conversion
    /// is amortized over every frame in between and the per-frame cost is a
    /// pure blend. Tagged with the range it was built for, so a job on
    /// full-range footage can never reuse a limited-range conversion.
    yuv: Option<(ColorRange, YuvTile)>,
}

/// The depth graph's counterpart to `CachedTile`. It is keyed on a whole
/// `GraphPlan` rather than on the info box's line strings because the graph
/// has no equally cheap stand-in: its picture depends on every plotted point.
struct CachedGraph {
    plan: GraphPlan,
    image: RgbaImage,
    /// Built on first use and kept as long as `image` is, tagged with the
    /// range it was built for -- exactly as `CachedTile::yuv` is.
    yuv: Option<(ColorRange, YuvTile)>,
}
impl OverlayCache {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Line height in pixels: 4.5% of the frame height, floored so a very small
/// frame still gets legible text. Every other overlay size is pinned to this
/// one, so it doubles as the frame-relative scale factor (see
/// `OverlayMetrics`).
fn line_height_for(height: u32) -> i32 {
    ((height as f64 * 0.045) as i32).max(MIN_LINE_HEIGHT)
}

const MIN_LINE_HEIGHT: i32 = 24;
/// What `line_height_for` returns at 1080p (1080 * 0.045, truncated), where
/// the overlay's sizes were originally hand-tuned.
const REFERENCE_LINE_HEIGHT: f32 = 48.0;

/// Frame-relative sizing for the info box.
///
/// The overlay's proportions were hand-tuned at roughly 1080p, but only the
/// line height was ever expressed as a fraction of the frame -- the font and
/// padding were hard-coded pixel counts. On 5.3K footage the box therefore
/// grew to 134px per line while the glyphs stayed 22px: far too small to
/// read, and mushy once a player scales the picture back down. Deriving
/// every size from one factor keeps the overlay the same fraction of the
/// picture at any resolution, and by construction leaves the 1080p rendering
/// byte-identical to what it was.
struct OverlayMetrics {
    line_height: i32,
    font: PxScale,
    padding: i32,
}

impl OverlayMetrics {
    fn for_frame(height: u32) -> Self {
        let line_height = line_height_for(height);
        let scale = line_height as f32 / REFERENCE_LINE_HEIGHT;
        Self {
            line_height,
            // Floored for the same reason as the line height: below roughly
            // 800px tall, strict proportionality would shrink the text past
            // readable, so legibility wins over exact scaling there.
            font: PxScale::from((22.0 * scale).max(16.0)),
            padding: (14.0 * scale).max(6.0).round() as i32,
        }
    }
}

/// Renders `lines` into a standalone RGBA tile: a translucent background
/// (alpha baked in at 0.45) with opaque text drawn on top. The tile carries its own
/// alpha channel so it can be composited onto any frame later without
/// redoing font/text work.
fn render_tile(lines: &[String], x: i32, y: i32, metrics: &OverlayMetrics) -> CachedTile {
    let padding = metrics.padding;
    let scale = metrics.font;
    let font = font();

    let box_w = lines
        .iter()
        .map(|line| text_size(scale, font, line).0 as i32)
        .max()
        .unwrap_or(0)
        + padding * 2;
    let box_h = metrics.line_height * lines.len() as i32 + padding;

    let bg_alpha = (0.45_f32 * 255.0).round() as u8;
    let mut tile = RgbaImage::from_pixel(box_w.max(1) as u32, box_h.max(1) as u32, Rgba([20, 20, 20, bg_alpha]));

    let mut text_y = padding / 2;
    for line in lines {
        draw_text_mut(
            &mut tile,
            Rgba([230, 245, 255, 255]),
            padding,
            text_y,
            scale,
            font,
            line,
        );
        text_y += metrics.line_height;
    }

    CachedTile {
        lines: lines.to_vec(),
        x,
        y,
        line_height: metrics.line_height,
        image: tile,
        yuv: None,
    }
}

/// Alpha-composites `tile` (its own per-pixel alpha) onto `img` at `(x, y)`,
/// clipped to image bounds -- the per-frame counterpart to `render_tile`,
/// doing only the blend math with no font/text work.
///
/// Every overlay element -- the info box and the depth graph alike -- reaches
/// the frame through this and `composite_tile_yuv`, which is what lets one set
/// of drawing code serve both a packed-RGB frame (the GUI preview) and the
/// planar YUV frames the pipeline moves between its ffmpeg subprocesses: only
/// the two composites know anything about pixel layout.
fn composite_tile(img: &mut RgbImage, tile: &RgbaImage, x: i32, y: i32) {
    let (img_w, img_h) = img.dimensions();
    let (tile_w, tile_h) = tile.dimensions();
    let x0 = x.max(0) as u32;
    let y0 = y.max(0) as u32;
    let x1 = (x.saturating_add(tile_w as i32).max(0) as u32).min(img_w);
    let y1 = (y.saturating_add(tile_h as i32).max(0) as u32).min(img_h);

    for py in y0..y1 {
        for px in x0..x1 {
            let tile_px = tile.get_pixel((px as i32 - x) as u32, (py as i32 - y) as u32).0;
            let alpha = tile_px[3] as f32 / 255.0;
            if alpha <= 0.0 {
                continue;
            }
            let orig = img.get_pixel(px, py).0;
            let mut blended = [0u8; 3];
            for c in 0..3 {
                let v = alpha * tile_px[c] as f32 + (1.0 - alpha) * orig[c] as f32;
                blended[c] = v.round().clamp(0.0, 255.0) as u8;
            }
            img.put_pixel(px, py, Rgb(blended));
        }
    }
}

/// Position of the info box on a `w` x `h` frame, aligned so it can be
/// composited into 4:2:0 chroma without straddling a block. The RGB and YUV
/// paths share it so the GUI's preview lands on exactly the same pixels as
/// the burned-in output.
fn overlay_origin(w: u32, h: u32) -> (i32, i32) {
    (
        align_to_chroma_grid((w as f64 * 0.04) as i32),
        align_to_chroma_grid((h as f64 * 0.06) as i32),
    )
}

/// Renders the info box into `cache` if the cached tile is stale, and returns
/// it. Shared by both composite paths so the caching rule lives in one place.
fn cached_overlay_tile<'a>(
    cache: &'a mut OverlayCache,
    lines: &[String],
    w: u32,
    h: u32,
) -> Option<&'a mut CachedTile> {
    let (x, y) = overlay_origin(w, h);
    let metrics = OverlayMetrics::for_frame(h);

    let needs_render = match &cache.tile {
        Some(cached) => {
            cached.x != x
                || cached.y != y
                || cached.line_height != metrics.line_height
                || cached.lines.as_slice() != lines
        }
        None => true,
    };
    if needs_render {
        cache.tile = Some(render_tile(lines, x, y, &metrics));
    }
    cache.tile.as_mut()
}

/// Draws the info box onto a packed-RGB frame (the GUI's sync preview).
pub fn draw_overlay(img: &mut RgbImage, lines: &[String], cache: &mut OverlayCache) {
    let (w, h) = img.dimensions();
    if let Some(cached) = cached_overlay_tile(cache, lines, w, h) {
        composite_tile(img, &cached.image, cached.x, cached.y);
    }
}

/// Draws the info box onto a planar YUV frame (the processing pipeline).
pub fn draw_overlay_yuv(frame: &mut Yuv420Frame, lines: &[String], cache: &mut OverlayCache, range: ColorRange) {
    let (w, h) = (frame.width(), frame.height());
    let Some(cached) = cached_overlay_tile(cache, lines, w, h) else {
        return;
    };

    let stale = !matches!(&cached.yuv, Some((cached_range, _)) if *cached_range == range);
    if stale {
        cached.yuv = Some((range, YuvTile::from_rgba(&cached.image, range)));
    }
    let (_, tile) = cached.yuv.as_ref().expect("just populated");
    composite_tile_yuv(frame, tile, cached.x, cached.y);
}

/// Frame-relative sizing for the depth graph, on the same factor as
/// `OverlayMetrics` -- its axis labels were a hard-coded 14px and its strokes
/// a single pixel, which is invisible on a 5K frame.
#[derive(PartialEq)]
struct GraphMetrics {
    font: PxScale,
    inset: i32,
    stroke: i32,
}

impl GraphMetrics {
    fn for_frame(height: u32) -> Self {
        let scale = line_height_for(height) as f32 / REFERENCE_LINE_HEIGHT;
        Self {
            font: PxScale::from((14.0 * scale).max(11.0)),
            inset: (4.0 * scale).max(2.0).round() as i32,
            stroke: (scale.round() as i32).max(1),
        }
    }
}

/// `draw_line_segment_mut` draws a single pixel wide. The depth profile runs
/// left to right, so stacking the segment vertically thickens it without the
/// stair-stepping a naive perpendicular offset would give on a near-flat line.
fn draw_thick_line(img: &mut RgbaImage, a: (f32, f32), b: (f32, f32), color: Rgba<u8>, thickness: i32) {
    let half = (thickness - 1) as f32 / 2.0;
    for i in 0..thickness {
        let dy = i as f32 - half;
        draw_line_segment_mut(img, (a.0, a.1 + dy), (b.0, b.1 + dy), color);
    }
}

/// Same idea for the graph's border: nested one-pixel rectangles, inset one
/// step at a time, so it stays visible as the frame grows.
fn draw_thick_hollow_rect(img: &mut RgbaImage, x: i32, y: i32, w: u32, h: u32, color: Rgba<u8>, thickness: i32) {
    for i in 0..thickness {
        let inset = i as u32;
        if w <= inset * 2 || h <= inset * 2 {
            break;
        }
        let rect = Rect::at(x + i, y + i).of_size(w - inset * 2, h - inset * 2);
        draw_hollow_rect_mut(img, rect, color);
    }
}

/// An RGBA tile converted once into the planes and resolutions a 4:2:0
/// composite needs, so the per-frame blend does no colour math at all.
///
/// The info box's tile only re-renders when its text changes -- roughly once
/// a second -- so this conversion is amortized across every frame in between,
/// which is what keeps the drawing stage at a few percent of the runtime.
pub(crate) struct YuvTile {
    width: u32,
    height: u32,
    /// Luma and coverage at full resolution, one entry per tile pixel.
    luma: Vec<u8>,
    alpha: Vec<u8>,
    chroma_w: u32,
    chroma_h: u32,
    /// Chroma and *mean* coverage per 2x2 luma block. The mean matters: a
    /// block half covered by the tile's edge must blend the frame's colour
    /// halfway, or the overlay gets a hard chroma fringe its luma does not have.
    u: Vec<u8>,
    v: Vec<u8>,
    chroma_alpha: Vec<u8>,
}

/// BT.709 luma weights. Only the faint blue tint of the info box's text is
/// chromatic at all -- every other colour in the overlay is a neutral grey,
/// for which every matrix agrees -- so picking 709 here (correct for the HD
/// and UHD footage this tool targets) can at worst shift that one tint
/// imperceptibly on SD sources.
const KR: f32 = 0.2126;
const KG: f32 = 0.7152;
const KB: f32 = 0.0722;

impl YuvTile {
    pub(crate) fn from_rgba(tile: &RgbaImage, range: ColorRange) -> Self {
        let (width, height) = tile.dimensions();
        let (chroma_w, chroma_h) = (width.div_ceil(2), height.div_ceil(2));

        let mut luma = vec![0u8; (width * height) as usize];
        let mut alpha = vec![0u8; (width * height) as usize];
        let mut u = vec![NEUTRAL_CHROMA; (chroma_w * chroma_h) as usize];
        let mut v = vec![NEUTRAL_CHROMA; (chroma_w * chroma_h) as usize];
        let mut chroma_alpha = vec![0u8; (chroma_w * chroma_h) as usize];

        for py in 0..height {
            for px in 0..width {
                let rgba = tile.get_pixel(px, py).0;
                let idx = (py * width + px) as usize;
                luma[idx] = rgb_to_luma(rgba[0], rgba[1], rgba[2], range);
                alpha[idx] = rgba[3];
            }
        }

        // Average each 2x2 block into one chroma sample, weighting colour by
        // coverage so fully transparent pixels contribute no colour.
        for cy in 0..chroma_h {
            for cx in 0..chroma_w {
                let mut sum_u = 0.0f32;
                let mut sum_v = 0.0f32;
                let mut sum_a = 0.0f32;
                let mut count = 0.0f32;
                for dy in 0..2 {
                    for dx in 0..2 {
                        let (px, py) = (cx * 2 + dx, cy * 2 + dy);
                        if px >= width || py >= height {
                            continue;
                        }
                        let rgba = tile.get_pixel(px, py).0;
                        let a = rgba[3] as f32 / 255.0;
                        let (cu, cv) = rgb_to_chroma(rgba[0], rgba[1], rgba[2], range);
                        sum_u += cu * a;
                        sum_v += cv * a;
                        sum_a += a;
                        count += 1.0;
                    }
                }
                let idx = (cy * chroma_w + cx) as usize;
                if sum_a > 0.0 {
                    u[idx] = (sum_u / sum_a).round().clamp(0.0, 255.0) as u8;
                    v[idx] = (sum_v / sum_a).round().clamp(0.0, 255.0) as u8;
                }
                if count > 0.0 {
                    chroma_alpha[idx] = ((sum_a / count) * 255.0).round().clamp(0.0, 255.0) as u8;
                }
            }
        }

        Self {
            width,
            height,
            luma,
            alpha,
            chroma_w,
            chroma_h,
            u,
            v,
            chroma_alpha,
        }
    }
}

/// Converts an sRGB triple to a luma sample in `range`.
fn rgb_to_luma(r: u8, g: u8, b: u8, range: ColorRange) -> u8 {
    let y = KR * r as f32 + KG * g as f32 + KB * b as f32;
    range.luma_for_grey(y.round().clamp(0.0, 255.0) as u8)
}

/// Converts an sRGB triple to `(U, V)` chroma samples centred on
/// `NEUTRAL_CHROMA`. A neutral input returns exactly neutral chroma in both
/// ranges, which is what keeps the grey palette matrix-independent.
fn rgb_to_chroma(r: u8, g: u8, b: u8, range: ColorRange) -> (f32, f32) {
    let (r, g, b) = (r as f32, g as f32, b as f32);
    let y = KR * r + KG * g + KB * b;
    // Chroma excursion: full range uses all 256 codes, limited uses 224.
    let span = match range {
        ColorRange::Full => 255.0,
        ColorRange::Limited => 224.0,
    };
    let u = (b - y) / (2.0 * (1.0 - KB)) * (span / 255.0) + NEUTRAL_CHROMA as f32;
    let v = (r - y) / (2.0 * (1.0 - KR)) * (span / 255.0) + NEUTRAL_CHROMA as f32;
    (u, v)
}

/// Alpha-composites a pre-converted tile into a planar 4:2:0 frame.
///
/// The luma pass is an ordinary per-pixel blend over the full-resolution Y
/// plane; the chroma pass walks the half-resolution grid once, blending U and
/// V together. Both are clipped to the frame, and pixels the tile does not
/// cover are never read or written -- untouched picture stays bit-exact,
/// which the old `yuv420p -> rgb24 -> yuv420p` round-trip could not promise.
pub(crate) fn composite_tile_yuv(frame: &mut Yuv420Frame, tile: &YuvTile, x: i32, y: i32) {
    let (frame_w, frame_h) = (frame.width(), frame.height());
    let (chroma_frame_w, chroma_frame_h) = frame.chroma_dimensions();

    // --- luma ---
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + tile.width as i32).min(frame_w as i32);
    let y1 = (y + tile.height as i32).min(frame_h as i32);
    if x1 > x0 && y1 > y0 {
        let plane = frame.y_plane_mut();
        for py in y0..y1 {
            for px in x0..x1 {
                let t_idx = ((py - y) as u32 * tile.width + (px - x) as u32) as usize;
                let alpha = tile.alpha[t_idx];
                if alpha == 0 {
                    continue;
                }
                let f_idx = py as usize * frame_w as usize + px as usize;
                plane[f_idx] = blend_u8(tile.luma[t_idx], plane[f_idx], alpha);
            }
        }
    }

    // --- chroma, at half resolution on both axes ---
    // `x` and `y` are even (see `align_to_chroma_grid`), so the tile's own
    // 2x2 blocks line up exactly with the frame's chroma grid and the mapping
    // is a plain halving of the offset.
    let cx_off = x / 2;
    let cy_off = y / 2;
    let cx0 = cx_off.max(0);
    let cy0 = cy_off.max(0);
    let cx1 = (cx_off + tile.chroma_w as i32).min(chroma_frame_w as i32);
    let cy1 = (cy_off + tile.chroma_h as i32).min(chroma_frame_h as i32);
    if cx1 <= cx0 || cy1 <= cy0 {
        return;
    }
    let (u_plane, v_plane) = frame.chroma_planes_mut();
    for cy in cy0..cy1 {
        for cx in cx0..cx1 {
            let t_idx = ((cy - cy_off) as u32 * tile.chroma_w + (cx - cx_off) as u32) as usize;
            let alpha = tile.chroma_alpha[t_idx];
            if alpha == 0 {
                continue;
            }
            let f_idx = cy as usize * chroma_frame_w as usize + cx as usize;
            u_plane[f_idx] = blend_u8(tile.u[t_idx], u_plane[f_idx], alpha);
            v_plane[f_idx] = blend_u8(tile.v[t_idx], v_plane[f_idx], alpha);
        }
    }
}

/// `src` over `dst` at `alpha/255`, rounded. Kept in integer arithmetic: this
/// runs a few million times per frame.
fn blend_u8(src: u8, dst: u8, alpha: u8) -> u8 {
    let a = alpha as u32;
    (((src as u32 * a) + (dst as u32 * (255 - a)) + 127) / 255) as u8
}

/// Light grey for the depth curve, which used to be cyan (`[100, 220, 255]`).
///
/// The pipeline composites into 4:2:0 planes, where chroma is stored at half
/// resolution: a saturated line only a few pixels wide has its colour
/// averaged across 2x2 blocks, which makes it bleed and alias in a way the
/// old full-resolution RGB compositing hid. A neutral grey has no chroma to
/// subsample, so the curve stays exactly as crisp as its luma. Greys are also
/// identical under BT.601 and BT.709, so nothing here depends on correctly
/// detecting the footage's colour matrix.
const DEPTH_CURVE_GREY: u8 = 210;

/// Rounds a tile's placement down to an even coordinate.
///
/// 4:2:0 stores one chroma sample per 2x2 luma block, so a tile starting on
/// an odd row or column would straddle those blocks and its chroma would
/// bleed a pixel outside its own edges. Costs at most one pixel of position.
fn align_to_chroma_grid(value: i32) -> i32 {
    value & !1
}

/// Everything the graph's drawing step reads: the tile is a pure function of
/// this, which is what lets it double as the cache key. Unlike the info box --
/// whose text changes about once a second and can be compared as strings --
/// the graph's picture changes whenever a plotted point moves by a pixel, so
/// the key has to be the plotted geometry itself.
#[derive(PartialEq)]
struct GraphPlan {
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    metrics: GraphMetrics,
    /// The polyline in tile-local pixels. Quantizing here rather than at draw
    /// time is what makes the key exact: two frames with the same plan
    /// rasterize identically because they rasterize from the same numbers.
    points: Vec<(i32, i32)>,
    min_label: String,
    max_label: String,
}

/// Works out what the depth-profile graph should look like, or `None` when
/// there is nothing to plot.
///
/// The plot covers the whole dive so far -- elapsed zero to `dive_sec` -- not
/// a trailing window, so the descent stays on screen for the entire dive and
/// the shape the viewer sees is the shape of the dive.
fn plan_graph(samples: &[DiveSample], times: &[f64], dive_sec: f64, frame_w: u32, frame_h: u32) -> Option<GraphPlan> {
    if samples.is_empty() {
        return None;
    }

    let graph_w = (frame_w as f64 * 0.32) as u32;
    let graph_h = (frame_h as f64 * 0.18) as u32;
    if graph_w == 0 || graph_h == 0 {
        return None;
    }
    let x = align_to_chroma_grid((frame_w as f64 * 0.04) as i32);
    let y = align_to_chroma_grid((frame_h as f64 * 0.72) as i32);

    // The plot always spans the whole dive so far: it starts at elapsed zero
    // and its right edge is "now". The box is a fixed fraction of the frame,
    // so the seconds-per-pixel resolution drops as the dive gets longer --
    // that is the intent, a profile that grows rather than a window that
    // slides and loses the descent.
    //
    // The edge is snapped down to a whole second, the rate dive computers
    // actually log at and the rate the info box counts at, so the plot gains
    // its next second exactly when the displayed dive time does. It also
    // makes the plan stable within a second: without it the horizontal scale
    // creeps every frame and, across hundreds of points, a few always cross a
    // rounding boundary -- enough to miss the cache on nearly every frame
    // while changing nothing anyone can see.
    let end_sec = dive_sec.floor().max(1.0);

    let end_idx = times.partition_point(|&t| t <= end_sec);
    let window = &samples[..end_idx];
    if window.is_empty() {
        return None;
    }

    let depths: Vec<f64> = window.iter().filter_map(|s| s.depth_m).collect();
    if depths.is_empty() {
        return None;
    }

    let mut max_depth = depths.iter().cloned().fold(f64::MIN, f64::max);
    let min_depth = depths.iter().cloned().fold(f64::MAX, f64::min);
    if (max_depth - min_depth).abs() < 1e-9 {
        max_depth = min_depth + 1.0;
    }

    let mut points: Vec<(i32, i32)> = Vec::new();
    for sample in window {
        let Some(depth) = sample.depth_m else { continue };
        let t = sample.elapsed_sec;
        if t < 0.0 || t > end_sec {
            continue;
        }
        let tx = t / end_sec;
        let ty = (depth - min_depth) / (max_depth - min_depth);
        // Tile-local coordinates: the frame offset is applied by the composite.
        let px = tx * (graph_w as f64 - 2.0) + 1.0;
        let py = ty * (graph_h as f64 - 2.0) + 1.0;
        let point = (px.round() as i32, py.round() as i32);
        // A whole-dive plot puts thousands of samples into a box a few hundred
        // pixels wide, so long runs of samples land on one pixel. Dropping the
        // repeats bounds the segment count by the box size instead of the dive
        // length, and it is also what keeps the plan stable frame to frame:
        // late in a dive another 1/30 s moves no point, so the cache holds.
        if points.last() == Some(&point) {
            continue;
        }
        points.push(point);
    }

    Some(GraphPlan {
        width: graph_w,
        height: graph_h,
        x,
        y,
        metrics: GraphMetrics::for_frame(frame_h),
        points,
        // min_depth (shallowest) plots at the top of the box, max_depth
        // (deepest) at the bottom.
        min_label: format!("{min_depth:.1}m"),
        max_label: format!("{max_depth:.1}m"),
    })
}

/// Renders a plan into its own tile.
///
/// The graph used to draw straight onto the frame. Rendering to a tile means
/// all the per-pixel work happens on a buffer a few percent of the frame's
/// size (better cache locality), and -- more importantly -- it leaves
/// compositing as the single operation that has to know the frame's pixel
/// format.
fn render_graph_tile(plan: &GraphPlan) -> RgbaImage {
    // The translucent background is baked into the tile's alpha channel
    // instead of being blended against the frame up front, so the frame is
    // read exactly once, during the composite.
    let bg_alpha = (0.35_f32 * 255.0).round() as u8;
    let mut tile = RgbaImage::from_pixel(plan.width, plan.height, Rgba([10, 10, 10, bg_alpha]));

    draw_thick_hollow_rect(
        &mut tile,
        0,
        0,
        plan.width,
        plan.height,
        Rgba([90, 90, 90, 255]),
        plan.metrics.stroke,
    );

    for pair in plan.points.windows(2) {
        draw_thick_line(
            &mut tile,
            (pair[0].0 as f32, pair[0].1 as f32),
            (pair[1].0 as f32, pair[1].1 as f32),
            Rgba([DEPTH_CURVE_GREY, DEPTH_CURVE_GREY, DEPTH_CURVE_GREY, 255]),
            plan.metrics.stroke,
        );
    }

    let axis_scale = plan.metrics.font;
    let axis_font = font();
    let label_inset = plan.metrics.inset;
    let (_, min_label_h) = text_size(axis_scale, axis_font, &plan.min_label);
    draw_text_mut(
        &mut tile,
        Rgba([200, 200, 200, 255]),
        label_inset,
        label_inset / 2,
        axis_scale,
        axis_font,
        &plan.min_label,
    );
    draw_text_mut(
        &mut tile,
        Rgba([200, 200, 200, 255]),
        label_inset,
        plan.height as i32 - min_label_h as i32 - label_inset / 2,
        axis_scale,
        axis_font,
        &plan.max_label,
    );

    tile
}

/// Returns the cached graph tile, re-rendering it only when the plan changed.
///
/// The plan only changes when the plot reaches a new whole second, so at
/// 30 fps 29 frames out of 30 pay a plan comparison and a blend instead of a
/// re-render and a YUV conversion -- the same bargain `cached_overlay_tile`
/// makes for the info box, whose text turns over on the same cadence.
fn cached_graph_tile<'a>(
    cache: &'a mut OverlayCache,
    samples: &[DiveSample],
    times: &[f64],
    dive_sec: f64,
    w: u32,
    h: u32,
) -> Option<&'a mut CachedGraph> {
    let plan = plan_graph(samples, times, dive_sec, w, h)?;

    let needs_render = !matches!(&cache.graph, Some(cached) if cached.plan == plan);
    if needs_render {
        let image = render_graph_tile(&plan);
        cache.graph = Some(CachedGraph { plan, image, yuv: None });
    }
    cache.graph.as_mut()
}

/// Draws the depth profile onto a packed-RGB frame (the GUI's sync preview).
pub fn draw_depth_graph(
    img: &mut RgbImage,
    samples: &[DiveSample],
    times: &[f64],
    dive_sec: f64,
    cache: &mut OverlayCache,
) {
    let (w, h) = img.dimensions();
    if let Some(cached) = cached_graph_tile(cache, samples, times, dive_sec, w, h) {
        let (x, y) = (cached.plan.x, cached.plan.y);
        composite_tile(img, &cached.image, x, y);
    }
}

/// Draws the depth profile onto a planar YUV frame (the processing pipeline).
pub fn draw_depth_graph_yuv(
    frame: &mut Yuv420Frame,
    samples: &[DiveSample],
    times: &[f64],
    dive_sec: f64,
    range: ColorRange,
    cache: &mut OverlayCache,
) {
    let (w, h) = (frame.width(), frame.height());
    let Some(cached) = cached_graph_tile(cache, samples, times, dive_sec, w, h) else {
        return;
    };

    let stale = !matches!(&cached.yuv, Some((cached_range, _)) if *cached_range == range);
    if stale {
        cached.yuv = Some((range, YuvTile::from_rgba(&cached.image, range)));
    }
    let (_, tile) = cached.yuv.as_ref().expect("just populated");
    composite_tile_yuv(frame, tile, cached.plan.x, cached.plan.y);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a flat-grey YUV frame and the equivalent RGB frame, so the two
    /// composite paths start from identical pictures.
    fn matching_frames(w: u32, h: u32, grey: u8, range: ColorRange) -> (RgbImage, Yuv420Frame) {
        let rgb = RgbImage::from_pixel(w, h, Rgb([grey, grey, grey]));
        let mut data = vec![range.luma_for_grey(grey); (w * h) as usize];
        data.resize(Yuv420Frame::buffer_size(w, h), NEUTRAL_CHROMA);
        let yuv = Yuv420Frame::from_raw(w, h, data).expect("even dimensions");
        (rgb, yuv)
    }

    /// The load-bearing test for the planar-YUV pipeline: compositing the
    /// same tile through the RGB path and the YUV path must land on the same
    /// picture. A wrong colour matrix, a mis-set colour range, an off-by-one
    /// in the chroma grid or a bad blend all show up here as a divergence,
    /// where in production they would only surface as a subtly wrong-looking
    /// overlay in the encoded file.
    ///
    /// Compared on luma, at full resolution and pixel for pixel. Chroma is
    /// deliberately excluded: 4:2:0 stores one sample per 2x2 block, so at a
    /// glyph edge the reconstructed *colour* legitimately differs from the
    /// RGB reference by several levels. That loss is inherent to the format,
    /// not a defect in this code, and asserting on it would only pin the
    /// artefact in place. The chroma planes are covered instead by
    /// `grey_palette_leaves_chroma_neutral` and
    /// `chroma_planes_are_not_swapped`.
    #[test]
    fn yuv_and_rgb_composites_agree_on_luma() {
        for range in [ColorRange::Limited, ColorRange::Full] {
            for grey in [0u8, 64, 128, 200, 255] {
                let (mut rgb, mut yuv) = matching_frames(64, 64, grey, range);
                let lines = vec!["Depth: 12.3 m".to_string(), "Dive time: 01:23".to_string()];

                draw_overlay(&mut rgb, &lines, &mut OverlayCache::new());
                draw_overlay_yuv(&mut yuv, &lines, &mut OverlayCache::new(), range);

                let raw = yuv.as_raw();
                let mut worst = 0i32;
                for py in 0..64u32 {
                    for px in 0..64u32 {
                        let got = raw[(py * 64 + px) as usize] as i32;
                        let reference = rgb.get_pixel(px, py).0;
                        let want = rgb_to_luma(reference[0], reference[1], reference[2], range) as i32;
                        worst = worst.max((got - want).abs());
                    }
                }
                assert!(
                    worst <= 2,
                    "luma diverged by {worst} for {range:?} on grey {grey} (rounding allows 2)"
                );
            }
        }
    }

    /// Guards the one mistake an all-grey palette cannot catch by itself:
    /// with neutral colours, swapping the U and V planes changes nothing, so
    /// the swap would sit undetected until someone added a coloured element.
    /// Red must push V above neutral and U below it; blue the reverse.
    #[test]
    fn chroma_planes_are_not_swapped() {
        let range = ColorRange::Full;
        for (color, expect_u_above, expect_v_above) in [([255u8, 0, 0], false, true), ([0, 0, 255], true, false)] {
            let tile = RgbaImage::from_pixel(16, 16, Rgba([color[0], color[1], color[2], 255]));
            let (_, mut yuv) = matching_frames(64, 64, 128, range);
            composite_tile_yuv(&mut yuv, &YuvTile::from_rgba(&tile, range), 8, 8);

            let (cw, _) = yuv.chroma_dimensions();
            let luma_len = 64usize * 64;
            let chroma_len = (cw * cw) as usize;
            // Sample well inside the patch, away from any edge block.
            let c_idx = (8usize * cw as usize) + 8;
            let u = yuv.as_raw()[luma_len + c_idx];
            let v = yuv.as_raw()[luma_len + chroma_len + c_idx];

            assert_eq!(
                u > NEUTRAL_CHROMA,
                expect_u_above,
                "U plane wrong for {color:?} (got {u})"
            );
            assert_eq!(
                v > NEUTRAL_CHROMA,
                expect_v_above,
                "V plane wrong for {color:?} (got {v})"
            );
        }
    }

    /// Pixels the overlay does not cover must come back bit-identical. The
    /// old rgb24 round-trip could not promise this -- it resampled the chroma
    /// of every pixel in the frame, every frame -- and it is the quality half
    /// of the reason for moving the pipe to planar YUV.
    #[test]
    fn yuv_composite_leaves_uncovered_pixels_untouched() {
        let range = ColorRange::Limited;
        let (_, mut yuv) = matching_frames(320, 240, 100, range);
        let before = yuv.as_raw().to_vec();

        let lines = vec!["Depth: 1.0 m".to_string()];
        draw_overlay_yuv(&mut yuv, &lines, &mut OverlayCache::new(), range);
        let after = yuv.as_raw().to_vec();

        // The tile sits in the top-left; the bottom half of the luma plane is
        // nowhere near it and must be byte-for-byte unchanged.
        let luma_len = 320usize * 240;
        let bottom_half = luma_len / 2..luma_len;
        assert_eq!(
            before[bottom_half.clone()],
            after[bottom_half],
            "the composite touched pixels outside the overlay"
        );
        assert_ne!(before, after, "the overlay should have changed something");
    }

    /// A grey overlay must not tint the picture: every colour in the palette
    /// is neutral precisely so the chroma planes stay at 128 and the result
    /// is independent of the footage's BT.601/BT.709 matrix.
    #[test]
    fn grey_palette_leaves_chroma_neutral() {
        let range = ColorRange::Full;
        let (_, mut yuv) = matching_frames(320, 240, 128, range);
        let samples = vec![sample(0.0, 1.0), sample(10.0, 5.0), sample(20.0, 3.0)];
        let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();

        draw_depth_graph_yuv(&mut yuv, &samples, &times, 15.0, range, &mut OverlayCache::new());

        let (cw, ch) = yuv.chroma_dimensions();
        let raw = yuv.as_raw();
        let luma_len = 320usize * 240;
        let chroma_len = (cw * ch) as usize;
        for i in 0..chroma_len {
            assert_eq!(raw[luma_len + i], NEUTRAL_CHROMA, "U drifted at {i}");
            assert_eq!(raw[luma_len + chroma_len + i], NEUTRAL_CHROMA, "V drifted at {i}");
        }
    }

    /// White text has to hit the right code depending on the range, or it
    /// reads as clipped super-white on limited-range footage and dull grey on
    /// full-range footage.
    #[test]
    fn overlay_luma_respects_the_footage_range() {
        let lines = vec!["Depth: 9.9 m".to_string()];
        let brightest = |range: ColorRange| {
            let (_, mut yuv) = matching_frames(320, 240, 0, range);
            draw_overlay_yuv(&mut yuv, &lines, &mut OverlayCache::new(), range);
            let luma_len = 320usize * 240;
            *yuv.as_raw()[..luma_len].iter().max().expect("non-empty")
        };
        let limited = brightest(ColorRange::Limited);
        let full = brightest(ColorRange::Full);
        assert!(limited <= 235, "limited-range text hit illegal super-white: {limited}");
        assert!(limited >= 200, "limited-range text came out too dark: {limited}");
        assert!(
            full > limited,
            "full-range text should be brighter: {full} vs {limited}"
        );
    }

    /// The tile must start on an even row and column, or its chroma straddles
    /// the frame's 2x2 blocks and bleeds a pixel outside its own edges.
    #[test]
    fn overlay_origin_lands_on_the_chroma_grid() {
        for (w, h) in [(1920, 1080), (5312, 2988), (3840, 2160), (641, 481), (320, 240)] {
            let (x, y) = overlay_origin(w, h);
            assert_eq!(x % 2, 0, "odd x for {w}x{h}");
            assert_eq!(y % 2, 0, "odd y for {w}x{h}");
        }
    }

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
    fn build_overlay_lines_falls_back_to_no_data() {
        let lines = build_overlay_lines(&[Field::Depth], &[], &[], 5.0, false);
        assert_eq!(lines, vec!["No data".to_string()]);
    }

    #[test]
    fn build_overlay_lines_includes_time_and_depth() {
        let samples = vec![sample(0.0, 1.5)];
        let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();
        let lines = build_overlay_lines(&[Field::Time, Field::Depth], &samples, &times, 10.0, false);
        assert_eq!(lines[0], "Dive time: 00:10");
        assert_eq!(lines[1], "Depth: 1.5 m");
    }

    #[test]
    fn build_overlay_lines_carries_forward_sparse_temperature() {
        let mut with_temp = sample(0.0, 1.0);
        with_temp.temp_c = Some(18.0);
        let samples = vec![with_temp, sample(10.0, 2.0), sample(20.0, 3.0)];
        let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();

        let lines = build_overlay_lines(&[Field::Temp], &samples, &times, 20.0, false);
        assert_eq!(lines, vec!["Temp: 18.0 C".to_string()]);
    }

    #[test]
    fn build_overlay_lines_interpolates_between_samples() {
        let samples = vec![sample(0.0, 1.0), sample(10.0, 2.0)];
        let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();

        let lines = build_overlay_lines(&[Field::Depth], &samples, &times, 5.0, true);
        assert_eq!(lines, vec!["Depth: 1.5 m".to_string()]);
    }

    #[test]
    fn build_overlay_lines_interpolation_skips_sparse_gaps_and_carries_last_value() {
        let mut with_temp = sample(0.0, 1.0);
        with_temp.temp_c = Some(10.0);
        let samples = vec![with_temp, sample(10.0, 2.0), sample(20.0, 3.0)];
        let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();

        // No later temperature reading to interpolate towards, so this
        // still carries the last known value forward instead of extrapolating.
        let lines = build_overlay_lines(&[Field::Temp], &samples, &times, 20.0, true);
        assert_eq!(lines, vec!["Temp: 10.0 C".to_string()]);
    }

    #[test]
    fn cubic_hermite_matches_linear_when_tangents_equal_secant() {
        // Straight line through all four points: both endpoint tangents
        // equal the segment secant, so the cubic must reduce to the linear
        // value exactly, same as it does with no prev/next at all.
        let value = cubic_hermite(15.0, Some((0.0, 0.0)), (10.0, 1.0), (20.0, 2.0), Some((30.0, 3.0)));
        assert!((value - 1.5).abs() < 1e-9, "expected 1.5, got {value}");
    }

    #[test]
    fn cubic_hermite_curves_toward_neighbor_slope() {
        // p0's incoming slope (0..20, via prev) is shallower than the
        // segment's own secant (0..10 -> 10), so the curve should bow below
        // the segment's linear midpoint (5.0) rather than sitting on it.
        let value = cubic_hermite(15.0, Some((0.0, 0.0)), (10.0, 0.0), (20.0, 10.0), None);
        assert!(
            value < 5.0,
            "expected the fitted curve to diverge from the linear midpoint, got {value}"
        );
    }

    #[test]
    fn build_overlay_lines_interpolation_curves_through_neighbors() {
        // No sample after 20.0, so p1's tangent falls back to the segment
        // secant (1.0/sec) while p0's tangent is pulled shallower by the
        // flatter 0..20 run leading into it -- the cubic fit lands well
        // below the segment's own linear midpoint (5.0).
        let samples = vec![sample(0.0, 0.0), sample(10.0, 0.0), sample(20.0, 10.0)];
        let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();

        let lines = build_overlay_lines(&[Field::Depth], &samples, &times, 15.0, true);
        assert_eq!(lines, vec!["Depth: 4.4 m".to_string()]);
    }

    #[test]
    fn draw_overlay_does_not_panic_on_small_image() {
        let mut img = RgbImage::new(320, 240);
        let mut cache = OverlayCache::new();
        draw_overlay(&mut img, &["Dive time: 00:10".to_string()], &mut cache);
    }

    #[test]
    fn draw_overlay_reuses_cached_tile_for_identical_lines_and_rerenders_on_change() {
        let mut img = RgbImage::new(320, 240);
        let mut cache = OverlayCache::new();
        let lines = vec!["Dive time: 00:10".to_string(), "Depth: 1.5 m".to_string()];

        draw_overlay(&mut img, &lines, &mut cache);
        let tile_ptr_before = cache.tile.as_ref().unwrap().image.as_raw().as_ptr();
        draw_overlay(&mut img, &lines, &mut cache);
        let tile_ptr_after = cache.tile.as_ref().unwrap().image.as_raw().as_ptr();
        assert_eq!(
            tile_ptr_before, tile_ptr_after,
            "unchanged lines must reuse the cached tile"
        );

        let other_lines = vec!["Dive time: 00:20".to_string(), "Depth: 2.0 m".to_string()];
        draw_overlay(&mut img, &other_lines, &mut cache);
        assert_eq!(cache.tile.as_ref().unwrap().lines, other_lines);
    }

    #[test]
    fn draw_depth_graph_does_not_panic() {
        let mut img = RgbImage::new(320, 240);
        let samples = vec![sample(0.0, 1.0), sample(5.0, 3.0), sample(10.0, 2.0)];
        let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();
        draw_depth_graph(&mut img, &samples, &times, 10.0, &mut OverlayCache::new());
    }

    /// The plot spans the whole dive so far, so a sample from the first
    /// minute still shapes the picture an hour in -- the old trailing
    /// 600 s window had scrolled the descent off the left edge by then.
    #[test]
    fn depth_graph_spans_the_whole_dive_not_a_trailing_window() {
        let tail: Vec<DiveSample> = (0..60).map(|i| sample(600.0 + i as f64 * 50.0, 5.0)).collect();

        let mut deep = vec![sample(30.0, 40.0)];
        deep.extend(tail.iter().cloned());
        let mut shallow = vec![sample(30.0, 5.5)];
        shallow.extend(tail.iter().cloned());

        let times: Vec<f64> = deep.iter().map(|s| s.elapsed_sec).collect();
        let with_spike = render_graph_tile(&plan_graph(&deep, &times, 3600.0, 320, 240).unwrap());
        let without_spike = render_graph_tile(&plan_graph(&shallow, &times, 3600.0, 320, 240).unwrap());

        assert_ne!(
            with_spike.as_raw(),
            without_spike.as_raw(),
            "a sample 30 s into the dive must still affect the plot at 60 minutes"
        );
    }

    /// The graph is the expensive tile to build, and late in a dive its
    /// picture is identical for many frames in a row, so the cache has to
    /// survive the frame-to-frame advance in `dive_sec` and still notice a
    /// step big enough to move the plot.
    #[test]
    fn depth_graph_reuses_its_tile_until_the_plotted_picture_changes() {
        let samples: Vec<DiveSample> = (0..2400)
            .map(|i| sample(i as f64, 10.0 + (i as f64 / 100.0).sin() * 5.0))
            .collect();
        let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();
        let mut img = RgbImage::new(1920, 1080);
        let mut cache = OverlayCache::new();

        draw_depth_graph(&mut img, &samples, &times, 2000.0, &mut cache);
        let first = cache.graph.as_ref().unwrap().image.as_raw().as_ptr();

        // One frame later at 30 fps: still the same whole second, so the plan
        // is identical and nothing may be re-rendered.
        draw_depth_graph(&mut img, &samples, &times, 2000.0 + 1.0 / 30.0, &mut cache);
        let next_frame = cache.graph.as_ref().unwrap().image.as_raw().as_ptr();
        assert_eq!(first, next_frame, "an unchanged plot must reuse the cached tile");

        // Far enough along that new samples have entered the plot.
        draw_depth_graph(&mut img, &samples, &times, 2300.0, &mut cache);
        let later = cache.graph.as_ref().unwrap().image.as_raw().as_ptr();
        assert_ne!(first, later, "a changed plot must re-render");
    }

    /// The YUV conversion is the other half of the graph's per-frame cost,
    /// and it is only worth caching the tile if the conversion rides along.
    #[test]
    fn depth_graph_reuses_its_yuv_conversion_with_the_tile() {
        let range = ColorRange::Full;
        let samples: Vec<DiveSample> = (0..2400).map(|i| sample(i as f64, 10.0)).collect();
        let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();
        let (_, mut yuv) = matching_frames(320, 240, 128, range);
        let mut cache = OverlayCache::new();

        draw_depth_graph_yuv(&mut yuv, &samples, &times, 2000.0, range, &mut cache);
        let first = cache.graph.as_ref().unwrap().yuv.as_ref().unwrap().1.luma.as_ptr();

        draw_depth_graph_yuv(&mut yuv, &samples, &times, 2000.0 + 1.0 / 30.0, range, &mut cache);
        let again = cache.graph.as_ref().unwrap().yuv.as_ref().unwrap().1.luma.as_ptr();
        assert_eq!(first, again, "an unchanged tile must not be reconverted");

        // A different colour range must not reuse the old conversion.
        draw_depth_graph_yuv(&mut yuv, &samples, &times, 2000.0, ColorRange::Limited, &mut cache);
        let converted = cache.graph.as_ref().unwrap().yuv.as_ref().unwrap();
        assert_eq!(converted.0, ColorRange::Limited);
    }

    /// 1080p is the resolution the overlay's sizes were hand-tuned at, so
    /// making them frame-relative must leave it exactly as it was.
    #[test]
    fn metrics_at_1080p_match_the_hand_tuned_originals() {
        let metrics = OverlayMetrics::for_frame(1080);
        assert_eq!(metrics.line_height, 48);
        assert_eq!(metrics.font.x, 22.0);
        assert_eq!(metrics.padding, 14);

        let graph = GraphMetrics::for_frame(1080);
        assert_eq!(graph.font.x, 14.0);
        assert_eq!(graph.inset, 4);
        assert_eq!(graph.stroke, 1);
    }

    /// The regression: the font used to be a hard-coded 22px at every
    /// resolution, so a 5.3K frame got a 134px-tall line with 22px glyphs in
    /// it. Text must grow with the box, keeping the ratio it has at 1080p.
    #[test]
    fn text_scales_with_the_frame_instead_of_staying_22px() {
        let reference = OverlayMetrics::for_frame(1080);
        let gopro_5k = OverlayMetrics::for_frame(2988);

        let box_growth = gopro_5k.line_height as f32 / reference.line_height as f32;
        let font_growth = gopro_5k.font.x / reference.font.x;
        assert!(box_growth > 2.5, "fixture should be a much taller frame: {box_growth}");
        assert!(
            (font_growth - box_growth).abs() < 0.01,
            "font grew {font_growth}x while the box grew {box_growth}x -- they must stay in step"
        );
        assert!(gopro_5k.padding > reference.padding);

        // The graph's labels and strokes ride the same factor.
        let graph = GraphMetrics::for_frame(2988);
        assert!((graph.font.x / GraphMetrics::for_frame(1080).font.x - box_growth).abs() < 0.01);
        assert!(graph.stroke >= 3, "a 1px stroke is invisible at 5K: {}", graph.stroke);
    }

    /// Below roughly 800px tall, strict proportionality would shrink the text
    /// past readable, so both fonts hold at a floor -- the same trade the
    /// line height already made with `MIN_LINE_HEIGHT`.
    #[test]
    fn text_stops_shrinking_on_very_small_frames() {
        let tiny = OverlayMetrics::for_frame(120);
        assert_eq!(tiny.line_height, MIN_LINE_HEIGHT);
        assert_eq!(tiny.font.x, 16.0);
        assert!(tiny.padding >= 6);
        assert!(GraphMetrics::for_frame(120).font.x >= 11.0);
    }

    /// The cached tile is keyed on the frame's line height too: a tile
    /// rendered for one resolution has the wrong font baked into it for
    /// another.
    #[test]
    fn cached_tile_is_rerendered_when_the_frame_size_changes() {
        let lines = vec!["Depth: 1.5 m".to_string()];
        let mut cache = OverlayCache::new();

        let mut small = RgbImage::new(320, 240);
        draw_overlay(&mut small, &lines, &mut cache);
        let small_tile = cache.tile.as_ref().unwrap().image.dimensions();

        let mut large = RgbImage::new(5312, 2988);
        draw_overlay(&mut large, &lines, &mut cache);
        let large_tile = cache.tile.as_ref().unwrap().image.dimensions();

        assert!(
            large_tile.0 > small_tile.0 && large_tile.1 > small_tile.1,
            "tile was not re-rendered for the larger frame: {small_tile:?} -> {large_tile:?}"
        );
    }
}
