use std::sync::OnceLock;

use ab_glyph::{FontRef, PxScale};
use image::{Rgb, RgbImage, Rgba, RgbaImage};
use imageproc::drawing::{draw_hollow_rect_mut, draw_line_segment_mut, draw_text_mut, text_size};
use imageproc::rect::Rect;

use crate::csv_data::format_duration;
use crate::lookup::choose_sample_index;
use crate::model::{field_raw_value, format_field_value, value_for_field, DiveSample, Field};

const FONT_BYTES: &[u8] = include_bytes!("../assets/fonts/DejaVuSans.ttf");

static FONT: OnceLock<FontRef<'static>> = OnceLock::new();

/// Parses the bundled font once and caches it -- `draw_overlay`/`draw_depth_graph`
/// call this every frame, and re-parsing the same static bytes on every call
/// was pure repeated work.
pub fn font() -> &'static FontRef<'static> {
    FONT.get_or_init(|| FontRef::try_from_slice(FONT_BYTES).expect("bundled DejaVuSans.ttf must be a valid font"))
}

/// Alpha-blends a solid color into a rectangular region of `img`, clipped to
/// image bounds. `imageproc::draw_filled_rect_mut` is opaque-only, so this
/// replaces `cv2.addWeighted` for the translucent info boxes.
fn blend_rect_alpha(img: &mut RgbImage, x: i32, y: i32, w: u32, h: u32, color: Rgb<u8>, alpha: f32) {
    let (img_w, img_h) = img.dimensions();
    let x0 = x.max(0) as u32;
    let y0 = y.max(0) as u32;
    let x1 = (x.saturating_add(w as i32).max(0) as u32).min(img_w);
    let y1 = (y.saturating_add(h as i32).max(0) as u32).min(img_h);

    for py in y0..y1 {
        for px in x0..x1 {
            let orig = img.get_pixel(px, py).0;
            let mut blended = [0u8; 3];
            for c in 0..3 {
                let v = alpha * color.0[c] as f32 + (1.0 - alpha) * orig[c] as f32;
                blended[c] = v.round().clamp(0.0, 255.0) as u8;
            }
            img.put_pixel(px, py, Rgb(blended));
        }
    }
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
fn interpolated_value(samples: &[DiveSample], times: &[f64], dive_sec: f64, idx: usize, field: Field) -> Option<String> {
    let before_idx = (0..=idx).rev().find(|&j| field_raw_value(&samples[j], field).is_some())?;
    let before = (times[before_idx], field_raw_value(&samples[before_idx], field).unwrap());
    let after_idx = (idx + 1..times.len()).find(|&j| field_raw_value(&samples[j], field).is_some());

    let value = match after_idx {
        Some(after_idx) if times[after_idx] > before.0 => {
            let after = (times[after_idx], field_raw_value(&samples[after_idx], field).unwrap());
            let prev = (0..before_idx).rev().find_map(|j| field_raw_value(&samples[j], field).map(|v| (times[j], v)));
            let next = (after_idx + 1..times.len()).find_map(|j| field_raw_value(&samples[j], field).map(|v| (times[j], v)));
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
/// (alpha baked in, matching the 0.45 opacity `blend_rect_alpha` used to
/// apply directly) with opaque text drawn on top. The tile carries its own
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
        draw_text_mut(&mut tile, Rgba([230, 245, 255, 255]), padding, text_y, scale, font, line);
        text_y += metrics.line_height;
    }

    CachedTile {
        lines: lines.to_vec(),
        x,
        y,
        line_height: metrics.line_height,
        image: tile,
    }
}

/// Alpha-composites `tile` (its own per-pixel alpha) onto `img` at `(x, y)`,
/// clipped to image bounds -- the per-frame counterpart to `render_tile`,
/// doing only the blend math with no font/text work.
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

pub fn draw_overlay(img: &mut RgbImage, lines: &[String], cache: &mut OverlayCache) {
    let (w, h) = img.dimensions();
    let x = (w as f64 * 0.04) as i32;
    let y = (h as f64 * 0.06) as i32;
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

    if let Some(cached) = &cache.tile {
        composite_tile(img, &cached.image, cached.x, cached.y);
    }
}

/// Frame-relative sizing for the depth graph, on the same factor as
/// `OverlayMetrics` -- its axis labels were a hard-coded 14px and its strokes
/// a single pixel, which is invisible on a 5K frame.
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
fn draw_thick_line(img: &mut RgbImage, a: (f32, f32), b: (f32, f32), color: Rgb<u8>, thickness: i32) {
    let half = (thickness - 1) as f32 / 2.0;
    for i in 0..thickness {
        let dy = i as f32 - half;
        draw_line_segment_mut(img, (a.0, a.1 + dy), (b.0, b.1 + dy), color);
    }
}

/// Same idea for the graph's border: nested one-pixel rectangles, inset one
/// step at a time, so it stays visible as the frame grows.
fn draw_thick_hollow_rect(img: &mut RgbImage, x: i32, y: i32, w: u32, h: u32, color: Rgb<u8>, thickness: i32) {
    for i in 0..thickness {
        let inset = i as u32;
        if w <= inset * 2 || h <= inset * 2 {
            break;
        }
        let rect = Rect::at(x + i, y + i).of_size(w - inset * 2, h - inset * 2);
        draw_hollow_rect_mut(img, rect, color);
    }
}

pub fn draw_depth_graph(img: &mut RgbImage, samples: &[DiveSample], times: &[f64], dive_sec: f64, window_sec: f64) {
    if samples.is_empty() {
        return;
    }

    let (w, h) = img.dimensions();
    let graph_w = (w as f64 * 0.32) as u32;
    let graph_h = (h as f64 * 0.18) as u32;
    let x = (w as f64 * 0.04) as i32;
    let y = (h as f64 * 0.72) as i32;

    let start_sec = (dive_sec - window_sec).max(0.0);
    let end_sec = (start_sec + 1.0).max(dive_sec);

    let start_idx = times.partition_point(|&t| t < start_sec);
    let end_idx = times.partition_point(|&t| t <= end_sec);
    let window = &samples[start_idx..end_idx];
    if window.is_empty() {
        return;
    }

    let depths: Vec<f64> = window.iter().filter_map(|s| s.depth_m).collect();
    if depths.is_empty() {
        return;
    }

    let mut max_depth = depths.iter().cloned().fold(f64::MIN, f64::max);
    let min_depth = depths.iter().cloned().fold(f64::MAX, f64::min);
    if (max_depth - min_depth).abs() < 1e-9 {
        max_depth = min_depth + 1.0;
    }

    let metrics = GraphMetrics::for_frame(h);

    blend_rect_alpha(img, x, y, graph_w, graph_h, Rgb([10, 10, 10]), 0.35);
    if graph_w > 0 && graph_h > 0 {
        draw_thick_hollow_rect(img, x, y, graph_w, graph_h, Rgb([90, 90, 90]), metrics.stroke);
    }

    let mut points: Vec<(f32, f32)> = Vec::new();
    for sample in window {
        let Some(depth) = sample.depth_m else { continue };
        let t = sample.elapsed_sec;
        if t < start_sec || t > end_sec {
            continue;
        }
        let tx = (t - start_sec) / (end_sec - start_sec);
        let ty = (depth - min_depth) / (max_depth - min_depth);
        let px = x as f64 + tx * (graph_w as f64 - 2.0) + 1.0;
        let py = y as f64 + ty * (graph_h as f64 - 2.0) + 1.0;
        points.push((px as f32, py as f32));
    }

    for pair in points.windows(2) {
        draw_thick_line(img, pair[0], pair[1], Rgb([100, 220, 255]), metrics.stroke);
    }

    let axis_scale = metrics.font;
    let axis_font = font();
    let label_inset = metrics.inset;
    let max_label = format!("{max_depth:.1}m");
    let min_label = format!("{min_depth:.1}m");
    let (_, min_label_h) = text_size(axis_scale, axis_font, &min_label);
    // min_depth (shallowest) plots at the top of the box, max_depth (deepest) at the bottom.
    draw_text_mut(
        img,
        Rgb([200, 200, 200]),
        x + label_inset,
        y + label_inset / 2,
        axis_scale,
        axis_font,
        &min_label,
    );
    draw_text_mut(
        img,
        Rgb([200, 200, 200]),
        x + label_inset,
        y + graph_h as i32 - min_label_h as i32 - label_inset / 2,
        axis_scale,
        axis_font,
        &max_label,
    );
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
        assert!(value < 5.0, "expected the fitted curve to diverge from the linear midpoint, got {value}");
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
        assert_eq!(tile_ptr_before, tile_ptr_after, "unchanged lines must reuse the cached tile");

        let other_lines = vec!["Dive time: 00:20".to_string(), "Depth: 2.0 m".to_string()];
        draw_overlay(&mut img, &other_lines, &mut cache);
        assert_eq!(cache.tile.as_ref().unwrap().lines, other_lines);
    }

    #[test]
    fn draw_depth_graph_does_not_panic() {
        let mut img = RgbImage::new(320, 240);
        let samples = vec![sample(0.0, 1.0), sample(5.0, 3.0), sample(10.0, 2.0)];
        let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();
        draw_depth_graph(&mut img, &samples, &times, 10.0, 600.0);
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
