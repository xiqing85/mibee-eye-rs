//! GFL (Generalized Focal Loss) post-processing for NanoDet ONNX models.
//!
//! Decodes the raw ONNX output tensor into [`Detection`]s following NanoDet's
//! ncnn reference (`demo_ncnn/nanodet.cpp`):
//!
//! 1. **Classification** — the first `NUM_CLASSES` channels per point are
//!    class scores; argmax picks the label. The `nanodet-plus-m_320.onnx`
//!    export folds the sigmoid into the graph (verified via graph inspection),
//!    so scores are already probabilities and no extra sigmoid is applied.
//! 2. **GFL regression** — the remaining `4 * (REG_MAX + 1)` channels hold a
//!    discrete distance distribution per box side (l, t, r, b). The expected
//!    distance is `dot(softmax(bins), [0..=REG_MAX])` in stride units.
//! 3. **Bbox** — `x1 = (grid_x - l) * stride`, `y1 = (grid_y - t) * stride`,
//!    `x2 = (grid_x + r) * stride`, `y2 = (grid_y + b) * stride`, clamped to
//!    the input frame.
//! 4. **NMS** — per-class non-maximum suppression at IoU 0.5.
//!
//! # Model output layout
//!
//! NanoDet exports output one point-major tensor (`[1, points, 112]`) whose
//! `points` follows the input size (see [`Grid`]): the 320 export emits
//! 2125 points (40² + 20² + 10² + 5² at strides [8, 16, 32, 64]), the 416
//! export 3598 (52² + 26² + 13² + 7²). 112 channels = 80 classes +
//! 32 regression (4 sides × 8 bins).

use crate::features::ai::Detection;
use crate::features::FeatureError;

/// Number of COCO object classes.
const NUM_CLASSES: usize = 80;
/// Maximum bin index of the GFL regression distribution (8 bins: 0..=7).
const REG_MAX: usize = 7;
/// Number of distance bins per box side.
const NUM_BINS: usize = REG_MAX + 1;
/// Regression channels: 4 sides × 8 bins.
const NUM_REGRESSION: usize = 4 * NUM_BINS;
/// Total channels per grid point.
const NUM_CHANNELS: usize = NUM_CLASSES + NUM_REGRESSION;
/// Feature-map strides of the FPN levels.
const STRIDES: [u32; 4] = [8, 16, 32, 64];
/// NMS intersection-over-union threshold.
const NMS_IOU_THRESHOLD: f32 = 0.5;

/// FPN grid geometry derived from the model's (square) input size.
///
/// Each stride level contributes `ceil(input / stride)²` points, ordered
/// level-major (stride 8 first) and row-major within a level — matching
/// NanoDet's `generate_grid_center_priors`. Deriving the layout from the
/// input size (instead of hardcoding the 320 export's 2125 points) is what
/// lets one decoder serve every same-family model (320, 416, …).
#[derive(Debug, Clone)]
pub struct Grid {
    /// Square model input size this grid belongs to.
    pub input: u32,
    sizes: [usize; 4],
    offsets: [usize; 4],
    points: usize,
}

impl Grid {
    /// Build the grid layout for a square model input of `input` pixels.
    #[must_use]
    pub fn for_input(input: u32) -> Self {
        let sizes = [8usize, 16, 32, 64].map(|s| input.div_ceil(s as u32) as usize);
        let mut offsets = [0usize; 4];
        let mut acc = 0;
        for i in 0..4 {
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

    /// Total points across all levels (the ONNX output's dim 1).
    #[must_use]
    pub fn num_points(&self) -> usize {
        self.points
    }

    /// Map a flat point index to its (level, stride, grid_x, grid_y).
    fn coords(&self, point_idx: usize) -> (usize, u32, usize, usize) {
        let level = self
            .offsets
            .iter()
            .rposition(|&offset| offset <= point_idx)
            .unwrap_or(0);
        let local = point_idx - self.offsets[level];
        let grid_w = self.sizes[level];
        (level, STRIDES[level], local % grid_w, local / grid_w)
    }
}

/// COCO 80-class labels in model output order (index = class id).
/// The COCO label for a class index (shared by every decoder family).
pub(crate) fn coco_label(idx: usize) -> &'static str {
    COCO_LABELS.get(idx).copied().unwrap_or("unknown")
}

const COCO_LABELS: [&str; NUM_CLASSES] = [
    "person",
    "bicycle",
    "car",
    "motorcycle",
    "airplane",
    "bus",
    "train",
    "truck",
    "boat",
    "traffic light",
    "fire hydrant",
    "stop sign",
    "parking meter",
    "bench",
    "bird",
    "cat",
    "dog",
    "horse",
    "sheep",
    "cow",
    "elephant",
    "bear",
    "zebra",
    "giraffe",
    "backpack",
    "umbrella",
    "handbag",
    "tie",
    "suitcase",
    "frisbee",
    "skis",
    "snowboard",
    "sports ball",
    "kite",
    "baseball bat",
    "baseball glove",
    "skateboard",
    "surfboard",
    "tennis racket",
    "bottle",
    "wine glass",
    "cup",
    "fork",
    "knife",
    "spoon",
    "bowl",
    "banana",
    "apple",
    "sandwich",
    "orange",
    "broccoli",
    "carrot",
    "hot dog",
    "pizza",
    "donut",
    "cake",
    "chair",
    "couch",
    "potted plant",
    "bed",
    "dining table",
    "toilet",
    "tv",
    "laptop",
    "mouse",
    "remote",
    "keyboard",
    "cell phone",
    "microwave",
    "oven",
    "toaster",
    "sink",
    "refrigerator",
    "book",
    "clock",
    "vase",
    "scissors",
    "teddy bear",
    "hair drier",
    "toothbrush",
];

/// A decoded detection candidate before NMS.
#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    pub(crate) label: usize,
    pub(crate) confidence: f32,
    pub(crate) x1: f32,
    pub(crate) y1: f32,
    pub(crate) x2: f32,
    pub(crate) y2: f32,
}

/// Decode the ONNX output tensor into detections.
///
/// # Arguments
///
/// * `output` - Flat output tensor data, `NUM_POINTS × NUM_CHANNELS` floats in
///   point-major layout (`[1, 2125, 112]`).
/// * `confidence_threshold` - Minimum class score for a candidate to survive
///   the pre-NMS filter.
///
/// # Errors
///
/// Returns `FeatureError::Runtime` if the output length does not match the
/// expected `NUM_POINTS × NUM_CHANNELS`.
pub fn postprocess(
    output: &[f32],
    grid: &Grid,
    confidence_threshold: f32,
) -> Result<Vec<Detection>, FeatureError> {
    let expected = grid.num_points() * NUM_CHANNELS;
    if output.len() != expected {
        return Err(FeatureError::Runtime(format!(
            "Unexpected ONNX output length: got {}, expected {} ({} points × {} channels for input {})",
            output.len(),
            expected,
            grid.num_points(),
            NUM_CHANNELS,
            grid.input
        )));
    }

    let mut candidates: Vec<Candidate> = Vec::new();

    for (point_idx, point) in output.as_chunks::<NUM_CHANNELS>().0.iter().enumerate() {
        let (_level, stride, grid_x, grid_y) = grid.coords(point_idx);

        // Classification: argmax over the class channels (already sigmoid'd).
        let mut label = 0usize;
        let mut confidence = point[0];
        for (class, &score) in point[..NUM_CLASSES].iter().enumerate().skip(1) {
            if score > confidence {
                label = class;
                confidence = score;
            }
        }
        if confidence < confidence_threshold {
            continue;
        }

        // GFL regression: expected distance per side in stride units.
        let distances = decode_distances(&point[NUM_CLASSES..]);

        // Bbox anchored at the grid cell top-left corner, clamped to the frame.
        let x1 = (grid_x as f32 - distances[0]) * stride as f32;
        let y1 = (grid_y as f32 - distances[1]) * stride as f32;
        let x2 = (grid_x as f32 + distances[2]) * stride as f32;
        let y2 = (grid_y as f32 + distances[3]) * stride as f32;

        candidates.push(Candidate {
            label,
            confidence,
            x1: x1.max(0.0),
            y1: y1.max(0.0),
            x2: x2.min(grid.input as f32),
            y2: y2.min(grid.input as f32),
        });
    }

    // Sort by confidence descending, then apply per-class NMS.
    candidates.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
    let kept = nms(&candidates, NMS_IOU_THRESHOLD);

    Ok(kept
        .into_iter()
        .map(|c| Detection {
            label: COCO_LABELS[c.label].to_string(),
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

/// Scale detections from model-input pixel space back to the source video
/// frame's pixel space.
///
/// [`crate::ai::preprocess::preprocess`] stretches the source frame into the
/// (square) model input without letterboxing, so x and y carry independent
/// scale factors (e.g. 1280×720 → 320×320 scales x by 4.0 but y by 2.25).
/// The inverse mapping must be applied before bboxes leave the detector:
/// the Web API contract (SPEC §4.6) is **video pixel coordinates**.
///
/// Boxes are clamped to the frame bounds so rounding cannot push an edge
/// one pixel outside.
pub fn scale_detections_to_frame(
    detections: Vec<Detection>,
    model_w: u32,
    model_h: u32,
    frame_w: u32,
    frame_h: u32,
) -> Vec<Detection> {
    if model_w == 0 || model_h == 0 || frame_w == 0 || frame_h == 0 {
        return detections;
    }
    let scale_x = frame_w as f32 / model_w as f32;
    let scale_y = frame_h as f32 / model_h as f32;
    detections
        .into_iter()
        .map(|mut det| {
            let (x, y, w, h) = det.bbox;
            let x = ((x as f32 * scale_x).round() as u32).min(frame_w.saturating_sub(1));
            let y = ((y as f32 * scale_y).round() as u32).min(frame_h.saturating_sub(1));
            let w = ((w as f32 * scale_x).round() as u32).min(frame_w - x);
            let h = ((h as f32 * scale_y).round() as u32).min(frame_h - y);
            det.bbox = (x, y, w, h);
            det
        })
        .collect()
}

/// Decode the 4 box-side distances (l, t, r, b) from the regression channels.
///
/// Each side has `NUM_BINS` channels forming a discrete distribution over
/// distances `0..=REG_MAX` (in stride units). The expected distance is the
/// softmax-weighted sum: `sum(j * softmax(bins)[j])`.
fn decode_distances(regression: &[f32]) -> [f32; 4] {
    let mut distances = [0.0f32; 4];
    for (side, distance) in distances.iter_mut().enumerate() {
        let bins = &regression[side * NUM_BINS..(side + 1) * NUM_BINS];
        let weights = softmax(bins);
        *distance = weights
            .iter()
            .enumerate()
            .map(|(bin, &weight)| bin as f32 * weight)
            .sum();
    }
    distances
}

/// Numerically stable softmax over a slice.
fn softmax(values: &[f32]) -> Vec<f32> {
    let max = values.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let exps: Vec<f32> = values.iter().map(|&v| (v - max).exp()).collect();
    let sum: f32 = exps.iter().sum();
    exps.iter().map(|&e| e / sum).collect()
}

/// Per-class non-maximum suppression.
///
/// Candidates must be sorted by confidence descending. A candidate is kept if
/// its IoU with every already-kept candidate of the same class is ≤ threshold.
pub(crate) fn nms(candidates: &[Candidate], iou_threshold: f32) -> Vec<Candidate> {
    let mut kept: Vec<Candidate> = Vec::new();
    for candidate in candidates {
        let suppressed = kept
            .iter()
            .any(|k| k.label == candidate.label && iou(k, candidate) > iou_threshold);
        if !suppressed {
            kept.push(candidate.clone());
        }
    }
    kept
}

/// Intersection-over-union of two boxes.
fn iou(a: &Candidate, b: &Candidate) -> f32 {
    let inter_w = a.x2.min(b.x2) - a.x1.max(b.x1);
    let inter_h = a.y2.min(b.y2) - a.y1.max(b.y1);
    if inter_w <= 0.0 || inter_h <= 0.0 {
        return 0.0;
    }
    let inter = inter_w * inter_h;
    let union = (a.x2 - a.x1) * (a.y2 - a.y1) + (b.x2 - b.x1) * (b.y2 - b.y1) - inter;
    inter / union
}

#[cfg(test)]
mod tests {
    use super::*;

    mod scale_to_frame {
        use super::*;

        /// Real-world case: 1280×720 frame stretched into a 320×320 model
        /// input (x scale 4.0, y scale 2.25). A model-space box must map to
        /// video pixels, not be returned raw.
        #[test]
        fn test_maps_model_box_to_video_pixels() {
            let dets = vec![Detection {
                label: "chair".to_string(),
                confidence: 0.65,
                bbox: (272, 272, 48, 48),
            }];
            let scaled = scale_detections_to_frame(dets, 320, 320, 1280, 720);
            assert_eq!(scaled[0].bbox, (1088, 612, 192, 108));
        }

        #[test]
        fn test_identity_when_sizes_match() {
            let dets = vec![Detection {
                label: "person".to_string(),
                confidence: 0.9,
                bbox: (10, 20, 30, 40),
            }];
            let scaled = scale_detections_to_frame(dets, 320, 320, 320, 320);
            assert_eq!(scaled[0].bbox, (10, 20, 30, 40));
        }

        #[test]
        fn test_clamps_to_frame_bounds() {
            let dets = vec![Detection {
                label: "person".to_string(),
                confidence: 0.9,
                bbox: (300, 300, 30, 30),
            }];
            let scaled = scale_detections_to_frame(dets, 320, 320, 1280, 720);
            // 300*2.25 = 675, +30*2.25 = 67.5 → 742.5 > 720 must clamp.
            let (x, y, w, h) = scaled[0].bbox;
            assert_eq!((x, y), (1200, 675));
            assert!(y + h <= 720, "y+h={}", y + h);
            assert!(x + w <= 1280, "x+w={}", x + w);
        }

        #[test]
        fn test_preserves_label_and_confidence() {
            let dets = vec![Detection {
                label: "potted plant".to_string(),
                confidence: 0.42,
                bbox: (0, 0, 1, 1),
            }];
            let scaled = scale_detections_to_frame(dets, 320, 320, 640, 480);
            assert_eq!(scaled[0].label, "potted plant");
            assert!((scaled[0].confidence - 0.42).abs() < 1e-6);
        }
    }

    /// Build a synthetic output tensor encoding one known detection.
    ///
    /// Class 0 ("person") at grid (x=20, y=20) on the stride-8 level
    /// (point index 20 * 40 + 20 = 820). Regression bins are one-hot at
    /// distances (l=2, t=3, r=4, b=5) in stride units, so the decoded bbox is
    /// `((20-2)*8, (20-3)*8, (20+4)*8, (20+5)*8)` = (144, 136, 192, 200).
    fn synthetic_output_with_detection() -> Vec<f32> {
        let grid = Grid::for_input(320);
        let mut output = vec![0.0f32; grid.num_points() * NUM_CHANNELS];
        let point = 20 * 40 + 20;
        let base = point * NUM_CHANNELS;

        // Classification: class 0 scores 0.9, all others 0.01.
        output[base] = 0.9;
        for class in 1..NUM_CLASSES {
            output[base + class] = 0.01;
        }

        // Regression: a large logit on the target bin makes softmax ≈ one-hot.
        for (side, distance) in [2usize, 3, 4, 5].iter().enumerate() {
            output[base + NUM_CLASSES + side * NUM_BINS + distance] = 20.0;
        }
        output
    }

    #[test]
    fn test_synthetic_detection_decodes_bbox_within_2px() {
        let output = synthetic_output_with_detection();
        let grid = Grid::for_input(320);
        let detections = postprocess(&output, &grid, 0.4).expect("postprocess should succeed");

        assert_eq!(detections.len(), 1, "exactly one detection expected");
        let detection = &detections[0];
        assert_eq!(detection.label, "person");
        assert!((detection.confidence - 0.9).abs() < 1e-6);

        let (x, y, w, h) = detection.bbox;
        assert!((x as i32 - 144).abs() <= 2, "x={x}, expected 144");
        assert!((y as i32 - 136).abs() <= 2, "y={y}, expected 136");
        assert!((w as i32 - 48).abs() <= 2, "w={w}, expected 48");
        assert!((h as i32 - 64).abs() <= 2, "h={h}, expected 64");
    }

    #[test]
    fn test_all_zero_output_yields_no_detections() {
        let grid = Grid::for_input(320);
        let output = vec![0.0f32; grid.num_points() * NUM_CHANNELS];
        let detections = postprocess(&output, &grid, 0.4).expect("postprocess should succeed");
        assert!(detections.is_empty());
    }

    #[test]
    fn test_short_output_returns_error() {
        let grid = Grid::for_input(320);
        let output = vec![0.0f32; grid.num_points() * NUM_CHANNELS - 1];
        let result = postprocess(&output, &grid, 0.4);
        match result {
            Err(FeatureError::Runtime(msg)) => {
                assert!(msg.contains("Unexpected ONNX output length"));
            }
            other => panic!("expected Runtime error, got {other:?}"),
        }
    }

    #[test]
    fn test_grid_for_input_320_matches_legacy_constants() {
        let grid = Grid::for_input(320);
        assert_eq!(grid.num_points(), 2125);
        // Level boundaries of the classic 320 export: stride-8 level owns
        // [0, 1600), stride-16 [1600, 2000), stride-32 [2000, 2100), 64 the rest.
        assert_eq!(grid.coords(0), (0, 8, 0, 0));
        assert_eq!(grid.coords(1599), (0, 8, 39, 39));
        assert_eq!(grid.coords(1600), (1, 16, 0, 0));
        assert_eq!(grid.coords(2000), (2, 32, 0, 0));
        assert_eq!(grid.coords(2100), (3, 64, 0, 0));
        assert_eq!(grid.coords(2124), (3, 64, 4, 4));
    }

    #[test]
    fn test_grid_for_input_416_matches_onnx_export() {
        // The nanodet-plus-m_416.onnx export emits [1, 3598, 112]:
        // ceil(416/8)² + ceil(416/16)² + ceil(416/32)² + ceil(416/64)²
        // = 52² + 26² + 13² + 7² = 3598.
        let grid = Grid::for_input(416);
        assert_eq!(grid.num_points(), 3598);
        assert_eq!(grid.coords(0), (0, 8, 0, 0));
        assert_eq!(grid.coords(52 * 52 - 1), (0, 8, 51, 51));
        assert_eq!(grid.coords(52 * 52), (1, 16, 0, 0));
        assert_eq!(grid.coords(52 * 52 + 26 * 26), (2, 32, 0, 0));
        assert_eq!(grid.coords(52 * 52 + 26 * 26 + 13 * 13), (3, 64, 0, 0));
        assert_eq!(grid.coords(3597), (3, 64, 6, 6));

        // A 320-length tensor must NOT decode as a 416 grid (and vice versa).
        let wrong = vec![0.0f32; 2125 * NUM_CHANNELS];
        assert!(postprocess(&wrong, &grid, 0.4).is_err());
        let right = vec![0.0f32; 3598 * NUM_CHANNELS];
        assert!(postprocess(&right, &grid, 0.4).is_ok());
    }

    #[test]
    fn test_synthetic_detection_decodes_on_416_grid() {
        // Same one-hot construction on the stride-8 level of the 416 grid:
        // grid (x=30, y=25) → point 25 * 52 + 30; distances (2,3,4,5)
        // → bbox ((30-2)*8, (25-3)*8, …) = (224, 176, 50, 64) w/h.
        let grid = Grid::for_input(416);
        let mut output = vec![0.0f32; grid.num_points() * NUM_CHANNELS];
        let point = 25 * 52 + 30;
        let base = point * NUM_CHANNELS;
        output[base] = 0.9;
        for class in 1..NUM_CLASSES {
            output[base + class] = 0.01;
        }
        for (side, distance) in [2usize, 3, 4, 5].iter().enumerate() {
            output[base + NUM_CLASSES + side * NUM_BINS + distance] = 20.0;
        }
        let detections = postprocess(&output, &grid, 0.4).expect("416 grid must decode");
        assert_eq!(detections.len(), 1);
        let (x, y, w, h) = detections[0].bbox;
        assert!((x as i32 - 224).abs() <= 2, "x={x}, expected 224");
        assert!((y as i32 - 176).abs() <= 2, "y={y}, expected 176");
        assert!((w as i32 - 50).abs() <= 2, "w={w}, expected 50");
        assert!((h as i32 - 64).abs() <= 2, "h={h}, expected 64");
    }

    #[test]
    fn test_nms_suppresses_same_class_overlap() {
        let candidates = vec![
            Candidate {
                label: 0,
                confidence: 0.9,
                x1: 10.0,
                y1: 10.0,
                x2: 100.0,
                y2: 100.0,
            },
            Candidate {
                label: 0,
                confidence: 0.8,
                x1: 20.0,
                y1: 20.0,
                x2: 110.0,
                y2: 110.0,
            },
        ];
        let kept = nms(&candidates, 0.5);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].confidence, 0.9);
    }

    #[test]
    fn test_nms_keeps_different_classes() {
        let candidates = vec![
            Candidate {
                label: 0,
                confidence: 0.9,
                x1: 10.0,
                y1: 10.0,
                x2: 100.0,
                y2: 100.0,
            },
            Candidate {
                label: 2,
                confidence: 0.8,
                x1: 20.0,
                y1: 20.0,
                x2: 110.0,
                y2: 110.0,
            },
        ];
        let kept = nms(&candidates, 0.5);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn test_coco_labels() {
        assert_eq!(COCO_LABELS.len(), 80);
        assert_eq!(COCO_LABELS[0], "person");
        assert_eq!(COCO_LABELS[2], "car");
        assert_eq!(COCO_LABELS[16], "dog");
        assert_eq!(COCO_LABELS[79], "toothbrush");
    }
}
