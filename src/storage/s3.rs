use async_trait::async_trait;
use s3::bucket::Bucket;
use s3::creds::Credentials;
use s3::region::Region;
use std::time::{Duration, SystemTime};
use tokio::time::sleep;

use super::{Segment, StorageBackend, StorageError, StoredSegment, StoredSegmentInfo};

/// Maximum retry attempts for S3 operations.
const MAX_RETRIES: u32 = 3;
/// Base delay for exponential backoff (milliseconds).
const BASE_RETRY_MS: u64 = 500;

/// S3-compatible object storage backend.
///
/// Stores segments as objects in an S3-compatible bucket.
/// Credentials are read from environment variables (never from config files):
///
/// | Variable | Required | Description |
/// |---|---|---|
/// | `S3_ENDPOINT` | Yes | S3-compatible endpoint URL (e.g. `https://s3.amazonaws.com`) |
/// | `S3_REGION` | Yes | AWS region (e.g. `us-east-1`) |
/// | `S3_BUCKET` | Yes | Bucket name to store segments in |
/// | `S3_ACCESS_KEY` | Yes | Access key ID |
/// | `S3_SECRET_KEY` | Yes | Secret access key |
pub struct S3Storage {
    bucket: Bucket,
}

impl S3Storage {
    /// Create a new `S3Storage` from environment variables.
    ///
    /// # Errors
    ///
    /// Returns `StorageError::Config` if any required env var is missing or the
    /// S3 client configuration fails.
    pub fn from_env() -> Result<Self, StorageError> {
        let endpoint = std::env::var("S3_ENDPOINT")
            .map_err(|_| StorageError::Config("S3_ENDPOINT not set".into()))?;
        let region = std::env::var("S3_REGION")
            .map_err(|_| StorageError::Config("S3_REGION not set".into()))?;
        let bucket_name = std::env::var("S3_BUCKET")
            .map_err(|_| StorageError::Config("S3_BUCKET not set".into()))?;
        let access_key = std::env::var("S3_ACCESS_KEY")
            .map_err(|_| StorageError::Config("S3_ACCESS_KEY not set".into()))?;
        let secret_key = std::env::var("S3_SECRET_KEY")
            .map_err(|_| StorageError::Config("S3_SECRET_KEY not set".into()))?;

        let credentials = Credentials::new(Some(&access_key), Some(&secret_key), None, None, None)
            .map_err(|e| StorageError::Config(format!("S3 credentials error: {e}")))?;

        let s3_region = Region::Custom {
            region: region.clone(),
            endpoint: endpoint.clone(),
        };

        let bucket = Bucket::new(&bucket_name, s3_region, credentials)
            .map_err(|e| StorageError::Config(format!("S3 bucket config error: {e}")))?;

        Ok(Self { bucket })
    }

    /// Build an object key from a segment's metadata.
    fn build_key(start_time: SystemTime, sequence_num: u64) -> String {
        let millis = start_time
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        format!("{millis}_{sequence_num:05}.m4v")
    }
}

#[async_trait]
impl StorageBackend for S3Storage {
    async fn save(&self, segment: &Segment) -> Result<StoredSegment, StorageError> {
        let key = Self::build_key(segment.start_time, segment.sequence_num);
        let data = segment.data.clone();

        let mut last_err = None;
        for attempt in 0..MAX_RETRIES {
            let result = self.bucket.put_object(&key, &data).await;

            match result {
                Ok(_response) => {
                    return Ok(StoredSegment {
                        id: key.clone(),
                        path: format!("s3://{}/{}", self.bucket.name(), &key),
                        size_bytes: segment.data.len() as u64,
                    });
                }
                Err(e) => {
                    last_err = Some(StorageError::Io(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("S3 put_object failed: {e}"),
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
                "S3 save failed after retries",
            ))
        }))
    }

    async fn list(&self) -> Result<Vec<StoredSegmentInfo>, StorageError> {
        Err(StorageError::Config(
            "S3 list not implemented (requires ListObjectsV2)".into(),
        ))
    }

    async fn delete(&self, _id: &str) -> Result<(), StorageError> {
        Err(StorageError::Config(
            "S3 delete not implemented (requires DeleteObject)".into(),
        ))
    }

    async fn cleanup_expired(&self) -> Result<u64, StorageError> {
        Err(StorageError::Config("S3 cleanup not implemented".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
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

    /// Build an S3Storage pointing at a mock server endpoint.
    /// The bucket is created with path-style addressing so requests go to
    /// `{endpoint}/{bucket}/{key}`.
    fn make_mock_storage(endpoint: &str, bucket_name: &str) -> S3Storage {
        let region = Region::Custom {
            region: "us-east-1".into(),
            endpoint: endpoint.into(),
        };
        // Use anonymous credentials for the mock
        let creds = Credentials::new(Some("minio"), Some("minio123"), None, None, None).unwrap();
        let bucket = Bucket::new(bucket_name, region, creds).unwrap();
        S3Storage { bucket }
    }

    #[tokio::test]
    async fn from_env_success() {
        temp_env::with_vars(
            [
                ("S3_ENDPOINT", Some("http://localhost:9999")),
                ("S3_REGION", Some("us-east-1")),
                ("S3_BUCKET", Some("test-bucket")),
                ("S3_ACCESS_KEY", Some("minio")),
                ("S3_SECRET_KEY", Some("minio123")),
            ],
            || {
                let store = S3Storage::from_env();
                assert!(store.is_ok());
            },
        );
    }

    #[tokio::test]
    async fn from_env_missing_var() {
        temp_env::with_vars(
            [
                ("S3_ENDPOINT", None::<&str>),
                ("S3_REGION", Some("us-east-1")),
                ("S3_BUCKET", Some("test-bucket")),
                ("S3_ACCESS_KEY", Some("minio")),
                ("S3_SECRET_KEY", Some("minio123")),
            ],
            || {
                let err = S3Storage::from_env().unwrap_err();
                assert!(matches!(err, StorageError::Config(_)));
            },
        );
    }

    #[tokio::test]
    async fn save_success() {
        let mock_server = MockServer::start().await;
        let bucket_name = "test-bucket";
        let store = make_mock_storage(&mock_server.uri(), bucket_name);

        // S3 PutObject: PUT /{bucket}/{key} -> 200 with ETag
        Mock::given(method("PUT"))
            .and(path(format!("/{bucket_name}/12345_00000.m4v")))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-amz-request-id", "req1")
                    .set_body_bytes(
                        b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><PutObjectResult><ETag>\"abc123\"</ETag></PutObjectResult>",
                    ),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(12345);
        let seg = make_segment(b"s3 data payload", start, 0);
        let saved = store.save(&seg).await.unwrap();

        assert!(saved.id.ends_with(".m4v"));
        assert_eq!(saved.size_bytes, 15);
        assert!(saved.path.contains(bucket_name));
    }

    #[tokio::test]
    async fn save_failure_after_retries() {
        let mock_server = MockServer::start().await;
        let bucket_name = "test-bucket";
        let store = make_mock_storage(&mock_server.uri(), bucket_name);

        // Always return 500 InternalError
        Mock::given(method("PUT"))
            .and(path(format!("/{bucket_name}/12345_00001.m4v")))
            .respond_with(ResponseTemplate::new(500))
            .expect(3) // MAX_RETRIES
            .mount(&mock_server)
            .await;

        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(12345);
        let seg = make_segment(b"fail", start, 1);
        let result = store.save(&seg).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn save_retry_then_success() {
        let mock_server = MockServer::start().await;
        let bucket_name = "test-bucket";
        let store = make_mock_storage(&mock_server.uri(), bucket_name);

        // First respond with 503
        Mock::given(method("PUT"))
            .and(path(format!("/{bucket_name}/12345_00002.m4v")))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&mock_server)
            .await;

        // Then respond with success
        Mock::given(method("PUT"))
            .and(path(format!("/{bucket_name}/12345_00002.m4v")))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&mock_server)
            .await;

        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(12345);
        let seg = make_segment(b"retry success", start, 2);
        let saved = store.save(&seg).await.unwrap();

        assert!(saved.id.ends_with(".m4v"));
        assert_eq!(saved.size_bytes, 13);
    }
}
