//! YUV420 → BGR → resize → normalization preprocessing for AI inference.
//!
//! This module implements the image preprocessing pipeline required for
//! ONNX Runtime-based object detection:
//!
//! 1. Convert YUV420 I420 planar to BGR using BT.601 coefficients
//! 2. Resize to target resolution using nearest-neighbor sampling
//! 3. Normalize with per-channel mean and standard deviation
//! 4. Output as NCHW float32 tensor (channel-first: B, G, R planes)
//!
//! # Example
//!
//! ```ignore
//! use crate::ai::preprocess::preprocess;
//!
//! let frame_data: Vec<u8> = /* YUV420 I420 data */;
//! let input = preprocess(&frame_data, 640, 480, 320, 320)?;
//! // input.len() == 3 * 320 * 320 == 307200 (NCHW f32)
//! # Ok::<(), crate::features::FeatureError>(())
//! ```

use crate::features::FeatureError;

/// Normalization constants for ImageNet-style preprocessing.
///
/// These are the mean and standard deviation values used by many
/// computer vision models (e.g., YOLOv8, MobileNet) trained on ImageNet.
///
/// Values are in BGR order (matching OpenCV's default).
const MEAN: [f32; 3] = [103.53, 116.28, 123.675];
const STD: [f32; 3] = [57.375, 57.12, 58.395];

/// Preprocess a YUV420 frame for AI inference.
///
/// # Pipeline
///
/// 1. Convert YUV420 I420 planar to BGR using BT.601 coefficients
/// 2. Resize to `dst_w × dst_h` using nearest-neighbor sampling
/// 3. Normalize: `(pixel - mean) / std` (no [0,1] scaling, no divide by 255)
/// 4. Layout as NCHW float32 (channel-first: B-plane, G-plane, R-plane)
///
/// # Arguments
///
/// * `frame_data` - YUV420 I420 planar data (Y plane, then U, then V)
/// * `src_w` - Source width in pixels
/// * `src_h` - Source height in pixels
/// * `dst_w` - Destination width (e.g., 320)
/// * `dst_h` - Destination height (e.g., 320)
///
/// # Returns
///
/// * `Ok(Vec<f32>)` - NCHW tensor of length `3 * dst_w * dst_h`
/// * `Err(FeatureError::Runtime)` - If input is too short for YUV420
///
/// # YUV420 I420 layout
///
/// The input data is organized as three consecutive planes:
/// - Y plane: `src_w × src_h` bytes (luma)
/// - U plane: `src_w/2 × src_h/2` bytes (chroma blue)
/// - V plane: `src_w/2 × src_h/2` bytes (chroma red)
///
/// Total required size: `src_w * src_h * 3 / 2` bytes.
///
/// # BT.601 conversion
///
/// The conversion uses full-range JPEG coefficients (no headroom):
/// ```text
/// R = Y + 1.402 * (V - 128)
/// G = Y - 0.344 * (U - 128) - 0.714 * (V - 128)
/// B = Y + 1.772 * (U - 128)
/// ```
///
/// Results are clamped to [0, 255] before normalization.
///
/// # Normalization
///
/// Per-channel normalization is applied directly to pixel values in [0, 255]:
/// ```text
/// normalized = (pixel - mean[channel]) / std[channel]
/// ```
///
/// The output is **not** scaled to [0, 1] or divided by 255.
///
/// # NCHW layout
///
/// The output vector contains channel-major data:
/// ```text
/// [B(0,0), B(1,0), ..., B(dst_w-1,0), B(0,1), ..., B(dst_w-1,dst_h-1),
///  G(0,0), G(1,0), ..., G(dst_w-1,dst_h-1),
///  R(0,0), R(1,0), ..., R(dst_w-1,dst_h-1)]
/// ```
pub fn preprocess(
    frame_data: &[u8],
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
) -> Result<Vec<f32>, FeatureError> {
    let (src_w, src_h, dst_w, dst_h) = (
        src_w as usize,
        src_h as usize,
        dst_w as usize,
        dst_h as usize,
    );

    // Validate input size for YUV420 I420
    let y_size = src_w * src_h;
    let uv_size = src_w / 2 * src_h / 2;
    let required_size = y_size + 2 * uv_size;

    if frame_data.len() < required_size {
        return Err(FeatureError::Runtime(format!(
            "YUV420 frame too short: got {} bytes, need {} ({}x{})",
            frame_data.len(),
            required_size,
            src_w,
            src_h
        )));
    }

    // Compute scaling factors for nearest-neighbor resize
    let scale_x = src_w as f32 / dst_w as f32;
    let scale_y = src_h as f32 / dst_h as f32;

    // Allocate output buffer in NCHW format
    let mut output = vec![0.0f32; 3 * dst_w * dst_h];

    // Precompute source UV dimensions
    let src_uv_w = src_w / 2;
    let _src_uv_h = src_h / 2;
    // Process each destination pixel
    for dy in 0..dst_h {
        let sx = (dy as f32 * scale_y) as usize;
        let sx_uv = sx / 2;

        let y_row_offset = sx * src_w;
        let uv_row_offset = sx_uv * src_uv_w;

        for dx in 0..dst_w {
            let sy = (dx as f32 * scale_x) as usize;
            let sy_uv = sy / 2;

            // Fetch YUV values using nearest-neighbor
            let y_idx = y_row_offset + sy;
            let uv_idx = uv_row_offset + sy_uv;

            let y = frame_data[y_idx] as f32;
            let u = frame_data[y_size + uv_idx] as f32 - 128.0;
            let v = frame_data[y_size + uv_size + uv_idx] as f32 - 128.0;

            // BT.601 YUV to BGR conversion (full-range JPEG coefficients)
            let r = (y + 1.402 * v).clamp(0.0, 255.0);
            let g = (y - 0.344 * u - 0.714 * v).clamp(0.0, 255.0);
            let b = (y + 1.772 * u).clamp(0.0, 255.0);

            // Normalize and store in NCHW layout
            let pixel_idx = dy * dst_w + dx;

            output[pixel_idx] = (b - MEAN[0]) / STD[0];
            output[dst_w * dst_h + pixel_idx] = (g - MEAN[1]) / STD[1];
            output[2 * dst_w * dst_h + pixel_idx] = (r - MEAN[2]) / STD[2];
        }
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that output has the correct size for 320×320 NCHW.
    #[test]
    fn test_output_size() {
        // Create a minimal valid YUV420 frame (640x480)
        let src_w = 640u32;
        let src_h = 480u32;
        let frame_size = (src_w * src_h * 3 / 2) as usize;
        let frame_data = vec![128u8; frame_size];

        let result = preprocess(&frame_data, src_w, src_h, 320, 320);

        assert!(result.is_ok(), "preprocess should succeed");
        let output = result.unwrap();
        assert_eq!(
            output.len(),
            3 * 320 * 320,
            "output should have NCHW size (3 channels * 320 * 320)"
        );
        assert_eq!(
            output.len(),
            307200,
            "output should be exactly 307200 floats"
        );
    }

    /// Test normalization formula with known values.
    ///
    /// For pixel value 128, mean=103.53, std=57.375:
    /// (128 - 103.53) / 57.375 ≈ 0.4266
    #[test]
    fn test_normalization_formula() {
        // Create a YUV420 frame where all pixels convert to RGB value 128
        let src_w = 320u32;
        let src_h = 320u32;
        let y_size = (src_w * src_h) as usize;
        let uv_size = (src_w / 2 * src_h / 2) as usize;
        let mut frame_data = vec![0u8; y_size + 2 * uv_size];

        // Set Y plane to 128 (middle gray)
        frame_data[..y_size].fill(128);

        // Set U and V planes to 128 (zero chroma)
        frame_data[y_size..y_size + uv_size].fill(128); // U = 128
        frame_data[y_size + uv_size..].fill(128); // V = 128

        let result = preprocess(&frame_data, src_w, src_h, 320, 320);

        assert!(result.is_ok());
        let output = result.unwrap();

        // All channels should have the same value since U=V=128
        let expected_b = (128.0 - MEAN[0]) / STD[0];
        let expected_g = (128.0 - MEAN[1]) / STD[1];
        let expected_r = (128.0 - MEAN[2]) / STD[2];

        // Check first pixel values
        let tolerance = 0.001;
        assert!((output[0] - expected_b).abs() < tolerance);
        assert!((output[320 * 320] - expected_g).abs() < tolerance);
        assert!((output[2 * 320 * 320] - expected_r).abs() < tolerance);

        // Verify the specific test case mentioned in requirements
        assert!((expected_b - 0.4266).abs() < tolerance);
    }

    /// Test that all-zero YUV produces negative normalized values.
    ///
    /// When all YUV values are 0, RGB conversion gives (0, 0, 0).
    /// Normalization: (0 - mean) / std should be negative.
    #[test]
    fn test_all_zero_yuv() {
        let src_w = 320u32;
        let src_h = 320u32;
        let frame_size = (src_w * src_h * 3 / 2) as usize;
        let frame_data = vec![0u8; frame_size];

        let result = preprocess(&frame_data, src_w, src_h, 320, 320);

        assert!(result.is_ok());
        let output = result.unwrap();

        // All-zero YUV (Y=0, U=0, V=0): BT.601 gives R=0 (clamped), G=135 (0-0.344*-128-0.714*-128), B=0 (clamped)
        // B channel: (0 - 103.53) / 57.375 < 0 (negative)
        // G channel: (135 - 116.28) / 57.12 > 0 (positive, because V=0 U=0 → G is high)
        let expected_b = (0.0_f64 - MEAN[0] as f64) / STD[0] as f64;
        assert!((output[0] - expected_b as f32).abs() < 0.001);
    }

    /// Test that short input returns error.
    #[test]
    fn test_short_input_returns_error() {
        let src_w = 640u32;
        let src_h = 480u32;
        let required_size = (src_w * src_h * 3 / 2) as usize;
        let frame_data = vec![0u8; required_size - 1]; // One byte short

        let result = preprocess(&frame_data, src_w, src_h, 320, 320);

        assert!(result.is_err());
        match result {
            Err(FeatureError::Runtime(msg)) => {
                assert!(
                    msg.contains("too short"),
                    "error should mention 'too short'"
                );
            }
            other => panic!("expected Runtime error, got {:?}", other),
        }
    }

    /// Test nearest-neighbor resize behavior.
    ///
    /// Verify that downsampling by 2x picks the correct pixels.
    #[test]
    fn test_nearest_neighbor_downsample() {
        let src_w = 2u32;
        let src_h = 2u32;
        let y_size = (src_w * src_h) as usize;
        let uv_size = (src_w / 2 * src_h / 2) as usize;
        let mut frame_data = vec![0u8; y_size + 2 * uv_size];

        // Create a checkerboard pattern in Y plane
        frame_data[0] = 255; // (0,0)
        frame_data[1] = 0; // (1,0)
        frame_data[2] = 0; // (0,1)
        frame_data[3] = 255; // (1,1)

        // Neutral chroma
        frame_data[y_size] = 128;
        frame_data[y_size + uv_size] = 128;

        // Downsample to 1x1
        let result = preprocess(&frame_data, src_w, src_h, 1, 1);

        assert!(result.is_ok());
        let output = result.unwrap();

        // Should pick the (0,0) pixel due to nearest-neighbor rounding
        // Y=255, U=128, V=128 → R=255, G=255, B=255
        // After normalization, all channels should be high positive
        assert_eq!(output.len(), 3);
        assert!(output[0] > 0.0, "B should be positive");
        assert!(output[1] > 0.0, "G should be positive");
        assert!(output[2] > 0.0, "R should be positive");
    }

    /// Test that different source sizes work correctly.
    #[test]
    fn test_various_source_sizes() {
        let sizes = vec![(640, 480), (1280, 720), (1920, 1080)];

        for (src_w, src_h) in sizes {
            let frame_size = (src_w * src_h * 3 / 2) as usize;
            let frame_data = vec![128u8; frame_size];

            let result = preprocess(&frame_data, src_w, src_h, 320, 320);

            assert!(result.is_ok(), "should succeed for {}x{}", src_w, src_h);
            let output = result.unwrap();
            assert_eq!(output.len(), 307200, "output size should be constant");
        }
    }

    /// Test that different destination sizes work correctly.
    #[test]
    fn test_various_destination_sizes() {
        let src_w = 640u32;
        let src_h = 480u32;
        let frame_size = (src_w * src_h * 3 / 2) as usize;
        let frame_data = vec![128u8; frame_size];

        let dst_sizes = vec![(224, 224), (320, 320), (640, 640)];

        for (dst_w, dst_h) in dst_sizes {
            let result = preprocess(&frame_data, src_w, src_h, dst_w, dst_h);

            assert!(result.is_ok(), "should succeed for {}x{}", dst_w, dst_h);
            let output = result.unwrap();
            assert_eq!(
                output.len(),
                3 * dst_w as usize * dst_h as usize,
                "output size should match destination"
            );
        }
    }
}
