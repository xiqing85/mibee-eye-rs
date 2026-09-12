//! Mock AI detector for testing and development.
//!
//! Returns canned detections without performing real inference.

use crate::features::ai::{AiDetector, Detection};
use crate::features::FeatureError;
use async_trait::async_trait;

/// Mock detector that always returns 3 canned detections.
///
/// Useful for testing the AI pipeline without running actual inference.
pub struct MockAiDetector;

impl MockAiDetector {
    /// Create a new mock detector.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for MockAiDetector {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AiDetector for MockAiDetector {
    async fn detect(
        &self,
        _frame_data: &[u8],
        _width: u32,
        _height: u32,
    ) -> Result<Vec<Detection>, FeatureError> {
        // Return 3 canned detections with high confidence. Bboxes are in
        // video-frame pixels (SPEC §4.6), sized for a 640×480 frame.
        Ok(vec![
            Detection {
                label: "person".to_string(),
                confidence: 0.95,
                bbox: (100, 200, 150, 300),
            },
            Detection {
                label: "car".to_string(),
                confidence: 0.87,
                bbox: (400, 300, 200, 150),
            },
            Detection {
                label: "dog".to_string(),
                confidence: 0.72,
                bbox: (600, 400, 100, 120),
            },
        ])
    }

    fn model_name(&self) -> &str {
        "mock-detector-v1"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn test_mock_detector_returns_three_detections() {
        let detector = MockAiDetector::new();
        let dummy_frame = vec![0u8; 640 * 480 * 3];

        let result = detector.detect(&dummy_frame, 640, 480).await;

        assert!(result.is_ok());
        let detections = result.unwrap();
        assert_eq!(detections.len(), 3);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_mock_detector_model_name() {
        let detector = MockAiDetector::new();
        assert_eq!(detector.model_name(), "mock-detector-v1");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_mock_detector_default() {
        let detector = MockAiDetector::new();
        let dummy_frame = vec![0u8; 100];

        let result = detector.detect(&dummy_frame, 320, 240).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 3);
    }
}
