pub mod local;

#[cfg(feature = "storage-s3")]
pub mod s3;
#[cfg(feature = "storage-smb")]
pub mod smb;
#[cfg(feature = "storage-webdav")]
pub mod webdav;

use async_trait::async_trait;
use std::fmt;
use std::time::SystemTime;

/// A raw segment to be stored.
pub struct Segment {
    pub data: Vec<u8>,
    pub start_time: SystemTime,
    pub duration_secs: u32,
    pub is_complete: bool,
    pub sequence_num: u64,
}

/// Result of a successful save operation.
pub struct StoredSegment {
    pub id: String,
    pub path: String,
    pub size_bytes: u64,
}

/// Info for a stored segment.
#[derive(Debug)]
pub struct StoredSegmentInfo {
    pub id: String,
    pub path: String,
    pub size_bytes: u64,
    pub timestamp: SystemTime,
}

/// Storage error variants.
#[derive(Debug)]
pub enum StorageError {
    Io(std::io::Error),
    NotFound(String),
    Config(String),
    Full(String),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::Io(e) => write!(f, "IO error: {}", e),
            StorageError::NotFound(id) => write!(f, "segment not found: {}", id),
            StorageError::Config(msg) => write!(f, "configuration error: {}", msg),
            StorageError::Full(msg) => write!(f, "storage full: {}", msg),
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StorageError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for StorageError {
    fn from(e: std::io::Error) -> Self {
        StorageError::Io(e)
    }
}

#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// Save a segment to storage.
    async fn save(&self, segment: &Segment) -> Result<StoredSegment, StorageError>;

    /// List all stored segments.
    async fn list(&self) -> Result<Vec<StoredSegmentInfo>, StorageError>;

    /// Delete a stored segment by id.
    async fn delete(&self, id: &str) -> Result<(), StorageError>;

    /// Remove expired segments, returning the number removed.
    async fn cleanup_expired(&self) -> Result<u64, StorageError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_storage_error_display() {
        assert_eq!(
            StorageError::NotFound("abc123".into()).to_string(),
            "segment not found: abc123"
        );
        assert_eq!(
            StorageError::Config("missing path".into()).to_string(),
            "configuration error: missing path"
        );
        assert_eq!(
            StorageError::Full("95%".into()).to_string(),
            "storage full: 95%"
        );
        let io = StorageError::Io(std::io::Error::other("disk detached"));
        assert!(io.to_string().starts_with("IO error:"));
    }

    #[test]
    fn test_storage_error_source_and_from() {
        assert!(std::error::Error::source(&StorageError::Io(std::io::Error::other("x"))).is_some());
        assert!(
            std::error::Error::source(&StorageError::NotFound("n".into())).is_none(),
            "non-Io variants carry no source"
        );
        // From<io::Error> lets ? work in backend impls.
        let converted: StorageError = std::io::Error::other("read failed").into();
        assert!(matches!(converted, StorageError::Io(_)));
    }
}
