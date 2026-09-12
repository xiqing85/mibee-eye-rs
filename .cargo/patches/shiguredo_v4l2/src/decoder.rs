//! H.264 ハードウェアデコーダー。
//!
//! V4L2 M2M デバイス (`/dev/video10`) を使用して
//! H.264 データを I420 フレームにデコードする。

use std::collections::VecDeque;
use std::os::fd::RawFd;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::buffer::BufferSet;
use crate::device::Device;
use crate::format::{Memory, Resolution};
use crate::poller::{PollEvent, Poller, PollerConfig, Timestamp};
use crate::queue::{CaptureQueue, OutputQueue};
use crate::sys;

/// デコーダーの設定。
pub struct DecoderConfig {
    /// デバイスパス。デフォルト: "/dev/video10"。
    pub device_path: String,
    /// 入力メモリ方式。デフォルト: Mmap。
    pub input_memory: Memory,
    /// 出力メモリ方式。デフォルト: Mmap。
    pub output_memory: Memory,
    /// OUTPUT バッファ数。デフォルト: 4。
    pub output_buffer_count: u32,
    /// CAPTURE バッファ数。デフォルト: 4。
    pub capture_buffer_count: u32,
}

impl DecoderConfig {
    /// デフォルト設定を作成する。
    pub fn new() -> Self {
        DecoderConfig {
            device_path: "/dev/video10".to_string(),
            input_memory: Memory::Mmap,
            output_memory: Memory::Mmap,
            output_buffer_count: 4,
            capture_buffer_count: 4,
        }
    }
}

impl Default for DecoderConfig {
    fn default() -> Self {
        Self::new()
    }
}

type DecodeMmapFill<'a, T> = dyn FnMut(&mut [u8], &T) -> Option<usize> + 'a;

/// デコーダー入力。
pub enum DecodeInput<'a, T> {
    /// mmap 入力バッファを直接初期化するクロージャ。
    ///
    /// `None` を返した場合は `Error::MmapInputNotProduced` を返す。
    Mmap(&'a mut DecodeMmapFill<'a, T>),
    /// DMABUF 入力。
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

/// デコードされたフレーム。
///
/// このハンドルが `Drop` されると、内部 CAPTURE バッファが自動再キューされる。
pub struct DecodedFrame<T> {
    requeue: Option<RequeueToken>,
    index: u32,
    bytesused: u32,
    timestamp_us: i64,
    user_data: T,
}

impl<T> DecodedFrame<T> {
    fn new(
        capture_queue: Arc<CaptureQueue>,
        pending_async_errors: Arc<Mutex<VecDeque<crate::error::Error>>>,
        index: u32,
        bytesused: u32,
        timestamp: Timestamp,
        user_data: T,
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
            user_data,
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

    /// ユーザーデータを返す。
    pub fn user_data(&self) -> &T {
        &self.user_data
    }
}

impl<T> Drop for DecodedFrame<T> {
    fn drop(&mut self) {
        if let Some(token) = self.requeue.take() {
            token.requeue();
        }
    }
}

/// デコード結果を通知するためのハンドラー
///
/// デコード処理が完了するたびに [`DecodeHandler::on_decoded`] が呼ばれる。
/// 解像度が変更された場合は [`DecodeHandler::on_resolution_changed`] が呼ばれる。
pub trait DecodeHandler: Send + 'static {
    /// ユーザーデータ型
    type UserData: Send + 'static;
    /// エラー型
    type Error: From<crate::error::Error> + Send + 'static;
    /// デコード完了時に呼ばれる
    fn on_decoded(&mut self, result: Result<DecodedFrame<Self::UserData>, Self::Error>);
    /// 解像度変更時に呼ばれる
    fn on_resolution_changed(&mut self, resolution: Resolution);
}

/// `FnMut` クロージャを [`DecodeHandler`] にするラッパー
pub struct FnDecodeHandler<T, E = crate::error::Error> {
    f: Box<dyn FnMut(Result<DecodedFrame<T>, E>) + Send + 'static>,
    on_res_change: Box<dyn FnMut(Resolution) + Send + 'static>,
}

impl<T, E> FnDecodeHandler<T, E> {
    pub fn new<F, G>(f: F, on_resolution_changed: G) -> Self
    where
        F: FnMut(Result<DecodedFrame<T>, E>) + Send + 'static,
        G: FnMut(Resolution) + Send + 'static,
    {
        Self {
            f: Box::new(f),
            on_res_change: Box::new(on_resolution_changed),
        }
    }
}

impl<T, E> DecodeHandler for FnDecodeHandler<T, E>
where
    T: Send + 'static,
    E: From<crate::error::Error> + Send + 'static,
{
    type UserData = T;
    type Error = E;
    fn on_decoded(&mut self, result: Result<DecodedFrame<T>, E>) {
        (self.f)(result);
    }
    fn on_resolution_changed(&mut self, resolution: Resolution) {
        (self.on_res_change)(resolution);
    }
}

struct DecoderRuntime<T> {
    output_queue: OutputQueue,
    capture_queue: Option<Arc<CaptureQueue>>,
    resolution: Option<Resolution>,
    capture_started: bool,
    pending_values: VecDeque<T>,
}

struct DecoderShared<T> {
    runtime: Mutex<DecoderRuntime<T>>,
    fd: RawFd,
    input_memory: Memory,
    output_memory: Memory,
    capture_buffer_count: u32,
    pending_async_errors: Arc<Mutex<VecDeque<crate::error::Error>>>,
}

impl<T> DecoderShared<T> {
    fn drain_pending_async_errors(&self) -> Vec<crate::error::Error> {
        let mut errors = Vec::new();
        if let Ok(mut pending) = self.pending_async_errors.lock() {
            errors.extend(pending.drain(..));
        }
        errors
    }
}

/// H.264 ハードウェアデコーダー。
///
/// フィールド宣言順序は Drop 順序に影響する。
/// `device` (fd) はキューやポーラーより後に Drop されなければならない。
pub struct H264Decoder<H: DecodeHandler> {
    poller: Option<Poller>,
    shared: Arc<DecoderShared<H::UserData>>,
    device: Device,
}

impl<H: DecodeHandler> H264Decoder<H> {
    /// デコーダーを初期化する。
    pub fn new(config: DecoderConfig, mut handler: H) -> crate::error::Result<Self> {
        let device = Device::open(&config.device_path)?;
        let fd = device.raw_fd();

        // OUTPUT フォーマット設定 (H.264 入力)
        Self::set_output_format(fd)?;

        let output_v4l2_memory = config.input_memory.to_v4l2();

        // OUTPUT バッファ確保
        let output_buffers = BufferSet::allocate(
            fd,
            sys::V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
            output_v4l2_memory,
            config.output_buffer_count,
            false,
        )?;
        let output_queue = OutputQueue::new(
            fd,
            sys::V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
            output_v4l2_memory,
            output_buffers,
        );

        // OUTPUT STREAMON
        sys::ioctl_streamon(fd, sys::V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE)?;

        // SOURCE_CHANGE イベントを購読 (非対応のデバイスでは無視)
        let sub = sys::v4l2_event_subscription {
            r#type: sys::V4L2_EVENT_SOURCE_CHANGE,
            id: 0,
            flags: 0,
            reserved: [0; 5],
        };
        let subscribe_events = sys::ioctl_subscribe_event(fd, &sub).is_ok();

        let shared = Arc::new(DecoderShared {
            runtime: Mutex::new(DecoderRuntime {
                output_queue,
                capture_queue: None,
                resolution: None,
                capture_started: false,
                pending_values: VecDeque::new(),
            }),
            fd,
            input_memory: config.input_memory,
            output_memory: config.output_memory,
            capture_buffer_count: config.capture_buffer_count,
            pending_async_errors: Arc::new(Mutex::new(VecDeque::new())),
        });

        // handler は poller スレッドのクロージャーが単独所有する。
        let poller_shared = shared.clone();
        let poller = Poller::start(
            PollerConfig {
                fd,
                output_buf_type: sys::V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
                output_memory: output_v4l2_memory,
                capture_buf_type: sys::V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
                capture_memory: sys::V4L2_MEMORY_MMAP,
                subscribe_events,
            },
            move |event| Self::handle_event(&poller_shared, &mut handler, event),
        );

        Ok(H264Decoder {
            poller: Some(poller),
            shared,
            device,
        })
    }

    /// H.264 データをデコードキューへ投入する。
    pub fn decode(
        &mut self,
        input: DecodeInput<'_, H::UserData>,
        timestamp_us: i64,
        user_data: H::UserData,
    ) -> crate::error::Result<()> {
        let input_memory = self.shared.input_memory;
        let mut runtime = self.lock_runtime()?;

        let output_index = runtime
            .output_queue
            .dequeue_available()
            .ok_or(crate::error::Error::NoAvailableBuffer)?;

        let enqueue_result = match input {
            DecodeInput::Mmap(fill) => {
                if !matches!(input_memory, Memory::Mmap) {
                    Err(crate::error::Error::InvalidFormat {
                        reason: "decoder is configured for DMABUF input".to_string(),
                    })
                } else {
                    let mut fill_with_user_data =
                        |buf: &mut [u8]| -> Option<usize> { fill(buf, &user_data) };
                    runtime.output_queue.enqueue(
                        output_index,
                        &mut fill_with_user_data,
                        timestamp_us,
                    )
                }
            }
            DecodeInput::DmaBuf {
                fd,
                bytesused,
                length,
            } => {
                if !matches!(input_memory, Memory::DmaBuf) {
                    Err(crate::error::Error::InvalidFormat {
                        reason: "decoder is configured for MMAP input".to_string(),
                    })
                } else {
                    runtime.output_queue.enqueue_dmabuf(
                        output_index,
                        fd,
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

        runtime.pending_values.push_back(user_data);
        Ok(())
    }

    /// 現在の解像度を取得する。
    pub fn resolution(&self) -> Option<Resolution> {
        match self.shared.runtime.lock() {
            Ok(runtime) => runtime.resolution,
            Err(_) => None,
        }
    }

    fn lock_runtime(&self) -> crate::error::Result<MutexGuard<'_, DecoderRuntime<H::UserData>>> {
        self.shared
            .runtime
            .lock()
            .map_err(|_| crate::error::Error::PollerAborted)
    }

    fn handle_event(shared: &Arc<DecoderShared<H::UserData>>, handler: &mut H, event: PollEvent) {
        for err in shared.drain_pending_async_errors() {
            handler.on_decoded(Err(err.into()));
        }

        match event {
            PollEvent::OutputDequeued { index } => {
                if let Ok(mut runtime) = shared.runtime.lock() {
                    runtime.output_queue.return_buffer(index);
                } else {
                    handler.on_decoded(Err(crate::error::Error::PollerAborted.into()));
                }
            }
            PollEvent::SourceChanged => Self::handle_source_change(shared, handler),
            PollEvent::CaptureDequeued {
                index,
                bytesused,
                flags: _,
                timestamp,
            } => Self::handle_capture(shared, handler, index, bytesused, timestamp),
            PollEvent::Error(err) => handler.on_decoded(Err(err.into())),
        }
    }

    fn handle_source_change(shared: &Arc<DecoderShared<H::UserData>>, handler: &mut H) {
        let resolution_result = (|| -> crate::error::Result<Resolution> {
            let fd = shared.fd;
            let mut runtime = shared
                .runtime
                .lock()
                .map_err(|_| crate::error::Error::PollerAborted)?;

            // CAPTURE ストリームを停止 (開始済みの場合)
            if runtime.capture_started {
                let _ = sys::ioctl_streamoff(fd, sys::V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE);
                runtime.capture_started = false;
            }

            // 古い CAPTURE バッファを解放
            runtime.capture_queue = None;

            // G_FMT で新しい解像度を取得
            let mut fmt = sys::zeroed_format(sys::V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE);
            sys::ioctl_g_fmt(fd, &mut fmt)?;

            let (width, height, stride) = unsafe {
                (
                    fmt.fmt.pix_mp.width,
                    fmt.fmt.pix_mp.height,
                    fmt.fmt.pix_mp.plane_fmt[0].bytesperline,
                )
            };

            let resolution = Resolution {
                width,
                height,
                stride,
            };
            runtime.resolution = Some(resolution);

            let export_dmabuf = matches!(shared.output_memory, Memory::DmaBuf);

            // 新しい CAPTURE バッファを確保
            let capture_buffers = BufferSet::allocate(
                fd,
                sys::V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
                sys::V4L2_MEMORY_MMAP,
                shared.capture_buffer_count,
                export_dmabuf,
            )?;
            let capture_queue = Arc::new(CaptureQueue::new(
                fd,
                sys::V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
                sys::V4L2_MEMORY_MMAP,
                capture_buffers,
            ));

            // 全 CAPTURE バッファを QBUF
            capture_queue.enqueue_all()?;
            runtime.capture_queue = Some(capture_queue);

            // CAPTURE STREAMON
            sys::ioctl_streamon(fd, sys::V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE)?;
            runtime.capture_started = true;

            Ok(resolution)
        })();

        match resolution_result {
            Ok(resolution) => handler.on_resolution_changed(resolution),
            Err(err) => handler.on_decoded(Err(err.into())),
        }
    }

    fn handle_capture(
        shared: &Arc<DecoderShared<H::UserData>>,
        handler: &mut H,
        index: u32,
        bytesused: u32,
        timestamp: Timestamp,
    ) {
        enum CaptureDispatch<U> {
            Frame {
                frame: DecodedFrame<U>,
            },
            Error {
                err: crate::error::Error,
                capture_queue: Option<Arc<CaptureQueue>>,
            },
        }

        let dispatch: CaptureDispatch<H::UserData> = match shared.runtime.lock() {
            Ok(mut runtime) => {
                let capture_queue = runtime.capture_queue.clone();
                match capture_queue.clone() {
                    None => CaptureDispatch::Error {
                        err: crate::error::Error::NotStarted,
                        capture_queue: None,
                    },
                    Some(capture_queue_for_frame) => match runtime.pending_values.pop_front() {
                        None => CaptureDispatch::Error {
                            err: crate::error::Error::NoAvailableBuffer,
                            capture_queue,
                        },
                        Some(user_data) => match DecodedFrame::new(
                            capture_queue_for_frame,
                            shared.pending_async_errors.clone(),
                            index,
                            bytesused,
                            timestamp,
                            user_data,
                        ) {
                            Ok(frame) => CaptureDispatch::Frame { frame },
                            Err(err) => CaptureDispatch::Error { err, capture_queue },
                        },
                    },
                }
            }
            Err(_) => CaptureDispatch::Error {
                err: crate::error::Error::PollerAborted,
                capture_queue: None,
            },
        };

        match dispatch {
            CaptureDispatch::Frame { frame } => {
                handler.on_decoded(Ok(frame));
            }
            CaptureDispatch::Error { err, capture_queue } => {
                handler.on_decoded(Err(err.into()));
                if let Some(capture_queue) = capture_queue
                    && let Err(requeue_err) = capture_queue.enqueue(index)
                {
                    handler.on_decoded(Err(requeue_err.into()));
                }
            }
        }
    }

    fn set_output_format(fd: RawFd) -> crate::error::Result<()> {
        let mut fmt = sys::zeroed_format(sys::V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE);
        let pix_mp = unsafe { &mut fmt.fmt.pix_mp };
        pix_mp.pixelformat = sys::V4L2_PIX_FMT_H264;
        pix_mp.num_planes = 1;
        pix_mp.plane_fmt[0].sizeimage = 512 * 1024; // 512KB

        sys::ioctl_s_fmt(fd, &mut fmt)
    }
}

impl<H: DecodeHandler> Drop for H264Decoder<H> {
    fn drop(&mut self) {
        // Poller を先に停止
        if let Some(ref mut poller) = self.poller {
            poller.stop();
        }
        self.poller = None;

        let fd = self.device.raw_fd();

        if let Ok(mut runtime) = self.shared.runtime.lock() {
            // CAPTURE キューを解放
            if runtime.capture_started {
                let _ = sys::ioctl_streamoff(fd, sys::V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE);
            }
            runtime.capture_queue = None;
            runtime.capture_started = false;
        }

        let _ = sys::ioctl_streamoff(fd, sys::V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE);
    }
}
