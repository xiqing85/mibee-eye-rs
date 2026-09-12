//! 画像フォーマット変換器。
//!
//! V4L2 M2M デバイス (`/dev/video12`) を使用して、
//! 拡大縮小や I420 と NV12 の相互変換を行う。

use std::collections::VecDeque;
use std::os::fd::RawFd;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::buffer::BufferSet;
use crate::device::Device;
use crate::format::{Memory, PixelFormat, Resolution};
use crate::poller::{PollEvent, Poller, PollerConfig, Timestamp};
use crate::queue::{CaptureQueue, OutputQueue};
use crate::sys;

/// 画像変換器の設定。
pub struct ConverterConfig {
    /// デバイスパス。デフォルト: "/dev/video12"。
    pub device_path: String,
    /// 入力映像の幅。
    pub input_width: u32,
    /// 入力映像の高さ。
    pub input_height: u32,
    /// 入力メモリ方式。
    pub input_memory: Memory,
    /// 入力ピクセルフォーマット。
    pub input_pixel_format: PixelFormat,
    /// 出力映像の幅。
    pub output_width: u32,
    /// 出力映像の高さ。
    pub output_height: u32,
    /// 出力メモリ方式。
    pub output_memory: Memory,
    /// 出力ピクセルフォーマット。
    pub output_pixel_format: PixelFormat,
    /// OUTPUT/CAPTURE バッファ数。
    pub buffer_count: u32,
}

impl ConverterConfig {
    /// デフォルト設定を作成する。
    pub fn new(input_width: u32, input_height: u32, output_width: u32, output_height: u32) -> Self {
        ConverterConfig {
            device_path: "/dev/video12".to_string(),
            input_width,
            input_height,
            input_memory: Memory::Mmap,
            input_pixel_format: PixelFormat::Yuv420,
            output_width,
            output_height,
            output_memory: Memory::Mmap,
            output_pixel_format: PixelFormat::Nv12,
            buffer_count: 4,
        }
    }
}

type ConvertMmapFill<'a, T> = dyn FnMut(&mut [u8], &Resolution, &T) -> Option<usize> + 'a;

/// 変換入力。
pub enum ConvertInput<'a, T> {
    /// mmap 入力バッファを直接初期化するクロージャ。
    ///
    /// `None` を返した場合は `Error::MmapInputNotProduced` を返す。
    Mmap(&'a mut ConvertMmapFill<'a, T>),
    /// DMABUF 入力バッファ。
    DmaBuf {
        fd: RawFd,
        bytesused: u32,
        length: u32,
    },
}

struct RequeueToken {
    capture_queue: Arc<CaptureQueue>,
    index: u32,
    pending_async_errors: Arc<Mutex<VecDeque<crate::error::Error>>>,
}

impl RequeueToken {
    fn requeue(self) {
        if let Err(err) = self.capture_queue.enqueue(self.index)
            && let Ok(mut pending) = self.pending_async_errors.lock()
        {
            pending.push_back(err);
        }
    }
}

/// 変換されたフレーム。
///
/// このハンドルが `Drop` されると、内部 CAPTURE バッファが自動再キューされる。
pub struct ConvertedFrame {
    requeue: Option<RequeueToken>,
    index: u32,
    bytesused: u32,
    timestamp_us: i64,
}

impl ConvertedFrame {
    fn new(
        capture_queue: Arc<CaptureQueue>,
        pending_async_errors: Arc<Mutex<VecDeque<crate::error::Error>>>,
        index: u32,
        bytesused: u32,
        timestamp: Timestamp,
    ) -> crate::error::Result<Self> {
        let capacity = capture_queue.buffers().plane(index, 0).length as usize;
        let bytesused_usize = bytesused as usize;
        if bytesused_usize > capacity {
            return Err(crate::error::Error::InputTooLarge {
                size: bytesused_usize,
                capacity,
            });
        }

        Ok(Self {
            requeue: Some(RequeueToken {
                capture_queue,
                index,
                pending_async_errors,
            }),
            index,
            bytesused,
            timestamp_us: timestamp.tv_sec * 1_000_000 + timestamp.tv_usec,
        })
    }

    /// MMAP 出力時のデータ参照を返す。DMABUF 出力時は `None`。
    pub fn data(&self) -> Option<&[u8]> {
        let token = self.requeue.as_ref()?;
        let data = token.capture_queue.buffers().mmap_slice(self.index, 0)?;
        Some(&data[..self.bytesused as usize])
    }

    /// DMABUF 出力時の fd を返す。MMAP 出力時は `None`。
    pub fn dmabuf_fd(&self) -> Option<RawFd> {
        self.requeue
            .as_ref()
            .and_then(|token| token.capture_queue.buffers().dmabuf_fd(self.index, 0))
    }

    /// バッファインデックスを返す。
    pub fn index(&self) -> u32 {
        self.index
    }

    /// `bytesused` を返す。
    pub fn bytesused(&self) -> u32 {
        self.bytesused
    }

    /// バッファ長を返す。
    pub fn length(&self) -> u32 {
        self.requeue
            .as_ref()
            .map(|token| token.capture_queue.buffers().plane(self.index, 0).length)
            .unwrap_or(0)
    }

    /// タイムスタンプ (マイクロ秒) を返す。
    pub fn timestamp_us(&self) -> i64 {
        self.timestamp_us
    }
}

impl Drop for ConvertedFrame {
    fn drop(&mut self) {
        if let Some(token) = self.requeue.take() {
            token.requeue();
        }
    }
}

/// 変換器のコールバック出力。
pub enum ConvertCallbackOutput<T> {
    /// 変換されたフレーム。
    Frame { frame: ConvertedFrame, value: T },
}

type ConverterCallback<T> = dyn FnMut(crate::error::Result<ConvertCallbackOutput<T>>) + Send;

struct ConverterRuntime<T> {
    output_queue: OutputQueue,
    started: bool,
    pending_values: VecDeque<T>,
}

struct ConverterShared<T> {
    runtime: Mutex<ConverterRuntime<T>>,
    capture_queue: Arc<CaptureQueue>,
    input_resolution: Resolution,
    output_resolution: Resolution,
    input_memory: Memory,
    output_v4l2_memory: u32,
    pending_async_errors: Arc<Mutex<VecDeque<crate::error::Error>>>,
}

impl<T> ConverterShared<T> {
    fn drain_pending_async_errors(&self) -> Vec<crate::error::Error> {
        let mut errors = Vec::new();
        if let Ok(mut pending) = self.pending_async_errors.lock() {
            errors.extend(pending.drain(..));
        }
        errors
    }
}

/// 画像変換器。
///
/// フィールド宣言順序は Drop 順序に影響する。
/// `device` (fd) はキューやポーラーより後に Drop されなければならない。
pub struct ImageConverter<T> {
    poller: Option<Poller>,
    shared: Arc<ConverterShared<T>>,
    callback: Option<Box<ConverterCallback<T>>>,
    device: Device,
}

impl<T: Send + 'static> ImageConverter<T> {
    /// 変換器を初期化する。
    pub fn new<F>(config: ConverterConfig, callback: F) -> crate::error::Result<Self>
    where
        F: FnMut(crate::error::Result<ConvertCallbackOutput<T>>) + Send + 'static,
    {
        Self::validate_pixel_format(config.input_pixel_format, "input")?;
        Self::validate_pixel_format(config.output_pixel_format, "output")?;

        let device = Device::open(&config.device_path)?;
        let fd = device.raw_fd();

        let input_resolution = Self::set_output_format(
            fd,
            config.input_width,
            config.input_height,
            config.input_pixel_format,
            "input resolution",
        )?;
        let output_resolution = Self::set_capture_format(
            fd,
            config.output_width,
            config.output_height,
            config.output_pixel_format,
            "output resolution",
        )?;

        let output_v4l2_memory = config.input_memory.to_v4l2();
        let output_buffers = BufferSet::allocate(
            fd,
            sys::V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
            output_v4l2_memory,
            config.buffer_count,
            false,
        )?;
        let output_queue = OutputQueue::new(
            fd,
            sys::V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
            output_v4l2_memory,
            output_buffers,
        );

        // 出力が DMABUF の場合は MMAP バッファを export して FD を返す。
        let capture_v4l2_memory = sys::V4L2_MEMORY_MMAP;
        let export_dmabuf = matches!(config.output_memory, Memory::DmaBuf);
        let capture_buffers = BufferSet::allocate(
            fd,
            sys::V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
            capture_v4l2_memory,
            config.buffer_count,
            export_dmabuf,
        )?;
        let capture_queue = Arc::new(CaptureQueue::new(
            fd,
            sys::V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
            capture_v4l2_memory,
            capture_buffers,
        ));
        capture_queue.enqueue_all()?;

        let runtime = ConverterRuntime {
            output_queue,
            started: false,
            pending_values: VecDeque::new(),
        };

        let shared = Arc::new(ConverterShared {
            runtime: Mutex::new(runtime),
            capture_queue,
            input_resolution,
            output_resolution,
            input_memory: config.input_memory,
            output_v4l2_memory,
            pending_async_errors: Arc::new(Mutex::new(VecDeque::new())),
        });

        Ok(ImageConverter {
            poller: None,
            shared,
            callback: Some(Box::new(callback)),
            device,
        })
    }

    /// 1 フレームを変換キューに投入する。
    pub fn convert(
        &mut self,
        input: ConvertInput<'_, T>,
        timestamp_us: i64,
        value: T,
    ) -> crate::error::Result<()> {
        let fd = self.device.raw_fd();
        let input_memory = self.shared.input_memory;
        let input_resolution = self.shared.input_resolution;
        let mut needs_start = false;

        {
            let mut runtime = self.lock_runtime()?;

            let output_index = runtime
                .output_queue
                .dequeue_available()
                .ok_or(crate::error::Error::NoAvailableBuffer)?;

            let enqueue_result = match input {
                ConvertInput::Mmap(fill) => {
                    if input_memory != Memory::Mmap {
                        Err(crate::error::Error::InvalidFormat {
                            reason: "converter is configured for DMABUF input".to_string(),
                        })
                    } else {
                        let mut fill_with_resolution = |buf: &mut [u8]| -> Option<usize> {
                            fill(buf, &input_resolution, &value)
                        };
                        runtime.output_queue.enqueue(
                            output_index,
                            &mut fill_with_resolution,
                            timestamp_us,
                        )
                    }
                }
                ConvertInput::DmaBuf {
                    fd: dmabuf_fd,
                    bytesused,
                    length,
                } => {
                    if input_memory != Memory::DmaBuf {
                        Err(crate::error::Error::InvalidFormat {
                            reason: "converter is configured for MMAP input".to_string(),
                        })
                    } else {
                        runtime.output_queue.enqueue_dmabuf(
                            output_index,
                            dmabuf_fd,
                            bytesused,
                            length,
                            timestamp_us,
                        )
                    }
                }
            };

            if let Err(err) = enqueue_result {
                runtime.output_queue.return_buffer(output_index);
                return Err(err);
            }

            runtime.pending_values.push_back(value);

            if !runtime.started {
                sys::ioctl_streamon(fd, sys::V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE)?;
                sys::ioctl_streamon(fd, sys::V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE)?;
                runtime.started = true;
                needs_start = true;
            }
        }

        if needs_start {
            self.start_poller();
        }

        Ok(())
    }

    /// `S_FMT` 後に確定した入力解像度を返す。
    pub fn input_resolution(&self) -> Resolution {
        self.shared.input_resolution
    }

    /// `S_FMT` 後に確定した出力解像度を返す。
    pub fn output_resolution(&self) -> Resolution {
        self.shared.output_resolution
    }

    fn lock_runtime(&self) -> crate::error::Result<MutexGuard<'_, ConverterRuntime<T>>> {
        self.shared
            .runtime
            .lock()
            .map_err(|_| crate::error::Error::PollerAborted)
    }

    fn start_poller(&mut self) {
        if self.poller.is_some() {
            return;
        }

        let Some(mut callback) = self.callback.take() else {
            return;
        };

        let output_v4l2_memory = self.shared.output_v4l2_memory;

        let fd = self.device.raw_fd();
        let shared = self.shared.clone();

        // callback は poller スレッドのクロージャーが単独所有する。
        self.poller = Some(Poller::start(
            PollerConfig {
                fd,
                output_buf_type: sys::V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
                output_memory: output_v4l2_memory,
                capture_buf_type: sys::V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
                capture_memory: sys::V4L2_MEMORY_MMAP,
                subscribe_events: false,
            },
            move |event| Self::handle_event(&shared, callback.as_mut(), event),
        ));
    }

    fn handle_event(
        shared: &Arc<ConverterShared<T>>,
        callback: &mut ConverterCallback<T>,
        event: PollEvent,
    ) {
        for err in shared.drain_pending_async_errors() {
            callback(Err(err));
        }

        match event {
            PollEvent::OutputDequeued { index } => {
                if let Ok(mut runtime) = shared.runtime.lock() {
                    runtime.output_queue.return_buffer(index);
                } else {
                    callback(Err(crate::error::Error::PollerAborted));
                }
            }
            PollEvent::CaptureDequeued {
                index,
                bytesused,
                flags: _,
                timestamp,
            } => Self::handle_capture(shared, callback, index, bytesused, timestamp),
            PollEvent::SourceChanged => {
                // コンバーターでは発生しない。
            }
            PollEvent::Error(err) => callback(Err(err)),
        }
    }

    fn handle_capture(
        shared: &Arc<ConverterShared<T>>,
        callback: &mut ConverterCallback<T>,
        index: u32,
        bytesused: u32,
        timestamp: Timestamp,
    ) {
        let pending_value = match shared.runtime.lock() {
            Ok(mut runtime) => {
                let Some(value) = runtime.pending_values.pop_front() else {
                    drop(runtime);
                    callback(Err(crate::error::Error::NoAvailableBuffer));
                    if let Err(requeue_err) = shared.capture_queue.enqueue(index) {
                        callback(Err(requeue_err));
                    }
                    return;
                };
                value
            }
            Err(_) => {
                callback(Err(crate::error::Error::PollerAborted));
                return;
            }
        };

        let capture_queue = shared.capture_queue.clone();
        let frame = match ConvertedFrame::new(
            capture_queue.clone(),
            shared.pending_async_errors.clone(),
            index,
            bytesused,
            timestamp,
        ) {
            Ok(frame) => frame,
            Err(err) => {
                callback(Err(err));
                if let Err(requeue_err) = capture_queue.enqueue(index) {
                    callback(Err(requeue_err));
                }
                return;
            }
        };

        callback(Ok(ConvertCallbackOutput::Frame {
            frame,
            value: pending_value,
        }));
    }

    fn validate_pixel_format(fmt: PixelFormat, name: &str) -> crate::error::Result<()> {
        match fmt {
            PixelFormat::Yuv420 | PixelFormat::Nv12 => Ok(()),
            _ => Err(crate::error::Error::InvalidFormat {
                reason: format!("converter {name} does not support {:?}", fmt),
            }),
        }
    }

    fn set_output_format(
        fd: RawFd,
        width: u32,
        height: u32,
        pixel_format: PixelFormat,
        name: &str,
    ) -> crate::error::Result<Resolution> {
        let mut fmt = sys::zeroed_format(sys::V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE);
        let pix_mp = unsafe { &mut fmt.fmt.pix_mp };

        pix_mp.width = width;
        pix_mp.height = height;
        pix_mp.pixelformat = pixel_format.to_fourcc();
        pix_mp.field = sys::V4L2_FIELD_ANY;
        pix_mp.colorspace = sys::V4L2_COLORSPACE_DEFAULT;
        pix_mp.num_planes = 1;
        pix_mp.plane_fmt[0].bytesperline = width;
        pix_mp.plane_fmt[0].sizeimage = Self::frame_size(width, height) as u32;

        sys::ioctl_s_fmt(fd, &mut fmt)?;

        let actual = unsafe { &fmt.fmt.pix_mp };
        if actual.width == 0 || actual.height == 0 || actual.plane_fmt[0].bytesperline == 0 {
            return Err(crate::error::Error::InvalidFormat {
                reason: format!("{name} is invalid after VIDIOC_S_FMT"),
            });
        }

        Ok(Resolution {
            width: actual.width,
            height: actual.height,
            stride: actual.plane_fmt[0].bytesperline,
        })
    }

    fn set_capture_format(
        fd: RawFd,
        width: u32,
        height: u32,
        pixel_format: PixelFormat,
        name: &str,
    ) -> crate::error::Result<Resolution> {
        let mut fmt = sys::zeroed_format(sys::V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE);
        let pix_mp = unsafe { &mut fmt.fmt.pix_mp };

        pix_mp.width = width;
        pix_mp.height = height;
        pix_mp.pixelformat = pixel_format.to_fourcc();
        pix_mp.field = sys::V4L2_FIELD_ANY;
        pix_mp.colorspace = sys::V4L2_COLORSPACE_DEFAULT;
        pix_mp.num_planes = 1;
        pix_mp.plane_fmt[0].bytesperline = width;
        pix_mp.plane_fmt[0].sizeimage = Self::frame_size(width, height) as u32;

        sys::ioctl_s_fmt(fd, &mut fmt)?;

        let actual = unsafe { &fmt.fmt.pix_mp };
        if actual.width == 0 || actual.height == 0 || actual.plane_fmt[0].bytesperline == 0 {
            return Err(crate::error::Error::InvalidFormat {
                reason: format!("{name} is invalid after VIDIOC_S_FMT"),
            });
        }

        Ok(Resolution {
            width: actual.width,
            height: actual.height,
            stride: actual.plane_fmt[0].bytesperline,
        })
    }

    fn frame_size(width: u32, height: u32) -> usize {
        (width as usize) * (height as usize) * 3 / 2
    }
}

impl<T> Drop for ImageConverter<T> {
    fn drop(&mut self) {
        // Poller を先に停止
        if let Some(ref mut poller) = self.poller {
            poller.stop();
        }
        self.poller = None;

        let fd = self.device.raw_fd();
        if let Ok(mut runtime) = self.shared.runtime.lock()
            && runtime.started
        {
            let _ = sys::ioctl_streamoff(fd, sys::V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE);
            let _ = sys::ioctl_streamoff(fd, sys::V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE);
            runtime.started = false;
        }
    }
}
