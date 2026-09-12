use crate::features::FeatureError;
use async_trait::async_trait;

/// Reserved trait for WebRTC streaming sessions.
///
/// **Not implemented in this version.**
#[async_trait]
pub trait WebRtcStreamer: Send + Sync {
    /// Create a new WebRTC session from a received SDP offer and return the SDP answer.
    async fn create_session(&self, sdp_offer: &str) -> Result<String, FeatureError>;

    /// Tear down an active WebRTC session.
    async fn close_session(&self, id: &str) -> Result<(), FeatureError>;
}

/// Stub implementation that always returns [`FeatureError::NotImplemented`].
pub struct StubWebRtcStreamer;

#[async_trait]
impl WebRtcStreamer for StubWebRtcStreamer {
    async fn create_session(&self, _sdp_offer: &str) -> Result<String, FeatureError> {
        Err(FeatureError::NotImplemented(
            "WebRTC streaming not implemented in this version".into(),
        ))
    }

    async fn close_session(&self, _id: &str) -> Result<(), FeatureError> {
        Err(FeatureError::NotImplemented(
            "WebRTC streaming not implemented in this version".into(),
        ))
    }
}
