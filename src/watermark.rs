//! Video watermark (SPEC §5.2): burns custom text + a real-time clock into
//! I420 (YU12) frames before encoding, OSD-style — every downstream consumer
//! (RTSP, ONVIF, GB28181, recordings, web snapshots, AI) sees the same burn,
//! exactly like the device-level flips.
//!
//! Rendering is cached: the composed line (text + timestamp) only changes
//! when the formatted timestamp changes (typically once per second), so glyph
//! rasterization runs at ~1 Hz and each frame pays only a small luma/chroma
//! blit. Style is fixed in v1: anti-aliased white text with a 1px black
//! outline (readable on any background), 16 px frame margin.
//!
//! Fonts: the embedded ASCII subset covers the timestamp and ASCII text out
//! of the box; `watermark.font_path` loads a full TTF/OTF at runtime (e.g. a
//! CJK font for Chinese text). A failed `font_path` load falls back to the
//! embedded font (fail-open) and logs a warning.

use chrono::{DateTime, Local};
use fontdue::Font;

use crate::config::{Position, WatermarkConfig};

/// Embedded default font: Noto Sans CJK SC subsetted to printable ASCII
/// (~8 KB; license: `assets/fonts/LICENSE-NotoCJK.txt`). Same design as the
/// shipped CJK subset, so ASCII-only and CJK watermarks look consistent.
/// Non-ASCII text needs `watermark.font_path` pointing at a font with those
/// glyphs.
pub const EMBEDDED_FONT: &[u8] = include_bytes!("../assets/fonts/NotoSans-Latin-ASCII.ttf");

/// BT.601 studio-swing luma for text (white) and outline (black).
const Y_TEXT: u8 = 235;
const Y_OUTLINE: u8 = 16;
/// Neutral chroma blended into every 2×2 block touched by the mask.
const UV_NEUTRAL: u8 = 128;
/// Distance of the mask from the frame edges (SPEC §5.2: fixed, not configurable).
const MARGIN_PX: usize = 16;
/// Coverages below this are imperceptible — skipped entirely.
const MIN_ALPHA: u8 = 12;

#[derive(Debug)]
pub enum WatermarkError {
    FontLoad(String),
}

impl std::fmt::Display for WatermarkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WatermarkError::FontLoad(msg) => write!(f, "watermark font load failed: {msg}"),
        }
    }
}

impl std::error::Error for WatermarkError {}

/// strftime specifiers accepted in `watermark.timestamp_format` (SPEC §5.2).
const ALLOWED_SPECIFIERS: &[u8] = b"YmdFHMST%";

/// True if `fmt` only contains whitelisted strftime specifiers and literals.
pub fn valid_timestamp_format(fmt: &str) -> bool {
    let bytes = fmt.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            i += 1;
            continue;
        }
        let Some(&spec) = bytes.get(i + 1) else {
            return false; // trailing '%'
        };
        if !ALLOWED_SPECIFIERS.contains(&spec) {
            return false;
        }
        i += 2;
    }
    true
}

/// Format `now` with the validated strftime subset (chrono handles rendering).
pub fn format_timestamp(fmt: &str, now: &DateTime<Local>) -> String {
    now.format(fmt).to_string()
}

/// A rasterized text line: anti-aliased fill coverage (glyph alpha) plus its
/// 1px grayscale dilation (the black stroke, including under the glyph).
struct TextMask {
    w: usize,
    h: usize,
    /// Glyph coverage per pixel, 0..=255.
    fill: Vec<u8>,
    /// Stroke coverage per pixel, 0..=255 (max of the 3×3 neighborhood of
    /// `fill`, so it also covers the glyph interior — the white fill is
    /// composited on top of it).
    outline: Vec<u8>,
}

impl TextMask {
    #[cfg(test)]
    fn painted(&self) -> usize {
        self.fill.iter().filter(|&&a| a >= 128).count()
    }
}

/// Rasterize one line of text at `px` height into an anti-aliased fill
/// coverage mask plus a 1px grayscale-dilated stroke mask.
fn rasterize_line(font: &Font, line: &str, px: f32) -> TextMask {
    // First pass: pen advance + ascent/descent to size the mask.
    let mut pen = 0.0_f32;
    let mut ascent = 0_i32;
    let mut descent = 0_i32;
    for ch in line.chars() {
        let m = font.metrics(ch, px);
        ascent = ascent.max(-m.ymin);
        descent = descent.max(m.ymin + m.height as i32);
        pen += m.advance_width;
    }
    let w = pen.ceil().max(0.0) as usize;
    let h = (ascent + descent).max(0) as usize;
    if w == 0 || h == 0 {
        return TextMask {
            w,
            h,
            fill: Vec::new(),
            outline: Vec::new(),
        };
    }
    let mut fill = vec![0_u8; w * h];
    // Second pass: stamp the sub-pixel coverage fontdue produced — this is
    // the anti-aliasing; no thresholding.
    let mut pen = 0.0_f32;
    for ch in line.chars() {
        let (m, bitmap) = font.rasterize(ch, px);
        let gx = (pen + m.xmin as f32).round().max(0.0) as usize;
        let gy = (ascent + m.ymin).max(0) as usize;
        for row in 0..m.height {
            let dy = gy + row;
            if dy >= h {
                break;
            }
            for col in 0..m.width {
                let dx = gx + col;
                if dx >= w {
                    break;
                }
                let idx = dy * w + dx;
                fill[idx] = fill[idx].max(bitmap[row * m.width + col]);
            }
        }
        pen += m.advance_width;
    }
    // Stroke = grayscale dilation of fill (max over the 3×3 neighborhood,
    // including the center, so the stroke never punches through the glyph).
    let mut outline = vec![0_u8; w * h];
    for y in 0..h {
        for x in 0..w {
            let a = fill[y * w + x];
            if a == 0 {
                continue;
            }
            for dy in -1_i64..=1 {
                for dx in -1_i64..=1 {
                    let ny = y as i64 + dy;
                    let nx = x as i64 + dx;
                    if ny < 0 || nx < 0 || ny >= h as i64 || nx >= w as i64 {
                        continue;
                    }
                    let idx = (ny as usize) * w + nx as usize;
                    outline[idx] = outline[idx].max(a);
                }
            }
        }
    }
    TextMask {
        w,
        h,
        fill,
        outline,
    }
}

/// Top-left origin of the mask for `position` inside a `width`×`height`
/// frame, clamped so at least part of the mask stays visible.
fn origin(position: Position, width: usize, height: usize, mw: usize, mh: usize) -> (usize, usize) {
    let x = match position {
        Position::TopLeft | Position::BottomLeft => MARGIN_PX,
        Position::TopRight | Position::BottomRight => width.saturating_sub(mw + MARGIN_PX),
    };
    let y = match position {
        Position::TopLeft | Position::TopRight => MARGIN_PX,
        Position::BottomLeft | Position::BottomRight => height.saturating_sub(mh + MARGIN_PX),
    };
    (
        x.min(width.saturating_sub(1)),
        y.min(height.saturating_sub(1)),
    )
}

/// Runtime watermark renderer. Owned by the capture thread; not `Sync` by
/// design (single-threaded use, cache included).
pub struct Watermark {
    text: String,
    show_timestamp: bool,
    timestamp_format: String,
    position: Position,
    font_size: f32,
    font: Font,
    cache_key: Option<String>,
    cache_mask: Option<TextMask>,
    rasterizations: u64,
}

impl Watermark {
    /// Build from config. `font_path` failures fall back to the embedded
    /// ASCII font with a warning (fail-open); only a broken embedded font
    /// (impossible in practice) produces an error.
    pub fn new(cfg: &WatermarkConfig) -> Result<Self, WatermarkError> {
        let font = if cfg.font_path.is_empty() {
            Font::from_bytes(EMBEDDED_FONT, fontdue::FontSettings::default())
                .map_err(|e| WatermarkError::FontLoad(format!("embedded font: {e}")))?
        } else {
            match std::fs::read(&cfg.font_path) {
                Ok(data) => match Font::from_bytes(&data[..], fontdue::FontSettings::default()) {
                    Ok(f) => f,
                    Err(e) => {
                        log::warn!(
                            "watermark: font_path '{}' unparseable ({e}) — falling back to embedded ASCII font",
                            cfg.font_path
                        );
                        Font::from_bytes(EMBEDDED_FONT, fontdue::FontSettings::default())
                            .map_err(|e| WatermarkError::FontLoad(format!("embedded font: {e}")))?
                    }
                },
                Err(e) => {
                    log::warn!(
                        "watermark: font_path '{}' unreadable ({e}) — falling back to embedded ASCII font",
                        cfg.font_path
                    );
                    Font::from_bytes(EMBEDDED_FONT, fontdue::FontSettings::default())
                        .map_err(|e| WatermarkError::FontLoad(format!("embedded font: {e}")))?
                }
            }
        };
        if !cfg.text.is_empty() {
            let missing: Vec<char> = cfg
                .text
                .chars()
                .filter(|&c| !c.is_whitespace() && !font.has_glyph(c))
                .collect();
            if !missing.is_empty() {
                log::warn!(
                    "watermark: font lacks glyphs for {:?} — they will render as missing-glyph boxes; \
                     set watermark.font_path to a font covering them",
                    missing
                );
            }
        }
        Ok(Self {
            text: cfg.text.clone(),
            show_timestamp: cfg.show_timestamp,
            timestamp_format: cfg.timestamp_format.clone(),
            position: cfg.position,
            font_size: cfg.font_size as f32,
            font,
            cache_key: None,
            cache_mask: None,
            rasterizations: 0,
        })
    }

    /// Whether anything would be painted (config validation normally makes
    /// this always true when enabled).
    pub fn has_content(&self) -> bool {
        self.show_timestamp || !self.text.is_empty()
    }

    /// The full composed line for `now`: `text` + two spaces + timestamp,
    /// with either half omitted when disabled/empty.
    fn line(&self, now: &DateTime<Local>) -> String {
        let ts = if self.show_timestamp {
            format_timestamp(&self.timestamp_format, now)
        } else {
            String::new()
        };
        match (self.text.is_empty(), ts.is_empty()) {
            (true, true) => String::new(),
            (true, false) => ts,
            (false, true) => self.text.clone(),
            (false, false) => format!("{}  {}", self.text, ts),
        }
    }

    /// Burn the watermark into an I420 (YU12) planar frame in place.
    ///
    /// `data.len()` must be `width * height * 3 / 2`. The mask is re-rasterized
    /// only when the composed line changes (typically once per second).
    pub fn render_into(&mut self, data: &mut [u8], width: usize, height: usize) {
        if !self.has_content() {
            return;
        }
        let line = self.line(&Local::now());
        if line.is_empty() {
            return;
        }
        if self.cache_key.as_deref() != Some(line.as_str()) {
            self.cache_mask = Some(rasterize_line(&self.font, &line, self.font_size));
            self.cache_key = Some(line);
            self.rasterizations += 1;
        }
        let Some(mask) = &self.cache_mask else {
            return;
        };
        blit_mask(data, width, height, mask, self.position);
    }

    /// Number of rasterizations performed (cache efficiency observable in tests).
    #[cfg(test)]
    fn rasterization_count(&self) -> u64 {
        self.rasterizations
    }
}

/// Alpha-composite `fg` over the current value `bg` (both 0..=255 luma),
/// rounding to nearest.
fn blend_over(bg: u8, fg: u8, alpha: u8) -> u8 {
    let bg = bg as u32;
    let fg = fg as u32;
    let a = alpha as u32;
    ((bg * (255 - a) + fg * a + 127) / 255) as u8
}

/// Paint `mask` into an I420 frame at the position-derived origin, clipping
/// at frame edges. Anti-aliased: luma is alpha-composited background → black
/// stroke → white text; each touched 2×2 chroma block is blended toward
/// neutral with the block's max coverage.
fn blit_mask(data: &mut [u8], width: usize, height: usize, mask: &TextMask, position: Position) {
    if mask.w == 0 || mask.h == 0 || width == 0 || height == 0 {
        return;
    }
    let expected = width * height * 3 / 2;
    if data.len() < expected {
        return;
    }
    let (x0, y0) = origin(position, width, height, mask.w, mask.h);
    let chroma_stride = width / 2;
    let u_plane = width * height;
    let v_plane = u_plane + chroma_stride * (height / 2);

    // Luma pass: stroke first, glyph on top — order-independent where they
    // overlap (the stroke also covers the glyph interior).
    for my in 0..mask.h {
        let dy = y0 + my;
        if dy >= height {
            break;
        }
        for mx in 0..mask.w {
            let dx = x0 + mx;
            if dx >= width {
                break;
            }
            let stroke = mask.outline[my * mask.w + mx];
            let glyph = mask.fill[my * mask.w + mx];
            if stroke < MIN_ALPHA && glyph < MIN_ALPHA {
                continue;
            }
            let idx = dy * width + dx;
            let mut y = data[idx];
            if stroke >= MIN_ALPHA {
                y = blend_over(y, Y_OUTLINE, stroke);
            }
            if glyph >= MIN_ALPHA {
                y = blend_over(y, Y_TEXT, glyph);
            }
            data[idx] = y;
        }
    }

    // Chroma pass: one blend per covered 2×2 cell, using the cell's max
    // coverage so the (subsampling) color wash matches the perceived ink.
    let cells_y = ((y0 + mask.h).min(height)).div_ceil(2);
    let cells_x = ((x0 + mask.w).min(width)).div_ceil(2);
    for cy in (y0 / 2)..cells_y {
        for cx in (x0 / 2)..cells_x {
            let mut coverage = 0_u8;
            for dy in (2 * cy)..(2 * cy + 2) {
                for dx in (2 * cx)..(2 * cx + 2) {
                    let my = dy.saturating_sub(y0);
                    let mx = dx.saturating_sub(x0);
                    if my >= mask.h || mx >= mask.w || dy >= height || dx >= width {
                        continue;
                    }
                    let a = mask.outline[my * mask.w + mx].max(mask.fill[my * mask.w + mx]);
                    coverage = coverage.max(a);
                }
            }
            if coverage < MIN_ALPHA {
                continue;
            }
            let cidx = cy * chroma_stride + cx;
            data[u_plane + cidx] = blend_over(data[u_plane + cidx], UV_NEUTRAL, coverage);
            data[v_plane + cidx] = blend_over(data[v_plane + cidx], UV_NEUTRAL, coverage);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str, show_ts: bool, position: Position, font_size: u32) -> WatermarkConfig {
        WatermarkConfig {
            enabled: true,
            text: text.to_string(),
            show_timestamp: show_ts,
            timestamp_format: "%Y-%m-%d %H:%M:%S".to_string(),
            position,
            font_size,
            font_path: String::new(),
        }
    }

    /// 64×32 I420 frame: mid-gray luma, non-neutral chroma so paints stand out.
    fn frame(w: usize, h: usize) -> Vec<u8> {
        let mut data = vec![128_u8; w * h * 3 / 2];
        let (u_off, v_off) = (w * h, w * h + (w / 2) * (h / 2));
        for b in &mut data[u_off..u_off + (w / 2) * (h / 2)] {
            *b = 90;
        }
        for b in &mut data[v_off..] {
            *b = 180;
        }
        data
    }

    fn wm_or_fail(cfg: &WatermarkConfig) -> Watermark {
        Watermark::new(cfg).expect("embedded font always loads")
    }

    #[test]
    fn timestamp_format_whitelist() {
        assert!(valid_timestamp_format("%Y-%m-%d %H:%M:%S"));
        assert!(valid_timestamp_format("%F %T"));
        assert!(valid_timestamp_format("cam %% %Y"));
        assert!(valid_timestamp_format("literal text"));
        assert!(!valid_timestamp_format("%y")); // 2-digit year not allowed
        assert!(!valid_timestamp_format("%Q"));
        assert!(!valid_timestamp_format("100%"));
        assert!(!valid_timestamp_format("trailing %"));
    }

    #[test]
    fn format_timestamp_uses_chrono() {
        let dt = Local::now();
        let s = format_timestamp("%Y-%m-%d %H:%M:%S", &dt);
        assert_eq!(s.len(), 19);
        assert_eq!(s.as_bytes()[4], b'-');
        assert_eq!(s.as_bytes()[10], b' ');
    }

    #[test]
    fn blit_paints_antialiased_white_text_with_black_outline() {
        let mut data = frame(64, 32);
        let mask = rasterize_line(
            &wm_or_fail(&config("ABC", false, Position::TopLeft, 16)).font,
            "ABC",
            16.0,
        );
        assert!(mask.painted() > 0);
        // The mask itself carries anti-aliased coverage: interior ≈ opaque,
        // edges partial.
        assert!(
            mask.fill.contains(&255),
            "glyph interior must reach full coverage"
        );
        assert!(
            mask.fill.iter().any(|&a| (16..255).contains(&a)),
            "glyph edges must carry partial coverage (anti-aliasing)"
        );
        blit_mask(&mut data, 64, 32, &mask, Position::TopLeft);
        let luma = &data[..64 * 32];
        // Strong white text cores…
        assert!(
            luma.iter().any(|&y| y >= 200),
            "no white text pixels painted"
        );
        // …strong black outline…
        assert!(
            luma.iter().any(|&y| y <= 60),
            "no black outline pixels painted"
        );
        // …and anti-aliased in-between pixels (gray 128 background blended).
        assert!(
            luma.iter().any(|&y| (100..200).contains(&y)),
            "no anti-aliased transition pixels"
        );
        // Untouched region (bottom-right corner) keeps the gray background.
        assert_eq!(data[31 * 64 + 63], 128);
    }

    #[test]
    fn blit_blends_chroma_of_painted_blocks_only() {
        let mut data = frame(64, 32);
        let mask = rasterize_line(
            &wm_or_fail(&config("ABC", false, Position::TopLeft, 16)).font,
            "ABC",
            16.0,
        );
        blit_mask(&mut data, 64, 32, &mask, Position::TopLeft);
        let (u_off, v_off) = (64 * 32, 64 * 32 + 32 * 16);
        // Covered blocks moved (partially) toward neutral 128 from 90…
        let blended = (0..32 * 16)
            .filter(|&i| (91..128).contains(&data[u_off + i]))
            .count();
        assert!(blended > 0, "no chroma blocks blended toward neutral");
        // …and at least one is strongly inked.
        assert!(
            (0..32 * 16).any(|i| data[u_off + i] >= 120),
            "no strongly-covered chroma block"
        );
        // Last chroma block (row 15 of 16, col 31 of 32) must stay untouched.
        assert_eq!(
            data[v_off + 15 * 32 + 31],
            180,
            "far chroma block must stay untouched"
        );
    }

    #[test]
    fn blit_positions_and_clamps() {
        for pos in [
            Position::TopLeft,
            Position::TopRight,
            Position::BottomLeft,
            Position::BottomRight,
        ] {
            let mut data = frame(64, 32);
            let mask = rasterize_line(
                &wm_or_fail(&config("ABC", false, pos, 16)).font,
                "ABC",
                16.0,
            );
            blit_mask(&mut data, 64, 32, &mask, pos);
            let painted = data[..64 * 32].iter().any(|&y| y >= 200);
            assert!(painted, "{pos:?} painted nothing");
            match pos {
                Position::TopLeft => {
                    assert_eq!(data[16 * 64 + 16], 128, "background inside top-left margin")
                }
                Position::TopRight => assert_eq!(data[16 * 64 + 16], 128),
                Position::BottomLeft => assert_eq!(data[63], 128, "top rows untouched"),
                Position::BottomRight => assert_eq!(data[63], 128),
            }
        }
        // Oversized mask on a tiny frame must clip without panicking.
        let mut data = frame(8, 8);
        let mask = rasterize_line(
            &wm_or_fail(&config("ABC", false, Position::TopLeft, 96)).font,
            "ABC",
            96.0,
        );
        blit_mask(&mut data, 8, 8, &mask, Position::TopRight);
    }

    #[test]
    fn render_caches_mask_until_line_changes() {
        // Static text → constant line → exactly one rasterization ever.
        let mut wm = wm_or_fail(&config("FIXED", false, Position::TopLeft, 16));
        let mut data = frame(64, 32);
        wm.render_into(&mut data, 64, 32);
        wm.render_into(&mut data, 64, 32);
        wm.render_into(&mut data, 64, 32);
        assert_eq!(
            wm.rasterization_count(),
            1,
            "constant line must rasterize once"
        );
        // Forcing a different cache key re-rasterizes exactly once more.
        wm.cache_key = Some("different".to_string());
        wm.render_into(&mut data, 64, 32);
        assert_eq!(wm.rasterization_count(), 2);
    }

    #[test]
    fn render_paints_timestamp() {
        let mut wm = wm_or_fail(&config("", true, Position::TopLeft, 16));
        let mut data = frame(64, 32);
        wm.render_into(&mut data, 64, 32);
        assert!(
            data[..64 * 32].iter().any(|&y| y >= 200),
            "timestamp not painted"
        );
    }

    #[test]
    fn no_content_is_noop() {
        let mut wm = wm_or_fail(&config("", false, Position::TopLeft, 16));
        assert!(!wm.has_content());
        let mut data = frame(64, 32);
        let before = data.clone();
        wm.render_into(&mut data, 64, 32);
        assert_eq!(data, before, "frame must be untouched without content");
    }

    #[test]
    fn cjk_text_renders_with_cjk_font() {
        let cfg = WatermarkConfig {
            font_path: concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/assets/fonts/NotoSansSC-Common.otf"
            )
            .to_string(),
            ..config("水印测试", false, Position::TopLeft, 24)
        };
        let wm = wm_or_fail(&cfg);
        let mask = rasterize_line(&wm.font, "水印测试", 24.0);
        assert!(mask.painted() > 0, "CJK glyphs produced no pixels");
    }

    #[test]
    fn missing_font_path_falls_back_to_embedded() {
        let cfg = WatermarkConfig {
            font_path: "/nonexistent/font.ttf".to_string(),
            ..config("ABC", false, Position::TopLeft, 16)
        };
        let mut wm = wm_or_fail(&cfg);
        let mut data = frame(64, 32);
        wm.render_into(&mut data, 64, 32);
        assert!(
            data[..64 * 32].iter().any(|&y| y >= 200),
            "fallback embedded font painted nothing"
        );
    }

    #[test]
    fn short_buffer_is_skipped_not_panicked() {
        let mut wm = wm_or_fail(&config("ABC", false, Position::TopLeft, 16));
        let mut short = vec![0_u8; 10];
        wm.render_into(&mut short, 64, 32);
    }

    /// Renders a synthetic 640×360 frame with the watermark to a PPM in
    /// `tmp/` for human visual inspection (not part of the regular suite —
    /// run with `cargo test --lib watermark -- --ignored` from the repo
    /// root).
    #[test]
    #[ignore]
    fn preview_writes_ppm() {
        let cfg = config("MiBee 104 前门", true, Position::TopLeft, 24);
        let cfg = WatermarkConfig {
            font_path: concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/assets/fonts/NotoSansSC-Common.otf"
            )
            .to_string(),
            ..cfg
        };
        let mut wm = wm_or_fail(&cfg);
        let (w, h) = (640_usize, 360_usize);
        // Noisy gray-blue background so both white and black inks are
        // exercised against a realistic mid-tone.
        let mut data = vec![0_u8; w * h * 3 / 2];
        for y in 0..h {
            for x in 0..w {
                data[y * w + x] = 100 + ((x * 7 + y * 13) % 40) as u8;
            }
        }
        for b in &mut data[w * h..] {
            *b = 110;
        }
        wm.render_into(&mut data, w, h);

        // YUV→RGB (BT.601 full swing) and write a binary PPM.
        let mut rgb = Vec::with_capacity(w * h * 3);
        let (u_off, v_off) = (w * h, w * h + (w / 2) * (h / 2));
        for y in 0..h {
            for x in 0..w {
                let yy = data[y * w + x] as f32;
                let u = data[u_off + (y / 2) * (w / 2) + x / 2] as f32 - 128.0;
                let v = data[v_off + (y / 2) * (w / 2) + x / 2] as f32 - 128.0;
                let r = (yy + 1.402 * v).clamp(0.0, 255.0) as u8;
                let g = (yy - 0.344 * u - 0.714 * v).clamp(0.0, 255.0) as u8;
                let b = (yy + 1.772 * u).clamp(0.0, 255.0) as u8;
                rgb.extend_from_slice(&[r, g, b]);
            }
        }
        let out = format!("P6\n{w} {h}\n255\n", w = w, h = h);
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tmp/watermark-preview.ppm");
        std::fs::write(path, out.as_bytes()).unwrap();
        std::fs::write(path, [out.as_bytes(), &rgb].concat()).unwrap();
    }
}
