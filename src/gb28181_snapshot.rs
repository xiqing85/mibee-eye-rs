//! Product glue for the gb28181-rs `SnapshotExecutor` (issue
//! xiqing85/mibee-eye-raspi-go#28, GB/T 28181-2022 A.2.1.24 +
//! A.2.5.7): capture JPEGs from the shared latest-YUV frame (the
//! `/snapshot` pipeline) and POST each body to the command's UploadURL
//! via a minimal HTTP/1.1 client — platform upload URLs are plain-LAN
//! `http://`, and this repo keeps a minimal-dependency posture (no HTTP
//! client crate for one POST shape).

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use gb28181_rs::snapshot::{SnapshotCommand, SnapshotExchange, SnapshotExecutor};
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ImageBuffer, Rgb};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::camera::v4l2_capture::LatestYuv;

/// Executes platform snapshot commands against the shared latest-YUV
/// frame. Installed via `Gb28181Server::with_snapshot_executor`.
pub struct YuvSnapshotUploader {
    /// The same shared slot the `/snapshot` endpoint serves.
    pub latest_yuv: LatestYuv,
}

impl SnapshotExecutor for YuvSnapshotUploader {
    fn execute(&self, cmd: SnapshotCommand) -> SnapshotExchange {
        let yuv = self.latest_yuv.clone();
        Box::pin(async move {
            let n = cmd.snap_num.clamp(1, 10);
            let interval = Duration::from_secs(u64::from(cmd.interval.unwrap_or(1).max(1)));
            let mut ids = Vec::new();
            let mut last_err: Option<anyhow::Error> = None;
            for i in 0..n {
                if i > 0 {
                    tokio::time::sleep(interval).await;
                }
                match capture_jpeg(&yuv) {
                    Ok(jpeg) => match http_post_jpeg(&cmd.upload_url, &jpeg).await {
                        Ok(id) => ids.push(id),
                        Err(e) => {
                            eprintln!("gb28181: snapshot upload failed: {e:#}");
                            last_err = Some(e);
                        }
                    },
                    Err(e) => {
                        eprintln!("gb28181: snapshot capture failed: {e:#}");
                        last_err = Some(e);
                    }
                }
            }
            // Empty result = failed exchange (A.2.5.7); partial results
            // stay — the platform derives complete/partial from the
            // count vs SnapNum.
            if ids.is_empty() {
                if let Some(e) = last_err {
                    return Err(e);
                }
            }
            Ok(ids)
        })
    }
}

/// One JPEG from the shared latest-YUV frame — the same conversion and
/// quality the `/snapshot` endpoint serves.
fn capture_jpeg(yuv: &LatestYuv) -> Result<Vec<u8>> {
    let guard = yuv
        .lock()
        .map_err(|_| anyhow!("latest-YUV slot poisoned"))?;
    let (w, h, data) = guard
        .as_ref()
        .ok_or_else(|| anyhow!("no frame available"))?;
    let rgb = crate::web::api::yuv420_to_rgb(data, *w, *h);
    let img = ImageBuffer::<Rgb<u8>, Vec<u8>>::from_raw(*w, *h, rgb)
        .ok_or_else(|| anyhow!("frame {w}x{h} invalid"))?;
    let mut buf = Vec::new();
    let mut enc = JpegEncoder::new_with_quality(&mut buf, 40);
    enc.encode_image(&DynamicImage::ImageRgb8(img))
        .context("jpeg encode")?;
    Ok(buf)
}

/// Minimal one-shot HTTP/1.1 POST for a LAN platform URL (http:// only,
/// `Connection: close`). Returns the reply JSON's `path` — the
/// uploaded-file ID echoed in the completion notify.
async fn http_post_jpeg(url: &str, jpeg: &[u8]) -> Result<String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow!("unsupported upload URL (http:// only): {url}"))?;
    let (host_port, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };

    let mut stream = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect(host_port),
    )
    .await
    .context("connect timeout")?
    .with_context(|| format!("connect {host_port}"))?;

    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        jpeg.len()
    );
    stream
        .write_all(head.as_bytes())
        .await
        .context("write head")?;
    stream.write_all(jpeg).await.context("write body")?;

    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut raw))
        .await
        .context("reply timeout")?
        .context("read reply")?;
    let text = String::from_utf8_lossy(&raw);

    let status_line = text.lines().next().unwrap_or_default().to_string();
    let ok = status_line
        .split_whitespace()
        .nth(1)
        .map(|code| code.starts_with('2'))
        .unwrap_or(false);
    if !ok {
        return Err(anyhow!("platform answered {status_line}"));
    }
    let (_, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow!("reply without header separator"))?;
    let value: serde_json::Value = serde_json::from_str(body.trim()).with_context(|| {
        format!(
            "decode reply: {}",
            body.chars().take(120).collect::<String>()
        )
    })?;
    value
        .get("path")
        .and_then(|p| p.as_str())
        .filter(|p| !p.is_empty())
        .map(|p| p.to_string())
        .ok_or_else(|| anyhow!("reply carries no path"))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Serves exactly one upload request like the NVR's endpoint: raw
    /// JPEG body, JSON `{"path":...}` reply. Returns the captured
    /// request line and body. Operates on raw bytes — the JPEG body is
    /// not UTF-8.
    async fn serve_one_upload(listener: TcpListener) -> (String, Vec<u8>) {
        let (mut conn, _) = listener.accept().await.expect("accept");
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = conn.read(&mut chunk).await.expect("read request");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            let sep = buf
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .expect("separator");
            let head = String::from_utf8_lossy(&buf[..sep]).to_string();
            let body_len = buf.len() - sep - 4;
            let len: usize = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(": ")?;
                    if k.eq_ignore_ascii_case("content-length") {
                        v.trim().parse().ok()
                    } else {
                        None
                    }
                })
                .expect("content-length");
            if body_len >= len {
                break;
            }
        }
        let sep = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("separator");
        let head = String::from_utf8_lossy(&buf[..sep]).to_string();
        let request_line = head.lines().next().unwrap_or_default().to_string();
        let jpeg = buf[sep + 4..].to_vec();

        let reply = r#"{"status":"stored","path":"store/2026/09/10/frame.jpg"}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            reply.len(),
            reply
        );
        conn.write_all(response.as_bytes())
            .await
            .expect("write reply");
        conn.shutdown().await.ok();
        (request_line, jpeg)
    }

    /// A small valid YUV420 frame (4x2 gray) in the shared slot.
    fn yuv_slot_value() -> LatestYuv {
        // 4*2 luma + 4 chroma-a + 4 chroma-b.
        Arc::new(std::sync::Mutex::new(Some((
            4,
            2,
            vec![128u8; 4 * 2 + 4 + 4],
        ))))
    }

    #[tokio::test]
    async fn http_post_posts_jpeg_verbatim_and_parses_path() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!(
            "http://{addr}/api/gb28181/snapshot/upload?session=0123456789abcdef0123456789abcdef"
        );

        let jpeg: Vec<u8> = vec![0xFF, 0xD8, 0x00, 0x01, 0x02, 0xFF, 0xD9];
        let url_owned = url.clone();
        let jpeg_owned = jpeg.clone();
        let poster = tokio::spawn(async move { http_post_jpeg(&url_owned, &jpeg_owned).await });
        let (request_line, body) = serve_one_upload(listener).await;
        let id = poster.await.unwrap().expect("post ok");

        assert_eq!(
            request_line,
            "POST /api/gb28181/snapshot/upload?session=0123456789abcdef0123456789abcdef HTTP/1.1"
        );
        assert_eq!(body, jpeg);
        assert_eq!(id, "store/2026/09/10/frame.jpg");
    }

    #[tokio::test]
    async fn uploader_executes_capture_and_upload() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/upload");

        let uploader = YuvSnapshotUploader {
            latest_yuv: yuv_slot_value(),
        };
        let exec = tokio::spawn(uploader.execute(SnapshotCommand {
            snap_num: 1,
            interval: None,
            upload_url: url,
            session_id: "s".to_string(),
        }));
        let (request_line, body) = serve_one_upload(listener).await;
        let ids = exec.await.unwrap().expect("exchange ok");

        assert_eq!(request_line, "POST /upload HTTP/1.1");
        // Real JPEG out of the YUV→RGB→JPEG pipeline.
        assert!(body.len() > 100, "jpeg len = {}", body.len());
        assert_eq!(body[0], 0xFF);
        assert_eq!(body[1], 0xD8);
        assert_eq!(ids, vec!["store/2026/09/10/frame.jpg".to_string()]);
    }

    #[tokio::test]
    async fn empty_slot_fails_the_exchange() {
        let uploader = YuvSnapshotUploader {
            latest_yuv: Arc::new(std::sync::Mutex::new(None)),
        };
        let ids = uploader
            .execute(SnapshotCommand {
                snap_num: 1,
                interval: None,
                upload_url: "http://127.0.0.1:1/upload".to_string(),
                session_id: "s".to_string(),
            })
            .await;
        assert!(ids.is_err(), "want error, got {ids:?}");
    }
}
