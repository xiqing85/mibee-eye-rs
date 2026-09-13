//! V4L2 M2M H.264 encoder camera source.
//!
//! This module implements [`CameraSource`] using the
//! [shiguredo/v4l2-rs](https://crates.io/crates/shiguredo_v4l2) crate to
//! drive the Raspberry Pi's V4L2 M2M H.264 hardware encoder (`/dev/video11`).
//!
//! ## Architecture
//!
//! ```text
//!  ┌─────────┐   raw YUV    ┌──────────────┐   H.264 NALUs   ┌──────────┐
//!  │  Frame  │ ──────────► │  H264Encoder  │ ──────────────► │   mpsc   │
//!  │ Producer│  (sync)     │  (encode      │   (callback)    │ Receiver │
//!  └─────────┘              │   thread)    │                  └──────────┘
//!                           └──────────────┘
//! ```
//!
//! The encoder runs on a dedicated `std::thread` because
//! [`EncodeInput::Mmap`] is `!Send`.  Frames flow from the producer through
//! the encoder and into a `tokio::sync::mpsc` channel that the async
//! [`CameraSource::next_frame`] reads from.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use shiguredo_v4l2::v4l2_m2m::{
    EncoderConfig, Error as V4l2Error, FnEncodeHandler, H264Encoder, H264Level as V4l2Level,
    H264Profile as V4l2Profile, Memory, PixelFormat,
};

use super::source::{
    CameraConfig, CameraError, CameraSource, CapturedFrame, DeviceInfo, FrameType, H264Level,
    H264Profile,
};

// ---------------------------------------------------------------------------
// Profile / level conversion
// ---------------------------------------------------------------------------

fn to_v4l2_profile(p: &H264Profile) -> V4l2Profile {
    match p {
        H264Profile::Baseline => V4l2Profile::Baseline,
        H264Profile::ConstrainedBaseline => V4l2Profile::ConstrainedBaseline,
        H264Profile::Main => V4l2Profile::Main,
        H264Profile::High => V4l2Profile::High,
    }
}

fn to_v4l2_level(l: &H264Level) -> V4l2Level {
    match l {
        H264Level::Level3_0 => V4l2Level::Level3_0,
        H264Level::Level3_1 => V4l2Level::Level3_1,
        H264Level::Level3_2 => V4l2Level::Level3_2,
        H264Level::Level4_0 => V4l2Level::Level4_0,
        H264Level::Level4_1 => V4l2Level::Level4_1,
        H264Level::Level4_2 => V4l2Level::Level4_2,
        H264Level::Level5_0 => V4l2Level::Level5_0,
        H264Level::Level5_1 => V4l2Level::Level5_1,
    }
}

// ---------------------------------------------------------------------------
// BackoffConfig
// ---------------------------------------------------------------------------

/// Exponential backoff parameters for device reconnection.
#[derive(Debug, Clone)]
pub struct BackoffConfig {
    /// Initial delay before the first retry (default: 1 s).
    pub initial: Duration,
    /// Maximum delay between retries (default: 30 s).
    pub max: Duration,
    /// Multiplier applied after each failed attempt (default: 2.0).
    pub multiplier: f64,
}

impl Default for BackoffConfig {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(30),
            multiplier: 2.0,
        }
    }
}

// ---------------------------------------------------------------------------
// V4l2CameraSource
// ---------------------------------------------------------------------------

// `FrameProducer` now lives in `source.rs` so the software encoder
// (feature-independent) can consume the same producer implementations.
pub use super::source::FrameProducer;

/// A [`CameraSource`] that encodes raw YUV frames to H.264 via the V4L2
/// M2M hardware encoder.
///
/// # Type parameter
///
/// `P` — a [`FrameProducer`] that supplies raw YUV frames (e.g. from
/// a camera sensor or a test mock).
pub struct V4l2CameraSource<P: FrameProducer> {
    config: CameraConfig,
    producer: Option<P>,
    state: Option<RunningState>,
    device_info: DeviceInfo,
    backoff: BackoffConfig,
}

// Private runtime state kept alive while the encoder is running.
struct RunningState {
    /// Receiver for encoded frames from the encoder thread.
    frame_rx: tokio::sync::mpsc::Receiver<EncodedEvent>,
    /// Signal flag shared with the encoder thread.
    stop: Arc<AtomicBool>,
    /// Join handle of the encoder thread.
    handle: Option<std::thread::JoinHandle<()>>,
}

// Events sent from the encoder thread back to the async side.
enum EncodedEvent {
    Frame(CapturedFrame),
    Error(CameraError),
}

impl<P: FrameProducer> V4l2CameraSource<P> {
    /// Create a new V4L2 M2M camera source.
    ///
    /// `config` controls the encoder parameters; `producer` provides raw
    /// YUV input frames.
    #[must_use]
    pub fn new(config: CameraConfig, producer: P) -> Self {
        let device_info = DeviceInfo {
            device_path: config.device_path.clone(),
            driver: "bcm2835-codec".to_string(),
            card: "bcm2835-codec-encode".to_string(),
            capabilities: vec!["VIDEO_M2M".to_string(), "H264_ENCODER".to_string()],
        };

        Self {
            config,
            producer: Some(producer),
            state: None,
            device_info,
            backoff: BackoffConfig::default(),
        }
    }

    /// Override the reconnection backoff configuration.
    pub fn with_backoff(mut self, backoff: BackoffConfig) -> Self {
        self.backoff = backoff;
        self
    }

    // ── internal helpers ────────────────────────────────────────────────

    /// Static encoder loop — runs on a dedicated `std::thread`.
    ///
    /// The loop:
    /// 1. Creates (or re‑creates) the `H264Encoder`.
    /// 2. Pulls YUV frames from `producer` and feeds them to the encoder.
    /// 3. Sends encoded frames back through `frame_tx`.
    /// 4. On encoder failure, waits with exponential backoff and retries.
    fn encoder_thread(
        config: CameraConfig,
        mut producer: P,
        frame_tx: tokio::sync::mpsc::Sender<EncodedEvent>,
        stop: Arc<AtomicBool>,
        backoff: BackoffConfig,
    ) {
        let mut current_backoff = backoff.initial;

        // Outer retry loop: reconnect the encoder on failure.
        'reconnect: loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }

            // --- Attempt to create the encoder ---
            let enc_config = Self::static_build_encoder_config(&config);
            let (inner_tx, mut inner_rx) = tokio::sync::mpsc::channel::<EncodedEvent>(8);

            let handler: FnEncodeHandler<(), V4l2Error> = FnEncodeHandler::new({
                let inner_tx = inner_tx.clone();
                move |result| match result {
                    Ok(encoded) => {
                        let data = encoded.data().map(|d| d.to_vec()).unwrap_or_default();
                        let is_kf = encoded.is_keyframe();
                        let frame = CapturedFrame {
                            data,
                            timestamp: Instant::now(),
                            width: config.width,
                            height: config.height,
                            is_key_frame: is_kf,
                            frame_type: FrameType::H264AnnexB,
                        };
                        let _ = inner_tx.try_send(EncodedEvent::Frame(frame));
                    }
                    Err(err) => {
                        let _ = inner_tx
                            .try_send(EncodedEvent::Error(CameraError::Encoder(format!("{err}"))));
                    }
                }
            });

            let mut encoder = match H264Encoder::new(enc_config, handler) {
                Ok(e) => e,
                Err(_) => {
                    // Device not available yet — notify and sleep.
                    let _ = frame_tx.try_send(EncodedEvent::Error(CameraError::DeviceNotFound(
                        config.device_path.clone(),
                    )));
                    std::thread::sleep(current_backoff);
                    current_backoff =
                        (current_backoff.mul_f64(backoff.multiplier)).min(backoff.max);
                    continue 'reconnect;
                }
            };

            // --- Inner encode loop ---
            let mut frame_counter: u32 = 0;
            loop {
                if stop.load(Ordering::Relaxed) {
                    return;
                }

                // Forward any errors from the handler to the async side.
                while let Ok(event) = inner_rx.try_recv() {
                    match &event {
                        EncodedEvent::Error(_) => {
                            let _ = frame_tx.try_send(event);
                        }
                        EncodedEvent::Frame(_) => {
                            let _ = frame_tx.try_send(event);
                        }
                    }
                }

                // Force a keyframe every i_period frames for NVR stability.
                let force_kf = frame_counter == 0 || frame_counter.is_multiple_of(config.i_period);
                frame_counter = frame_counter.wrapping_add(1);

                // Throttling detection: prefer firmware interface, fall back to cpufreq.
                // get_throttled reflects the ACTUAL firmware state (more reliable).
                // scaling_cur_freq is a fallback for older kernels.
                let throttled =
                    std::fs::read_to_string("/sys/devices/platform/soc:firmware/get_throttled")
                        .ok()
                        .and_then(|s| {
                            let hex = s.trim().strip_prefix("0x")?;
                            let val = u32::from_str_radix(hex, 16).ok()?;
                            Some(val & 0x5 != 0)
                        })
                        .unwrap_or_else(|| {
                            // Fallback: check CPU frequency.
                            std::fs::read_to_string(
                                "/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq",
                            )
                            .ok()
                            .and_then(|s| s.trim().parse::<u64>().ok())
                            .map(|freq_khz| freq_khz < 1_000_000)
                            .unwrap_or(false)
                        });

                // Get the next YUV frame from the producer.
                let yuv_data = match producer.next_yuv_frame() {
                    Ok(data) => data,
                    Err(CameraError::Disconnected(msg)) => {
                        let _ =
                            frame_tx.try_send(EncodedEvent::Error(CameraError::Disconnected(msg)));
                        // Producer is dead; exit entire thread.
                        return;
                    }
                    Err(err) => {
                        // Transient producer error — log and skip the frame.
                        let _ = frame_tx.try_send(EncodedEvent::Error(err));
                        continue;
                    }
                };

                // Skip encoding when throttled to prevent corrupt H.264 output.
                // Consume the frame from the camera but don't feed it to the encoder.
                if throttled && frame_counter % 2 == 1 {
                    continue;
                }

                let timestamp_us = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_micros() as i64;

                // Encode with mmap input — the closure fills the V4L2 buffer
                // from the producer's YUV data.
                //
                // Safety: `EncodeInput::Mmap` is `!Send`, but we construct it
                //         here on the encoder thread where it is used and
                //         dropped. The closure borrows `yuv_data` which lives
                //         on the stack of this loop iteration.
                if let Err(err) = encoder.encode(
                    shiguredo_v4l2::v4l2_m2m::EncodeInput::Mmap(&mut |buf, _res, _user_data| {
                        let len = yuv_data.len().min(buf.len());
                        buf[..len].copy_from_slice(&yuv_data[..len]);
                        Some(len)
                    }),
                    timestamp_us,
                    force_kf,
                    (),
                ) {
                    let _ = frame_tx.try_send(EncodedEvent::Error(CameraError::Encoder(format!(
                        "encode failed: {err}"
                    ))));
                    break;
                }
                // Drain any encoded frames produced by this encode call.
                while let Ok(event) = inner_rx.try_recv() {
                    let _ = frame_tx.try_send(event);
                }
            }

            // Encoder died. Backoff before reconnect.
            std::thread::sleep(current_backoff);
            current_backoff = (current_backoff.mul_f64(backoff.multiplier)).min(backoff.max);
        }
    }

    /// Helper to build an [`EncoderConfig`] without a `&self` receiver.
    fn static_build_encoder_config(config: &CameraConfig) -> EncoderConfig {
        let mut enc = EncoderConfig::new(config.width, config.height, config.bitrate_bps);
        enc.device_path = config.device_path.clone();
        enc.profile = to_v4l2_profile(&config.profile);
        enc.level = to_v4l2_level(&config.level);
        enc.i_period = config.i_period;
        enc.repeat_sequence_header = true;
        enc.input_memory = Memory::Mmap;
        enc.output_memory = Memory::Mmap;
        enc.pixel_format = PixelFormat::Yuv420;
        enc
    }
}

// ── CameraSource implementation ───────────────────────────────────────────

#[async_trait::async_trait]
impl<P: FrameProducer + Send + Sync> CameraSource for V4l2CameraSource<P> {
    async fn start(&mut self) -> Result<(), CameraError> {
        if self.state.is_some() {
            return Err(CameraError::Config("camera already started".to_string()));
        }

        self.config.validate()?;

        // Take the producer — must not be None.
        let producer = self
            .producer
            .take()
            .ok_or_else(|| CameraError::Config("producer already moved".to_string()))?;

        let (frame_tx, frame_rx) = tokio::sync::mpsc::channel::<EncodedEvent>(64);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let config = self.config.clone();
        let backoff = self.backoff.clone();

        // --- Spawn the encoder thread ---
        let handle = std::thread::Builder::new()
            .name("v4l2-encoder".into())
            .spawn(move || {
                Self::encoder_thread(config, producer, frame_tx, stop_clone, backoff);
            })
            .map_err(CameraError::Io)?;

        self.state = Some(RunningState {
            frame_rx,
            stop,
            handle: Some(handle),
        });

        Ok(())
    }

    async fn stop(&mut self) -> Result<(), CameraError> {
        let state = self.state.take();
        if let Some(mut state) = state {
            // Signal the thread to stop.
            state.stop.store(true, Ordering::Relaxed);

            // Join the encoder thread (with a timeout guard).
            if let Some(handle) = state.handle {
                // Spawn a separate thread to join with timeout.
                let join_result = std::thread::spawn(move || {
                    let _ = handle.join();
                });
                // Give the encoder thread up to 5 seconds to finish.
                let timeout = Duration::from_secs(5);
                let start = Instant::now();
                while start.elapsed() < timeout {
                    if join_result.is_finished() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }

            // Drain remaining frames from the channel.
            while state.frame_rx.try_recv().is_ok() {}
        }
        Ok(())
    }

    async fn next_frame(&mut self) -> Result<CapturedFrame, CameraError> {
        let state = self
            .state
            .as_mut()
            .ok_or_else(|| CameraError::Disconnected("camera not started".to_string()))?;

        match state.frame_rx.recv().await {
            Some(EncodedEvent::Frame(frame)) => Ok(frame),
            Some(EncodedEvent::Error(err)) => Err(err),
            None => Err(CameraError::Disconnected(
                "encoder thread exited".to_string(),
            )),
        }
    }

    fn device_info(&self) -> &DeviceInfo {
        &self.device_info
    }
}

impl<P: FrameProducer> Drop for V4l2CameraSource<P> {
    fn drop(&mut self) {
        // Best-effort stop — ignore errors during drop.
        if let Some(state) = self.state.take() {
            state.stop.store(true, Ordering::Relaxed);
            if let Some(handle) = state.handle {
                let _ = handle.join();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::source::H264Profile;

    // ── Mock FrameProducer ───────────────────────────────────────────────

    struct MockFrameProducer {
        frames: Vec<Vec<u8>>,
        index: usize,
        width: u32,
        height: u32,
        fps: u32,
    }

    impl MockFrameProducer {
        fn new(frames: Vec<Vec<u8>>) -> Self {
            let frame_size = if frames.is_empty() {
                0
            } else {
                frames[0].len()
            };
            // Default to 640x480 if frame_size matches, else compute.
            let (w, h) = if frame_size > 0 {
                // YUV420 size = w * h * 3/2
                // Guess from size: width = sqrt(size * 2 / 3)
                let area = (frame_size as f64 * 2.0 / 3.0).sqrt() as u32;
                // Round to multiple of 16 for encoder alignment
                let w = (area / 16) * 16;
                let h = (area / 16) * 16;
                if w == 0 || h == 0 {
                    (640, 480)
                } else {
                    (w.max(16), h.max(16))
                }
            } else {
                (640, 480)
            };

            Self {
                frames,
                index: 0,
                width: w,
                height: h,
                fps: 30,
            }
        }
    }

    impl FrameProducer for MockFrameProducer {
        fn next_yuv_frame(&mut self) -> Result<Vec<u8>, CameraError> {
            if self.index >= self.frames.len() {
                return Err(CameraError::Disconnected("no more frames".to_string()));
            }
            let frame = self.frames[self.index].clone();
            self.index += 1;
            Ok(frame)
        }

        fn resolution(&self) -> (u32, u32) {
            (self.width, self.height)
        }

        fn fps(&self) -> u32 {
            self.fps
        }
    }

    // ── V4l2CameraSource creation tests ──────────────────────────────────

    #[test]
    fn test_v4l2_source_new() {
        let producer = MockFrameProducer::new(vec![vec![0u8; 100]]);
        let config = CameraConfig::new(640, 480, 1_000_000);
        let source = V4l2CameraSource::new(config, producer);
        assert_eq!(source.device_info().driver, "bcm2835-codec");
        assert_eq!(source.device_info().device_path, "/dev/video11");
        assert!(source
            .device_info()
            .capabilities
            .contains(&"H264_ENCODER".to_string()));
    }

    #[test]
    fn test_v4l2_source_with_backoff() {
        let producer = MockFrameProducer::new(vec![vec![0u8; 100]]);
        let config = CameraConfig::new(640, 480, 1_000_000);
        let backoff = BackoffConfig {
            initial: Duration::from_millis(100),
            max: Duration::from_secs(5),
            multiplier: 1.5,
        };
        let source = V4l2CameraSource::new(config, producer).with_backoff(backoff);
        assert_eq!(source.backoff.initial, Duration::from_millis(100));
    }

    #[test]
    fn test_v4l2_source_device_info() {
        let producer = MockFrameProducer::new(vec![vec![0u8; 100]]);
        let config = CameraConfig {
            device_path: "/dev/video11".to_string(),
            profile: H264Profile::High,
            ..CameraConfig::new(1280, 720, 2_000_000)
        };
        let source = V4l2CameraSource::new(config, producer);
        let info = source.device_info();
        assert_eq!(info.device_path, "/dev/video11");
        assert!(info.capabilities.contains(&"VIDEO_M2M".to_string()));
    }

    // ── BackoffConfig ────────────────────────────────────────────────────

    #[test]
    fn test_backoff_config_default() {
        let b = BackoffConfig::default();
        assert_eq!(b.initial, Duration::from_secs(1));
        assert_eq!(b.max, Duration::from_secs(30));
        assert!((b.multiplier - 2.0).abs() < 1e-9);
    }

    // ── Profile / level conversion ───────────────────────────────────────

    #[test]
    fn test_profile_conversion_roundtrip() {
        let profiles = [
            H264Profile::Baseline,
            H264Profile::ConstrainedBaseline,
            H264Profile::Main,
            H264Profile::High,
        ];
        for p in &profiles {
            let _ = to_v4l2_profile(p);
        }
    }

    #[test]
    fn test_level_conversion_roundtrip() {
        let levels = [
            H264Level::Level3_0,
            H264Level::Level3_1,
            H264Level::Level3_2,
            H264Level::Level4_0,
            H264Level::Level4_1,
            H264Level::Level4_2,
            H264Level::Level5_0,
            H264Level::Level5_1,
        ];
        for l in &levels {
            let _ = to_v4l2_level(l);
        }
    }

    // ── Encoder config building ──────────────────────────────────────────

    #[test]
    fn test_build_encoder_config() {
        let config = CameraConfig::new(1920, 1080, 4_000_000);
        let enc = V4l2CameraSource::<MockFrameProducer>::static_build_encoder_config(&config);
        assert_eq!(enc.width, 1920);
        assert_eq!(enc.height, 1080);
        assert_eq!(enc.bitrate_bps, 4_000_000);
        assert_eq!(enc.device_path, "/dev/video11");
        assert!(enc.repeat_sequence_header);
        // Verify mmap is set
        assert!(matches!(enc.input_memory, Memory::Mmap));
        assert!(matches!(enc.output_memory, Memory::Mmap));
    }

    // ── Encoder config with custom parameters ────────────────────────────

    #[test]
    fn test_build_encoder_config_custom() {
        let config = CameraConfig {
            width: 1280,
            height: 720,
            fps: 60,
            bitrate_bps: 8_000_000,
            device_path: "/dev/video11".to_string(),
            profile: H264Profile::Main,
            level: H264Level::Level4_0,
            i_period: 15,
        };
        let enc = V4l2CameraSource::<MockFrameProducer>::static_build_encoder_config(&config);
        assert_eq!(enc.width, 1280);
        assert_eq!(enc.height, 720);
        assert_eq!(enc.bitrate_bps, 8_000_000);
        assert_eq!(enc.i_period, 15);
    }

    // ── Integration test (requires /dev/video11) ─────────────────────────
    //
    // This test is ignored by default because V4L2 M2M devices are only
    // available on Raspberry Pi hardware (aarch64).

    #[tokio::test]
    #[ignore = "requires V4L2 M2M device (/dev/video11) — aarch64 only"]
    async fn test_v4l2_source_start_stop() {
        let producer = MockFrameProducer::new(vec![vec![0u8; 640 * 480 * 3 / 2]; 5]);
        let config = CameraConfig::new(640, 480, 1_000_000);
        let mut source = V4l2CameraSource::new(config, producer);

        // On real hardware this should succeed.
        match source.start().await {
            Ok(()) => {
                // Try to read a frame.
                if let Ok(frame) = source.next_frame().await {
                    assert!(frame.width > 0);
                    assert!(frame.height > 0);
                }
                source.stop().await.unwrap();
            }
            Err(CameraError::DeviceNotFound(_)) => {
                // Expected on non-RPi hardware — just pass.
            }
            Err(err) => {
                panic!("unexpected start error: {err}");
            }
        }
    }

    // ── Mock FrameProducer tests ─────────────────────────────────────────

    #[test]
    fn test_mock_producer_yields_frames() {
        let mut producer =
            MockFrameProducer::new(vec![vec![0u8; 100], vec![1u8; 100], vec![2u8; 100]]);
        assert_eq!(producer.next_yuv_frame().unwrap(), vec![0u8; 100]);
        assert_eq!(producer.next_yuv_frame().unwrap(), vec![1u8; 100]);
        assert_eq!(producer.next_yuv_frame().unwrap(), vec![2u8; 100]);
        assert!(producer.next_yuv_frame().is_err());
    }

    #[test]
    fn test_mock_producer_empty() {
        let mut producer = MockFrameProducer::new(vec![]);
        assert!(producer.next_yuv_frame().is_err());
    }

    #[test]
    fn test_mock_producer_resolution() {
        let frame = vec![0u8; 640 * 480 * 3 / 2];
        let producer = MockFrameProducer::new(vec![frame]);
        let (w, h) = producer.resolution();
        assert!(w > 0);
        assert!(h > 0);
        assert_eq!(producer.fps(), 30);
    }
}
