//! Software H.264 encoder camera source (openh264, feature-independent).
//!
//! Mirrors [`super::v4l2::V4l2CameraSource`]: a dedicated encoding thread
//! pulls raw I420 frames from a [`FrameProducer`] and encodes them with the
//! in-process openh264 encoder (BSD-2-Clause, built from vendored source),
//! delivering Annex-B H.264 frames over a tokio channel. Used on boards
//! without a V4L2 M2M encoder node (x86, most arm SBCs) and for
//! `camera.encoder = "software"` builds.
//!
//! The software encoder never touches a device node, so this module is NOT
//! behind the `v4l2-encoder` feature.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use async_trait::async_trait;
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, RateControlMode,
};
use openh264::formats::YUVSource;
use openh264::OpenH264API;
use tokio::sync::mpsc;

use super::source::{
    CameraConfig, CameraError, CameraSource, CapturedFrame, DeviceInfo, FrameProducer, FrameType,
};

// ---------------------------------------------------------------------------
// SoftwareCameraSource
// ---------------------------------------------------------------------------

/// A [`CameraSource`] that encodes raw YUV frames to H.264 in-process via
/// openh264 — no device node required.
///
/// # Type parameter
///
/// - `P` — the [`FrameProducer`] supplying raw I420 frames (e.g.
///   [`super::v4l2_capture::V4l2CaptureProducer`]).
pub struct SoftwareCameraSource<P: FrameProducer + Sync> {
    config: CameraConfig,
    producer: Option<P>,
    state: Option<RunningState>,
    device_info: DeviceInfo,
}

struct RunningState {
    frame_rx: mpsc::Receiver<ThreadEvent>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

enum ThreadEvent {
    Frame(CapturedFrame),
    Error(CameraError),
}

impl<P: FrameProducer + Sync> SoftwareCameraSource<P> {
    /// Create a new software-encoder camera source.
    ///
    /// `config` carries width/height/fps/bitrate/i_period; `producer`
    /// provides the raw YUV input frames.
    #[must_use]
    pub fn new(config: CameraConfig, producer: P) -> Self {
        let device_info = DeviceInfo {
            device_path: config.device_path.clone(),
            driver: "openh264".to_string(),
            card: "openh264 software encoder".to_string(),
            capabilities: vec!["H264_ENCODER".to_string(), "SOFTWARE".to_string()],
        };
        Self {
            config,
            producer: Some(producer),
            state: None,
            device_info,
        }
    }

    fn build_encoder_config(config: &CameraConfig) -> EncoderConfig {
        EncoderConfig::new()
            .bitrate(BitRate::from_bps(config.bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(config.fps as f32))
            .rate_control_mode(RateControlMode::Bitrate)
            .intra_frame_period(IntraFramePeriod::from_num_frames(config.i_period.max(1)))
    }
}

#[async_trait]
impl<P: FrameProducer + Sync> CameraSource for SoftwareCameraSource<P> {
    async fn start(&mut self) -> Result<(), CameraError> {
        if self.state.is_some() {
            return Ok(());
        }
        self.config.validate()?;

        let (tx, rx) = mpsc::channel::<ThreadEvent>(16);
        let stop = Arc::new(AtomicBool::new(false));
        let producer = self
            .producer
            .take()
            .ok_or_else(|| CameraError::Config("producer already consumed".into()))?;
        let enc_config = Self::build_encoder_config(&self.config);
        let width = self.config.width;
        let height = self.config.height;
        let stop_flag = Arc::clone(&stop);

        let handle = thread::Builder::new()
            .name("openh264-encoder".into())
            .spawn(move || {
                let mut producer = producer;
                let mut encoder =
                    match Encoder::with_api_config(OpenH264API::from_source(), enc_config) {
                        Ok(enc) => enc,
                        Err(e) => {
                            let _ = tx.blocking_send(ThreadEvent::Error(CameraError::Encoder(
                                format!("openh264 init failed: {e}"),
                            )));
                            return;
                        }
                    };

                loop {
                    if stop_flag.load(Ordering::Relaxed) {
                        break;
                    }
                    let raw = match producer.next_yuv_frame() {
                        Ok(frame) => frame,
                        Err(CameraError::Disconnected(msg)) => {
                            let _ = tx
                                .blocking_send(ThreadEvent::Error(CameraError::Disconnected(msg)));
                            break;
                        }
                        Err(e) => {
                            let _ = tx.blocking_send(ThreadEvent::Error(e));
                            break;
                        }
                    };

                    let view = I420View::new(&raw, width as usize, height as usize);
                    match encoder.encode(&view) {
                        Ok(bitstream) => {
                            let mut data = Vec::with_capacity(4096);
                            bitstream.write_vec(&mut data);
                            let key_frame = matches!(
                                bitstream.frame_type(),
                                openh264::encoder::FrameType::IDR | openh264::encoder::FrameType::I
                            );
                            let frame = CapturedFrame {
                                data,
                                timestamp: Instant::now(),
                                width,
                                height,
                                is_key_frame: key_frame,
                                frame_type: FrameType::H264AnnexB,
                            };
                            if tx.blocking_send(ThreadEvent::Frame(frame)).is_err() {
                                break; // receiver dropped — stop
                            }
                        }
                        Err(e) => {
                            let _ = tx.blocking_send(ThreadEvent::Error(CameraError::Encoder(
                                format!("openh264 encode failed: {e}"),
                            )));
                            break;
                        }
                    }
                }
            })
            .map_err(|e| CameraError::Io(std::io::Error::other(e)))?;

        self.state = Some(RunningState {
            frame_rx: rx,
            stop,
            handle: Some(handle),
        });
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), CameraError> {
        if let Some(mut state) = self.state.take() {
            state.stop.store(true, Ordering::Relaxed);
            // Drain the channel so the encoding thread's blocking_send
            // cannot deadlock on a full buffer.
            while state.frame_rx.try_recv().is_ok() {}
            if let Some(handle) = state.handle.take() {
                let _ = handle.join();
            }
        }
        Ok(())
    }

    async fn next_frame(&mut self) -> Result<CapturedFrame, CameraError> {
        let state = self
            .state
            .as_mut()
            .ok_or_else(|| CameraError::Config("software source not started".into()))?;
        match state.frame_rx.recv().await {
            Some(ThreadEvent::Frame(frame)) => Ok(frame),
            Some(ThreadEvent::Error(e)) => Err(e),
            None => Err(CameraError::Disconnected(
                "openh264 encoder thread exited".into(),
            )),
        }
    }

    fn device_info(&self) -> &DeviceInfo {
        &self.device_info
    }
}

impl<P: FrameProducer + Sync> Drop for SoftwareCameraSource<P> {
    fn drop(&mut self) {
        if let Some(mut state) = self.state.take() {
            state.stop.store(true, Ordering::Relaxed);
            while state.frame_rx.try_recv().is_ok() {}
            if let Some(handle) = state.handle.take() {
                let _ = handle.join();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// I420 zero-copy view
// ---------------------------------------------------------------------------

/// Borrows a raw I420 (YUV420 planar) buffer as an openh264 `YUVSource`.
///
/// Layout: `width*height` Y bytes, then `width*height/4` U, then V.
struct I420View<'a> {
    data: &'a [u8],
    width: usize,
    height: usize,
}

impl<'a> I420View<'a> {
    fn new(data: &'a [u8], width: usize, height: usize) -> Self {
        Self {
            data,
            width,
            height,
        }
    }

    fn y_len(&self) -> usize {
        self.width * self.height
    }

    fn uv_len(&self) -> usize {
        self.y_len() / 4
    }
}

impl YUVSource for I420View<'_> {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (self.width, self.width / 2, self.width / 2)
    }

    fn y(&self) -> &[u8] {
        &self.data[..self.y_len()]
    }

    fn u(&self) -> &[u8] {
        &self.data[self.y_len()..self.y_len() + self.uv_len()]
    }

    fn v(&self) -> &[u8] {
        &self.data[self.y_len() + self.uv_len()..self.y_len() + 2 * self.uv_len()]
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic producer: emits `frames` synthetic I420 frames, then
    /// reports the camera as disconnected so the encoder thread exits.
    struct MockProducer {
        width: u32,
        height: u32,
        frames_left: u32,
    }

    impl FrameProducer for MockProducer {
        fn next_yuv_frame(&mut self) -> Result<Vec<u8>, CameraError> {
            if self.frames_left == 0 {
                return Err(CameraError::Disconnected("mock exhausted".into()));
            }
            self.frames_left -= 1;
            let y_len = (self.width * self.height) as usize;
            let mut data = vec![0u8; y_len * 3 / 2];
            // Vary the lance plane so the encoder produces non-constant
            // input (openh264 can skip frames on a static scene).
            let seed = self.frames_left as u8;
            for (i, b) in data[..y_len].iter_mut().enumerate() {
                *b = seed.wrapping_add((i % 251) as u8);
            }
            Ok(data)
        }

        fn resolution(&self) -> (u32, u32) {
            (self.width, self.height)
        }

        fn fps(&self) -> u32 {
            15
        }
    }

    fn cfg(w: u32, h: u32) -> CameraConfig {
        // Software encoding has no device node; a placeholder satisfies
        // validate()'s non-empty device_path contract.
        CameraConfig {
            device_path: "/dev/null".to_string(),
            ..CameraConfig::new(w, h, 500_000)
        }
    }

    #[tokio::test]
    async fn first_output_contains_sps_pps_and_idr() {
        let producer = MockProducer {
            width: 64,
            height: 48,
            frames_left: 4,
        };
        let mut src = SoftwareCameraSource::new(cfg(64, 48), producer);
        src.start().await.unwrap();
        let frame = src.next_frame().await.unwrap();
        assert_eq!(frame.frame_type, FrameType::H264AnnexB);
        assert!(frame.is_key_frame, "first frame must be a key frame");
        assert!(frame.data.len() > 4);
        // Annex-B start code
        assert_eq!(&frame.data[..4], &[0, 0, 0, 1]);
        // SPS (NAL type 7) and PPS (type 8) must be present in the first
        // output — downstream parsers (RTSP/MSE/GB28181) require them.
        let types = collect_nalu_types(&frame.data);
        assert!(types.contains(&7), "SPS missing, nalus: {types:?}");
        assert!(types.contains(&8), "PPS missing, nalus: {types:?}");
        src.stop().await.unwrap();
    }

    #[tokio::test]
    async fn producer_disconnect_surfaces_as_error() {
        let producer = MockProducer {
            width: 64,
            height: 48,
            frames_left: 1,
        };
        let mut src = SoftwareCameraSource::new(cfg(64, 48), producer);
        src.start().await.unwrap();
        let _ = src.next_frame().await.unwrap();
        let err = src.next_frame().await.unwrap_err();
        assert!(
            matches!(err, CameraError::Disconnected(_)),
            "expected Disconnected, got {err:?}"
        );
    }

    #[test]
    fn i420_view_layout() {
        // 4x2 frame: y=8, u=2, v=2 → total 12 bytes
        let data: Vec<u8> = (0..12).collect();
        let view = I420View::new(&data, 4, 2);
        assert_eq!(view.dimensions(), (4, 2));
        assert_eq!(view.strides(), (4, 2, 2));
        assert_eq!(view.y(), &[0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(view.u(), &[8, 9]);
        assert_eq!(view.v(), &[10, 11]);
    }

    #[test]
    fn encoder_config_maps_camera_config() {
        let c = CameraConfig::new(1280, 720, 2_000_000);
        let _ = SoftwareCameraSource::<MockProducer>::build_encoder_config(&c);
    }

    /// NAL types (without start codes) of an Annex-B stream.
    fn collect_nalu_types(annexb: &[u8]) -> Vec<u8> {
        let mut types = Vec::new();
        let mut i = 0;
        while i + 4 <= annexb.len() {
            if &annexb[i..i + 4] == b"\x00\x00\x00\x01" {
                if i + 4 < annexb.len() {
                    types.push(annexb[i + 4] & 0x1f);
                }
                i += 4;
            } else {
                i += 1;
            }
        }
        types
    }
}
