use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::fs;

use super::{Segment, StorageBackend, StorageError, StoredSegment, StoredSegmentInfo};

/// Local filesystem storage backend.
pub struct LocalStorage {
    base_path: PathBuf,
    retention: Duration,
}

/// Convert a duration since unix epoch to (year, month, day) using the
/// Howard Hinnant algorithm.
fn epoch_days_to_date(days: u64) -> (u32, u32, u32) {
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year as u32, m as u32, d as u32)
}

/// Build the relative path `{YYYY-MM-DD}/{HH}/{unix_millis}_{seq:05}.m4v` from
/// a timestamp and sequence number.
fn build_relative_path(start_time: SystemTime, sequence_num: u64) -> PathBuf {
    let since_epoch = start_time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since_epoch.as_secs();
    let days = secs / 86400;
    let (year, month, day) = epoch_days_to_date(days);
    let hour = (secs % 86400) / 3600;
    let millis = since_epoch.as_millis();
    let filename = format!("{}_{:05}.m4v", millis, sequence_num);
    PathBuf::from(format!("{:04}-{:02}-{:02}", year, month, day))
        .join(format!("{:02}", hour))
        .join(filename)
}

impl LocalStorage {
    /// Create a new `LocalStorage` rooted at `base_path` with the given
    /// retention period.
    pub fn new(base_path: &str, retention_days: u32) -> Result<Self, StorageError> {
        let path = PathBuf::from(base_path);
        if retention_days == 0 {
            return Err(StorageError::Config("retention_days must be > 0".into()));
        }
        Ok(LocalStorage {
            base_path: path,
            retention: Duration::from_secs(u64::from(retention_days) * 86400),
        })
    }

    /// Build the absolute path for a segment.
    fn segment_path(&self, start_time: SystemTime, seq: u64) -> PathBuf {
        self.base_path.join(build_relative_path(start_time, seq))
    }

    /// Parse a stored segment info from a file path.
    async fn info_from_path(&self, path: &Path) -> Option<StoredSegmentInfo> {
        let rel = path.strip_prefix(&self.base_path).ok()?;
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

    /// Walk a directory stack (non-recursive) collecting files.
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

#[async_trait]
impl StorageBackend for LocalStorage {
    async fn save(&self, segment: &Segment) -> Result<StoredSegment, StorageError> {
        let path = self.segment_path(segment.start_time, segment.sequence_num);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(&path, &segment.data).await?;
        // Derive relative path for the id
        let rel = path.strip_prefix(&self.base_path).unwrap_or(&path);
        let id = rel.to_string_lossy().to_string();
        Ok(StoredSegment {
            id,
            path: path.to_string_lossy().to_string(),
            size_bytes: segment.data.len() as u64,
        })
    }

    async fn list(&self) -> Result<Vec<StoredSegmentInfo>, StorageError> {
        let files = self.collect_files().await?;
        let mut infos = Vec::with_capacity(files.len());
        for f in files {
            if let Some(info) = self.info_from_path(&f).await {
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

    async fn cleanup_expired(&self) -> Result<u64, StorageError> {
        let now = SystemTime::now();
        let files = self.collect_files().await?;
        let mut removed = 0u64;
        for f in files {
            let meta = fs::metadata(&f).await?;
            if let Ok(modified) = meta.modified() {
                if now.duration_since(modified).unwrap_or_default() > self.retention {
                    fs::remove_file(&f).await?;
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    async fn save_creates_file() {
        let dir = std::env::temp_dir().join("mibee_test_save_creates_file");
        let _ = fs::remove_dir_all(&dir).await;
        let store = LocalStorage::new(dir.to_str().unwrap(), 30).unwrap();
        let seg = make_segment(b"hello", SystemTime::now(), 0);
        let saved = store.save(&seg).await.unwrap();
        assert!(std::path::Path::new(&saved.path).exists());
        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn list_returns_saved() {
        let dir = std::env::temp_dir().join("mibee_test_list_returns_saved");
        let _ = fs::remove_dir_all(&dir).await;
        let store = LocalStorage::new(dir.to_str().unwrap(), 30).unwrap();
        let t = SystemTime::now();
        store.save(&make_segment(b"a", t, 0)).await.unwrap();
        store.save(&make_segment(b"b", t, 1)).await.unwrap();
        let list = store.list().await.unwrap();
        assert_eq!(list.len(), 2);
        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn delete_removes_file() {
        let dir = std::env::temp_dir().join("mibee_test_delete_removes_file");
        let _ = fs::remove_dir_all(&dir).await;
        let store = LocalStorage::new(dir.to_str().unwrap(), 30).unwrap();
        let seg = make_segment(b"delete me", SystemTime::now(), 0);
        let saved = store.save(&seg).await.unwrap();
        store.delete(&saved.id).await.unwrap();
        assert!(!std::path::Path::new(&saved.path).exists());
        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn delete_not_found() {
        let dir = std::env::temp_dir().join("mibee_test_delete_not_found");
        let _ = fs::remove_dir_all(&dir).await;
        let store = LocalStorage::new(dir.to_str().unwrap(), 30).unwrap();
        let result = store.delete("nonexistent_file").await;
        assert!(matches!(result, Err(StorageError::NotFound(_))));
        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn cleanup_keeps_recent() {
        let dir = std::env::temp_dir().join("mibee_test_cleanup_keeps_recent");
        let _ = fs::remove_dir_all(&dir).await;
        let store = LocalStorage::new(dir.to_str().unwrap(), 30).unwrap();
        let seg = make_segment(b"recent", SystemTime::now(), 0);
        store.save(&seg).await.unwrap();
        let removed = store.cleanup_expired().await.unwrap();
        assert_eq!(removed, 0);
        let list = store.list().await.unwrap();
        assert_eq!(list.len(), 1);
        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn creates_dirs() {
        let dir = std::env::temp_dir().join("mibee_test_creates_dirs/deep/nested");
        let _ = fs::remove_dir_all(&dir).await;
        let store = LocalStorage::new(dir.to_str().unwrap(), 30).unwrap();
        let seg = make_segment(b"dirs", SystemTime::now(), 0);
        let saved = store.save(&seg).await.unwrap();
        assert!(std::path::Path::new(&saved.path).exists());
        let _ = fs::remove_dir_all(&dir).await;
    }
}
