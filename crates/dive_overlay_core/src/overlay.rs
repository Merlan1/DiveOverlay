use ab_glyph::{FontRef, PxScale};
use image::{Rgb, RgbImage};
use imageproc::drawing::{draw_hollow_rect_mut, draw_line_segment_mut, draw_text_mut, text_size};
use imageproc::rect::Rect;

use crate::csv_data::format_duration;
use crate::lookup::choose_sample_index;
use crate::model::{value_for_field, DiveSample, Field};

const FONT_BYTES: &[u8] = include_bytes!("../assets/fonts/DejaVuSans.ttf");

pub fn font() -> FontRef<'static> {
    FontRef::try_from_slice(FONT_BYTES).expect("bundled DejaVuSans.ttf must be a valid font")
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
/// time (if requested) plus the latest known value for every other
/// requested field, falling back to "Keine Daten" if nothing is available
/// yet. Centralizing this (the original duplicated it between the CLI's
/// frame loop and the GUI's preview code) keeps CLI/GUI rendering identical.
pub fn build_overlay_lines(fields: &[Field], samples: &[DiveSample], times: &[f64], dive_sec: f64) -> Vec<String> {
    let mut lines = Vec::new();
    if fields.contains(&Field::Time) {
        lines.push(format!("Tauchzeit: {}", format_duration(dive_sec)));
    }

    if let Some(idx) = choose_sample_index(times, dive_sec) {
        for &field in fields {
            if field == Field::Time {
                continue;
            }
            if let Some(value) = last_known_value(&samples[..=idx], field) {
                lines.push(value);
            }
        }
    }

    if lines.is_empty() {
        lines.push("Keine Daten".to_string());
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

pub fn draw_overlay(img: &mut RgbImage, lines: &[String]) {
    let (w, h) = img.dimensions();
    let x = (w as f64 * 0.04) as i32;
    let y = (h as f64 * 0.06) as i32;
    let line_height = ((h as f64 * 0.045) as i32).max(24);
    let padding: i32 = 14;
    let scale = PxScale::from(22.0);
    let font = font();

    let box_w = lines
        .iter()
        .map(|line| text_size(scale, &font, line).0 as i32)
        .max()
        .unwrap_or(0)
        + padding * 2;
    let box_h = line_height * lines.len() as i32 + padding;

    blend_rect_alpha(img, x, y, box_w.max(0) as u32, box_h.max(0) as u32, Rgb([20, 20, 20]), 0.45);

    let mut text_y = y + padding / 2;
    for line in lines {
        draw_text_mut(img, Rgb([230, 245, 255]), x + padding, text_y, scale, &font, line);
        text_y += line_height;
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

    blend_rect_alpha(img, x, y, graph_w, graph_h, Rgb([10, 10, 10]), 0.35);
    if graph_w > 0 && graph_h > 0 {
        draw_hollow_rect_mut(img, Rect::at(x, y).of_size(graph_w, graph_h), Rgb([90, 90, 90]));
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
        draw_line_segment_mut(img, pair[0], pair[1], Rgb([100, 220, 255]));
    }

    let axis_scale = PxScale::from(14.0);
    let axis_font = font();
    let max_label = format!("{max_depth:.1}m");
    let min_label = format!("{min_depth:.1}m");
    let (_, min_label_h) = text_size(axis_scale, &axis_font, &min_label);
    // min_depth (shallowest) plots at the top of the box, max_depth (deepest) at the bottom.
    draw_text_mut(img, Rgb([200, 200, 200]), x + 4, y + 2, axis_scale, &axis_font, &min_label);
    draw_text_mut(
        img,
        Rgb([200, 200, 200]),
        x + 4,
        y + graph_h as i32 - min_label_h as i32 - 2,
        axis_scale,
        &axis_font,
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
        let lines = build_overlay_lines(&[Field::Depth], &[], &[], 5.0);
        assert_eq!(lines, vec!["Keine Daten".to_string()]);
    }

    #[test]
    fn build_overlay_lines_includes_time_and_depth() {
        let samples = vec![sample(0.0, 1.5)];
        let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();
        let lines = build_overlay_lines(&[Field::Time, Field::Depth], &samples, &times, 10.0);
        assert_eq!(lines[0], "Tauchzeit: 00:10");
        assert_eq!(lines[1], "Tiefe: 1.5 m");
    }

    #[test]
    fn build_overlay_lines_carries_forward_sparse_temperature() {
        let mut with_temp = sample(0.0, 1.0);
        with_temp.temp_c = Some(18.0);
        let samples = vec![with_temp, sample(10.0, 2.0), sample(20.0, 3.0)];
        let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();

        let lines = build_overlay_lines(&[Field::Temp], &samples, &times, 20.0);
        assert_eq!(lines, vec!["Temp: 18.0 C".to_string()]);
    }

    #[test]
    fn draw_overlay_does_not_panic_on_small_image() {
        let mut img = RgbImage::new(320, 240);
        draw_overlay(&mut img, &["Tauchzeit: 00:10".to_string()]);
    }

    #[test]
    fn draw_depth_graph_does_not_panic() {
        let mut img = RgbImage::new(320, 240);
        let samples = vec![sample(0.0, 1.0), sample(5.0, 3.0), sample(10.0, 2.0)];
        let times: Vec<f64> = samples.iter().map(|s| s.elapsed_sec).collect();
        draw_depth_graph(&mut img, &samples, &times, 10.0, 600.0);
    }
}
