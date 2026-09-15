//! V4L2 MMAP video capture from `/dev/video0`.
//!
//! Implements [`FrameProducer`] using raw V4L2 ioctls with MMAP buffer
//! streaming.  When the process is launched with
//! `LD_PRELOAD=.../libcamera/v4l2-compat.so`, the V4L2 calls are intercepted
//! by libcamera which manages the full sensor → CSI → ISP → YUV420 pipeline.
//!
//! Without the compat layer (on boards whose sensor outputs YUV directly)
//! the same code works as a plain V4L2 MMAP capture.

use std::ffi::CString;
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr;
use std::sync::Arc;

use crate::camera::source::CameraError;
use crate::camera::source::FrameProducer;

// ─────────────────────────────────────────────────────────────────────────
// V4L2 constants
// ─────────────────────────────────────────────────────────────────────────

const V4L2_BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const V4L2_MEMORY_MMAP: u32 = 1;
const V4L2_FIELD_NONE: u32 = 1;

/// V4L2 fourcc for YUV420 planar ("YU12").
const YU12_FOURCC: u32 = u32::from_le_bytes(*b"YU12");

const NUM_BUFFERS: usize = 4;

// ioctl direction flags.
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;

/// Compute a V4L2 ioctl request code at compile time.
const fn ioc(dir: u32, typ: u32, nr: u32, size: u32) -> u32 {
    (dir << 30) | (size << 16) | (typ << 8) | nr
}

const TYPE_V: u32 = b'V' as u32;

// ─────────────────────────────────────────────────────────────────────────
// V4L2 structures (repr(C), matching kernel layout on aarch64 Linux)
// ─────────────────────────────────────────────────────────────────────────

#[repr(C)]
#[derive(Default)]
struct V4l2PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    priv_: u32,
    flags: u32,
    _enc: u32,
    _quantization: u32,
    _xfer_func: u32,
}

// v4l2_format — uses a raw byte array for the union so we don't need
// to define every possible format variant.
#[repr(C)]
struct V4l2Format {
    type_: u32,
    // The kernel's union contains pointers (v4l2_window), giving it
    // 8-byte alignment. repr(C) inserts 4 bytes of padding here, matching
    // the kernel layout. Using [u64; 25] (200 bytes, align 8) forces this.
    fmt: [u64; 25], // 200 bytes = 25 × 8
}

impl V4l2Format {
    fn new_capture() -> Self {
        let mut fmt = Self {
            type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
            fmt: [0u64; 25],
        };
        let pix = V4l2PixFormat {
            width: 0,
            height: 0,
            pixelformat: 0,
            field: V4L2_FIELD_NONE,
            bytesperline: 0,
            sizeimage: 0,
            colorspace: 0,
            priv_: 0,
            flags: 0,
            _enc: 0,
            _quantization: 0,
            _xfer_func: 0,
        };
        let pix_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                &pix as *const V4l2PixFormat as *const u8,
                size_of::<V4l2PixFormat>(),
            )
        };
        let fmt_bytes: &mut [u8] = unsafe {
            std::slice::from_raw_parts_mut(fmt.fmt.as_mut_ptr() as *mut u8, fmt.fmt.len() * 8)
        };
        fmt_bytes[..pix_bytes.len()].copy_from_slice(pix_bytes);
        fmt
    }

    fn pix_mut(&mut self) -> &mut V4l2PixFormat {
        unsafe { &mut *(self.fmt.as_mut_ptr() as *mut V4l2PixFormat) }
    }
}

#[repr(C)]
#[derive(Default)]
struct V4l2RequestBuffers {
    count: u32,
    type_: u32,
    memory: u32,
    capabilities: u32,
    // On this kernel, flags is u8 and reserved is [u8; 3], total = 20 bytes.
    flags: u8,
    reserved: [u8; 3],
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct V4l2Timecode {
    type_: u32,
    flags: u32,
    frames: u8,
    seconds: u8,
    minutes: u8,
    hours: u8,
    userbits: [u8; 4],
}

/// V4L2 buffer for MMAP streaming.
///
/// We use a byte array for the `m` union (offset/userptr/planes/fd) to avoid
/// unsafe Rust unions.  On aarch64 the union is 8 bytes with 8-byte alignment.
#[repr(C)]
struct V4l2Buffer {
    index: u32,
    type_: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    timestamp: libc::timeval,
    timecode: V4l2Timecode,
    sequence: u32,
    /// Memory type (V4L2_MEMORY_MMAP = 1).
    memory: u32,
    /// Union of offset / userptr / planes / fd — 8 bytes on aarch64.
    m_offset: u32,
    _m_padding: u32,
    length: u32,
    reserved2: u32,
    request_fd: i32,
}

impl Default for V4l2Buffer {
    fn default() -> Self {
        Self {
            index: 0,
            type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
            bytesused: 0,
            flags: 0,
            field: V4L2_FIELD_NONE,
            timestamp: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            timecode: V4l2Timecode::default(),
            sequence: 0,
            memory: V4L2_MEMORY_MMAP,
            m_offset: 0,
            _m_padding: 0,
            length: 0,
            reserved2: 0,
            request_fd: 0,
        }
    }
}

// ioctl numbers (computed at compile time).
const VIDIOC_S_FMT: u32 = ioc(
    IOC_READ | IOC_WRITE,
    TYPE_V,
    5,
    size_of::<V4l2Format>() as u32,
);
const VIDIOC_REQBUFS: u32 = ioc(
    IOC_READ | IOC_WRITE,
    TYPE_V,
    8,
    size_of::<V4l2RequestBuffers>() as u32,
);
const VIDIOC_QUERYBUF: u32 = ioc(
    IOC_READ | IOC_WRITE,
    TYPE_V,
    9,
    size_of::<V4l2Buffer>() as u32,
);
const VIDIOC_QBUF: u32 = ioc(
    IOC_READ | IOC_WRITE,
    TYPE_V,
    15,
    size_of::<V4l2Buffer>() as u32,
);
const VIDIOC_DQBUF: u32 = ioc(
    IOC_READ | IOC_WRITE,
    TYPE_V,
    17,
    size_of::<V4l2Buffer>() as u32,
);
const VIDIOC_STREAMON: u32 = ioc(IOC_WRITE, TYPE_V, 18, size_of::<i32>() as u32);
const VIDIOC_STREAMOFF: u32 = ioc(IOC_WRITE, TYPE_V, 19, size_of::<i32>() as u32);

// ─────────────────────────────────────────────────────────────────────────
// ioctl helper
// ─────────────────────────────────────────────────────────────────────────

/// Perform an ioctl, returning `io::Error` on failure.
unsafe fn ioctl<T>(fd: RawFd, req: u32, arg: *mut T) -> std::io::Result<()> {
    let ret = libc::ioctl(fd, req as _, arg as *mut libc::c_void);
    if ret < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────
// YUV420 planar flipping (device-level permanent flip)
// ─────────────────────────────────────────────────────────────────────────

/// Flip a YU12 (`YYYY… UU… VV…`) frame in place.
///
/// Applied on the post-ISP YUV planes the producer hands to the encoder, so
/// the flip is baked into every downstream consumer (RTSP, ONVIF, GB28181,
/// recordings, snapshots, AI) and persists across restarts. Malformed
/// (too short) buffers are left untouched.
pub fn flip_yu12_in_place(buf: &mut [u8], width: usize, height: usize, hflip: bool, vflip: bool) {
    if !hflip && !vflip {
        return;
    }
    let y_size = width * height;
    let c_w = width / 2;
    let c_h = height / 2;
    if width == 0 || height == 0 || buf.len() < y_size + 2 * c_w * c_h {
        return;
    }
    let planes = [
        (0usize, width, height),
        (y_size, c_w, c_h),
        (y_size + c_w * c_h, c_w, c_h),
    ];
    let mut scratch = vec![0u8; planes.iter().map(|&(_, w, _)| w).max().unwrap_or(0)];
    for &(off, w, h) in planes.iter() {
        flip_plane_in_place(
            &mut buf[off..off + w * h],
            w,
            h,
            hflip,
            vflip,
            &mut scratch[..w],
        );
    }
}

/// Flip one plane in place; `scratch` is one row wide and reused per row swap.
fn flip_plane_in_place(
    plane: &mut [u8],
    w: usize,
    h: usize,
    hflip: bool,
    vflip: bool,
    scratch: &mut [u8],
) {
    debug_assert_eq!(plane.len(), w * h);
    debug_assert_eq!(scratch.len(), w);
    if vflip {
        let mut top = 0usize;
        let mut bottom = (h - 1) * w;
        while top < bottom {
            scratch.copy_from_slice(&plane[top..top + w]);
            // Rows don't overlap, so split once per iteration to satisfy
            // the borrow checker.
            let (head, tail) = plane.split_at_mut(bottom);
            let (top_row, bottom_row) = (&mut head[top..top + w], &mut tail[..w]);
            copy_row(bottom_row, top_row, hflip);
            copy_row(scratch, bottom_row, hflip);
            top += w;
            bottom -= w;
        }
        // Odd height: the middle row only needs internal mirroring.
        if top == bottom && hflip {
            plane[top..top + w].reverse();
        }
    } else if hflip {
        for row in plane.chunks_mut(w) {
            row.reverse();
        }
    }
}

/// Copy `src` into `dst`, optionally mirroring byte order.
fn copy_row(src: &[u8], dst: &mut [u8], mirror: bool) {
    debug_assert_eq!(src.len(), dst.len());
    if mirror {
        for (d, s) in dst.iter_mut().zip(src.iter().rev()) {
            *d = *s;
        }
    } else {
        dst.copy_from_slice(src);
    }
}

// ─────────────────────────────────────────────────────────────────────────
// V4l2CaptureProducer
// ─────────────────────────────────────────────────────────────────────────

/// Latest shared YUV frame slot: `(width, height, YUV420 bytes)`.
pub type LatestYuv = Arc<std::sync::Mutex<Option<(u32, u32, Vec<u8>)>>>;

/// A [`FrameProducer`] that captures YUV420 frames via V4L2 MMAP streaming.
///
/// Designed to run under `LD_PRELOAD=...v4l2-compat.so` where libcamera
/// transparently manages the camera pipeline.
pub struct V4l2CaptureProducer {
    /// Lazily opened on the encoder thread (compat layer is thread-local).
    inner: Option<Inner>,
    device_path: String,
    width: u32,
    height: u32,
    fps: u32,
    /// Latest YUV420 frame shared with the web server for snapshots.
    pub latest_yuv: LatestYuv,
    /// Interval for sharing YUV frames with AI/other consumers (in frames).
    yuv_share_interval: u32,
    frame_counter: u64,
    /// Device-level flips applied to every captured frame before it is
    /// shared with the encoder / snapshots / AI. Atomic so the GB
    /// FrameMirror control (A.2.3.2.9) can change them at runtime.
    flips: Arc<Flips>,
    /// Video watermark (SPEC §5.2) burned into every frame after the flips —
    /// same "baked into everything downstream" semantics.
    watermark: Option<crate::watermark::Watermark>,
}

struct Inner {
    fd: OwnedFd,
    buffers: Vec<(usize, u32)>, // (mmap ptr, length)
    streaming: bool,
}

/// Device-level flip flags shared between the capture thread (reader,
/// per frame) and runtime control writers (GB FrameMirror).
#[derive(Debug, Default)]
pub struct Flips {
    hflip: std::sync::atomic::AtomicBool,
    vflip: std::sync::atomic::AtomicBool,
}

impl Flips {
    /// Update both flags (relaxed — per-frame reads tolerate tearing on
    /// the exact transition frame).
    pub fn set(&self, hflip: bool, vflip: bool) {
        self.hflip
            .store(hflip, std::sync::atomic::Ordering::Relaxed);
        self.vflip
            .store(vflip, std::sync::atomic::Ordering::Relaxed);
    }

    /// Read both flags.
    pub fn load(&self) -> (bool, bool) {
        (
            self.hflip.load(std::sync::atomic::Ordering::Relaxed),
            self.vflip.load(std::sync::atomic::Ordering::Relaxed),
        )
    }
}

impl V4l2CaptureProducer {
    /// Create a new producer that will lazily open the device on first use.
    /// This ensures all V4L2 operations happen on the encoder thread, which
    /// is required because the libcamera v4l2-compat layer is thread-local.
    pub fn new(device_path: &str, width: u32, height: u32, fps: u32) -> Self {
        Self {
            inner: None,
            device_path: device_path.to_string(),
            width,
            height,
            fps,
            latest_yuv: Arc::new(std::sync::Mutex::new(None)),
            yuv_share_interval: 15,
            frame_counter: 0,
            flips: Arc::new(Flips::default()),
            watermark: None,
        }
    }

    /// Enable device-level flips (applied to every frame from here on).
    ///
    /// Must be called before the first [`FrameProducer::next_yuv_frame`] —
    /// the flags are read on the capture thread for each dequeued buffer.
    pub fn set_flips(&mut self, hflip: bool, vflip: bool) {
        self.flips.set(hflip, vflip);
    }

    /// The shared flip flags — a live handle for runtime changes (GB
    /// FrameMirror control wiring); the capture thread reads them every
    /// frame.
    pub fn flips_arc(&self) -> Arc<Flips> {
        Arc::clone(&self.flips)
    }

    /// Attach a watermark renderer (applied to every frame from here on,
    /// after the flips, before the frame is shared with the encoder /
    /// snapshots / AI). Must be called before the first
    /// [`FrameProducer::next_yuv_frame`].
    pub fn set_watermark(&mut self, watermark: crate::watermark::Watermark) {
        self.watermark = Some(watermark);
    }

    /// Open the device (called lazily from next_yuv_frame on the encoder thread).
    /// Set the interval for sharing YUV frames with AI/other consumers.
    ///
    /// # Arguments
    ///
    /// * `n` - The interval in frames (e.g., 3 = every 3rd frame).
    pub fn set_yuv_share_interval(&mut self, n: u32) {
        self.yuv_share_interval = n;
    }

    fn ensure_opened(&mut self) -> Result<(), CameraError> {
        if self.inner.is_some() {
            return Ok(());
        }

        let cstr = CString::new(self.device_path.as_str())
            .map_err(|e| CameraError::Config(e.to_string()))?;
        let fd_raw = unsafe { libc::open(cstr.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK) };
        if fd_raw < 0 {
            return Err(CameraError::DeviceNotFound(self.device_path.clone()));
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd_raw) };

        // Set format (YUV420)
        let mut fmt = V4l2Format::new_capture();
        {
            let pix = fmt.pix_mut();
            pix.width = self.width;
            pix.height = self.height;
            pix.pixelformat = YU12_FOURCC;
            pix.field = V4L2_FIELD_NONE;
        }
        unsafe { ioctl(fd.as_raw_fd(), VIDIOC_S_FMT, &mut fmt) }.map_err(CameraError::Io)?;
        self.width = fmt.pix_mut().width;
        self.height = fmt.pix_mut().height;

        // Request MMAP buffers
        let mut req = V4l2RequestBuffers {
            count: NUM_BUFFERS as u32,
            type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
            memory: V4L2_MEMORY_MMAP,
            ..Default::default()
        };
        unsafe { ioctl(fd.as_raw_fd(), VIDIOC_REQBUFS, &mut req) }.map_err(CameraError::Io)?;
        let count = req.count as usize;
        if count < 2 {
            return Err(CameraError::Config(format!(
                "device allocated only {count} buffers (need ≥ 2)"
            )));
        }

        // Query + mmap each buffer
        let mut buffers = Vec::with_capacity(count);
        let frame_size = (self.width as usize) * (self.height as usize) * 3 / 2;
        for i in 0..count {
            let mut buf = V4l2Buffer {
                index: i as u32,
                type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
                memory: V4L2_MEMORY_MMAP,
                ..Default::default()
            };
            unsafe { ioctl(fd.as_raw_fd(), VIDIOC_QUERYBUF, &mut buf) }.map_err(CameraError::Io)?;

            let length = if buf.length > 0 {
                buf.length
            } else {
                frame_size as u32
            };
            let ptr = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    length as usize,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    fd.as_raw_fd(),
                    buf.m_offset as libc::off_t,
                )
            };
            if ptr == libc::MAP_FAILED {
                return Err(CameraError::Io(std::io::Error::last_os_error()));
            }
            buffers.push((ptr as usize, length));
        }

        self.inner = Some(Inner {
            fd,
            buffers,
            streaming: false,
        });
        Ok(())
    }

    /// Start streaming and queue all buffers.
    fn start_streaming(&mut self) -> Result<(), CameraError> {
        let inner = self
            .inner
            .as_mut()
            .ok_or_else(|| CameraError::Config("not opened".to_string()))?;
        if inner.streaming {
            return Ok(());
        }

        for i in 0..inner.buffers.len() {
            let mut buf = V4l2Buffer {
                index: i as u32,
                type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
                memory: V4L2_MEMORY_MMAP,
                ..Default::default()
            };
            unsafe { ioctl(inner.fd.as_raw_fd(), VIDIOC_QBUF, &mut buf) }
                .map_err(CameraError::Io)?;
        }

        let mut buf_type: i32 = V4L2_BUF_TYPE_VIDEO_CAPTURE as i32;
        unsafe { ioctl(inner.fd.as_raw_fd(), VIDIOC_STREAMON, &mut buf_type) }
            .map_err(CameraError::Io)?;

        inner.streaming = true;
        Ok(())
    }
}
impl Drop for V4l2CaptureProducer {
    fn drop(&mut self) {
        if let Some(inner) = &mut self.inner {
            if inner.streaming {
                let mut buf_type: i32 = V4L2_BUF_TYPE_VIDEO_CAPTURE as i32;
                unsafe {
                    let _ = ioctl(inner.fd.as_raw_fd(), VIDIOC_STREAMOFF, &mut buf_type);
                }
            }
            for (ptr, len) in &inner.buffers {
                if *ptr != 0 {
                    unsafe { libc::munmap(*ptr as *mut libc::c_void, *len as usize) };
                }
            }
        }
    }
}

impl FrameProducer for V4l2CaptureProducer {
    fn next_yuv_frame(&mut self) -> Result<Vec<u8>, CameraError> {
        self.ensure_opened()?;
        if !self.inner.as_ref().unwrap().streaming {
            self.start_streaming()?;
        }

        loop {
            // Dequeue a filled buffer.
            let mut buf = V4l2Buffer {
                type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
                memory: V4L2_MEMORY_MMAP,
                ..Default::default()
            };

            // Use poll() to wait for a frame (handles O_NONBLOCK fd).
            let inner = self.inner.as_ref().unwrap();
            let mut pfd = libc::pollfd {
                fd: inner.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ret = unsafe { libc::poll(&mut pfd, 1, 5000) };
            if ret < 0 {
                return Err(CameraError::Io(std::io::Error::last_os_error()));
            }
            if ret == 0 {
                return Err(CameraError::Disconnected(
                    "capture poll timeout".to_string(),
                ));
            }
            let result = unsafe { ioctl(inner.fd.as_raw_fd(), VIDIOC_DQBUF, &mut buf) };
            match result {
                Ok(()) => {}
                Err(ref e) if e.raw_os_error() == Some(libc::EAGAIN) => continue,
                Err(e) => return Err(CameraError::Io(e)),
            }
            let idx = buf.index as usize;
            if idx >= inner.buffers.len() {
                return Err(CameraError::Disconnected(format!(
                    "buffer index {idx} out of range"
                )));
            }
            let (ptr, len) = inner.buffers[idx];
            let bytes_used = if buf.bytesused > 0 && buf.bytesused <= len {
                buf.bytesused as usize
            } else {
                len as usize
            };
            let mut data =
                unsafe { std::slice::from_raw_parts(ptr as *const u8, bytes_used) }.to_vec();

            // Device-level flip: baked into everything downstream of the
            // producer — encoder (RTSP/ONVIF/GB28181/recordings), web
            // snapshots (latest_yuv) and AI inference alike.
            let (hflip, vflip) = self.flips.load();
            if hflip || vflip {
                flip_yu12_in_place(
                    &mut data,
                    self.width as usize,
                    self.height as usize,
                    hflip,
                    vflip,
                );
            }

            // Watermark (SPEC §5.2): burned in after the flips, before the
            // frame is shared — every consumer sees the same burn.
            if let Some(watermark) = &mut self.watermark {
                watermark.render_into(&mut data, self.width as usize, self.height as usize);
            }

            // Requeue the buffer.
            let mut qbuf = V4l2Buffer {
                index: buf.index,
                type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
                memory: V4L2_MEMORY_MMAP,
                ..Default::default()
            };
            unsafe {
                ioctl(
                    self.inner.as_ref().unwrap().fd.as_raw_fd(),
                    VIDIOC_QBUF,
                    &mut qbuf,
                )
            }
            .map_err(CameraError::Io)?;

            // Share every 15th frame for web snapshots.
            self.frame_counter += 1;
            ::metrics::counter!("mibee_frames_captured_total").increment(1);
            if self
                .frame_counter
                .is_multiple_of(self.yuv_share_interval as u64)
            {
                if let Ok(mut guard) = self.latest_yuv.lock() {
                    *guard = Some((self.width, self.height, data.clone()));
                }
            }
            return Ok(data);
        }
    }

    fn resolution(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn fps(&self) -> u32 {
        self.fps
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Tests — YU12 flip correctness
// ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod flip_tests {
    use super::flip_yu12_in_place;

    /// Build a 4×4 YU12 frame with distinct values per plane:
    /// Y = 0..16, U = 20..24 (2×2), V = 30..34 (2×2).
    fn frame() -> Vec<u8> {
        let mut buf = Vec::with_capacity(24);
        buf.extend(0..16);
        buf.extend(20..24);
        buf.extend(30..34);
        buf
    }

    #[test]
    fn no_flip_is_noop() {
        let mut buf = frame();
        let before = buf.clone();
        flip_yu12_in_place(&mut buf, 4, 4, false, false);
        assert_eq!(buf, before);
    }

    #[test]
    fn vflip_reverses_row_order_per_plane() {
        let mut buf = frame();
        flip_yu12_in_place(&mut buf, 4, 4, false, true);
        // Y rows [0,1,2,3] reversed → [12..15, 8..11, 4..7, 0..3]
        assert_eq!(
            &buf[0..16],
            &(12..16)
                .chain(8..12)
                .chain(4..8)
                .chain(0..4)
                .collect::<Vec<_>>()
        );
        // U 2×2 rows [20,21],[22,23] → [22,23,20,21]
        assert_eq!(&buf[16..20], &[22, 23, 20, 21]);
        // V likewise
        assert_eq!(&buf[20..24], &[32, 33, 30, 31]);
    }

    #[test]
    fn hflip_mirrors_each_row_per_plane() {
        let mut buf = frame();
        flip_yu12_in_place(&mut buf, 4, 4, true, false);
        // Every 4-byte Y row and every 2-byte chroma row is reversed.
        for r in 0..4 {
            let row = &buf[r * 4..r * 4 + 4];
            assert_eq!(
                row,
                &[
                    (r * 4 + 3) as u8,
                    (r * 4 + 2) as u8,
                    (r * 4 + 1) as u8,
                    (r * 4) as u8
                ]
            );
        }
        assert_eq!(&buf[16..20], &[21, 20, 23, 22]);
        assert_eq!(&buf[20..24], &[31, 30, 33, 32]);
    }

    #[test]
    fn both_flips_is_180_rotation() {
        let mut buf = frame();
        flip_yu12_in_place(&mut buf, 4, 4, true, true);
        // Y plane fully reversed (180°).
        assert_eq!(&buf[0..16], &(0..16).rev().collect::<Vec<_>>());
        // Chroma planes fully reversed too.
        assert_eq!(&buf[16..20], &[23, 22, 21, 20]);
        assert_eq!(&buf[20..24], &[33, 32, 31, 30]);
    }

    #[test]
    fn odd_height_middle_row_is_mirrored_only() {
        // 4×3 frame: Y = 12 bytes, chroma 2×2 (height 3 → c_h = 1, no swap).
        let mut buf: Vec<u8> = (0..12).chain(20..24).chain(30..34).collect();
        flip_yu12_in_place(&mut buf, 4, 3, true, true);
        // Y: top↔bottom rows swapped+mirrored, middle row mirrored.
        assert_eq!(&buf[0..4], &[11, 10, 9, 8]);
        assert_eq!(&buf[4..8], &[7, 6, 5, 4]);
        assert_eq!(&buf[8..12], &[3, 2, 1, 0]);
        // Single chroma row: only mirrored.
        assert_eq!(&buf[12..16], &[21, 20, 23, 22]);
    }

    #[test]
    fn too_short_buffer_left_untouched() {
        let mut buf = vec![7u8; 10];
        flip_yu12_in_place(&mut buf, 4, 4, true, true);
        assert!(buf.iter().all(|&b| b == 7));
    }
}
