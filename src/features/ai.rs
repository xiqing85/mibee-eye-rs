use crate::features::FeatureError;
use async_trait::async_trait;
use serde::Serialize;

/// Result of an AI detection pass.
#[derive(Debug, Clone, Serialize)]
pub struct Detection {
    pub label: String,
    pub confidence: f32,
    pub bbox: (u32, u32, u32, u32), // x, y, width, height — video-frame pixels (SPEC §4.6)
}

/// Reserved trait for AI-based object / motion detection.
///
/// **Not implemented in this version.**
#[async_trait]
pub trait AiDetector: Send + Sync {
    /// Run inference on a raw video frame.
    async fn detect(
        &self,
        frame_data: &[u8],
        width: u32,
        height: u32,
    ) -> Result<Vec<Detection>, FeatureError>;

    /// Human-readable identifier for the loaded model.
    fn model_name(&self) -> &str;

    /// Square model input size in pixels (0 = unknown / not applicable).
    /// Registry metadata for uploaded models (SPEC §4.6).
    fn input_size(&self) -> u32 {
        0
    }
}

/// Stub implementation that always returns [`FeatureError::NotImplemented`].
pub struct StubAiDetector;

#[async_trait]
impl AiDetector for StubAiDetector {
    async fn detect(
        &self,
        _frame_data: &[u8],
        _width: u32,
        _height: u32,
    ) -> Result<Vec<Detection>, FeatureError> {
        Err(FeatureError::NotImplemented(
            "AI detection not implemented in this version".into(),
        ))
    }

    fn model_name(&self) -> &str {
        "stub"
    }
}
