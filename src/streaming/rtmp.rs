//! Minimal RTMP push client.

use std::io;

#[derive(Debug)]
pub enum RtmpError {
    Io(io::Error),
    Handshake(String),
    Protocol(String),
    Connect(String),
}

impl std::fmt::Display for RtmpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RtmpError::Io(e) => write!(f, "IO error: {}", e),
            RtmpError::Handshake(s) => write!(f, "Handshake error: {}", s),
            RtmpError::Protocol(s) => write!(f, "Protocol error: {}", s),
            RtmpError::Connect(s) => write!(f, "Connection error: {}", s),
        }
    }
}

impl std::error::Error for RtmpError {}

impl From<io::Error> for RtmpError {
    fn from(e: io::Error) -> Self {
        RtmpError::Io(e)
    }
}

pub struct RtmpPusher {
    url: String,
}

impl RtmpPusher {
    pub fn new(url: &str) -> Self {
        Self {
            url: url.to_string(),
        }
    }

    /// Parse RTMP URL and extract host, port, app, stream key
    pub fn parse_url(&self) -> Result<(String, u16, String, String), RtmpError> {
        // rtmp://host:port/app/streamkey
        let url = self
            .url
            .strip_prefix("rtmp://")
            .ok_or_else(|| RtmpError::Connect("invalid URL".into()))?;
        let (host_port, path) = url.split_once('/').unwrap_or((url, ""));
        let (host, port) = if let Some((h, p)) = host_port.split_once(':') {
            (h.to_string(), p.parse().unwrap_or(1935))
        } else {
            (host_port.to_string(), 1935u16)
        };
        let parts: Vec<&str> = path.splitn(2, '/').collect();
        let app = parts.first().unwrap_or(&"live").to_string();
        let stream_key = parts.get(1).unwrap_or(&"test").to_string();
        Ok((host, port, app, stream_key))
    }

    /// Generate RTMP handshake C0+C1 bytes (1 + 1536 = 1537 bytes)
    pub fn generate_handshake_c0c1() -> Vec<u8> {
        let mut buf = Vec::with_capacity(1537);
        buf.push(3); // C0: version 3
                     // C1: 4 bytes timestamp + 4 bytes zero + 1528 bytes random
        buf.extend_from_slice(&0u32.to_be_bytes()); // timestamp
        buf.extend_from_slice(&0u32.to_be_bytes()); // zero
        buf.extend(std::iter::repeat_n(0u8, 1528)); // random (zeros for simplicity)
        buf
    }

    /// Build FLV video tag for H.264 NALU
    pub fn build_flv_video_tag(data: &[u8], is_keyframe: bool) -> Vec<u8> {
        let mut tag = Vec::new();
        // Tag type: video (9)
        tag.push(9);
        // Data size (3 bytes, big-endian) — will be filled later
        let data_size = data.len() as u32;
        tag.push(((data_size >> 16) & 0xFF) as u8);
        tag.push(((data_size >> 8) & 0xFF) as u8);
        tag.push((data_size & 0xFF) as u8);
        // Timestamp (3 bytes) + timestamp extended (1 byte)
        tag.extend_from_slice(&[0, 0, 0, 0]);
        // Stream ID (3 bytes, always 0)
        tag.extend_from_slice(&[0, 0, 0]);
        // Video data: frame type (1=keyframe, 2=inter) + codec ID (7=AVC)
        let frame_type = if is_keyframe { 0x17 } else { 0x27 };
        tag.push(frame_type);
        // AVC packet type (0=seq header, 1=NALU)
        tag.push(0x01);
        // Composition time (3 bytes)
        tag.extend_from_slice(&[0, 0, 0]);
        // NALU data
        tag.extend_from_slice(data);
        tag
    }

    pub fn build_connect_command(_app: &str, _tc_url: &str) -> Vec<u8> {
        let mut buf = Vec::new();
        // AMF0 string: "connect"
        buf.push(0x02); // string marker
        let cmd = b"connect";
        buf.push((cmd.len() >> 8) as u8);
        buf.push(cmd.len() as u8);
        buf.extend_from_slice(cmd);
        // Transaction ID (AMF0 number): 1.0
        buf.push(0x00); // number marker
        buf.extend_from_slice(&f64::to_be_bytes(1.0));
        // Null object (simplified — real RTMP sends an object with app/tcUrl)
        buf.push(0x05); // null marker
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_url_standard() {
        let p = RtmpPusher::new("rtmp://example.com:1935/live/test");
        let (host, port, app, key) = p.parse_url().unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 1935);
        assert_eq!(app, "live");
        assert_eq!(key, "test");
    }

    #[test]
    fn test_parse_url_default_port() {
        let p = RtmpPusher::new("rtmp://example.com/live/stream1");
        let (host, port, _, _) = p.parse_url().unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 1935);
    }

    #[test]
    fn test_parse_url_invalid() {
        let p = RtmpPusher::new("http://wrong");
        assert!(p.parse_url().is_err());
    }

    #[test]
    fn test_handshake_c0c1_size() {
        let buf = RtmpPusher::generate_handshake_c0c1();
        assert_eq!(buf.len(), 1537);
        assert_eq!(buf[0], 3); // version
    }

    #[test]
    fn test_flv_video_tag_keyframe() {
        let nalu = vec![0x65; 100]; // IDR NALU
        let tag = RtmpPusher::build_flv_video_tag(&nalu, true);
        assert_eq!(tag[0], 9); // video tag type
        assert!(tag[11] & 0xF0 == 0x10); // keyframe bit
        assert_eq!(tag[11] & 0x0F, 0x07); // AVC codec
    }

    #[test]
    fn test_flv_video_tag_interframe() {
        let nalu = vec![0x41; 50];
        let tag = RtmpPusher::build_flv_video_tag(&nalu, false);
        assert!(tag[11] & 0xF0 == 0x20); // interframe
    }

    #[test]
    fn test_connect_command() {
        let cmd = RtmpPusher::build_connect_command("live", "rtmp://localhost/live");
        assert_eq!(cmd[0], 0x02); // string marker
                                  // Should contain "connect" somewhere
        assert!(cmd.windows(7).any(|w| w == b"connect"));
    }
}
