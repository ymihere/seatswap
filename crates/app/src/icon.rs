//! Tray icon drawing (spec §8.1, §8.2, §7.3).
//!
//! The icon is a disc in the active account's colour with its abbreviation and a
//! ring that shows how much of the binding usage limit is used. Everything is
//! drawn at run time so every size the platform asks for is sharp.

use ab_glyph::{point, Font, FontRef, Glyph, PxScale, ScaleFont};
use tiny_skia::{FillRule, LineCap, Paint, Path, PathBuilder, Pixmap, Stroke, Transform};

const FONT_BYTES: &[u8] = include_bytes!("../assets/DejaVuSans-Bold.ttf");

/// The grey of the unfilled part of the ring.
const RING_GREY: [u8; 3] = [0x55, 0x5b, 0x66];
/// The dark letter colour (`#0f1115`).
const LETTER_DARK: [u8; 3] = [0x0f, 0x11, 0x15];
const WHITE: [u8; 3] = [0xff, 0xff, 0xff];

/// What the tray icon shows for the active account (docs/implementation.md).
pub struct IconSpec {
    /// 1–2 characters, already validated.
    pub abbr: String,
    /// The account's colour.
    pub account_color: [u8; 3],
    /// Binding limit: percent 0..=100+ and its load colour. `None` = unknown → grey ring.
    pub ring: Option<(f64, [u8; 3])>,
    /// Draw the lock badge in the bottom-right quarter.
    pub locked: bool,
    /// White ring and disc outline, white letters on a transparent disc.
    pub monochrome: bool,
}

/// Straight (not premultiplied) RGBA, `size_px` × `size_px`, row-major.
pub fn render(spec: &IconSpec, size_px: u32) -> Vec<u8> {
    if size_px == 0 {
        return Vec::new();
    }
    let geometry = Geometry::new(size_px);
    let Some(mut pixmap) = Pixmap::new(size_px, size_px) else {
        // Unreachable for a non-zero size; loud in debug builds.
        debug_assert!(false, "a non-zero icon size is a valid pixmap size");
        return Vec::new();
    };

    draw_ring(&mut pixmap, spec, &geometry);
    draw_disc(&mut pixmap, spec, &geometry);
    draw_letters(&mut pixmap, spec, &geometry);
    if spec.locked {
        draw_lock(&mut pixmap, &geometry);
    }

    let mut rgba = Vec::with_capacity((size_px as usize) * (size_px as usize) * 4);
    for pixel in pixmap.pixels() {
        let color = pixel.demultiply();
        rgba.extend_from_slice(&[color.red(), color.green(), color.blue(), color.alpha()]);
    }
    rgba
}

/// The circle geometry shared by every part of the icon.
struct Geometry {
    size: f32,
    cx: f32,
    cy: f32,
    /// Outer edge of the ring.
    outer: f32,
    /// Inner edge of the ring.
    inner: f32,
    /// Radius of the account-coloured disc (inside the ring, with the 1 px gap).
    disc_radius: f32,
}

impl Geometry {
    fn new(size_px: u32) -> Geometry {
        let size = size_px as f32;
        // Keep the anti-aliased outer edge inside the canvas.
        let outer = (size / 2.0 - 0.5).max(0.5);
        let ring_width = (size / 8.0).max(2.0).min(outer);
        let inner = (outer - ring_width).max(0.0);
        let gap = if size_px >= 22 { 1.0 } else { 0.0 };
        let disc_radius = (inner - gap).max(0.0);
        Geometry {
            size,
            cx: size / 2.0,
            cy: size / 2.0,
            outer,
            inner,
            disc_radius,
        }
    }
}

fn draw_ring(pixmap: &mut Pixmap, spec: &IconSpec, geometry: &Geometry) {
    // The unfilled ring is a full annulus: grey, or white at 35 % in monochrome.
    let (unfilled, unfilled_alpha) = if spec.monochrome {
        (WHITE, 89)
    } else {
        (RING_GREY, 255)
    };
    if let Some(path) = annulus(geometry, geometry.outer, geometry.inner) {
        pixmap.fill_path(
            &path,
            &solid(unfilled, unfilled_alpha),
            FillRule::EvenOdd,
            Transform::identity(),
            None,
        );
    }

    let Some((percent, color)) = spec.ring else {
        return;
    };
    debug_assert!(percent >= 0.0, "ring percent must not be negative");
    // Negative percent is a programming error: draw the empty (grey) ring.
    let percent = if percent > 0.0 {
        percent.min(100.0)
    } else {
        0.0
    };
    if percent == 0.0 {
        return;
    }

    let filled = if spec.monochrome { WHITE } else { color };
    let paint = solid(filled, 255);
    if percent >= 100.0 {
        if let Some(path) = annulus(geometry, geometry.outer, geometry.inner) {
            pixmap.fill_path(
                &path,
                &paint,
                FillRule::EvenOdd,
                Transform::identity(),
                None,
            );
        }
    } else {
        let sweep = std::f32::consts::TAU * (percent as f32 / 100.0);
        if let Some(path) = arc_annulus(geometry, geometry.outer, geometry.inner, 0.0, sweep) {
            pixmap.fill_path(
                &path,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
    }
}

fn draw_disc(pixmap: &mut Pixmap, spec: &IconSpec, geometry: &Geometry) {
    if geometry.disc_radius <= 0.0 {
        return;
    }
    let Some(path) = PathBuilder::from_circle(geometry.cx, geometry.cy, geometry.disc_radius)
    else {
        return;
    };
    if spec.monochrome {
        // Transparent disc with a white outline.
        let stroke = Stroke {
            width: (geometry.size / 32.0).max(1.0),
            line_cap: LineCap::Butt,
            ..Stroke::default()
        };
        pixmap.stroke_path(
            &path,
            &solid(WHITE, 255),
            &stroke,
            Transform::identity(),
            None,
        );
    } else {
        pixmap.fill_path(
            &path,
            &solid(spec.account_color, 255),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

fn draw_letters(pixmap: &mut Pixmap, spec: &IconSpec, geometry: &Geometry) {
    let chars: Vec<char> = spec.abbr.chars().take(2).collect();
    if chars.is_empty() || geometry.disc_radius <= 0.0 {
        return;
    }
    let Ok(font) = FontRef::try_from_slice(FONT_BYTES) else {
        // The font is compiled in; an invalid asset is a build problem.
        debug_assert!(false, "the bundled icon font is valid");
        return;
    };
    let Some(h_glyph) = font.outline(font.glyph_id('H')) else {
        debug_assert!(false, "the bundled icon font has an H glyph");
        return;
    };
    // ab_glyph gives the outline box with y pointing up (min.y is the top, so
    // `height()` is negative); the cap height is its absolute size.
    let cap_ratio = h_glyph.bounds.height().abs() / font.height_unscaled();
    let disc_diameter = geometry.disc_radius * 2.0;
    let target_cap = disc_diameter * if chars.len() == 1 { 0.60 } else { 0.45 };
    let mut scale_y = target_cap / cap_ratio;
    if geometry.size >= 16.0 {
        // The font itself never gets smaller than 9 px.
        scale_y = scale_y.max(9.0);
    }

    // Measure the advance width at the chosen height, then squeeze x only.
    let measured = font.as_scaled(PxScale::from(scale_y));
    let mut advance = 0.0;
    for (index, ch) in chars.iter().enumerate() {
        let id = font.glyph_id(*ch);
        advance += measured.h_advance(id);
        if let Some(next) = chars.get(index + 1) {
            advance += measured.kern(id, font.glyph_id(*next));
        }
    }
    let max_width = disc_diameter * 0.8;
    let scale_x = if advance > max_width && advance > 0.0 {
        scale_y * (max_width / advance)
    } else {
        scale_y
    };

    let scale = PxScale {
        x: scale_x,
        y: scale_y,
    };
    let scaled = font.as_scaled(scale);
    let mut pen = 0.0;
    let mut positions = Vec::with_capacity(chars.len());
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for (index, ch) in chars.iter().enumerate() {
        let id = font.glyph_id(*ch);
        positions.push((id, pen));
        let glyph = Glyph {
            id,
            scale,
            position: point(pen, 0.0),
        };
        if let Some(outlined) = font.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();
            min_x = min_x.min(bounds.min.x);
            min_y = min_y.min(bounds.min.y);
            max_x = max_x.max(bounds.max.x);
            max_y = max_y.max(bounds.max.y);
        }
        pen += scaled.h_advance(id);
        if let Some(next) = chars.get(index + 1) {
            pen += scaled.kern(id, font.glyph_id(*next));
        }
    }
    if !min_x.is_finite() {
        return;
    }

    // Optical centring: the glyph bounding box, not the line metrics.
    let offset_x = geometry.cx - (min_x + max_x) / 2.0;
    let offset_y = geometry.cy - (min_y + max_y) / 2.0;

    let light_text = spec.monochrome
        || contrast_ratio(spec.account_color, WHITE)
            >= contrast_ratio(spec.account_color, LETTER_DARK);
    let color = if light_text { WHITE } else { LETTER_DARK };

    let radius_squared = geometry.disc_radius * geometry.disc_radius;
    let width = pixmap.width();
    let height = pixmap.height();
    let data = pixmap.data_mut();
    for (id, glyph_pen) in positions {
        let glyph = Glyph {
            id,
            scale,
            position: point(glyph_pen + offset_x, offset_y),
        };
        let Some(outlined) = font.outline_glyph(glyph) else {
            continue;
        };
        let origin = outlined.px_bounds().min;
        outlined.draw(|x, y, coverage| {
            let px = x as f32 + origin.x + 0.5;
            let py = y as f32 + origin.y + 0.5;
            // Keep the letters inside the disc.
            let dx = px - geometry.cx;
            let dy = py - geometry.cy;
            if dx * dx + dy * dy > radius_squared {
                return;
            }
            let glyph_x = (x as f32 + origin.x) as i64;
            let glyph_y = (y as f32 + origin.y) as i64;
            if glyph_x < 0 || glyph_y < 0 || glyph_x >= width as i64 || glyph_y >= height as i64 {
                return;
            }
            blend_over(data, width, glyph_x as u32, glyph_y as u32, color, coverage);
        });
    }
}

fn draw_lock(pixmap: &mut Pixmap, geometry: &Geometry) {
    let size = geometry.size;
    let badge = (size * 0.30).max(4.0);
    let margin = (size * 0.03).max(1.0);
    let center_x = geometry.cx + badge * 0.5 + margin;
    let center_y = geometry.cy + badge * 0.5 + margin;

    let body_width = badge * 0.62;
    let body_height = badge * 0.46;
    let body_left = center_x - body_width / 2.0;
    let body_top = center_y - body_height * 0.15;
    let outline = (badge * 0.08).max(0.5);

    // Dark outline first, white body on top.
    fill_rect(
        pixmap,
        body_left - outline,
        body_top - outline,
        body_width + 2.0 * outline,
        body_height + 2.0 * outline,
        LETTER_DARK,
    );
    fill_rect(pixmap, body_left, body_top, body_width, body_height, WHITE);

    // The shackle: a half circle above the body, dark like the outline.
    let shackle_radius = badge * 0.24;
    let steps = 24;
    let mut builder = PathBuilder::new();
    for step in 0..=steps {
        let angle = std::f32::consts::PI + std::f32::consts::PI * (step as f32 / steps as f32);
        let x = center_x + shackle_radius * angle.cos();
        let y = body_top + shackle_radius * angle.sin();
        if step == 0 {
            builder.move_to(x, y);
        } else {
            builder.line_to(x, y);
        }
    }
    if let Some(path) = builder.finish() {
        let stroke = Stroke {
            width: (badge * 0.16).max(1.0),
            line_cap: LineCap::Butt,
            ..Stroke::default()
        };
        pixmap.stroke_path(
            &path,
            &solid(LETTER_DARK, 255),
            &stroke,
            Transform::identity(),
            None,
        );
    }
}

fn fill_rect(pixmap: &mut Pixmap, x: f32, y: f32, width: f32, height: f32, color: [u8; 3]) {
    let Some(rect) = tiny_skia::Rect::from_xywh(x, y, width, height) else {
        return;
    };
    let mut builder = PathBuilder::new();
    builder.push_rect(rect);
    if let Some(path) = builder.finish() {
        pixmap.fill_path(
            &path,
            &solid(color, 255),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

fn solid(color: [u8; 3], alpha: u8) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color_rgba8(color[0], color[1], color[2], alpha);
    paint
}

fn annulus(geometry: &Geometry, outer: f32, inner: f32) -> Option<Path> {
    if outer <= 0.0 {
        return None;
    }
    let mut builder = PathBuilder::new();
    builder.push_circle(geometry.cx, geometry.cy, outer);
    if inner > 0.0 {
        builder.push_circle(geometry.cx, geometry.cy, inner);
    }
    builder.finish()
}

/// A filled sector of an annulus, clockwise from 12 o'clock.
fn arc_annulus(
    geometry: &Geometry,
    outer: f32,
    inner: f32,
    start: f32,
    sweep: f32,
) -> Option<Path> {
    if outer <= 0.0 || sweep <= 0.0 {
        return None;
    }
    let steps = ((sweep.abs() / (std::f32::consts::PI / 64.0)).ceil() as usize).max(2);
    let mut builder = PathBuilder::new();
    for step in 0..=steps {
        let angle = start + sweep * (step as f32 / steps as f32);
        let (sin, cos) = angle.sin_cos();
        let x = geometry.cx + outer * sin;
        let y = geometry.cy - outer * cos;
        if step == 0 {
            builder.move_to(x, y);
        } else {
            builder.line_to(x, y);
        }
    }
    for step in (0..=steps).rev() {
        let angle = start + sweep * (step as f32 / steps as f32);
        let (sin, cos) = angle.sin_cos();
        builder.line_to(geometry.cx + inner * sin, geometry.cy - inner * cos);
    }
    builder.close();
    builder.finish()
}

fn blend_over(data: &mut [u8], width: u32, x: u32, y: u32, color: [u8; 3], coverage: f32) {
    if x >= width {
        return;
    }
    let index = ((y as usize * width as usize) + x as usize) * 4;
    if index + 3 >= data.len() {
        return;
    }
    let coverage = coverage.clamp(0.0, 1.0);
    let source_alpha = (coverage * 255.0).round() as u32;
    if source_alpha == 0 {
        return;
    }
    let inverse = 255 - source_alpha;
    for channel in 0..3 {
        let source = (color[channel] as u32 * source_alpha + 127) / 255;
        let destination = data[index + channel] as u32;
        data[index + channel] = (source + (destination * inverse + 127) / 255) as u8;
    }
    let destination_alpha = data[index + 3] as u32;
    data[index + 3] = (source_alpha + (destination_alpha * inverse + 127) / 255).min(255) as u8;
}

/// WCAG 2.x relative luminance.
fn relative_luminance(color: [u8; 3]) -> f32 {
    fn linear(channel: u8) -> f32 {
        let value = channel as f32 / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * linear(color[0]) + 0.7152 * linear(color[1]) + 0.0722 * linear(color[2])
}

fn contrast_ratio(a: [u8; 3], b: [u8; 3]) -> f32 {
    let la = relative_luminance(a);
    let lb = relative_luminance(b);
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(abbr: &str, account_color: [u8; 3], ring: Option<(f64, [u8; 3])>) -> IconSpec {
        IconSpec {
            abbr: abbr.to_string(),
            account_color,
            ring,
            locked: false,
            monochrome: false,
        }
    }

    fn pixel(data: &[u8], size: u32, x: u32, y: u32) -> [u8; 4] {
        let index = ((y as usize * size as usize) + x as usize) * 4;
        [
            data[index],
            data[index + 1],
            data[index + 2],
            data[index + 3],
        ]
    }

    fn assert_close(actual: [u8; 4], expected: [u8; 4], tolerance: u8) {
        for channel in 0..4 {
            let difference = actual[channel].abs_diff(expected[channel]);
            assert!(
                difference <= tolerance,
                "pixel {actual:?} is not within {tolerance} of {expected:?} (channel {channel})"
            );
        }
    }

    /// The middle of the ring at `size`, using the contract's width.
    fn ring_mid_radius(size: u32) -> f32 {
        size as f32 / 2.0 - 0.5 - (size as f32 / 8.0).max(2.0) / 2.0
    }

    #[test]
    fn output_has_one_rgba_pixel_per_pixel() {
        for size in [16, 20, 22, 24, 32, 44, 64, 96] {
            let data = render(
                &spec("AS", [0x1e, 0x3a, 0x8a], Some((30.0, [0x22, 0xc5, 0x5e]))),
                size,
            );
            assert_eq!(data.len(), (size * size * 4) as usize, "size {size}");
        }
    }

    #[test]
    fn corners_are_transparent() {
        for size in [16, 20, 22, 24, 32, 44, 64, 96] {
            let data = render(
                &spec("AS", [0x1e, 0x3a, 0x8a], Some((30.0, [0x22, 0xc5, 0x5e]))),
                size,
            );
            for (x, y) in [(0, 0), (size - 1, 0), (0, size - 1), (size - 1, size - 1)] {
                assert_eq!(
                    pixel(&data, size, x, y)[3],
                    0,
                    "corner ({x},{y}) at size {size}"
                );
            }
        }
    }

    #[test]
    fn ring_begins_at_twelve_oclock() {
        let size = 22;
        let mid = ring_mid_radius(size);
        let x = size / 2;
        let y = (size as f32 / 2.0 - mid).floor() as u32;
        let green = [0x22, 0xc5, 0x5e];

        let filled = render(&spec("AS", [0x1e, 0x3a, 0x8a], Some((30.0, green))), size);
        assert_close(
            pixel(&filled, size, x, y),
            [green[0], green[1], green[2], 255],
            2,
        );

        let unknown = render(&spec("AS", [0x1e, 0x3a, 0x8a], None), size);
        assert_close(pixel(&unknown, size, x, y), [0x55, 0x5b, 0x66, 255], 2);
    }

    #[test]
    fn ring_fills_clockwise_past_three_oclock() {
        let size = 22;
        let mid = ring_mid_radius(size);
        let x = ((size as f32 / 2.0 + mid).floor()) as u32;
        let y = size / 2;
        let green = [0x22, 0xc5, 0x5e];

        let thirty = render(&spec("AS", [0x1e, 0x3a, 0x8a], Some((30.0, green))), size);
        assert_close(
            pixel(&thirty, size, x, y),
            [green[0], green[1], green[2], 255],
            2,
        );

        let twenty = render(&spec("AS", [0x1e, 0x3a, 0x8a], Some((20.0, green))), size);
        assert_close(pixel(&twenty, size, x, y), [0x55, 0x5b, 0x66, 255], 2);
    }

    #[test]
    fn disc_shows_the_account_colour_left_of_the_letters() {
        let size = 32;
        let account = [0x1e, 0x3a, 0x8a];
        let disc_radius = size as f32 / 2.0 - 0.5 - (size as f32 / 8.0).max(2.0) - 1.0;
        let data = render(&spec("WM", account, None), size);
        let x = ((size as f32 / 2.0 - disc_radius * 0.9).floor()) as u32;
        let y = size / 2;
        assert_close(
            pixel(&data, size, x, y),
            [account[0], account[1], account[2], 255],
            1,
        );
    }

    #[test]
    fn two_letters_fit_inside_the_disc_at_sixteen_px() {
        let size = 16;
        let disc_radius = size as f32 / 2.0 - 0.5 - (size as f32 / 8.0).max(2.0);
        let data = render(
            &spec("WM", [0x1e, 0x3a, 0x8a], Some((50.0, [0x22, 0xc5, 0x5e]))),
            size,
        );
        let mut white_inside = 0;
        for y in 0..size {
            for x in 0..size {
                let pixel = pixel(&data, size, x, y);
                let dx = x as f32 + 0.5 - size as f32 / 2.0;
                let dy = y as f32 + 0.5 - size as f32 / 2.0;
                let inside = dx * dx + dy * dy <= disc_radius * disc_radius + 0.01;
                let white = pixel[0] > 200 && pixel[1] > 200 && pixel[2] > 200 && pixel[3] == 255;
                if inside && white {
                    white_inside += 1;
                }
                assert!(
                    inside || !white,
                    "letter pixel outside the disc at ({x},{y})"
                );
            }
        }
        assert!(
            white_inside > 0,
            "the letters were not drawn inside the disc"
        );
    }

    #[test]
    fn letters_are_white_on_a_dark_account_and_dark_on_a_light_one() {
        let size = 64;
        let disc_radius = size as f32 / 2.0 - 0.5 - (size as f32 / 8.0).max(2.0) - 1.0;

        let dark_account = [0x1e, 0x3a, 0x8a];
        let dark_data = render(&spec("A", dark_account, None), size);
        let found = most_different_pixel(&dark_data, size, disc_radius, dark_account);
        assert!(
            found[0] > 200 && found[1] > 200 && found[2] > 200,
            "expected a white letter on #1e3a8a, found {found:?}"
        );
        assert_eq!(found[3], 255);

        let light_account = [0xfd, 0xe0, 0x47];
        let light_data = render(&spec("A", light_account, None), size);
        let found = most_different_pixel(&light_data, size, disc_radius, light_account);
        assert!(
            found[0] < 80 && found[1] < 80 && found[2] < 80,
            "expected a dark letter on #fde047, found {found:?}"
        );
        assert_eq!(found[3], 255);
    }

    fn most_different_pixel(data: &[u8], size: u32, radius: f32, reference: [u8; 3]) -> [u8; 4] {
        let mut best = [0, 0, 0, 0];
        let mut best_distance = -1i32;
        for y in 0..size {
            for x in 0..size {
                let dx = x as f32 + 0.5 - size as f32 / 2.0;
                let dy = y as f32 + 0.5 - size as f32 / 2.0;
                if dx * dx + dy * dy > (radius - 1.0) * (radius - 1.0) {
                    continue;
                }
                let pixel = pixel(data, size, x, y);
                let distance = (0..3)
                    .map(|channel| {
                        let difference = pixel[channel] as i32 - reference[channel] as i32;
                        difference * difference
                    })
                    .sum();
                if distance > best_distance {
                    best_distance = distance;
                    best = pixel;
                }
            }
        }
        best
    }

    #[test]
    fn rendering_is_deterministic() {
        let spec = spec("AS", [0x1e, 0x3a, 0x8a], Some((37.5, [0xf5, 0x9e, 0x0b])));
        for size in [16, 22, 32, 64] {
            assert_eq!(render(&spec, size), render(&spec, size), "size {size}");
        }
    }

    #[test]
    fn the_lock_badge_stays_in_the_bottom_right_quarter() {
        let size = 32;
        let unlocked_spec = spec("AS", [0x1e, 0x3a, 0x8a], Some((100.0, [0xef, 0x44, 0x44])));
        let unlocked = render(&unlocked_spec, size);
        let mut locked_spec = spec("AS", [0x1e, 0x3a, 0x8a], Some((100.0, [0xef, 0x44, 0x44])));
        locked_spec.locked = true;
        let locked = render(&locked_spec, size);

        let mut changes = 0;
        for y in 0..size {
            for x in 0..size {
                if pixel(&unlocked, size, x, y) != pixel(&locked, size, x, y) {
                    changes += 1;
                    assert!(
                        x >= size / 2 && y >= size / 2,
                        "lock badge changed ({x},{y}), outside the bottom-right quarter"
                    );
                }
            }
        }
        assert!(changes > 0, "the lock badge did not change any pixel");
    }

    /// Rows of the centre column band that hold letter ink (white letters on a
    /// dark disc). The band stays well inside the disc, so the ring and its
    /// white edge never count.
    fn letter_rows(rgba: &[u8], size: u32) -> u32 {
        let centre = size as f32 / 2.0;
        let band = size as f32 * 0.15;
        let mut rows = 0;
        for y in 0..size {
            let dy = y as f32 + 0.5 - centre;
            if dy.abs() > size as f32 * 0.3 {
                continue;
            }
            let ink = (0..size).any(|x| {
                let dx = x as f32 + 0.5 - centre;
                if dx.abs() > band {
                    return false;
                }
                let i = ((y * size + x) * 4) as usize;
                rgba[i] > 200 && rgba[i + 1] > 200 && rgba[i + 2] > 200
            });
            if ink {
                rows += 1;
            }
        }
        rows
    }

    /// Before the fix the cap height came out negative, the 9 px floor took
    /// over, and the letters were 9 px tall at every icon size.
    #[test]
    fn letters_grow_with_the_icon() {
        let spec = IconSpec {
            abbr: "H".to_string(),
            account_color: [0x1e, 0x3a, 0x8a],
            ring: None,
            locked: false,
            monochrome: false,
        };
        let small = letter_rows(&render(&spec, 64), 64);
        let large = letter_rows(&render(&spec, 256), 256);
        // One letter is 60 % of the disc; the disc is well over half the icon.
        assert!(small >= 15, "64 px icon: H is {small} rows tall");
        assert!(large >= 60, "256 px icon: H is {large} rows tall");
        assert!(
            large >= small * 3,
            "64 px: {small} rows, 256 px: {large} rows"
        );
    }

    #[test]
    #[ignore]
    fn write_previews() {
        // `target/` at the workspace root, whether or not CARGO_TARGET_DIR is set.
        let folder = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target/icon-preview");
        std::fs::create_dir_all(&folder).unwrap();

        let examples = [
            (
                "as-22",
                22,
                IconSpec {
                    abbr: "AS".to_string(),
                    account_color: [0x1e, 0x3a, 0x8a],
                    ring: Some((37.0, [0x22, 0xc5, 0x5e])),
                    locked: false,
                    monochrome: false,
                },
            ),
            (
                "p-32",
                32,
                IconSpec {
                    abbr: "P".to_string(),
                    account_color: [0x0f, 0x76, 0x6e],
                    ring: Some((72.0, [0xf5, 0x9e, 0x0b])),
                    locked: false,
                    monochrome: false,
                },
            ),
            (
                "wm-64-locked",
                64,
                IconSpec {
                    abbr: "WM".to_string(),
                    account_color: [0xfd, 0xe0, 0x47],
                    ring: Some((100.0, [0xef, 0x44, 0x44])),
                    locked: true,
                    monochrome: false,
                },
            ),
            (
                "as-64-mono",
                64,
                IconSpec {
                    abbr: "AS".to_string(),
                    account_color: [0x1e, 0x3a, 0x8a],
                    ring: Some((58.0, [0x22, 0xc5, 0x5e])),
                    locked: false,
                    monochrome: true,
                },
            ),
            // The app's own icon (launcher, window): crates/app/icons/icon.png.
            (
                "app-256",
                256,
                IconSpec {
                    abbr: "SC".to_string(),
                    account_color: [0xd9, 0x77, 0x57],
                    ring: Some((75.0, [0x22, 0xc5, 0x5e])),
                    locked: false,
                    monochrome: false,
                },
            ),
            (
                "unknown-64",
                64,
                IconSpec {
                    abbr: "AS".to_string(),
                    account_color: [0x1e, 0x3a, 0x8a],
                    ring: None,
                    locked: false,
                    monochrome: false,
                },
            ),
        ];

        for (name, size, example) in examples {
            let rgba = render(&example, size);
            let header = format!(
                "P7\nWIDTH {size}\nHEIGHT {size}\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n"
            );
            let mut pam = Vec::with_capacity(header.len() + rgba.len());
            pam.extend_from_slice(header.as_bytes());
            pam.extend_from_slice(&rgba);
            std::fs::write(folder.join(format!("{name}.pam")), pam).unwrap();
        }
    }
}
