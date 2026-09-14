//! YOLOX family pre/post-processing (registry family `yolox`).
//!
//! Semantics follow the official Megvii ONNX export (yolox_nano.onnx,
//! Apache-2.0), mirroring the YOLOX demo so all MiBee cameras decode
//! identically:
//!
//! - **Preprocess**: YUV420 → RGB, letterbox into the square input
//!   (aspect-preserving resize, pad 114), `/255` normalization, NCHW with
//!   RGB channel order.
//! - **Output**: `[1, points, 85]` raw predictions — 4 box deltas
//!   (grid-relative xy, log-space wh), 1 objectness, 80 class logits.
//!   Grid: 3 FPN levels at strides [8, 16, 32], `floor(input/stride)²`
//!   points each, level-major, row-major within a level.
//! - **Decode**: `x1y1 = (dxy + grid) * stride`, `wh = exp(dwh) * stride`,
//!   `score = sigmoid(obj) * max(sigmoid(cls))`; NMS at IoU 0.45.
//! - **Unmap**: letterbox offsets removed and the aspect scale inverted so
//!   bboxes land in video pixel coordinates (SPEC §4.6).

use crate::ai::postprocess::{nms, Candidate};
use crate::features::ai::Detection;
use crate::features::FeatureError;

/// COCO 80-class count (shares the label table with the NanoDet decoder).
const NUM_CLASSES: usize = 80;
/// Channels per prediction row: 4 box + 1 objectness + 80 classes.
const NUM_CHANNELS: usize = 4 + 1 + NUM_CLASSES;
/// FPN strides of the 3 YOLOX levels.
const STRIDES: [u32; 3] = [8, 16, 32];
/// NMS IoU threshold (YOLOX demo convention).
const NMS_IOU_THRESHOLD: f32 = 0.45;
/// Letterbox padding gray level (YOLOX convention).
const PAD_VALUE: f32 = 114.0;

/// Letterbox geometry for unmapping bboxes back to the source frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Letterbox {
    /// Aspect-preserving scale factor (source → letterboxed content).
    pub scale: f32,
    /// Left padding in input pixels.
    pub pad_x: u32,
    /// Top padding in input pixels.
    pub pad_y: u32,
}

/// FPN grid layout for a YOLOX input size (3 levels, floor division —
/// matches the export, unlike NanoDet's 4-level ceil).
#[derive(Debug, Clone)]
pub struct YoloxGrid {
    /// Square model input size.
    pub input: u32,
    sizes: [usize; 3],
    offsets: [usize; 3],
    points: usize,
}

impl YoloxGrid {
    /// Build the layout for a square input of `input` pixels.
    #[must_use]
    pub fn for_input(input: u32) -> Self {
        let sizes = [8usize, 16, 32].map(|s| input as usize / s);
        let mut offsets = [0usize; 3];
        let mut acc = 0;
        for i in 0..3 {
            offsets[i] = acc;
            acc += sizes[i] * sizes[i];
        }
        Self {
            input,
            sizes,
            offsets,
            points: acc,
        }
    }

    /// Total prediction rows (the ONNX output's dim 1).
    #[must_use]
    pub fn num_points(&self) -> usize {
        self.points
    }

    fn coords(&self, idx: usize) -> (usize, u32, usize, usize) {
        let level = self
            .offsets
            .iter()
            .rposition(|&offset| offset <= idx)
            .unwrap_or(0);
        let local = idx - self.offsets[level];
        let grid_w = self.sizes[level];
        (level, STRIDES[level], local % grid_w, local / grid_w)
    }
}

/// Preprocess a YUV420 frame for YOLOX: RGB letterbox, `/255`, NCHW.
///
/// Returns the input tensor plus the [`Letterbox`] geometry needed to map
/// detections back to the source frame.
///
/// # Errors
///
/// `FeatureError::Runtime` when the frame is too short for YUV420.
pub fn preprocess_yolox(
    frame_data: &[u8],
    src_w: u32,
    src_h: u32,
    input: u32,
) -> Result<(Vec<f32>, Letterbox), FeatureError> {
    let (src_w, src_h, input) = (src_w as usize, src_h as usize, input as usize);
    let y_size = src_w * src_h;
    let uv_size = src_w / 2 * src_h / 2;
    if frame_data.len() < y_size + 2 * uv_size {
        return Err(FeatureError::Runtime(format!(
            "YUV420 frame too short: got {} bytes, need {} ({}x{})",
            frame_data.len(),
            y_size + 2 * uv_size,
            src_w,
            src_h
        )));
    }

    let scale = (input as f32 / src_w as f32).min(input as f32 / src_h as f32);
    let tw = ((src_w as f32 * scale).round() as usize).max(1);
    let th = ((src_h as f32 * scale).round() as usize).max(1);
    let pad_x = (input - tw) / 2;
    let pad_y = (input - th) / 2;

    // YUV → RGB once per content pixel, stored as /255 RGB planes.
    let mut r_plane = vec![PAD_VALUE / 255.0; input * input];
    let mut g_plane = vec![PAD_VALUE / 255.0; input * input];
    let mut b_plane = vec![PAD_VALUE / 255.0; input * input];

    let src_uv_w = src_w / 2;
    for dy in 0..th {
        let sy = ((dy as f32 / scale) as usize).min(src_h - 1);
        let uv_y = sy / 2;
        for dx in 0..tw {
            let sx = ((dx as f32 / scale) as usize).min(src_w - 1);
            let uv_x = sx / 2;
            let y = frame_data[sy * src_w + sx] as f32;
            let u = frame_data[y_size + uv_y * src_uv_w + uv_x] as f32 - 128.0;
            let v = frame_data[y_size + uv_size + uv_y * src_uv_w + uv_x] as f32 - 128.0;

            let r = (y + 1.402 * v).clamp(0.0, 255.0) / 255.0;
            let g = (y - 0.344 * u - 0.714 * v).clamp(0.0, 255.0) / 255.0;
            let b = (y + 1.772 * u).clamp(0.0, 255.0) / 255.0;

            let dst = (dy + pad_y) * input + dx + pad_x;
            r_plane[dst] = r;
            g_plane[dst] = g;
            b_plane[dst] = b;
        }
    }

    // NCHW with RGB channel order (YOLOX convention).
    let mut output = Vec::with_capacity(3 * input * input);
    output.extend_from_slice(&r_plane);
    output.extend_from_slice(&g_plane);
    output.extend_from_slice(&b_plane);

    Ok((
        output,
        Letterbox {
            scale,
            pad_x: pad_x as u32,
            pad_y: pad_y as u32,
        },
    ))
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

/// Decode raw YOLOX predictions into video-pixel detections.
///
/// `frame_w`/`frame_h` are the source frame dimensions; the letterbox
/// geometry maps input-space boxes back through the aspect-preserving
/// resize (SPEC §4.6: bboxes are in native video pixel coordinates).
///
/// # Errors
///
/// `FeatureError::Runtime` when the output length does not match
/// `grid.num_points() * 85`.
pub fn postprocess_yolox(
    output: &[f32],
    grid: &YoloxGrid,
    letterbox: &Letterbox,
    frame_w: u32,
    frame_h: u32,
    confidence_threshold: f32,
) -> Result<Vec<Detection>, FeatureError> {
    let expected = grid.num_points() * NUM_CHANNELS;
    if output.len() != expected {
        return Err(FeatureError::Runtime(format!(
            "Unexpected YOLOX output length: got {}, expected {} ({} points × {} channels for input {})",
            output.len(),
            expected,
            grid.num_points(),
            NUM_CHANNELS,
            grid.input
        )));
    }

    let mut candidates: Vec<Candidate> = Vec::new();
    for (idx, row) in output.as_chunks::<NUM_CHANNELS>().0.iter().enumerate() {
        let (_level, stride, grid_x, grid_y) = grid.coords(idx);

        let obj = sigmoid(row[4]);
        let mut label = 0usize;
        let mut best = sigmoid(row[5]);
        for (class, &logit) in row[6..].iter().enumerate().skip(1) {
            let score = sigmoid(logit);
            if score > best {
                label = class + 1;
                best = score;
            }
        }
        let confidence = obj * best;
        if confidence < confidence_threshold {
            continue;
        }

        // Decode into input-pixel space.
        let x1 = (row[0] + grid_x as f32) * stride as f32;
        let y1 = (row[1] + grid_y as f32) * stride as f32;
        let w = row[2].exp() * stride as f32;
        let h = row[3].exp() * stride as f32;
        let x2 = x1 + w;
        let y2 = y1 + h;
        let (x1, y1, x2, y2) = (
            x1.max(0.0),
            y1.max(0.0),
            x2.min(grid.input as f32),
            y2.min(grid.input as f32),
        );

        // Unmap the letterbox back to the source frame.
        let inv = 1.0 / letterbox.scale;
        let fx1 = ((x1 - letterbox.pad_x as f32) * inv).max(0.0);
        let fy1 = ((y1 - letterbox.pad_y as f32) * inv).max(0.0);
        let fx2 = ((x2 - letterbox.pad_x as f32) * inv).min(frame_w as f32);
        let fy2 = ((y2 - letterbox.pad_y as f32) * inv).min(frame_h as f32);

        candidates.push(Candidate {
            label,
            confidence,
            x1: fx1,
            y1: fy1,
            x2: fx2,
            y2: fy2,
        });
    }

    candidates.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
    let kept = nms(&candidates, NMS_IOU_THRESHOLD);

    Ok(kept
        .into_iter()
        .map(|c| Detection {
            label: crate::ai::postprocess::coco_label(c.label).to_string(),
            confidence: c.confidence,
            bbox: (
                c.x1.round() as u32,
                c.y1.round() as u32,
                (c.x2 - c.x1).round() as u32,
                (c.y2 - c.y1).round() as u32,
            ),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grid_for_input_416_matches_onnx_export() {
        // yolox_nano.onnx emits [1, 3549, 85]: 52² + 26² + 13².
        let grid = YoloxGrid::for_input(416);
        assert_eq!(grid.num_points(), 3549);
        assert_eq!(grid.coords(0), (0, 8, 0, 0));
        assert_eq!(grid.coords(52 * 52 - 1), (0, 8, 51, 51));
        assert_eq!(grid.coords(52 * 52), (1, 16, 0, 0));
        assert_eq!(grid.coords(52 * 52 + 26 * 26), (2, 32, 0, 0));
        assert_eq!(grid.coords(3548), (2, 32, 12, 12));
    }

    #[test]
    fn test_grid_for_input_320() {
        let grid = YoloxGrid::for_input(320);
        assert_eq!(grid.num_points(), 40 * 40 + 20 * 20 + 10 * 10);
        assert_eq!(grid.coords(1600), (1, 16, 0, 0));
    }

    #[test]
    fn test_letterbox_geometry_16by9() {
        // 1280×720 into 416: scale = 416/1280 = 0.325 → content 416×234,
        // centered with 91 px top/bottom padding.
        let (tensor, lb) = preprocess_yolox(&vec![128u8; 1280 * 720 * 3 / 2], 1280, 720, 416)
            .expect("preprocess must succeed");
        assert_eq!(tensor.len(), 3 * 416 * 416);
        assert!((lb.scale - 0.325).abs() < 1e-4);
        assert_eq!(lb.pad_x, 0);
        assert_eq!(lb.pad_y, 91);
        // A padding pixel (top-left corner) is 114/255 in every plane.
        let pad_px = 114.0 / 255.0;
        for plane in 0..3 {
            assert!((tensor[plane * 416 * 416] - pad_px).abs() < 1e-4);
        }
    }

    #[test]
    fn test_letterbox_rgb_and_normalization() {
        // Neutral gray YUV (Y=128, U=V=128) converts to RGB 128 → 128/255.
        let src_w = 4u32;
        let src_h = 4u32;
        let mut frame = vec![0u8; (src_w * src_h * 3 / 2) as usize];
        let y_size = (src_w * src_h) as usize;
        let uv_size = (src_w / 2 * src_h / 2) as usize;
        frame[..y_size].fill(128);
        frame[y_size..y_size + uv_size].fill(128);
        frame[y_size + uv_size..].fill(128);

        let (tensor, lb) = preprocess_yolox(&frame, src_w, src_h, 4).expect("preprocess");
        assert_eq!(lb.pad_x, 0);
        assert_eq!(lb.pad_y, 0);
        let gray = 128.0 / 255.0;
        for plane in 0..3 {
            assert!((tensor[plane * 16] - gray).abs() < 1e-3);
        }
    }

    /// One known prediction on the 416 grid's stride-8 level:
    /// grid (26, 20) → row 20*52+26 = 1066; dxy = 0, dwh = ln 2 →
    /// xy = (208, 160), wh = (16, 16); identity letterbox + 416 frame.
    fn synthetic_row() -> (usize, [f32; 5]) {
        let row = 20 * 52 + 26;
        (row, [0.0, 0.0, 2f32.ln(), 2f32.ln(), 20.0])
    }

    /// Output buffer with every row's objectness suppressed (sigmoid(0)
    /// is 0.5, so zeros alone would decode as low-confidence detections —
    /// a real export never emits that, and tests must not either).
    fn silent_output(grid: &YoloxGrid) -> Vec<f32> {
        let mut output = vec![0.0f32; grid.num_points() * NUM_CHANNELS];
        for row in output.as_chunks_mut::<NUM_CHANNELS>().0 {
            row[4] = -20.0;
        }
        output
    }

    #[test]
    fn test_synthetic_decodes_with_identity_letterbox() {
        let grid = YoloxGrid::for_input(416);
        let mut output = silent_output(&grid);
        let (row, head) = synthetic_row();
        let base = row * NUM_CHANNELS;
        output[base..base + 5].copy_from_slice(&head);
        output[base + 5] = 20.0; // class 0 logit → sigmoid ≈ 1

        let lb = Letterbox {
            scale: 1.0,
            pad_x: 0,
            pad_y: 0,
        };
        let detections = postprocess_yolox(&output, &grid, &lb, 416, 416, 0.05).expect("decode");
        assert_eq!(detections.len(), 1);
        let d = &detections[0];
        assert_eq!(d.label, "person");
        assert!(d.confidence > 0.99);
        let (x, y, w, h) = d.bbox;
        assert_eq!((x, y, w, h), (208, 160, 16, 16));
    }

    #[test]
    fn test_letterbox_unmap_scales_back_to_frame() {
        // Same synthetic box, but decoded through the 1280×720 letterbox:
        // input-space (208..224, 160..176) → /0.325 → ≈(640..689, 492..542).
        let grid = YoloxGrid::for_input(416);
        let mut output = silent_output(&grid);
        let (row, head) = synthetic_row();
        let base = row * NUM_CHANNELS;
        output[base..base + 5].copy_from_slice(&head);
        output[base + 5] = 20.0;

        let lb = Letterbox {
            scale: 0.325,
            pad_x: 0,
            pad_y: 91,
        };
        let detections = postprocess_yolox(&output, &grid, &lb, 1280, 720, 0.05).expect("decode");
        let (x, y, w, h) = detections[0].bbox;
        assert_eq!(x, 640, "208/0.325");
        assert_eq!(y, 212, "(160-91)/0.325");
        assert_eq!(w, 49, "16/0.325");
        assert_eq!(h, 49);
    }

    #[test]
    fn test_wrong_length_rejected() {
        let grid = YoloxGrid::for_input(416);
        let lb = Letterbox {
            scale: 1.0,
            pad_x: 0,
            pad_y: 0,
        };
        let wrong = vec![0.0f32; 2125 * NUM_CHANNELS];
        assert!(
            postprocess_yolox(&wrong, &grid, &lb, 416, 416, 0.05).is_err(),
            "a nanodet-sized tensor must not decode as yolox"
        );
    }

    #[test]
    fn test_objectness_gate_suppresses_low_score() {
        // High class score but objectness −20 (sigmoid ≈ 2e-9) must filter.
        let grid = YoloxGrid::for_input(416);
        let mut output = silent_output(&grid);
        let (row, mut head) = synthetic_row();
        head[4] = -20.0;
        let base = row * NUM_CHANNELS;
        output[base..base + 5].copy_from_slice(&head);
        output[base + 5] = 20.0;

        let lb = Letterbox {
            scale: 1.0,
            pad_x: 0,
            pad_y: 0,
        };
        let detections = postprocess_yolox(&output, &grid, &lb, 416, 416, 0.05).expect("decode");
        assert!(detections.is_empty());
    }
}
