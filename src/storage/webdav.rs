use async_trait::async_trait;
use reqwest::Client;
use std::time::{Duration, SystemTime};
use tokio::time::sleep;

use super::{Segment, StorageBackend, StorageError, StoredSegment, StoredSegmentInfo};

/// Maximum retry attempts for HTTP operations.
const MAX_RETRIES: u32 = 3;
/// Base delay for exponential backoff (milliseconds).
const BASE_RETRY_MS: u64 = 500;

/// WebDAV storage backend.
///
/// Stores segments by issuing HTTP PUT requests to a WebDAV server.
/// Credentials are read from environment variables (never from config files):
///
/// | Variable | Required | Description |
/// |---|---|---|
/// | `WEBDAV_URL` | Yes | Base URL of the WebDAV server (e.g. `https://example.com/dav/`) |
/// | `WEBDAV_USERNAME` | Yes | WebDAV authentication username |
/// | `WEBDAV_PASSWORD` | Yes | WebDAV authentication password |
pub struct WebdavStorage {
    client: Client,
    base_url: String,
    username: String,
    password: String,
}

impl WebdavStorage {
    /// Create a new `WebdavStorage` from environment variables.
    ///
    /// # Errors
    ///
    /// Returns `StorageError::Config` if any required env var is missing or the
    /// URL cannot be parsed.
    pub fn from_env() -> Result<Self, StorageError> {
        let base_url = std::env::var("WEBDAV_URL")
            .map_err(|_| StorageError::Config("WEBDAV_URL not set".into()))?;
        let username = std::env::var("WEBDAV_USERNAME")
            .map_err(|_| StorageError::Config("WEBDAV_USERNAME not set".into()))?;
        let password = std::env::var("WEBDAV_PASSWORD")
            .map_err(|_| StorageError::Config("WEBDAV_PASSWORD not set".into()))?;

        // Validate URL is parseable
        reqwest::Url::parse(&base_url)
            .map_err(|e| StorageError::Config(format!("invalid WEBDAV_URL: {e}")))?;

        Ok(Self {
            client: Client::new(),
            base_url,
            username,
            password,
        })
    }

    /// Build the full URL for a given storage key.
    fn build_url(&self, key: &str) -> String {
        let base = self.base_url.trim_end_matches('/');
        format!("{base}/{key}")
    }

    /// Build a storage key from a segment's metadata.
    fn build_key(start_time: SystemTime, sequence_num: u64) -> String {
        let millis = start_time
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        format!("{millis}_{sequence_num:05}.m4v")
    }
}

#[async_trait]
impl StorageBackend for WebdavStorage {
    async fn save(&self, segment: &Segment) -> Result<StoredSegment, StorageError> {
        let key = Self::build_key(segment.start_time, segment.sequence_num);
        let url = self.build_url(&key);
        let data = segment.data.clone();

        let mut last_err = None;
        for attempt in 0..MAX_RETRIES {
            let result = self
                .client
                .put(&url)
                .basic_auth(&self.username, Some(&self.password))
                .body(data.clone())
                .send()
                .await;

            match result {
                Ok(resp) if resp.status().is_success() => {
                    return Ok(StoredSegment {
                        id: key,
                        path: url,
                        size_bytes: segment.data.len() as u64,
                    });
                }
                Ok(resp) => {
                    last_err = Some(StorageError::Io(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("WebDAV PUT returned HTTP {}", resp.status()),
                    )));
                }
                Err(e) => {
                    last_err = Some(StorageError::Io(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("WebDAV request failed: {e}"),
                    )));
                }
            }

            if attempt + 1 < MAX_RETRIES {
                sleep(Duration::from_millis(BASE_RETRY_MS * 2u64.pow(attempt))).await;
            }
        }

        Err(last_err.unwrap_or_else(|| {
            StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "WebDAV save failed after retries",
            ))
        }))
    }

    async fn list(&self) -> Result<Vec<StoredSegmentInfo>, StorageError> {
        Err(StorageError::Config(
            "WebDAV list not implemented (requires PROPFIND)".into(),
        ))
    }

    async fn delete(&self, _id: &str) -> Result<(), StorageError> {
        Err(StorageError::Config(
            "WebDAV delete not implemented (requires WebDAV DELETE)".into(),
        ))
    }

    async fn cleanup_expired(&self) -> Result<u64, StorageError> {
        Err(StorageError::Config(
            "WebDAV cleanup not implemented".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{basic_auth, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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
        temp_env::with_vars(
            [
                ("WEBDAV_URL", Some("http://localhost:9999/dav/")),
                ("WEBDAV_USERNAME", Some("user")),
                ("WEBDAV_PASSWORD", Some("pass")),
            ],
            || {
                let store = WebdavStorage::from_env();
                assert!(store.is_ok());
            },
        );
    }

    #[tokio::test]
    async fn from_env_missing_url() {
        temp_env::with_vars(
            [
                ("WEBDAV_URL", None::<&str>),
                ("WEBDAV_USERNAME", Some("user")),
                ("WEBDAV_PASSWORD", Some("pass")),
            ],
            || {
                let err = WebdavStorage::from_env().unwrap_err();
                assert!(matches!(err, StorageError::Config(_)));
            },
        );
    }

    #[tokio::test]
    async fn from_env_invalid_url() {
        temp_env::with_vars(
            [
                ("WEBDAV_URL", Some("not a url")),
                ("WEBDAV_USERNAME", Some("user")),
                ("WEBDAV_PASSWORD", Some("pass")),
            ],
            || {
                let err = WebdavStorage::from_env().unwrap_err();
                assert!(matches!(err, StorageError::Config(_)));
            },
        );
    }

    #[tokio::test]
    async fn save_success() {
        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/dav/12345_00000.m4v"))
            .and(basic_auth("user", "pass"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&mock_server)
            .await;

        let store = WebdavStorage {
            client: Client::new(),
            base_url: format!("{}/dav", mock_server.uri()),
            username: "user".into(),
            password: "pass".into(),
        };

        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(12345);
        let seg = make_segment(b"webdav data", start, 0);
        let saved = store.save(&seg).await.unwrap();

        assert!(saved.id.ends_with(".m4v"));
        assert_eq!(saved.size_bytes, 11);
    }

    #[tokio::test]
    async fn save_retry_then_success() {
        let mock_server = MockServer::start().await;

        // First respond with 503, then 201
        Mock::given(method("PUT"))
            .and(path("/dav/12345_00001.m4v"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("PUT"))
            .and(path("/dav/12345_00001.m4v"))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&mock_server)
            .await;

        let store = WebdavStorage {
            client: Client::new(),
            base_url: format!("{}/dav", mock_server.uri()),
            username: "user".into(),
            password: "pass".into(),
        };

        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(12345);
        let seg = make_segment(b"retry data", start, 1);
        let saved = store.save(&seg).await.unwrap();

        assert!(saved.id.ends_with(".m4v"));
        assert_eq!(saved.size_bytes, 10);
    }

    #[tokio::test]
    async fn save_failure_after_retries() {
        let mock_server = MockServer::start().await;

        // Always return 500
        Mock::given(method("PUT"))
            .and(path("/dav/12345_00002.m4v"))
            .respond_with(ResponseTemplate::new(500))
            .expect(3) // MAX_RETRIES
            .mount(&mock_server)
            .await;

        let store = WebdavStorage {
            client: Client::new(),
            base_url: format!("{}/dav", mock_server.uri()),
            username: "user".into(),
            password: "pass".into(),
        };

        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(12345);
        let seg = make_segment(b"fail data", start, 2);
        let result = store.save(&seg).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn list_not_implemented() {
        temp_env::with_vars(
            [
                ("WEBDAV_URL", Some("http://localhost:9999/dav/")),
                ("WEBDAV_USERNAME", Some("user")),
                ("WEBDAV_PASSWORD", Some("pass")),
            ],
            || -> Result<(), StorageError> {
                let store = WebdavStorage::from_env()?;
                let err = store.list().await.unwrap_err();
                assert!(matches!(err, StorageError::Config(_)));
                Ok(())
            },
        )
        .unwrap();
    }
}
