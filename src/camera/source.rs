use std::fmt;
use std::time::Instant;

use async_trait::async_trait;

// ---------------------------------------------------------------------------
// FrameType
// ---------------------------------------------------------------------------

/// The type of video frame data carried in a [`CapturedFrame`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    /// Raw YUV (planar or semi-planar) frame.
    Yuv,
    /// H.264 Annex B byte stream (NALUs prefixed with 0x00000001).
    H264AnnexB,
}

// ---------------------------------------------------------------------------
// CapturedFrame
// ---------------------------------------------------------------------------

/// A single video frame captured from a camera source.
///
/// The payload is stored in `data`; the interpretation depends on `frame_type`:
/// - [`FrameType::H264AnnexB`] → raw H.264 NALUs
/// - [`FrameType::Yuv`]       → raw YUV data (format depends on the source)
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    /// Raw frame data (YUV or H.264 byte stream).
    pub data: Vec<u8>,
    /// Monotonic timestamp recorded when the frame was captured.
    pub timestamp: Instant,
    /// Width of the frame in pixels.
    pub width: u32,
    /// Height of the frame in pixels.
    pub height: u32,
    /// Whether this frame is a key‑frame (IDR in H.264).
    pub is_key_frame: bool,
    /// The encoding type of `data`.
    pub frame_type: FrameType,
}

// ---------------------------------------------------------------------------
// DeviceInfo
// ---------------------------------------------------------------------------

/// Static metadata about a camera device.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// Path to the device node (e.g. `/dev/video11`).
    pub device_path: String,
    /// Kernel driver name.
    pub driver: String,
    /// Human‑readable card name.
    pub card: String,
    /// V4L2 capability flags (human‑readable, e.g. `["VIDEO_CAPTURE",
    /// "VIDEO_M2M"]`).
    pub capabilities: Vec<String>,
}

// ---------------------------------------------------------------------------
// CameraError
// ---------------------------------------------------------------------------

/// Errors originating from camera operations.
#[derive(Debug)]
pub enum CameraError {
    /// An I/O error (device open/read/write).
    Io(std::io::Error),
    /// The requested camera device was not found.
    DeviceNotFound(String),
    /// An error from the H.264 encoder (V4L2 M2M or software).
    Encoder(String),
    /// The camera device was disconnected during streaming.
    Disconnected(String),
    /// Invalid camera configuration.
    Config(String),
}

impl fmt::Display for CameraError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CameraError::Io(err) => write!(f, "Camera I/O error: {err}"),
            CameraError::DeviceNotFound(path) => {
                write!(f, "Camera device not found: {path}")
            }
            CameraError::Encoder(msg) => write!(f, "Camera encoder error: {msg}"),
            CameraError::Disconnected(msg) => write!(f, "Camera disconnected: {msg}"),
            CameraError::Config(msg) => write!(f, "Camera config error: {msg}"),
        }
    }
}

impl std::error::Error for CameraError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CameraError::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for CameraError {
    fn from(err: std::io::Error) -> Self {
        CameraError::Io(err)
    }
}

// ---------------------------------------------------------------------------
// H264Profile — independent of the shiguredo_v4l2 types, usable without
//              the `v4l2-encoder` feature.
// ---------------------------------------------------------------------------

/// H.264 profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum H264Profile {
    Baseline,
    ConstrainedBaseline,
    Main,
    #[default]
    High,
}

// ---------------------------------------------------------------------------
// H264Level — independent of the shiguredo_v4l2 types.
// ---------------------------------------------------------------------------
/// H.264 level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum H264Level {
    Level3_0,
    Level3_1,
    Level3_2,
    Level4_0,
    Level4_1,
    #[default]
    Level4_2,
    Level5_0,
    Level5_1,
}

// ---------------------------------------------------------------------------
// CameraConfig
// ---------------------------------------------------------------------------

/// Configuration parameters for a camera source.
#[derive(Debug, Clone)]
pub struct CameraConfig {
    /// Capture width in pixels.
    pub width: u32,
    /// Capture height in pixels.
    pub height: u32,
    /// Target frame rate (frames per second).
    pub fps: u32,
    /// Target H.264 bitrate in bits per second.
    pub bitrate_bps: u32,
    /// V4L2 device path (e.g. `/dev/video11`).
    pub device_path: String,
    /// H.264 profile.
    pub profile: H264Profile,
    /// H.264 level.
    pub level: H264Level,
    /// I‑frame period (number of frames between IDR frames).
    pub i_period: u32,
}

impl CameraConfig {
    /// Create a default configuration at the given resolution and bitrate.
    #[must_use]
    pub fn new(width: u32, height: u32, bitrate_bps: u32) -> Self {
        Self {
            width,
            height,
            fps: 30,
            bitrate_bps,
            // No invented device default: production wiring sets this from
            // the app config (camera.device / camera.encoder_device);
            // validate() rejects an empty path before any hardware use.
            device_path: String::new(),
            profile: H264Profile::default(),
            level: H264Level::default(),
            i_period: 30,
        }
    }

    /// Validate the configuration, returning [`CameraError::Config`] on
    /// failure.
    ///
    /// # Errors
    ///
    /// Returns `CameraError::Config` if any parameter is out of range.
    pub fn validate(&self) -> Result<(), CameraError> {
        if self.width == 0 || self.width > 7680 {
            return Err(CameraError::Config(format!(
                "width must be 1..7680, got {}",
                self.width
            )));
        }
        if self.height == 0 || self.height > 4320 {
            return Err(CameraError::Config(format!(
                "height must be 1..4320, got {}",
                self.height
            )));
        }
        if self.bitrate_bps == 0 || self.bitrate_bps > 200_000_000 {
            return Err(CameraError::Config(format!(
                "bitrate_bps must be 1..200000000, got {}",
                self.bitrate_bps
            )));
        }
        if self.fps == 0 || self.fps > 120 {
            return Err(CameraError::Config(format!(
                "fps must be 1..120, got {}",
                self.fps
            )));
        }
        if self.device_path.is_empty() {
            return Err(CameraError::Config(
                "device_path must not be empty".to_string(),
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// FrameProducer trait
// ---------------------------------------------------------------------------

/// Produces raw YUV video frames to be encoded by an H.264 encoder
/// (V4L2 M2M hardware node or the in-process software encoder).
///
/// Implementations capture from a camera sensor (direct V4L2 capture from
/// `/dev/video0`, USB/UVC or platform nodes) and return raw I420 (YUV420
/// planar) data.
///
/// This trait is **synchronous** — the encoding thread calls
/// `next_yuv_frame` in a tight loop.  Producers that perform I/O should
/// use blocking system calls.
pub trait FrameProducer: Send + 'static {
    /// Return the next raw YUV frame.
    ///
    /// # Errors
    ///
    /// Returns [`CameraError::Disconnected`] if the camera has been
    /// removed, or [`CameraError::Io`] for transient failures (the
    /// encoder loop will retry).
    fn next_yuv_frame(&mut self) -> Result<Vec<u8>, CameraError>;

    /// Width and height of frames produced by this source.
    fn resolution(&self) -> (u32, u32);

    /// Target frame rate in frames per second.
    fn fps(&self) -> u32;
}

// ---------------------------------------------------------------------------
// CameraSource trait
// ---------------------------------------------------------------------------

/// A camera source that can be started, stopped, and polled for frames.
///
/// Implementations are expected to be **single‑stream** — each instance
/// captures from exactly one camera device. Multi‑camera workflows compose
/// multiple `CameraSource` instances.
#[async_trait]
pub trait CameraSource: Send + Sync {
    /// Start capturing frames.
    ///
    /// # Errors
    ///
    /// Returns [`CameraError::DeviceNotFound`] if the underlying device
    /// cannot be opened, [`CameraError::Config`] if the configuration is
    /// invalid, or [`CameraError::Io`] for other I/O failures.
    async fn start(&mut self) -> Result<(), CameraError>;

    /// Stop capturing frames and release device resources.
    ///
    /// # Errors
    ///
    /// May return [`CameraError::Io`] if device shutdown fails.
    async fn stop(&mut self) -> Result<(), CameraError>;

    /// Block until the next frame is available.
    ///
    /// # Errors
    ///
    /// Returns [`CameraError::Disconnected`] if the device has been removed
    /// or the stream ended unexpectedly.
    async fn next_frame(&mut self) -> Result<CapturedFrame, CameraError>;

    /// Return a reference to the device information structure.
    #[must_use]
    fn device_info(&self) -> &DeviceInfo;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    // -- DeviceInfo ---------------------------------------------------------

    #[test]
    fn test_device_info_default() {
        let info = DeviceInfo {
            device_path: "/dev/video11".to_string(),
            driver: "bcm2835-codec".to_string(),
            card: "bcm2835-codec-encode".to_string(),
            capabilities: vec!["VIDEO_M2M".to_string()],
        };
        assert_eq!(info.device_path, "/dev/video11");
        assert_eq!(info.driver, "bcm2835-codec");
        assert!(info.capabilities.contains(&"VIDEO_M2M".to_string()));
    }

    // -- H264Profile --------------------------------------------------------

    #[test]
    fn test_h264_profile_default() {
        assert_eq!(H264Profile::default(), H264Profile::High);
    }

    #[test]
    fn test_h264_profile_debug() {
        let p = H264Profile::Baseline;
        assert!(!format!("{p:?}").is_empty());
    }

    // -- H264Level ----------------------------------------------------------

    #[test]
    fn test_h264_level_default() {
        assert_eq!(H264Level::default(), H264Level::Level4_2);
    }

    #[test]
    fn test_h264_level_ordering() {
        assert_ne!(H264Level::Level3_0, H264Level::Level5_0);
    }

    // -- CameraConfig -------------------------------------------------------

    #[test]
    fn test_camera_config_default() {
        let cfg = CameraConfig::new(1920, 1080, 4_000_000);
        assert_eq!(cfg.width, 1920);
        assert_eq!(cfg.height, 1080);
        assert_eq!(cfg.bitrate_bps, 4_000_000);
        assert_eq!(cfg.fps, 30);
        // No invented device default: callers wire the configured device
        // node; an empty path is rejected by validate().
        assert_eq!(cfg.device_path, "");
        assert!(
            cfg.validate().is_err(),
            "empty device_path must be rejected"
        );
        let mut cfg = cfg;
        cfg.device_path = "/dev/video11".to_string();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_camera_config_custom() {
        let cfg = CameraConfig {
            width: 1280,
            height: 720,
            fps: 15,
            bitrate_bps: 2_000_000,
            device_path: "/dev/video11".to_string(),
            profile: H264Profile::Main,
            level: H264Level::Level4_0,
            i_period: 15,
        };
        assert_eq!(cfg.width, 1280);
        assert_eq!(cfg.height, 720);
        assert_eq!(cfg.fps, 15);
        assert!(cfg.validate().is_ok());
    }

    // -- CameraConfig::validate ---------------------------------------------

    #[test]
    fn test_validate_zero_width() {
        let cfg = CameraConfig {
            width: 0,
            height: 1080,
            fps: 30,
            bitrate_bps: 4_000_000,
            device_path: "/dev/video11".to_string(),
            profile: H264Profile::High,
            level: H264Level::Level4_2,
            i_period: 30,
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_validate_zero_height() {
        let cfg = CameraConfig {
            width: 1920,
            height: 0,
            fps: 30,
            bitrate_bps: 4_000_000,
            device_path: "/dev/video11".to_string(),
            profile: H264Profile::High,
            level: H264Level::Level4_2,
            i_period: 30,
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_validate_zero_bitrate() {
        let cfg = CameraConfig {
            width: 1920,
            height: 1080,
            fps: 30,
            bitrate_bps: 0,
            device_path: "/dev/video11".to_string(),
            profile: H264Profile::High,
            level: H264Level::Level4_2,
            i_period: 30,
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_validate_zero_fps() {
        let cfg = CameraConfig {
            width: 640,
            height: 480,
            fps: 0,
            bitrate_bps: 1_000_000,
            device_path: "/dev/video11".to_string(),
            profile: H264Profile::High,
            level: H264Level::Level4_2,
            i_period: 30,
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_validate_empty_device_path() {
        let cfg = CameraConfig {
            width: 640,
            height: 480,
            fps: 30,
            bitrate_bps: 1_000_000,
            device_path: String::new(),
            profile: H264Profile::High,
            level: H264Level::Level4_2,
            i_period: 30,
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_validate_excessive_fps() {
        let cfg = CameraConfig {
            fps: 121,
            ..CameraConfig::new(640, 480, 1_000_000)
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_validate_excessive_width() {
        let cfg = CameraConfig {
            width: 8000,
            ..CameraConfig::new(640, 480, 1_000_000)
        };
        assert!(cfg.validate().is_err());
    }

    // -- CameraError --------------------------------------------------------

    #[test]
    fn test_camera_error_display_io() {
        let err = CameraError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "device missing",
        ));
        let msg = err.to_string();
        assert!(msg.contains("device missing"), "{msg}");
    }

    #[test]
    fn test_camera_error_display_device_not_found() {
        let err = CameraError::DeviceNotFound("/dev/video11".to_string());
        assert_eq!(err.to_string(), "Camera device not found: /dev/video11");
    }

    #[test]
    fn test_camera_error_display_encoder() {
        let err = CameraError::Encoder("init failed".to_string());
        assert_eq!(err.to_string(), "Camera encoder error: init failed");
    }

    #[test]
    fn test_camera_error_display_disconnected() {
        let err = CameraError::Disconnected("device removed".to_string());
        assert_eq!(err.to_string(), "Camera disconnected: device removed");
    }

    #[test]
    fn test_camera_error_display_config() {
        let err = CameraError::Config("bad resolution".to_string());
        assert_eq!(err.to_string(), "Camera config error: bad resolution");
    }

    #[test]
    fn test_camera_error_debug() {
        let err = CameraError::Config("test".to_string());
        assert!(format!("{err:?}").contains("Config"));
    }

    #[test]
    fn test_camera_error_source() {
        let io_err = std::io::Error::other("disk");
        let err = CameraError::Io(io_err);
        assert!(err.source().is_some());

        let cfg_err = CameraError::Config("bad".to_string());
        assert!(cfg_err.source().is_none());
    }

    #[test]
    fn test_camera_error_from_io() {
        let io_err = std::io::Error::other("fail");
        let err: CameraError = io_err.into();
        assert!(matches!(err, CameraError::Io(_)));
    }

    // -- CapturedFrame ------------------------------------------------------

    #[test]
    fn test_captured_frame_construction() {
        let frame = CapturedFrame {
            data: vec![0u8; 100],
            timestamp: Instant::now(),
            width: 640,
            height: 480,
            is_key_frame: true,
            frame_type: FrameType::H264AnnexB,
        };
        assert_eq!(frame.data.len(), 100);
        assert!(frame.is_key_frame);
        assert_eq!(frame.frame_type, FrameType::H264AnnexB);
    }

    #[test]
    fn test_frame_type_debug() {
        assert!(!format!("{:?}", FrameType::Yuv).is_empty());
        assert!(!format!("{:?}", FrameType::H264AnnexB).is_empty());
    }

    // -- Mock CameraSource (interface compliance) ---------------------------

    struct MockCamera {
        info: DeviceInfo,
        started: bool,
        frames: Vec<CapturedFrame>,
    }

    #[async_trait]
    impl CameraSource for MockCamera {
        async fn start(&mut self) -> Result<(), CameraError> {
            if self.started {
                return Err(CameraError::Config("already started".to_string()));
            }
            self.started = true;
            Ok(())
        }

        async fn stop(&mut self) -> Result<(), CameraError> {
            self.started = false;
            Ok(())
        }

        async fn next_frame(&mut self) -> Result<CapturedFrame, CameraError> {
            if !self.started {
                return Err(CameraError::Disconnected("not started".to_string()));
            }
            if self.frames.is_empty() {
                return Err(CameraError::Disconnected("no more frames".to_string()));
            }
            Ok(self.frames.remove(0))
        }

        fn device_info(&self) -> &DeviceInfo {
            &self.info
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_mock_camera_lifecycle() {
        let info = DeviceInfo {
            device_path: "/dev/mock".to_string(),
            driver: "mock".to_string(),
            card: "Mock Camera".to_string(),
            capabilities: vec![],
        };
        let frame = CapturedFrame {
            data: vec![0u8; 10],
            timestamp: Instant::now(),
            width: 640,
            height: 480,
            is_key_frame: true,
            frame_type: FrameType::H264AnnexB,
        };
        let mut cam = MockCamera {
            info,
            started: false,
            frames: vec![frame],
        };

        // start → next → stop
        cam.start().await.unwrap();
        assert_eq!(cam.device_info().driver, "mock");
        let f = cam.next_frame().await.unwrap();
        assert!(f.is_key_frame);
        assert!(cam.next_frame().await.is_err()); // depleted

        cam.stop().await.unwrap();
        assert!(cam.next_frame().await.is_err()); // not started
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_mock_camera_double_start() {
        let mut cam = MockCamera {
            info: DeviceInfo {
                device_path: "/dev/mock".to_string(),
                driver: "mock".to_string(),
                card: "Mock Camera".to_string(),
                capabilities: vec![],
            },
            started: false,
            frames: vec![],
        };
        cam.start().await.unwrap();
        let err = cam.start().await.unwrap_err();
        assert!(matches!(err, CameraError::Config(_)));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_mock_camera_next_before_start() {
        let mut cam = MockCamera {
            info: DeviceInfo {
                device_path: "/dev/mock".to_string(),
                driver: "mock".to_string(),
                card: "Mock Camera".to_string(),
                capabilities: vec![],
            },
            started: false,
            frames: vec![],
        };
        let err = cam.next_frame().await.unwrap_err();
        assert!(matches!(err, CameraError::Disconnected(_)));
    }
}
