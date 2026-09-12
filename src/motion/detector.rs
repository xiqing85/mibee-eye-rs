use std::time::Instant;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Detection resolution (width).
const DETECT_WIDTH: u32 = 320;

/// Detection resolution (height).
const DETECT_HEIGHT: u32 = 240;

// ---------------------------------------------------------------------------
// GrayFrame
// ---------------------------------------------------------------------------

/// A grayscale frame at detection resolution.
#[derive(Debug, Clone)]
pub struct GrayFrame {
    /// Flat pixel buffer (row‑major, one byte per pixel).
    pub data: Vec<u8>,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

// ---------------------------------------------------------------------------
// MotionEvent
// ---------------------------------------------------------------------------

/// Describes a single motion detection event.
#[derive(Debug, Clone)]
pub struct MotionEvent {
    /// Bounding box of the changed region `(x, y, w, h)` in detection
    /// coordinates (320×240 space).
    pub bbox: (u32, u32, u32, u32),
    /// Fraction of pixels that changed (0.0 – 1.0).
    pub score: f64,
    /// Monotonic timestamp of when the event was emitted.
    pub timestamp: Instant,
}

// ---------------------------------------------------------------------------
// MotionDetector
// ---------------------------------------------------------------------------

/// Frame‑differencing motion detector.
///
/// Processes incoming YUV420 frames, downscales to 320×240, and compares
/// successive grayscale frames to detect motion.  A simple area‑based filter
/// rejects isolated-pixel noise.
///
/// # Performance
///
/// Each call to [`process_frame`](Self::process_frame) allocates a
/// 320×240 = 76 800 byte buffer and runs in O(height × width) time.
/// On a Raspberry Pi 3B this takes < 2 ms.
pub struct MotionDetector {
    /// Detection sensitivity (0.0 – 1.0).  Higher values make the detector
    /// more sensitive to small changes.
    sensitivity: f64,
    /// Minimum number of changed pixels (at detection resolution) required
    /// to trigger an event.  Serves as the primary noise gate.
    min_area: u32,
    /// Minimum wall‑clock time (milliseconds) between consecutive events.
    cooldown_ms: u64,
    /// Timestamp of the last emitted event.
    last_event: Option<Instant>,
    /// Previous grayscale frame (at detection resolution).
    prev_frame: Option<GrayFrame>,
    /// Detection width  (always [`DETECT_WIDTH`]).
    width: u32,
    /// Detection height (always [`DETECT_HEIGHT`]).
    height: u32,
}

impl MotionDetector {
    /// Create a new motion detector.
    ///
    /// * `sensitivity` — Detection threshold 0.0 – 1.0.  Higher values
    ///   detect smaller changes.
    /// * `min_area`    — Minimum number of changed pixels (at 320×240) to
    ///   trigger an event.  Set this higher to reject camera noise.
    /// * `cooldown_ms` — Minimum time between successive events.  Prevents
    ///   event storms while motion is continuous.
    #[must_use]
    pub fn new(sensitivity: f64, min_area: u32, cooldown_ms: u64) -> Self {
        Self {
            sensitivity,
            min_area,
            cooldown_ms,
            last_event: None,
            prev_frame: None,
            width: DETECT_WIDTH,
            height: DETECT_HEIGHT,
        }
    }

    /// Update the sensitivity value (clamped to 0.0 – 1.0).
    pub fn set_sensitivity(&mut self, s: f64) {
        self.sensitivity = s.clamp(0.0, 1.0);
    }

    /// Clear the stored previous frame and event timestamp, resetting the
    /// detector to its initial state.
    pub fn reset(&mut self) {
        self.prev_frame = None;
        self.last_event = None;
    }

    /// Process a YUV420 frame and return a [`MotionEvent`] if motion was
    /// detected.
    ///
    /// The first frame is always stored and returns `None` (no previous
    /// frame to compare against).
    ///
    /// Returns `None` without panicking when `yuv_data` is too small to
    /// contain a full Y plane at `width × height`.
    pub fn process_frame(
        &mut self,
        yuv_data: &[u8],
        width: u32,
        height: u32,
    ) -> Option<MotionEvent> {
        // Guard against degenerate inputs.
        if width == 0 || height == 0 {
            return None;
        }

        let src_size = (width * height) as usize;
        if yuv_data.len() < src_size {
            return None;
        }

        // 1. Downscale Y plane to detection resolution.
        let current = Self::downscale_y_plane(yuv_data, width, height, self.width, self.height);

        // 2. If there is no previous frame, store this one and bail.
        let prev = match self.prev_frame.take() {
            Some(p) => p,
            None => {
                self.prev_frame = Some(GrayFrame {
                    data: current,
                    width: self.width,
                    height: self.height,
                });
                return None;
            }
        };

        // 3. Per‑pixel absolute difference with threshold.
        let threshold = ((1.0 - self.sensitivity) * 255.0) as u8;
        let det_w = self.width;
        let det_h = self.height;

        let mut changed_count: u32 = 0;
        let mut min_x = det_w;
        let mut min_y = det_h;
        let mut max_x: u32 = 0;
        let mut max_y: u32 = 0;

        for (i, (&cur, &prev_px)) in current.iter().zip(prev.data.iter()).enumerate() {
            let diff = cur.abs_diff(prev_px);
            if diff > threshold {
                changed_count += 1;
                let x = (i as u32) % det_w;
                let y = (i as u32) / det_w;
                if x < min_x {
                    min_x = x;
                }
                if y < min_y {
                    min_y = y;
                }
                if x > max_x {
                    max_x = x;
                }
                if y > max_y {
                    max_y = y;
                }
            }
        }

        // 4. Store current frame for the next comparison.
        self.prev_frame = Some(GrayFrame {
            data: current,
            width: self.width,
            height: self.height,
        });

        // 5. Area‑based noise gate.
        if changed_count < self.min_area {
            return None;
        }

        // 6. Cooldown check.
        let now = Instant::now();
        if let Some(last) = self.last_event {
            let elapsed = now.duration_since(last);
            if elapsed.as_millis() < self.cooldown_ms as u128 {
                return None;
            }
        }

        // 7. Build the event.
        let bbox = (
            min_x,
            min_y,
            max_x.saturating_sub(min_x).max(1),
            max_y.saturating_sub(min_y).max(1),
        );
        let score = changed_count as f64 / (det_w * det_h) as f64;

        self.last_event = Some(now);

        Some(MotionEvent {
            bbox,
            score,
            timestamp: now,
        })
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Extract the Y (luma) plane from YUV420 data and downscale it to
    /// `dst_w × dst_h` using nearest‑neighbour sampling.
    fn downscale_y_plane(
        yuv_data: &[u8],
        src_w: u32,
        src_h: u32,
        dst_w: u32,
        dst_h: u32,
    ) -> Vec<u8> {
        let src_size = (src_w * src_h) as usize;
        let y_plane = &yuv_data[..src_size];
        let mut out = vec![0u8; (dst_w * dst_h) as usize];

        // Nearest‑neighbour: divide source by destination to get step.
        let step_x = src_w / dst_w;
        let step_y = src_h / dst_h;

        if step_x == 0 || step_y == 0 {
            // Degenerate case — just copy the first `dst_w × dst_h` pixels.
            let copy_len = out.len().min(y_plane.len());
            out[..copy_len].copy_from_slice(&y_plane[..copy_len]);
            return out;
        }

        for dst_y in 0..dst_h {
            let src_y = (dst_y * step_y) as usize * src_w as usize;
            let dst_row = (dst_y * dst_w) as usize;
            for dst_x in 0..dst_w {
                let src_x = (dst_x * step_x) as usize;
                out[dst_row + dst_x as usize] = y_plane[src_y + src_x];
            }
        }

        out
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    /// Frame dimensions used in tests (source resolution).
    const SRC_W: u32 = 640;
    const SRC_H: u32 = 480;

    /// Size of the Y plane for a 640×480 YUV420 frame.
    const Y_SIZE: usize = (SRC_W * SRC_H) as usize;

    /// Total size of a 640×480 YUV420 frame (Y + U + V planes).
    const YUV_SIZE: usize = Y_SIZE + (Y_SIZE / 2);

    /// Create a YUV420 frame filled entirely with `y_value` for the luma
    /// plane (U and V are zeroed).
    fn make_flat_frame(y_value: u8) -> Vec<u8> {
        let mut buf = vec![0u8; YUV_SIZE];
        // Y plane
        buf[..Y_SIZE].fill(y_value);
        buf
    }

    /// Create a YUV420 frame where a rectangular region in the Y plane is
    /// set to `rect_value` and the rest is `bg_value`.
    fn make_frame_with_rect(
        bg_value: u8,
        rect_value: u8,
        rx: u32,
        ry: u32,
        rw: u32,
        rh: u32,
    ) -> Vec<u8> {
        let mut buf = vec![0u8; YUV_SIZE];
        // Background
        buf[..Y_SIZE].fill(bg_value);
        // Rectangle
        for y in ry..(ry + rh).min(SRC_H) {
            let row_start = (y * SRC_W + rx) as usize;
            let row_end = (y * SRC_W + (rx + rw).min(SRC_W)) as usize;
            buf[row_start..row_end].fill(rect_value);
        }
        buf
    }

    // ------------------------------------------------------------------
    // MotionDetector::new / set_sensitivity / reset
    // ------------------------------------------------------------------

    #[test]
    fn test_new_defaults() {
        let md = MotionDetector::new(0.5, 100, 1000);
        assert!((md.sensitivity - 0.5).abs() < f64::EPSILON);
        assert_eq!(md.min_area, 100);
        assert_eq!(md.cooldown_ms, 1000);
        assert!(md.last_event.is_none());
        assert!(md.prev_frame.is_none());
        assert_eq!(md.width, DETECT_WIDTH);
        assert_eq!(md.height, DETECT_HEIGHT);
    }

    #[test]
    fn test_set_sensitivity_clamp() {
        let mut md = MotionDetector::new(0.5, 100, 1000);
        md.set_sensitivity(1.5);
        assert!((md.sensitivity - 1.0).abs() < f64::EPSILON);
        md.set_sensitivity(-0.5);
        assert!((md.sensitivity - 0.0).abs() < f64::EPSILON);
        md.set_sensitivity(0.75);
        assert!((md.sensitivity - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn test_reset_clears_state() {
        let mut md = MotionDetector::new(0.5, 100, 1000);
        md.prev_frame = Some(GrayFrame {
            data: vec![0; (DETECT_WIDTH * DETECT_HEIGHT) as usize],
            width: DETECT_WIDTH,
            height: DETECT_HEIGHT,
        });
        md.last_event = Some(Instant::now());
        md.reset();
        assert!(md.prev_frame.is_none());
        assert!(md.last_event.is_none());
    }

    // ------------------------------------------------------------------
    // no_motion_first_frame
    // ------------------------------------------------------------------

    #[test]
    fn test_no_motion_first_frame() {
        let mut md = MotionDetector::new(0.5, 100, 1000);
        let frame = make_flat_frame(128);
        let result = md.process_frame(&frame, SRC_W, SRC_H);
        assert!(result.is_none(), "first frame must not trigger");
    }

    // ------------------------------------------------------------------
    // no_motion_identical_frames
    // ------------------------------------------------------------------

    #[test]
    fn test_no_motion_identical_frames() {
        let mut md = MotionDetector::new(0.5, 100, 1000);
        let frame = make_flat_frame(128);

        // First frame — store, no event.
        assert!(md.process_frame(&frame, SRC_W, SRC_H).is_none());
        // Second identical frame — no event.
        assert!(md.process_frame(&frame, SRC_W, SRC_H).is_none());
    }

    // ------------------------------------------------------------------
    // motion_detected
    // ------------------------------------------------------------------

    #[test]
    fn test_motion_detected() {
        let mut md = MotionDetector::new(0.9, 100, 1000);

        // First frame — all grey.
        let frame_a = make_flat_frame(128);
        assert!(md.process_frame(&frame_a, SRC_W, SRC_H).is_none());

        // Second frame — white rectangle in the centre.
        let frame_b = make_frame_with_rect(128, 200, 100, 80, 50, 40);
        let event = md.process_frame(&frame_b, SRC_W, SRC_H);
        assert!(event.is_some(), "different frames should trigger motion");

        let ev = event.unwrap();
        assert!(ev.score > 0.0);
        assert!(
            ev.bbox.2 > 0 && ev.bbox.3 > 0,
            "bbox must have non‑zero dimensions"
        );
    }

    // ------------------------------------------------------------------
    // cooldown_respected
    // ------------------------------------------------------------------

    #[test]
    fn test_cooldown_respected() {
        // Use a large cooldown so both frames fall inside the window.
        let mut md = MotionDetector::new(0.9, 100, 100_000);

        let frame_a = make_flat_frame(128);
        assert!(md.process_frame(&frame_a, SRC_W, SRC_H).is_none());

        let frame_b = make_frame_with_rect(128, 200, 100, 80, 50, 40);

        // First different frame — should trigger.
        assert!(md.process_frame(&frame_b, SRC_W, SRC_H).is_some());

        // Second different frame (still within cooldown) — should NOT trigger.
        let frame_c = make_frame_with_rect(128, 200, 300, 100, 50, 40);
        assert!(
            md.process_frame(&frame_c, SRC_W, SRC_H).is_none(),
            "event within cooldown must be suppressed"
        );
    }

    // ------------------------------------------------------------------
    // small_change_ignored
    // ------------------------------------------------------------------

    #[test]
    fn test_small_change_ignored() {
        // min_area is large relative to the number of changed pixels.
        let mut md = MotionDetector::new(0.9, 5000, 1000);

        let frame_a = make_flat_frame(128);
        assert!(md.process_frame(&frame_a, SRC_W, SRC_H).is_none());

        // Change only a tiny region (1 pixel at detection resolution).
        // At 640×480 → 320×240 with step_x=2, step_y=2, a 4×4 source
        // rectangle covers ~2×2 detection pixels = 4 changed pixels.
        let frame_b = make_frame_with_rect(128, 200, 10, 10, 4, 4);
        assert!(
            md.process_frame(&frame_b, SRC_W, SRC_H).is_none(),
            "tiny change below min_area must be ignored"
        );
    }

    // ------------------------------------------------------------------
    // bbox_correct
    // ------------------------------------------------------------------

    #[test]
    fn test_bbox_correct() {
        let mut md = MotionDetector::new(0.9, 50, 1000);

        let frame_a = make_flat_frame(128);
        assert!(md.process_frame(&frame_a, SRC_W, SRC_H).is_none());

        // Place a bright rectangle at a known position.
        // Source: (100, 80) … (100+200, 80+160) = (300, 240)
        // Detection: step_x = step_y = 2
        // Expected bbox ≈ (50, 40, 100, 80)  ← (100/2, 80/2, 200/2, 160/2)
        let frame_b = make_frame_with_rect(128, 220, 100, 80, 200, 160);
        let event = md.process_frame(&frame_b, SRC_W, SRC_H);
        assert!(event.is_some(), "motion should be detected");

        let ev = event.unwrap();
        // Allow a ±1 pixel tolerance due to integer division.
        assert!(
            ev.bbox.0 >= 49 && ev.bbox.0 <= 51,
            "bbox x ({}) should be ~50",
            ev.bbox.0
        );
        assert!(
            ev.bbox.1 >= 39 && ev.bbox.1 <= 41,
            "bbox y ({}) should be ~40",
            ev.bbox.1
        );
        assert!(
            ev.bbox.2 >= 99 && ev.bbox.2 <= 101,
            "bbox w ({}) should be ~100",
            ev.bbox.2
        );
        assert!(
            ev.bbox.3 >= 79 && ev.bbox.3 <= 81,
            "bbox h ({}) should be ~80",
            ev.bbox.3
        );
        assert!(ev.score > 0.0);
    }

    // ------------------------------------------------------------------
    // sensitivity_threshold
    // ------------------------------------------------------------------

    #[test]
    fn test_sensitivity_threshold() {
        // Sensitivity 0.2 → threshold = (1.0 - 0.2) * 255 ≈ 204.
        // A small change of ~50 should NOT trigger.
        let mut md_low = MotionDetector::new(0.2, 50, 1000);

        let frame_a = make_flat_frame(100);
        assert!(md_low.process_frame(&frame_a, SRC_W, SRC_H).is_none());

        // Frame B at Y=150 → diff=50, which is < 204, so no detection.
        let frame_b = make_flat_frame(150);
        assert!(
            md_low.process_frame(&frame_b, SRC_W, SRC_H).is_none(),
            "low sensitivity must NOT detect small changes"
        );

        // Sensitivity 0.99 → threshold = (1.0 - 0.99) * 255 ≈ 2.55.
        // A small change of ~10 SHOULD trigger.
        let mut md_high = MotionDetector::new(0.99, 50, 1000);

        assert!(md_high.process_frame(&frame_a, SRC_W, SRC_H).is_none());

        let frame_c = make_flat_frame(110); // diff=10
        assert!(
            md_high.process_frame(&frame_c, SRC_W, SRC_H).is_some(),
            "high sensitivity must detect small changes"
        );
    }

    // ------------------------------------------------------------------
    // Edge cases
    // ------------------------------------------------------------------

    #[test]
    fn test_zero_dimension_input() {
        let mut md = MotionDetector::new(0.5, 10, 1000);
        assert!(md.process_frame(&[], 0, 0).is_none());
        assert!(md.process_frame(&[], 640, 0).is_none());
        assert!(md.process_frame(&[], 0, 480).is_none());
    }

    #[test]
    fn test_truncated_data_returns_none() {
        let mut md = MotionDetector::new(0.5, 10, 1000);
        // Only 100 bytes, not enough for 640×480 Y plane.
        let small = vec![0u8; 100];
        assert!(md.process_frame(&small, SRC_W, SRC_H).is_none());
    }
}
