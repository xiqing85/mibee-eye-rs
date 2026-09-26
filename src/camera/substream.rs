//! Substream pipeline pieces (SPEC appendix A #20): tap the main capture
//! frames and re-encode a downscaled copy through a second encoder
//! session.
//!
//! ```text
//!   V4l2CaptureProducer ──► TappingFrameProducer ──► main encoder thread
//!                              │ clone (bounded, drop-on-full)
//!                              ▼
//!                        SubFrameProducer ──► second encoder session
//!                        (downscale + fps       (V4l2 M2M or openh264)
//!                         decimation)
//! ```
//!
//! The tap is placed *after* the producer's transforms (rotation / flips
//! / watermark bake in inside the producer), so the substream inherits
//! them without any extra work.

use std::sync::mpsc::{Receiver, SyncSender, TrySendError};

use super::downscale::downscale_yu420;
use super::source::{CameraError, FrameProducer};

/// Frame tap budget: a full frame clone per slot (~1.4 MiB at 720p).
/// Two slots absorb encoder jitter without letting a stalled sub encoder
/// pin main-capture memory; a full tap drops frames (the sub stream
/// resynchronises on its next IDR).
pub const TAP_CAPACITY: usize = 2;

/// [`FrameProducer`] decorator that forwards frames to the main encoder
/// and, when a tap is installed, also clones each frame into a bounded
/// channel for the substream pipeline. Tap send failures (channel full
/// or receiver gone) are silently ignored — the main stream must never
/// stall because of the substream.
pub struct TappingFrameProducer<P: FrameProducer> {
    inner: P,
    tap: Option<SyncSender<Vec<u8>>>,
}

impl<P: FrameProducer> TappingFrameProducer<P> {
    /// Wrap `producer`, optionally installing a substream tap.
    #[must_use]
    pub fn new(producer: P, tap: Option<SyncSender<Vec<u8>>>) -> Self {
        Self {
            inner: producer,
            tap,
        }
    }
}

impl<P: FrameProducer> FrameProducer for TappingFrameProducer<P> {
    fn next_yuv_frame(&mut self) -> Result<Vec<u8>, CameraError> {
        let frame = self.inner.next_yuv_frame()?;
        if let Some(tx) = &self.tap {
            match tx.try_send(frame.clone()) {
                Ok(()) | Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {}
            }
        }
        Ok(frame)
    }

    fn resolution(&self) -> (u32, u32) {
        self.inner.resolution()
    }

    fn fps(&self) -> u32 {
        self.inner.fps()
    }
}

/// [`FrameProducer`] fed by a [`TappingFrameProducer`]: receives full
/// main-resolution frames and emits downscaled ones at the configured
/// substream rate.
///
/// The receiver is wrapped in a `Mutex` so the producer is `Sync`
/// (required by [`super::software::SoftwareCameraSource`]); blocking
/// `recv` under the lock is fine — there is exactly one consumer.
pub struct SubFrameProducer {
    rx: std::sync::Mutex<Receiver<Vec<u8>>>,
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
    /// Emit every `fps_div`-th tapped frame (1 = keep up with the main
    /// rate).
    fps_div: u32,
    /// Effective sub rate (`main_fps / fps_div`) — metadata only; the
    /// producer is paced by the tap, not by a clock.
    fps: u32,
    counter: u32,
}

impl SubFrameProducer {
    /// Build from a tap receiver. `main_fps`/`sub_fps` drive the
    /// decimation ratio (`fps_div = max(1, main_fps / sub_fps)`).
    #[must_use]
    pub fn new(
        rx: Receiver<Vec<u8>>,
        src_w: u32,
        src_h: u32,
        dst_w: u32,
        dst_h: u32,
        main_fps: u32,
        sub_fps: u32,
    ) -> Self {
        let fps_div = if sub_fps == 0 || main_fps == 0 {
            1
        } else {
            (main_fps / sub_fps).max(1)
        };
        Self {
            rx: std::sync::Mutex::new(rx),
            src_w,
            src_h,
            dst_w,
            dst_h,
            fps_div,
            fps: if main_fps == 0 { 0 } else { main_fps / fps_div },
            counter: 0,
        }
    }
}

impl FrameProducer for SubFrameProducer {
    fn next_yuv_frame(&mut self) -> Result<Vec<u8>, CameraError> {
        let rx = self.rx.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let frame = rx.recv().map_err(|_| {
                CameraError::Disconnected(
                    "substream tap closed (main pipeline stopped)".to_string(),
                )
            })?;
            self.counter = self.counter.wrapping_add(1);
            if self.fps_div > 1 && !self.counter.is_multiple_of(self.fps_div) {
                continue;
            }
            return Ok(downscale_yu420(
                &frame, self.src_w, self.src_h, self.dst_w, self.dst_h,
            ));
        }
    }

    fn resolution(&self) -> (u32, u32) {
        (self.dst_w, self.dst_h)
    }

    fn fps(&self) -> u32 {
        self.fps
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

    struct ListProducer {
        frames: Vec<Vec<u8>>,
        i: usize,
    }
    impl FrameProducer for ListProducer {
        fn next_yuv_frame(&mut self) -> Result<Vec<u8>, CameraError> {
            match self.frames.get(self.i) {
                Some(f) => {
                    self.i += 1;
                    Ok(f.clone())
                }
                None => Err(CameraError::Disconnected("depleted".to_string())),
            }
        }
        fn resolution(&self) -> (u32, u32) {
            (8, 2)
        }
        fn fps(&self) -> u32 {
            10
        }
    }

    fn frame_8x2(tag: u8) -> Vec<u8> {
        let mut v = vec![tag; 16];
        v.extend(vec![tag; 4]);
        v.extend(vec![tag; 4]);
        v
    }

    #[test]
    fn tapping_producer_forwards_and_taps() {
        let (tx, rx) = sync_channel::<Vec<u8>>(TAP_CAPACITY);
        let mut p = TappingFrameProducer::new(
            ListProducer {
                frames: vec![frame_8x2(1), frame_8x2(2)],
                i: 0,
            },
            Some(tx),
        );
        assert_eq!(p.next_yuv_frame().unwrap(), frame_8x2(1));
        assert_eq!(p.next_yuv_frame().unwrap(), frame_8x2(2));
        assert_eq!(rx.recv().unwrap(), frame_8x2(1));
        assert_eq!(rx.recv().unwrap(), frame_8x2(2));
        assert_eq!(p.resolution(), (8, 2));
        assert_eq!(p.fps(), 10);
    }

    #[test]
    fn tapping_producer_without_tap_is_passthrough() {
        let mut p = TappingFrameProducer::new(
            ListProducer {
                frames: vec![frame_8x2(9)],
                i: 0,
            },
            None,
        );
        assert_eq!(p.next_yuv_frame().unwrap(), frame_8x2(9));
    }

    #[test]
    fn tap_drop_on_full_never_breaks_main() {
        let (tx, _rx) = sync_channel::<Vec<u8>>(1);
        let _ = tx.send(frame_8x2(0)); // fill the single slot
        let mut p = TappingFrameProducer::new(
            ListProducer {
                frames: vec![frame_8x2(1), frame_8x2(2)],
                i: 0,
            },
            Some(tx),
        );
        // Both frames still flow to the main consumer despite the full tap.
        assert_eq!(p.next_yuv_frame().unwrap(), frame_8x2(1));
        assert_eq!(p.next_yuv_frame().unwrap(), frame_8x2(2));
    }

    #[test]
    fn sub_producer_downscales_and_decimates() {
        let (tx, rx) = sync_channel::<Vec<u8>>(8);
        for tag in 1..=4u8 {
            tx.send(frame_8x2(tag)).unwrap();
        }
        drop(tx);
        // main_fps 10, sub_fps 5 → fps_div 2 → keep tags 2 and 4.
        let mut sub = SubFrameProducer::new(rx, 8, 2, 4, 2, 10, 5);
        let f1 = sub.next_yuv_frame().unwrap();
        assert_eq!(f1.len(), 4 * 2 * 3 / 2);
        assert_eq!(f1[0], 2, "first emitted frame is tag 2");
        let f2 = sub.next_yuv_frame().unwrap();
        assert_eq!(f2[0], 4);
        // Channel drained + sender dropped → Disconnected.
        let err = sub.next_yuv_frame().unwrap_err();
        assert!(matches!(err, CameraError::Disconnected(_)), "{err}");
    }

    #[test]
    fn sub_producer_full_rate_when_sub_fps_unconfigured() {
        let (tx, rx) = sync_channel::<Vec<u8>>(4);
        tx.send(frame_8x2(7)).unwrap();
        drop(tx);
        let mut sub = SubFrameProducer::new(rx, 8, 2, 4, 2, 15, 0);
        assert_eq!(
            sub.next_yuv_frame().unwrap()[0],
            7,
            "fps=0 keeps every frame"
        );
    }

    #[test]
    fn sub_producer_metadata_reflects_sub_geometry() {
        let (_tx, rx) = sync_channel::<Vec<u8>>(1);
        let sub = SubFrameProducer::new(rx, 8, 2, 4, 2, 10, 5);
        assert_eq!(sub.resolution(), (4, 2));
    }
}
