use crate::features::FeatureError;
use async_trait::async_trait;

/// Descriptor for a single camera known to the system.
#[derive(Debug, Clone)]
pub struct CameraInfo {
    pub id: String,
    pub name: String,
    pub status: String,
}

/// Configuration used when adding a new camera.
#[derive(Debug, Clone)]
pub struct CameraConfig {
    pub name: String,
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// Reserved trait for multi-camera lifecycle management.
///
/// **Not implemented in this version.**
#[async_trait]
pub trait CameraManager: Send + Sync {
    /// List all registered cameras.
    async fn list_cameras(&self) -> Result<Vec<CameraInfo>, FeatureError>;

    /// Register a new camera and return its assigned id.
    async fn add_camera(&self, config: CameraConfig) -> Result<String, FeatureError>;

    /// Remove a camera by id.
    async fn remove_camera(&self, id: &str) -> Result<(), FeatureError>;
}

/// Stub implementation that always returns [`FeatureError::NotImplemented`].
pub struct StubCameraManager;

#[async_trait]
impl CameraManager for StubCameraManager {
    async fn list_cameras(&self) -> Result<Vec<CameraInfo>, FeatureError> {
        Err(FeatureError::NotImplemented(
            "multi-camera management not implemented in this version".into(),
        ))
    }

    async fn add_camera(&self, _config: CameraConfig) -> Result<String, FeatureError> {
        Err(FeatureError::NotImplemented(
            "multi-camera management not implemented in this version".into(),
        ))
    }

    async fn remove_camera(&self, _id: &str) -> Result<(), FeatureError> {
        Err(FeatureError::NotImplemented(
            "multi-camera management not implemented in this version".into(),
        ))
    }
}
