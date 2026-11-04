//! Thermal pseudo-color rendering: To grid (32x24 °C) → ironbow RGB →
//! I420 for the encoder / BMP for the snapshot endpoint.
//!
//! Port of the bring-up viewer pipeline (percentile stretch + ironbow,
//! visual-model QA'd to "acceptable low-res thermal display" standard) with
//! bilinear upscale — the GB28181 placeholder/live stream and the SPEC
//! snapshot share this exact path.

use super::mlx90640::{HEIGHT, PIXELS, WIDTH};

/// Ironbow control points (t, r, g, b) — same feel as the viewer v4 palette.
const IRONBOW: [(f32, u8, u8, u8); 8] = [
    (0.00, 0, 0, 0),
    (0.15, 18, 8, 64),
    (0.35, 64, 12, 120),
    (0.55, 140, 30, 130),
    (0.72, 216, 88, 60),
    (0.86, 248, 168, 48),
    (0.96, 255, 232, 120),
    (1.00, 255, 255, 224),
];

pub fn ironbow(t: f32) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    for w in IRONBOW.windows(2) {
        let (t0, r0, g0, b0) = w[0];
        let (t1, r1, g1, b1) = w[1];
        if t <= t1 {
            let f = if t1 > t0 { (t - t0) / (t1 - t0) } else { 0.0 };
            return (
                (r0 as f32 + f * (r1 as i32 - r0 as i32) as f32) as u8,
                (g0 as f32 + f * (g1 as i32 - g0 as i32) as f32) as u8,
                (b0 as f32 + f * (b1 as i32 - b0 as i32) as f32) as u8,
            );
        }
    }
    let (.., r, g, b) = IRONBOW[IRONBOW.len() - 1];
    (r, g, b)
}

/// Percentile stretch parameters (low/high percentiles, gamma).
const P_LO: f32 = 0.02;
const P_HI: f32 = 0.98;
const GAMMA: f32 = 0.7;

/// Render with on-image temperature overlays (max/min crosshairs with
/// labels + center spot) — the thermal-camera look shared by every
/// consumer (webui card, viewer, snapshot, GB28181 stream).
pub fn render_rgb_annotated(
    to: &[f32; PIXELS],
    out_w: usize,
    out_h: usize,
    lo: Option<f32>,
    hi: Option<f32>,
) -> Vec<u8> {
    // The renderer's stretch and the overlay's color bar must agree.
    let mut sorted = *to;
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let lo_idx = ((PIXELS as f32 * P_LO) as usize).min(PIXELS - 1);
    let hi_idx = ((PIXELS as f32 * P_HI) as usize).min(PIXELS - 1);
    let (sorted_lo, sorted_hi) = (sorted[lo_idx], sorted[hi_idx]);
    let range = match (lo, hi) {
        (Some(lo), Some(hi)) if hi - lo > 1e-3 => (lo, hi),
        _ => (sorted_lo, sorted_hi),
    };
    let mut rgb = render_rgb_range(to, out_w, out_h, lo, hi);
    overlay_temperatures(to, &mut rgb, out_w, out_h, range);
    rgb
}

/// Render with an optional fixed display range (`lo`..`hi` °C); `None`
/// falls back to the percentile stretch. Fixed ranges keep colors stable
/// across frames (comparable playback) — e.g. `?lo=25&hi=45`.
pub fn render_rgb_range(
    to: &[f32; PIXELS],
    out_w: usize,
    out_h: usize,
    lo_override: Option<f32>,
    hi_override: Option<f32>,
) -> Vec<u8> {
    let (lo, hi) = match (lo_override, hi_override) {
        (Some(lo), Some(hi)) if hi - lo > 1e-3 => (lo, hi),
        _ => {
            let mut sorted = *to;
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            (
                sorted[((PIXELS as f32 * P_LO) as usize).min(PIXELS - 1)],
                sorted[((PIXELS as f32 * P_HI) as usize).min(PIXELS - 1)],
            )
        }
    };
    let span = if hi - lo > 1e-3 { hi - lo } else { 1.0 };

    let grid_rgb: Vec<(u8, u8, u8)> = to
        .iter()
        .map(|&v| {
            let t = ((v - lo) / span).clamp(0.0, 1.0).powf(1.0 / GAMMA);
            ironbow(t)
        })
        .collect();

    let mut out = vec![0u8; out_w * out_h * 3];
    for y in 0..out_h {
        // Bilinear source coordinates in the 32x24 grid (pixel centers).
        let gy = (y as f32 + 0.5) * HEIGHT as f32 / out_h as f32 - 0.5;
        let y0 = gy.floor().clamp(0.0, (HEIGHT - 1) as f32) as usize;
        let y1 = (y0 + 1).min(HEIGHT - 1);
        let fy = (gy - y0 as f32).clamp(0.0, 1.0);
        for x in 0..out_w {
            let gx = (x as f32 + 0.5) * WIDTH as f32 / out_w as f32 - 0.5;
            let x0 = gx.floor().clamp(0.0, (WIDTH - 1) as f32) as usize;
            let x1 = (x0 + 1).min(WIDTH - 1);
            let fx = (gx - x0 as f32).clamp(0.0, 1.0);
            let c00 = grid_rgb[y0 * WIDTH + x0];
            let c01 = grid_rgb[y0 * WIDTH + x1];
            let c10 = grid_rgb[y1 * WIDTH + x0];
            let c11 = grid_rgb[y1 * WIDTH + x1];
            let lerp = |a: f32, b: f32, f: f32| a + f * (b - a);
            let ch = |c0: u8, c1: u8, c2: u8, c3: u8| {
                lerp(
                    lerp(f32::from(c0), f32::from(c1), fx),
                    lerp(f32::from(c2), f32::from(c3), fx),
                    fy,
                )
            };
            let (r, g, b) = (
                ch(c00.0, c01.0, c10.0, c11.0),
                ch(c00.1, c01.1, c10.1, c11.1),
                ch(c00.2, c01.2, c10.2, c11.2),
            );
            let o = (y * out_w + x) * 3;
            out[o] = r.clamp(0.0, 255.0) as u8;
            out[o + 1] = g.clamp(0.0, 255.0) as u8;
            out[o + 2] = b.clamp(0.0, 255.0) as u8;
        }
    }
    out
}

/// RGB (packed, w*h*3) → I420 planar (BT.601 full range).
pub fn rgb_to_i420(rgb: &[u8], w: usize, h: usize) -> Vec<u8> {
    assert_eq!(rgb.len(), w * h * 3);
    assert!(
        w.is_multiple_of(2) && h.is_multiple_of(2),
        "I420 needs even dimensions"
    );
    let mut y = vec![0u8; w * h];
    let mut u = vec![0u8; w * h / 4];
    let mut v = vec![0u8; w * h / 4];
    for p in 0..w * h {
        let r = f32::from(rgb[p * 3]);
        let g = f32::from(rgb[p * 3 + 1]);
        let b = f32::from(rgb[p * 3 + 2]);
        y[p] = (0.299 * r + 0.587 * g + 0.114 * b)
            .round()
            .clamp(0.0, 255.0) as u8;
    }
    for cy in 0..h / 2 {
        for cx in 0..w / 2 {
            let mut us = 0f32;
            let mut vs = 0f32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let p = (cy * 2 + dy) * w + cx * 2 + dx;
                    us += -0.169 * f32::from(rgb[p * 3]) - 0.331 * f32::from(rgb[p * 3 + 1])
                        + 0.5 * f32::from(rgb[p * 3 + 2]);
                    vs += 0.5 * f32::from(rgb[p * 3])
                        - 0.419 * f32::from(rgb[p * 3 + 1])
                        - 0.081 * f32::from(rgb[p * 3 + 2]);
                }
            }
            let q = cy * (w / 2) + cx;
            u[q] = ((us / 4.0) + 128.0).round().clamp(0.0, 255.0) as u8;
            v[q] = ((vs / 4.0) + 128.0).round().clamp(0.0, 255.0) as u8;
        }
    }
    let mut out = Vec::with_capacity(w * h * 3 / 2);
    out.extend_from_slice(&y);
    out.extend_from_slice(&u);
    out.extend_from_slice(&v);
    out
}

/// Minimal uncompressed 24-bit BMP (row padded to 4 bytes), bottom-up.
pub fn rgb_to_bmp(rgb: &[u8], w: usize, h: usize) -> Vec<u8> {
    let row = (w * 3 + 3) & !3;
    let size = 54 + row * h;
    let mut out = vec![0u8; size];
    out[0..2].copy_from_slice(b"BM");
    out[2..6].copy_from_slice(&(size as u32).to_le_bytes());
    out[10..14].copy_from_slice(&54u32.to_le_bytes());
    out[14..18].copy_from_slice(&40u32.to_le_bytes());
    out[18..22].copy_from_slice(&(w as i32).to_le_bytes());
    out[22..26].copy_from_slice(&(h as i32).to_le_bytes());
    out[26..28].copy_from_slice(&1u16.to_le_bytes());
    out[28..30].copy_from_slice(&24u16.to_le_bytes());
    for y in 0..h {
        let src = (h - 1 - y) * w * 3;
        let dst = 54 + y * row;
        out[dst..dst + w * 3].copy_from_slice(&rgb[src..src + w * 3]);
    }
    out
}

/// Minimal 5x7 bitmap font for on-image temperature labels (digits, decimal
/// point, minus, C, degree ring). Rendered with 3x supersampling then box
/// filtering — anti-aliased edges, crisp look, zero font dependencies.
const FONT: [(char, [u8; 7]); 14] = [
    ('0', [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E]),
    ('1', [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E]),
    ('2', [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F]),
    ('3', [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E]),
    ('4', [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02]),
    ('5', [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E]),
    ('6', [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E]),
    ('7', [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08]),
    ('8', [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E]),
    ('9', [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C]),
    ('.', [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C]),
    ('-', [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00]),
    ('C', [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E]),
    ('\u{00b0}', [0x06, 0x09, 0x06, 0x00, 0x00, 0x00, 0x00]),
];

fn glyph(ch: char) -> Option<[u8; 7]> {
    FONT.iter().find(|(c, _)| *c == ch).map(|(_, rows)| *rows)
}

/// Blend one RGB pixel toward `color` by coverage (0..=255).
fn blend_px(rgb: &mut [u8], p: usize, color: (u8, u8, u8), coverage: u8) {
    let a = f32::from(coverage) / 255.0;
    for (c, chan) in [color.0, color.1, color.2].into_iter().enumerate() {
        let cur = f32::from(rgb[p + c]);
        rgb[p + c] = (cur + (f32::from(chan) - cur) * a)
            .round()
            .clamp(0.0, 255.0) as u8;
    }
}

/// Text metrics in destination pixels for a scale factor (tight degree-ring
/// advance accounted).
struct TextBox {
    w: usize,
    h: usize,
    pad: usize,
}

fn text_advance(text: &[char], scale: usize) -> usize {
    text.iter()
        .map(|&c| if c == '\u{00b0}' { 4 } else { 6 })
        .sum::<usize>()
        * scale
}

fn text_box(text: &[char], scale: usize) -> TextBox {
    let pad = 2 * scale;
    TextBox {
        w: text_advance(text, scale) + pad * 2,
        h: 7 * scale + pad * 2,
        pad,
    }
}

/// Anti-aliased text on a rounded translucent dark box blended over the
/// image (65% darkening — readable on any background without a solid slab).
#[allow(clippy::too_many_arguments)]
fn draw_text_aa(
    rgb: &mut [u8],
    w: usize,
    h: usize,
    x: usize,
    y: usize,
    text: &str,
    scale: usize,
    color: (u8, u8, u8),
) {
    let chars: Vec<char> = text.chars().collect();
    let b = text_box(&chars, scale);
    const MARGIN: usize = 6;
    let x = x.min(w.saturating_sub(b.w + MARGIN));
    let y = y.min(h.saturating_sub(b.h + MARGIN));
    if b.w == 0 || b.h == 0 || x >= w || y >= h {
        return;
    }

    // Rounded translucent background.
    let radius = 2 * scale;
    for dy in 0..b.h.min(h - y) {
        for dx in 0..b.w.min(w - x) {
            let nx = dx.min(b.w - dx).min(radius);
            let ny = dy.min(b.h - dy).min(radius);
            if nx + ny < radius {
                continue; // outside the rounded corner
            }
            let p = ((y + dy) * w + (x + dx)) * 3;
            for c in 0..3 {
                rgb[p + c] = (f32::from(rgb[p + c]) * 0.35) as u8;
            }
        }
    }

    // Glyph pass with 3x3 supersampling per destination pixel.
    const SS: usize = 3;
    for (ci, &ch) in chars.iter().enumerate() {
        let Some(rows) = glyph(ch) else { continue };
        // Tight advance after the degree ring keeps "36.7\u{00b0}C" compact.
        let origin = chars[..ci]
            .iter()
            .map(|&c| if c == '\u{00b0}' { 4 } else { 6 })
            .sum::<usize>()
            * scale;
        for gy in 0..7 * scale {
            for gx in 0..5 * scale {
                let mut hits = 0usize;
                for sy in 0..SS {
                    for sx in 0..SS {
                        let fx =
                            ((gx as f32 + sx as f32 / SS as f32) / scale as f32).floor() as i32;
                        let fy =
                            ((gy as f32 + sy as f32 / SS as f32) / scale as f32).floor() as i32;
                        if (0..5).contains(&fx)
                            && (0..7).contains(&fy)
                            && rows[fy as usize] & (1 << (4 - fx)) != 0
                        {
                            hits += 1;
                        }
                    }
                }
                if hits == 0 {
                    continue;
                }
                let coverage = (hits * 255 / (SS * SS)) as u8;
                let px = x + b.pad + origin + gx;
                let py = y + b.pad + gy;
                if px >= w || py >= h {
                    continue;
                }
                blend_px(rgb, (py * w + px) * 3, color, coverage);
            }
        }
    }
}

/// FLIR-style thin L-corner brackets centered on a spot (side `s` px):
/// 1px color core with a 1px dark outline, short corner arms — instrument
/// look, subtle on any background.
fn brackets(
    rgb: &mut [u8],
    w: usize,
    h: usize,
    cx: usize,
    cy: usize,
    s: usize,
    color: (u8, u8, u8),
) {
    let half = (s / 2) as i32;
    let arm = (half + 1).min(6);
    let (ccx, ccy) = (cx as i32, cy as i32);
    for (col, off) in [((0, 0, 0), 1i32), (color, 0)] {
        // Four corners: (±half, ±half), arms of length `arm` inward.
        for (sx, sy) in [(-1, -1), (1, -1), (-1, 1), (1, 1)] {
            for d in 0..arm {
                for (px, py) in [
                    (
                        ccx + sx * (half + off) - sx * off,
                        ccy + sy * (half + off) + sy * d - sy * off,
                    ),
                    (
                        ccx + sx * (half + off) + sx * d - sx * off,
                        ccy + sy * (half + off) - sy * off,
                    ),
                ] {
                    if px < 0 || py < 0 || px >= w as i32 || py >= h as i32 {
                        continue;
                    }
                    let p = (py as usize * w + px as usize) * 3;
                    rgb[p] = col.0;
                    rgb[p + 1] = col.1;
                    rgb[p + 2] = col.2;
                }
            }
        }
    }
}

/// Bake thermal-camera-style overlays onto a rendered frame: max/min/center
/// thin brackets with value labels (unified below-center anchor) and a
/// right-side ironbow color bar with range labels — the instrument look
/// shared by every consumer (webui card, viewer, snapshot, GB28181 stream).
#[allow(clippy::too_many_arguments)]
pub fn overlay_temperatures(
    to: &[f32; PIXELS],
    rgb: &mut [u8],
    w: usize,
    h: usize,
    range: (f32, f32),
) {
    let scale = if w >= 480 { 2 } else { 1 };
    let cell_w = w / WIDTH;
    let cell_h = h / HEIGHT;

    let (mut max_i, mut min_i) = (0usize, 0usize);
    for (i, v) in to.iter().enumerate() {
        if *v > to[max_i] {
            max_i = i;
        }
        if *v < to[min_i] {
            min_i = i;
        }
    }
    let label = |v: f32| -> String { format!("{v:.1}\u{00b0}C") };

    // Unified anchor rule: bracket centered on the cell, label directly
    // below the bracket, horizontally centered on it; flip above when the
    // label would run off the bottom.
    let mark = |rgb: &mut [u8], i: usize, color: (u8, u8, u8)| {
        let (gx, gy) = (i % WIDTH, i / WIDTH);
        let cx = gx * cell_w + cell_w / 2;
        let cy = gy * cell_h + cell_h / 2;
        brackets(rgb, w, h, cx, cy, (cell_w * 3 / 2).max(6), color);
        let chars: Vec<char> = label(to[i]).chars().collect();
        let b = text_box(&chars, scale);
        let lx = cx.saturating_sub(b.w / 2);
        let below = cy + cell_h;
        let ly = if below + b.h + 4 <= h {
            below
        } else {
            cy.saturating_sub(cell_h + b.h)
        };
        draw_text_aa(rgb, w, h, lx, ly, &label(to[i]), scale, color);
    };

    // Label text stays high-contrast white on every marker; the bracket
    // color carries the semantic (vision QA: colored text reads weaker).
    const WHITE: (u8, u8, u8) = (255, 255, 255);
    mark(rgb, max_i, (255, 255, 255));
    mark(rgb, min_i, (110, 235, 255));

    // Center spot: tight amber brackets + reading.
    let c_i = (HEIGHT / 2) * WIDTH + WIDTH / 2;
    let (cx, cy) = (w / 2, h / 2);
    brackets(rgb, w, h, cx, cy, cell_w.max(6), (255, 214, 92));
    let cchars: Vec<char> = label(to[c_i]).chars().collect();
    let cb = text_box(&cchars, scale);
    draw_text_aa(
        rgb,
        w,
        h,
        cx.saturating_sub(cb.w / 2),
        (cy + cell_h).min(h.saturating_sub(cb.h + 6)),
        &label(to[c_i]),
        scale,
        WHITE,
    );

    // Right-side color bar: ironbow vertical strip with top/mid/bottom range
    // labels — the "real instrument" anchor.
    let bar_w = (scale * 8).max(12);
    let bar_h = h * 2 / 3;
    let bx = w.saturating_sub(bar_w + 10);
    let by = (h - bar_h) / 2;
    for dy in 0..bar_h {
        let t = 1.0 - dy as f32 / bar_h as f32; // top = hot
        let (r, g, bl) = ironbow(t);
        for dx in 0..bar_w {
            let px = bx + dx;
            let py = by + dy;
            if px >= w || py >= h {
                continue;
            }
            let p = (py * w + px) * 3;
            rgb[p] = r;
            rgb[p + 1] = g;
            rgb[p + 2] = bl;
        }
    }
    // Thin outline around the bar.
    for (col, off) in [((0, 0, 0), 1i32), ((255, 255, 255), 0)] {
        for d in 0..=bar_h {
            for px in [bx as i32 - off, (bx + bar_w) as i32 + off - 1] {
                let py = by as i32 + d as i32 - off;
                if px < 0 || py < 0 || px >= w as i32 || py >= h as i32 {
                    continue;
                }
                let p = (py as usize * w + px as usize) * 3;
                rgb[p] = col.0;
                rgb[p + 1] = col.1;
                rgb[p + 2] = col.2;
            }
        }
    }
    let (lo, hi) = range;
    let mid = (lo + hi) / 2.0;
    let ranges = [(hi, by), (mid, by + bar_h / 2), (lo, by + bar_h)];
    for (v, ty) in ranges {
        let text = format!("{v:.1}\u{00b0}");
        let chars: Vec<char> = text.chars().collect();
        let b = text_box(&chars, scale);
        // Labels hug the bar's left side, vertically centered on the tick.
        draw_text_aa(
            rgb,
            w,
            h,
            bx.saturating_sub(b.w + 6),
            ty.saturating_sub(b.h / 2),
            &text,
            scale,
            (255, 255, 255),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ironbow_is_monotonic_and_clamped() {
        // Boundary inputs never panic and endpoints match the table.
        assert_eq!(ironbow(-0.5), ironbow(0.0));
        assert_eq!(ironbow(1.9), ironbow(1.0));
        assert_eq!(ironbow(0.0).0, 0);
        assert_eq!(ironbow(1.0), (255, 255, 224));
        // Heat monotonicity on the luma of a few stops.
        let lum = |t: f32| {
            let (r, g, b) = ironbow(t);
            r as u32 + g as u32 + b as u32
        };
        assert!(lum(0.9) > lum(0.5));
        assert!(lum(0.5) > lum(0.1));
    }

    #[test]
    fn render_sizes_and_i420_layout() {
        let mut to = [25f32; PIXELS];
        to[0] = 40.0; // one hot corner to exercise the stretch
        let rgb = render_rgb_range(&to, 320, 240, None, None);
        assert_eq!(rgb.len(), 320 * 240 * 3);
        let i420 = rgb_to_i420(&rgb, 320, 240);
        assert_eq!(i420.len(), 320 * 240 * 3 / 2);
        let bmp = rgb_to_bmp(&rgb, 320, 240);
        assert_eq!(&bmp[0..2], b"BM");
        assert_eq!(bmp.len(), 54 + (320 * 3) * 240);
    }
}
