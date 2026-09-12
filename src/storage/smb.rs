//! SMB storage backend — mount-point approach.
//!
//! Pure-Rust SMB protocol implementations exist but are immature, making a
//! full in-process SMB client a maintenance risk for this project.
//!
//! Instead, this backend relies on the **OS-level SMB/CIFS mount**. On Linux:
//!
//! ```bash
//! sudo mount -t cifs //server/share /mnt/smb -o username=user,password=pass
//! ```
//!
//! The mount is managed externally (e.g. fstab, systemd mount unit). This
//! backend writes video segments into the mounted directory just like it would
//! a local filesystem. The OS kernel handles network retries, credential
//! management, and reconnection.
//!
//! # Environment variables
//!
//! | Variable | Required | Description |
//! |---|---|---|
//! | `SMB_MOUNT_PATH` | Yes | Absolute path where the SMB share is mounted |
//!
//! Credentials are NOT stored by this backend — they are handled by the OS
//! mount command (or fstab entry).

use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tokio::fs;

use super::{Segment, StorageBackend, StorageError, StoredSegment, StoredSegmentInfo};

/// SMB storage backend.
///
/// Writes segments to a local directory that is expected to be a mounted SMB
/// share. All network-level concerns (reconnection, credentials, retries) are
/// delegated to the OS kernel's CIFS client.
pub struct SmbStorage {
    base_path: PathBuf,
}

impl SmbStorage {
    /// Create a new `SmbStorage` from environment variables.
    ///
    /// Reads `SMB_MOUNT_PATH` from the environment.
    ///
    /// # Errors
    ///
    /// Returns `StorageError::Config` if `SMB_MOUNT_PATH` is not set.
    pub fn from_env() -> Result<Self, StorageError> {
        let path_str = std::env::var("SMB_MOUNT_PATH")
            .map_err(|_| StorageError::Config("SMB_MOUNT_PATH not set".into()))?;

        let base_path = PathBuf::from(&path_str);
        if !base_path.is_absolute() {
            return Err(StorageError::Config(format!(
                "SMB_MOUNT_PATH must be absolute: {path_str}"
            )));
        }

        Ok(Self { base_path })
    }

    /// Build an absolute file path for a segment.
    fn segment_path(&self, start_time: SystemTime, sequence_num: u64) -> PathBuf {
        let key = build_key(start_time, sequence_num);
        self.base_path.join(&key)
    }
}

/// Build a filename key from segment metadata.
fn build_key(start_time: SystemTime, sequence_num: u64) -> String {
    let millis = start_time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{millis}_{sequence_num:05}.m4v")
}

#[async_trait]
impl StorageBackend for SmbStorage {
    async fn save(&self, segment: &Segment) -> Result<StoredSegment, StorageError> {
        let path = self.segment_path(segment.start_time, segment.sequence_num);

        // Ensure parent directory exists (the share root should already exist,
        // but we want to be defensive about nested paths).
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }

        fs::write(&path, &segment.data).await?;

        let id = path
            .strip_prefix(&self.base_path)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();

        Ok(StoredSegment {
            id,
            path: path.to_string_lossy().to_string(),
            size_bytes: segment.data.len() as u64,
        })
    }

    /// List segments by scanning the mount directory recursively.
    async fn list(&self) -> Result<Vec<StoredSegmentInfo>, StorageError> {
        let mut files = Vec::new();
        let mut dirs = vec![self.base_path.clone()];

        while let Some(dir) = dirs.pop() {
            let mut read = match fs::read_dir(&dir).await {
                Ok(r) => r,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(StorageError::Io(e)),
            };

            while let Some(entry) = read.next_entry().await? {
                let ft = entry.file_type().await?;
                if ft.is_dir() {
                    dirs.push(entry.path());
                } else if ft.is_file() {
                    files.push(entry.path());
                }
            }
        }

        let mut infos = Vec::with_capacity(files.len());
        for f in &files {
            if let Some(info) = info_from_path(f, &self.base_path).await {
                infos.push(info);
            }
        }

        infos.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(infos)
    }

    async fn delete(&self, id: &str) -> Result<(), StorageError> {
        let path = self.base_path.join(id);
        if !path.exists() {
            return Err(StorageError::NotFound(id.to_string()));
        }
        fs::remove_file(&path).await?;
        Ok(())
    }

    /// Remove segments older than a hardcoded default retention (30 days),
    /// since SMB doesn't have its own retention config yet.
    async fn cleanup_expired(&self) -> Result<u64, StorageError> {
        let retention = std::time::Duration::from_secs(30 * 86400);
        let now = SystemTime::now();
        let files = self.collect_files().await?;
        let mut removed = 0u64;

        for f in files {
            if let Ok(meta) = fs::metadata(&f).await {
                if let Ok(modified) = meta.modified() {
                    if now.duration_since(modified).unwrap_or_default() > retention {
                        fs::remove_file(&f).await?;
                        removed += 1;
                    }
                }
            }
        }

        Ok(removed)
    }
}

// --- private helpers reused by tests ---

impl SmbStorage {
    /// Walk the mount directory collecting file paths.
    async fn collect_files(&self) -> Result<Vec<PathBuf>, StorageError> {
        let mut files = Vec::new();
        let mut dirs = vec![self.base_path.clone()];
        while let Some(dir) = dirs.pop() {
            let mut read = match fs::read_dir(&dir).await {
                Ok(r) => r,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(StorageError::Io(e)),
            };
            while let Some(entry) = read.next_entry().await? {
                let ft = entry.file_type().await?;
                if ft.is_dir() {
                    dirs.push(entry.path());
                } else if ft.is_file() {
                    files.push(entry.path());
                }
            }
        }
        Ok(files)
    }
}

/// Read file metadata and produce a `StoredSegmentInfo`.
async fn info_from_path(path: &Path, base: &Path) -> Option<StoredSegmentInfo> {
    let rel = path.strip_prefix(base).ok()?;
    let id = rel.to_string_lossy().to_string();
    let metadata = fs::metadata(path).await.ok()?;
    let timestamp = metadata.modified().ok()?;
    let size_bytes = metadata.len();
    Some(StoredSegmentInfo {
        id,
        path: path.to_string_lossy().to_string(),
        size_bytes,
        timestamp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn make_segment(data: &[u8], start: SystemTime, seq: u64) -> Segment {
        Segment {
            data: data.to_vec(),
            start_time: start,
            duration_secs: 10,
            is_complete: true,
            sequence_num: seq,
        }
    }

    #[tokio::test]
    async fn from_env_success() {
        temp_env::with_vars([("SMB_MOUNT_PATH", Some("/mnt/smb"))], || {
            let store = SmbStorage::from_env();
            assert!(store.is_ok());
        });
    }

    #[tokio::test]
    async fn from_env_missing() {
        temp_env::with_vars([("SMB_MOUNT_PATH", None::<&str>)], || {
            let err = SmbStorage::from_env().unwrap_err();
            assert!(matches!(err, StorageError::Config(_)));
        });
    }

    #[tokio::test]
    async fn from_env_relative_path() {
        temp_env::with_vars([("SMB_MOUNT_PATH", Some("relative/path"))], || {
            let err = SmbStorage::from_env().unwrap_err();
            assert!(matches!(err, StorageError::Config(_)));
        });
    }

    #[tokio::test]
    async fn save_creates_file() {
        let dir = std::env::temp_dir().join("mibee_test_smb_save");
        let _ = fs::remove_dir_all(&dir).await;
        let store = SmbStorage {
            base_path: dir.clone(),
        };

        let seg = make_segment(b"smb content", UNIX_EPOCH + Duration::from_secs(1000), 0);
        let saved = store.save(&seg).await.unwrap();

        assert!(Path::new(&saved.path).exists());
        assert_eq!(saved.size_bytes, 11);
        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn save_and_list() {
        let dir = std::env::temp_dir().join("mibee_test_smb_list");
        let _ = fs::remove_dir_all(&dir).await;
        let store = SmbStorage {
            base_path: dir.clone(),
        };

        let t = UNIX_EPOCH + Duration::from_secs(2000);
        store.save(&make_segment(b"a", t, 0)).await.unwrap();
        store.save(&make_segment(b"b", t, 1)).await.unwrap();

        let list = store.list().await.unwrap();
        assert_eq!(list.len(), 2);

        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn delete_removes_file() {
        let dir = std::env::temp_dir().join("mibee_test_smb_delete");
        let _ = fs::remove_dir_all(&dir).await;
        let store = SmbStorage {
            base_path: dir.clone(),
        };

        let seg = make_segment(b"delete me", UNIX_EPOCH + Duration::from_secs(3000), 0);
        let saved = store.save(&seg).await.unwrap();
        store.delete(&saved.id).await.unwrap();
        assert!(!Path::new(&saved.path).exists());

        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn delete_not_found() {
        let dir = std::env::temp_dir().join("mibee_test_smb_delete_nf");
        let _ = fs::remove_dir_all(&dir).await;
        let store = SmbStorage {
            base_path: dir.clone(),
        };

        let result = store.delete("nonexistent.m4v").await;
        assert!(matches!(result, Err(StorageError::NotFound(_))));

        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn cleanup_keeps_recent() {
        let dir = std::env::temp_dir().join("mibee_test_smb_cleanup");
        let _ = fs::remove_dir_all(&dir).await;
        let store = SmbStorage {
            base_path: dir.clone(),
        };

        store
            .save(&make_segment(b"recent", SystemTime::now(), 0))
            .await
            .unwrap();

        let removed = store.cleanup_expired().await.unwrap();
        assert_eq!(removed, 0);

        let list = store.list().await.unwrap();
        assert_eq!(list.len(), 1);

        let _ = fs::remove_dir_all(&dir).await;
    }
}
