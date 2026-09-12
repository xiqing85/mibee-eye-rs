use crate::features::FeatureError;
use async_trait::async_trait;

/// Reserved trait for H.265 / HEVC software encoding.
///
/// **Not implemented in this version.**
#[async_trait]
pub trait H265Encoder: Send + Sync {
    /// Encode a raw YUV frame to H.265 / HEVC.
    async fn encode(
        &self,
        yuv_data: &[u8],
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>, FeatureError>;
}

/// Stub implementation that always returns [`FeatureError::NotImplemented`].
pub struct StubH265Encoder;

#[async_trait]
impl H265Encoder for StubH265Encoder {
    async fn encode(
        &self,
        _yuv_data: &[u8],
        _width: u32,
        _height: u32,
    ) -> Result<Vec<u8>, FeatureError> {
        Err(FeatureError::NotImplemented(
            "H.265 encoding not implemented in this version".into(),
        ))
    }
}
