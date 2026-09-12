//! ONNX Runtime detector implementation.

#[cfg(feature = "ai")]
use crate::ai::registry::Family;
#[cfg(feature = "ai")]
use crate::features::ai::{AiDetector, Detection};
#[cfg(feature = "ai")]
use crate::features::FeatureError;
#[cfg(feature = "ai")]
use async_trait::async_trait;
#[cfg(feature = "ai")]
use ort::session::Session;
#[cfg(feature = "ai")]
use ort::value::Tensor;
#[cfg(feature = "ai")]
use std::path::Path;
#[cfg(feature = "ai")]
use std::sync::Mutex;

/// Pre-NMS confidence filter applied inside post-processing.
///
/// The configured `confidence_threshold` is applied by `AiModule` after
/// `detect()` returns; this lower pre-filter only trims obviously-empty
/// grid points to keep NMS cheap.
#[cfg(feature = "ai")]
const PRE_NMS_CONFIDENCE: f32 = 0.05;

/// ONNX Runtime-based AI detector.
///
/// Loads an ONNX model file and runs inference using the `ort` crate.
#[cfg(feature = "ai")]
#[derive(Debug)]
pub struct OrtDetector {
    /// Path to the ONNX model file.
    model_path: String,
    /// Decoder family — selects the pre/post-processing pair.
    family: crate::ai::registry::Family,
    /// ONNX Runtime session. Wrapped in a mutex because `run()` takes `&mut self`.
    session: Mutex<Session>,
    /// Name of the model's input tensor.
    input_name: String,
    /// Name of the model's output tensor.
    output_name: String,
    /// Model input width (read from the session's input shape).
    input_width: u32,
    /// Model input height (read from the session's input shape).
    input_height: u32,
}

#[cfg(feature = "ai")]
impl OrtDetector {
    /// Create a new ONNX Runtime detector from a model file.
    ///
    /// # Arguments
    ///
    /// * `model_path` - Path to the ONNX model file (e.g., "models/nanodet-m.onnx").
    ///
    /// # Errors
    ///
    /// Returns `FeatureError::Runtime` if the model cannot be loaded or its
    /// input/output names and shape cannot be determined.
    pub fn new(model_path: &str, family: Family) -> Result<Self, FeatureError> {
        // Validate model file exists before attempting to load.
        if !Path::new(model_path).exists() {
            return Err(FeatureError::Runtime(format!(
                "ONNX model file not found: {model_path}"
            )));
        }

        // Build ONNX Runtime session with 2 intra-op threads.
        let session = Session::builder()
            .map_err(|e| FeatureError::Runtime(format!("Failed to create session builder: {e}")))?
            .with_intra_threads(2)
            .map_err(|e| FeatureError::Runtime(format!("Failed to set intra_threads: {e}")))?
            .commit_from_file(model_path)
            .map_err(|e| FeatureError::Runtime(format!("Failed to load ONNX model: {e}")))?;

        // Read input/output names and the input tensor shape from the session.
        let input = session
            .inputs()
            .first()
            .ok_or_else(|| FeatureError::Runtime("ONNX model has no inputs".to_string()))?;
        let output = session
            .outputs()
            .first()
            .ok_or_else(|| FeatureError::Runtime("ONNX model has no outputs".to_string()))?;

        let input_name = input.name().to_string();
        let output_name = output.name().to_string();

        // NCHW layout: [batch, channels, height, width].
        let shape = input
            .dtype()
            .tensor_shape()
            .ok_or_else(|| FeatureError::Runtime("ONNX model input is not a tensor".to_string()))?;
        let input_height = shape.get(2).copied().unwrap_or(-1);
        let input_width = shape.get(3).copied().unwrap_or(-1);
        if input_height <= 0 || input_width <= 0 {
            return Err(FeatureError::Runtime(format!(
                "ONNX model input shape has dynamic dimensions: {shape:?}"
            )));
        }

        Ok(Self {
            model_path: model_path.to_string(),
            family,
            session: Mutex::new(session),
            input_name,
            output_name,
            input_width: input_width as u32,
            input_height: input_height as u32,
        })
    }
}

#[cfg(feature = "ai")]
#[async_trait]
impl AiDetector for OrtDetector {
    async fn detect(
        &self,
        frame_data: &[u8],
        width: u32,
        height: u32,
    ) -> Result<Vec<Detection>, FeatureError> {
        if self.input_width != self.input_height {
            return Err(FeatureError::Runtime(format!(
                "detector expects a square input, got {}x{}",
                self.input_width, self.input_height
            )));
        }

        // 1. Preprocess per family: NanoDet stretches into BGR mean/std;
        //    YOLOX letterboxes into RGB /255 (keeping the geometry so
        //    bboxes can be mapped back later).
        let (input, letterbox) = match self.family {
            crate::ai::registry::Family::NanoDet => (
                crate::ai::preprocess::preprocess(
                    frame_data,
                    width,
                    height,
                    self.input_width,
                    self.input_height,
                )?,
                None,
            ),
            crate::ai::registry::Family::Yolox => {
                let (input, lb) = crate::ai::yolox::preprocess_yolox(
                    frame_data,
                    width,
                    height,
                    self.input_width,
                )?;
                (input, Some(lb))
            }
        };

        // 2. Build the input tensor from the NCHW data.
        let shape = vec![1_i64, 3, self.input_height as i64, self.input_width as i64];
        let tensor = Tensor::from_array((shape, input))
            .map_err(|e| FeatureError::Runtime(format!("Failed to build input tensor: {e}")))?;

        // 3. Run inference.
        let mut session = self
            .session
            .lock()
            .map_err(|_| FeatureError::Runtime("ONNX session mutex poisoned".to_string()))?;
        let outputs = session
            .run(ort::inputs![self.input_name.as_str() => tensor])
            .map_err(|e| FeatureError::Runtime(format!("ONNX inference error: {e}")))?;

        let output = outputs
            .get(&self.output_name)
            .ok_or_else(|| FeatureError::Runtime("ONNX model produced no outputs".to_string()))?;

        // 4. Extract the flat f32 tensor data and run the family decoder.
        //    Bboxes are returned in video-frame pixels (SPEC §4.6); the
        //    grid layout follows the model's actual input size.
        let (_shape, data) = output
            .try_extract_tensor::<f32>()
            .map_err(|e| FeatureError::Runtime(format!("Failed to extract output tensor: {e}")))?;

        match self.family {
            crate::ai::registry::Family::NanoDet => {
                let grid = crate::ai::postprocess::Grid::for_input(self.input_width);
                let detections =
                    crate::ai::postprocess::postprocess(data, &grid, PRE_NMS_CONFIDENCE)?;
                Ok(crate::ai::postprocess::scale_detections_to_frame(
                    detections,
                    self.input_width,
                    self.input_height,
                    width,
                    height,
                ))
            }
            crate::ai::registry::Family::Yolox => {
                let lb = letterbox
                    .ok_or_else(|| FeatureError::Runtime("missing letterbox".to_string()))?;
                let grid = crate::ai::yolox::YoloxGrid::for_input(self.input_width);
                crate::ai::yolox::postprocess_yolox(
                    data,
                    &grid,
                    &lb,
                    width,
                    height,
                    PRE_NMS_CONFIDENCE,
                )
            }
        }
    }

    fn model_name(&self) -> &str {
        &self.model_path
    }

    fn input_size(&self) -> u32 {
        self.input_width
    }
}

#[cfg(all(test, feature = "ai"))]
mod tests {
    use super::*;

    #[test]
    fn test_ort_detector_constructor_error_missing_file() {
        // Constructor should fail when model file doesn't exist.
        let result = OrtDetector::new("nonexistent_model.onnx", Family::NanoDet);
        assert!(result.is_err());
        match result {
            Err(FeatureError::Runtime(msg)) => {
                assert!(msg.contains("not found"));
            }
            other => panic!("Expected FeatureError::Runtime, got {other:?}"),
        }
    }
}
