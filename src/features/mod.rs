pub mod ai;
pub mod h265;
pub mod multi_camera;
pub mod webrtc;

// ---------------------------------------------------------------------------
// Shared error type for reserved feature stubs
// ---------------------------------------------------------------------------

/// Error type for feature stubs.
#[derive(Debug, Clone)]
pub enum FeatureError {
    /// The feature is recognised but not yet implemented.
    NotImplemented(String),
    /// The feature is disabled at compile‑time or runtime.
    Disabled(String),
    /// Runtime error during feature execution (e.g., ONNX Runtime failure).
    Runtime(String),
}

impl std::fmt::Display for FeatureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FeatureError::NotImplemented(msg) => write!(f, "not implemented: {msg}"),
            FeatureError::Disabled(msg) => write!(f, "disabled: {msg}"),
            FeatureError::Runtime(msg) => write!(f, "runtime error: {msg}"),
        }
    }
}

impl std::error::Error for FeatureError {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::ai::AiDetector;
    use crate::features::h265::H265Encoder;
    use crate::features::multi_camera::CameraManager;
    use crate::features::webrtc::WebRtcStreamer;

    #[test]
    fn trait_not_implemented_ai() {
        let detector = ai::StubAiDetector;
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let result = rt.block_on(detector.detect(b"", 0, 0));
        match result {
            Err(FeatureError::NotImplemented(_)) => {} // expected
            other => panic!("expected NotImplemented, got {other:?}"),
        }
    }

    #[test]
    fn trait_not_implemented_multi_camera() {
        let mgr = multi_camera::StubCameraManager;
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let result = rt.block_on(mgr.list_cameras());
        match result {
            Err(FeatureError::NotImplemented(_)) => {} // expected
            other => panic!("expected NotImplemented, got {other:?}"),
        }
    }

    #[test]
    fn trait_not_implemented_webrtc() {
        let streamer = webrtc::StubWebRtcStreamer;
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let result = rt.block_on(streamer.create_session(""));
        match result {
            Err(FeatureError::NotImplemented(_)) => {} // expected
            other => panic!("expected NotImplemented, got {other:?}"),
        }
    }

    #[test]
    fn trait_not_implemented_h265() {
        let encoder = h265::StubH265Encoder;
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let result = rt.block_on(encoder.encode(b"", 0, 0));
        match result {
            Err(FeatureError::NotImplemented(_)) => {} // expected
            other => panic!("expected NotImplemented, got {other:?}"),
        }
    }
}
