//! H.264 ハードウェアエンコーダー。
//!
//! V4L2 M2M デバイス (`/dev/video11`) を使用して
//! I420 / NV12 フレームを H.264 にエンコードする。

use std::collections::VecDeque;
use std::os::fd::RawFd;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::buffer::BufferSet;
use crate::device::Device;
use crate::format::{Memory, PixelFormat, Resolution};
use crate::poller::{PollEvent, Poller, PollerConfig, Timestamp};
use crate::queue::{CaptureQueue, OutputQueue};
use crate::sys;

/// H.264 プロファイル。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H264Profile {
    Baseline,
    ConstrainedBaseline,
    Main,
    High,
}

impl H264Profile {
    /// V4L2 定数に変換する。
    pub fn to_v4l2(self) -> i32 {
        match self {
            H264Profile::Baseline => sys::V4L2_MPEG_VIDEO_H264_PROFILE_BASELINE,
            H264Profile::ConstrainedBaseline => {
                sys::V4L2_MPEG_VIDEO_H264_PROFILE_CONSTRAINED_BASELINE
            }
            H264Profile::Main => sys::V4L2_MPEG_VIDEO_H264_PROFILE_MAIN,
            H264Profile::High => sys::V4L2_MPEG_VIDEO_H264_PROFILE_HIGH,
        }
    }

    /// V4L2 定数から変換する。
    pub fn from_v4l2(value: i32) -> Option<Self> {
        match value {
            sys::V4L2_MPEG_VIDEO_H264_PROFILE_BASELINE => Some(H264Profile::Baseline),
            sys::V4L2_MPEG_VIDEO_H264_PROFILE_CONSTRAINED_BASELINE => {
                Some(H264Profile::ConstrainedBaseline)
            }
            sys::V4L2_MPEG_VIDEO_H264_PROFILE_MAIN => Some(H264Profile::Main),
            sys::V4L2_MPEG_VIDEO_H264_PROFILE_HIGH => Some(H264Profile::High),
            _ => None,
        }
    }
}

/// H.264 レベル。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H264Level {
    Level3_0,
    Level3_1,
    Level3_2,
    Level4_0,
    Level4_1,
    Level4_2,
    Level5_0,
    Level5_1,
}

impl H264Level {
    /// V4L2 定数に変換する。
    pub fn to_v4l2(self) -> i32 {
        match self {
            H264Level::Level3_0 => sys::V4L2_MPEG_VIDEO_H264_LEVEL_3_0,
            H264Level::Level3_1 => sys::V4L2_MPEG_VIDEO_H264_LEVEL_3_1,
            H264Level::Level3_2 => sys::V4L2_MPEG_VIDEO_H264_LEVEL_3_2,
            H264Level::Level4_0 => sys::V4L2_MPEG_VIDEO_H264_LEVEL_4_0,
            H264Level::Level4_1 => sys::V4L2_MPEG_VIDEO_H264_LEVEL_4_1,
            H264Level::Level4_2 => sys::V4L2_MPEG_VIDEO_H264_LEVEL_4_2,
            H264Level::Level5_0 => sys::V4L2_MPEG_VIDEO_H264_LEVEL_5_0,
            H264Level::Level5_1 => sys::V4L2_MPEG_VIDEO_H264_LEVEL_5_1,
        }
    }

    /// V4L2 定数から変換する。
    pub fn from_v4l2(value: i32) -> Option<Self> {
        match value {
            sys::V4L2_MPEG_VIDEO_H264_LEVEL_3_0 => Some(H264Level::Level3_0),
            sys::V4L2_MPEG_VIDEO_H264_LEVEL_3_1 => Some(H264Level::Level3_1),
            sys::V4L2_MPEG_VIDEO_H264_LEVEL_3_2 => Some(H264Level::Level3_2),
            sys::V4L2_MPEG_VIDEO_H264_LEVEL_4_0 => Some(H264Level::Level4_0),
            sys::V4L2_MPEG_VIDEO_H264_LEVEL_4_1 => Some(H264Level::Level4_1),
            sys::V4L2_MPEG_VIDEO_H264_LEVEL_4_2 => Some(H264Level::Level4_2),
            sys::V4L2_MPEG_VIDEO_H264_LEVEL_5_0 => Some(H264Level::Level5_0),
            sys::V4L2_MPEG_VIDEO_H264_LEVEL_5_1 => Some(H264Level::Level5_1),
            _ => None,
        }
    }
}

/// エンコーダーの設定。
pub struct EncoderConfig {
    /// デバイスパス。デフォルト: "/dev/video11"。
    pub device_path: String,
    /// 入力映像の幅。
    pub width: u32,
    /// 入力映像の高さ。
    pub height: u32,
    /// 入力映像の stride。0 の場合は width と同じ。
    pub stride: u32,
    /// H.264 プロファイル。デフォルト: High。
    pub profile: H264Profile,
    /// H.264 レベル。デフォルト: 4.2。
    pub level: H264Level,
    /// I フレーム間隔 (フレーム数)。デフォルト: 500。
    pub i_period: u32,
    /// SPS/PPS を各キーフレームに付加するか。デフォルト: true。
    pub repeat_sequence_header: bool,
    /// ビットレート (bps)。
    pub bitrate_bps: u32,
    /// OUTPUT バッファ数。デフォルト: 4。
    pub output_buffer_count: u32,
    /// CAPTURE バッファ数。デフォルト: 4。
    pub capture_buffer_count: u32,
    /// 入力メモリ方式。デフォルト: Mmap。
    pub input_memory: Memory,
    /// 出力メモリ方式。デフォルト: Mmap。
    pub output_memory: Memory,
    /// 入力ピクセルフォーマット。デフォルト: Yuv420 (I420)。
    pub pixel_format: PixelFormat,
}

impl EncoderConfig {
    /// デフォルト設定を作成する。解像度とビットレートは必須。
    pub fn new(width: u32, height: u32, bitrate_bps: u32) -> Self {
        EncoderConfig {
            device_path: "/dev/video11".to_string(),
            width,
            height,
            stride: 0,
            profile: H264Profile::High,
            level: H264Level::Level4_2,
            i_period: 500,
            repeat_sequence_header: true,
            bitrate_bps,
            output_buffer_count: 4,
            capture_buffer_count: 4,
            input_memory: Memory::Mmap,
            output_memory: Memory::Mmap,
            pixel_format: PixelFormat::Yuv420,
        }
    }
}

type EncodeMmapFill<'a, T> = dyn FnMut(&mut [u8], &Resolution, &T) -> Option<usize> + 'a;

/// エンコーダーへの入力。
pub enum EncodeInput<'a, T> {
    /// MMAP 入力バッファを直接初期化するクロージャ。
    ///
    /// `None` を返した場合は `Error::MmapInputNotProduced` を返す。
    Mmap(&'a mut EncodeMmapFill<'a, T>),
    /// DMABUF ファイルディスクリプタ。
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

/// エンコードされた H.264 フレーム。
///
/// このハンドルが `Drop` されると、内部 CAPTURE バッファが自動再キューされる。
pub struct EncodedFrame<T> {
    requeue: Option<RequeueToken>,
    index: u32,
    bytesused: u32,
    is_keyframe: bool,
    timestamp_us: i64,
    user_data: T,
}

impl<T> EncodedFrame<T> {
    fn new(
        capture_queue: Arc<CaptureQueue>,
        pending_async_errors: Arc<Mutex<VecDeque<crate::error::Error>>>,
        index: u32,
        bytesused: u32,
        flags: u32,
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
            is_keyframe: flags & sys::V4L2_BUF_FLAG_KEYFRAME != 0,
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

    /// キーフレームかどうかを返す。
    pub fn is_keyframe(&self) -> bool {
        self.is_keyframe
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

impl<T> Drop for EncodedFrame<T> {
    fn drop(&mut self) {
        if let Some(token) = self.requeue.take() {
            token.requeue();
        }
    }
}

/// エンコード結果を通知するためのハンドラー
///
/// エンコード処理が完了するたびに [`EncodeHandler::on_encoded`] が呼ばれる。
pub trait EncodeHandler: Send + 'static {
    /// ユーザーデータ型
    type UserData: Send + 'static;
    /// エラー型
    type Error: From<crate::error::Error> + Send + 'static;
    /// エンコード完了時に呼ばれる
    fn on_encoded(&mut self, result: Result<EncodedFrame<Self::UserData>, Self::Error>);
}

/// `FnMut` クロージャを [`EncodeHandler`] にするラッパー
pub struct FnEncodeHandler<T, E = crate::error::Error> {
    f: Box<dyn FnMut(Result<EncodedFrame<T>, E>) + Send + 'static>,
}

impl<T, E> FnEncodeHandler<T, E> {
    pub fn new<F>(f: F) -> Self
    where
        F: FnMut(Result<EncodedFrame<T>, E>) + Send + 'static,
    {
        Self { f: Box::new(f) }
    }
}

impl<T, E> EncodeHandler for FnEncodeHandler<T, E>
where
    T: Send + 'static,
    E: From<crate::error::Error> + Send + 'static,
{
    type UserData = T;
    type Error = E;
    fn on_encoded(&mut self, result: Result<EncodedFrame<T>, E>) {
        (self.f)(result);
    }
}

#[derive(Debug, Clone, Copy)]
struct ConfiguredOutputFormat {
    width: u32,
    height: u32,
    stride: u32,
}

struct EncoderRuntime<T> {
    output_queue: OutputQueue,
    started: bool,
    pending_values: VecDeque<T>,
}

struct EncoderShared<T> {
    runtime: Mutex<EncoderRuntime<T>>,
    capture_queue: Arc<CaptureQueue>,
    resolution: Resolution,
    input_memory: Memory,
    output_memory: u32,
    pending_async_errors: Arc<Mutex<VecDeque<crate::error::Error>>>,
}

impl<T> EncoderShared<T> {
    fn drain_pending_async_errors(&self) -> Vec<crate::error::Error> {
        let mut errors = Vec::new();
        if let Ok(mut pending) = self.pending_async_errors.lock() {
            errors.extend(pending.drain(..));
        }
        errors
    }
}

/// H.264 ハードウェアエンコーダー。
///
/// フィールド宣言順序は Drop 順序に影響する。
/// `device` (fd) はキューやポーラーより後に Drop されなければならない。
pub struct H264Encoder<H: EncodeHandler> {
    poller: Option<Poller>,
    shared: Arc<EncoderShared<H::UserData>>,
    handler: Option<H>,
    device: Device,
}

impl<H: EncodeHandler> H264Encoder<H> {
    /// エンコーダーを初期化する。
    pub fn new(config: EncoderConfig, handler: H) -> crate::error::Result<Self> {
        let device = Device::open(&config.device_path)?;
        let fd = device.raw_fd();

        let stride = if config.stride == 0 {
            config.width
        } else {
            config.stride
        };

        // ピクセルフォーマットのバリデーション
        match config.pixel_format {
            PixelFormat::Yuv420 | PixelFormat::Nv12 => {}
            _ => {
                return Err(crate::error::Error::InvalidFormat {
                    reason: format!("encoder input does not support {:?}", config.pixel_format),
                });
            }
        }

        // H.264 コントロール設定
        Self::set_controls(fd, &config)?;

        // OUTPUT フォーマット設定
        let output_memory = config.input_memory.to_v4l2();

        let output_format =
            Self::set_output_format(fd, config.width, config.height, stride, config.pixel_format)?;

        // CAPTURE フォーマット設定
        Self::set_capture_format(fd, output_format.width, output_format.height)?;

        // OUTPUT バッファ確保
        let output_buffers = BufferSet::allocate(
            fd,
            sys::V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
            output_memory,
            config.output_buffer_count,
            false,
        )?;
        let output_queue = OutputQueue::new(
            fd,
            sys::V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
            output_memory,
            output_buffers,
        );

        // CAPTURE バッファ確保
        let export_capture_dmabuf = matches!(config.output_memory, Memory::DmaBuf);
        let capture_buffers = BufferSet::allocate(
            fd,
            sys::V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
            sys::V4L2_MEMORY_MMAP,
            config.capture_buffer_count,
            export_capture_dmabuf,
        )?;
        let capture_queue = Arc::new(CaptureQueue::new(
            fd,
            sys::V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
            sys::V4L2_MEMORY_MMAP,
            capture_buffers,
        ));

        // 全 CAPTURE バッファを QBUF
        capture_queue.enqueue_all()?;

        let resolution = Resolution {
            width: output_format.width,
            height: output_format.height,
            stride: output_format.stride,
        };

        let runtime = EncoderRuntime {
            output_queue,
            started: false,
            pending_values: VecDeque::new(),
        };

        let shared = Arc::new(EncoderShared {
            runtime: Mutex::new(runtime),
            capture_queue,
            resolution,
            input_memory: config.input_memory,
            output_memory,
            pending_async_errors: Arc::new(Mutex::new(VecDeque::new())),
        });

        Ok(H264Encoder {
            poller: None,
            shared,
            handler: Some(handler),
            device,
        })
    }

    /// フレームをエンキューする。
    pub fn encode(
        &mut self,
        frame: EncodeInput<'_, H::UserData>,
        timestamp_us: i64,
        force_keyframe: bool,
        user_data: H::UserData,
    ) -> crate::error::Result<()> {
        let fd = self.device.raw_fd();
        let input_memory = self.shared.input_memory;
        let resolution = self.shared.resolution;

        // キーフレーム強制
        if force_keyframe {
            self.force_keyframe()?;
        }

        let mut needs_start = false;
        {
            let mut runtime = self.lock_runtime()?;

            let output_index = runtime
                .output_queue
                .dequeue_available()
                .ok_or(crate::error::Error::NoAvailableBuffer)?;

            let enqueue_result = match frame {
                EncodeInput::Mmap(fill) => {
                    if !matches!(input_memory, Memory::Mmap) {
                        Err(crate::error::Error::InvalidFormat {
                            reason: "encoder is configured for DMABUF input".to_string(),
                        })
                    } else {
                        let mut fill_with_resolution = |buf: &mut [u8]| -> Option<usize> {
                            fill(buf, &resolution, &user_data)
                        };
                        runtime.output_queue.enqueue(
                            output_index,
                            &mut fill_with_resolution,
                            timestamp_us,
                        )
                    }
                }
                EncodeInput::DmaBuf {
                    fd: dmabuf_fd,
                    bytesused,
                    length,
                } => {
                    if !matches!(input_memory, Memory::DmaBuf) {
                        Err(crate::error::Error::InvalidFormat {
                            reason: "encoder is configured for MMAP input".to_string(),
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

            runtime.pending_values.push_back(user_data);

            if !runtime.started {
                // 順序: OUTPUT QBUF → OUTPUT STREAMON → CAPTURE STREAMON
                sys::ioctl_streamon(fd, sys::V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE)?;
                sys::ioctl_streamon(fd, sys::V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE)?;
                runtime.started = true;
                needs_start = true;
            }
        }

        if needs_start {
            self.start_poller();
        }

        Ok(())
    }

    /// ビットレートを変更する。
    pub fn set_bitrate(&mut self, bitrate_bps: u32) -> crate::error::Result<()> {
        let value = i32::try_from(bitrate_bps).map_err(|_| crate::error::Error::InvalidFormat {
            reason: format!("bitrate exceeds i32 maximum: {bitrate_bps}"),
        })?;
        let ctrl = sys::v4l2_control {
            id: sys::V4L2_CID_MPEG_VIDEO_BITRATE,
            value,
        };
        sys::ioctl_s_ctrl(self.device.raw_fd(), &ctrl)
    }

    /// 次のフレームを強制的にキーフレームにする。
    pub fn force_keyframe(&mut self) -> crate::error::Result<()> {
        let ctrl = sys::v4l2_control {
            id: sys::V4L2_CID_MPEG_VIDEO_FORCE_KEY_FRAME,
            value: 1,
        };
        sys::ioctl_s_ctrl(self.device.raw_fd(), &ctrl)
    }

    /// 現在の解像度を取得する。
    pub fn resolution(&self) -> Resolution {
        self.shared.resolution
    }

    fn lock_runtime(&self) -> crate::error::Result<MutexGuard<'_, EncoderRuntime<H::UserData>>> {
        self.shared
            .runtime
            .lock()
            .map_err(|_| crate::error::Error::PollerAborted)
    }

    fn start_poller(&mut self) {
        if self.poller.is_some() {
            return;
        }

        let Some(mut handler) = self.handler.take() else {
            return;
        };

        let fd = self.device.raw_fd();
        let output_memory = self.shared.output_memory;

        let shared = self.shared.clone();
        // handler は poller スレッドのクロージャーが単独所有する。
        self.poller = Some(Poller::start(
            PollerConfig {
                fd,
                output_buf_type: sys::V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
                output_memory,
                capture_buf_type: sys::V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
                capture_memory: sys::V4L2_MEMORY_MMAP,
                subscribe_events: false,
            },
            move |event| Self::handle_event(&shared, &mut handler, event),
        ));
    }

    fn handle_event(shared: &Arc<EncoderShared<H::UserData>>, handler: &mut H, event: PollEvent) {
        for err in shared.drain_pending_async_errors() {
            handler.on_encoded(Err(err.into()));
        }

        match event {
            PollEvent::OutputDequeued { index } => {
                if let Ok(mut runtime) = shared.runtime.lock() {
                    runtime.output_queue.return_buffer(index);
                } else {
                    handler.on_encoded(Err(crate::error::Error::PollerAborted.into()));
                }
            }
            PollEvent::CaptureDequeued {
                index,
                bytesused,
                flags,
                timestamp,
            } => Self::handle_capture(shared, handler, index, bytesused, flags, timestamp),
            PollEvent::Error(err) => handler.on_encoded(Err(err.into())),
            PollEvent::SourceChanged => {
                // エンコーダーでは発生しない。
            }
        }
    }

    fn handle_capture(
        shared: &Arc<EncoderShared<H::UserData>>,
        handler: &mut H,
        index: u32,
        bytesused: u32,
        flags: u32,
        timestamp: Timestamp,
    ) {
        let pending_user_data = match shared.runtime.lock() {
            Ok(mut runtime) => {
                let Some(user_data) = runtime.pending_values.pop_front() else {
                    drop(runtime);
                    handler.on_encoded(Err(crate::error::Error::NoAvailableBuffer.into()));
                    if let Err(err) = shared.capture_queue.enqueue(index) {
                        handler.on_encoded(Err(err.into()));
                    }
                    return;
                };
                user_data
            }
            Err(_) => {
                handler.on_encoded(Err(crate::error::Error::PollerAborted.into()));
                return;
            }
        };
        let capture_queue = shared.capture_queue.clone();

        let frame = match EncodedFrame::new(
            capture_queue.clone(),
            shared.pending_async_errors.clone(),
            index,
            bytesused,
            flags,
            timestamp,
            pending_user_data,
        ) {
            Ok(frame) => frame,
            Err(err) => {
                handler.on_encoded(Err(err.into()));
                if let Err(requeue_err) = capture_queue.enqueue(index) {
                    handler.on_encoded(Err(requeue_err.into()));
                }
                return;
            }
        };

        handler.on_encoded(Ok(frame));
    }

    fn set_controls(fd: RawFd, config: &EncoderConfig) -> crate::error::Result<()> {
        // 各コントロールはデバイスによってサポートされない場合があるため非致命的に設定する

        // プロファイル
        let _ = sys::ioctl_s_ctrl(
            fd,
            &sys::v4l2_control {
                id: sys::V4L2_CID_MPEG_VIDEO_H264_PROFILE,
                value: config.profile.to_v4l2(),
            },
        );

        // レベル
        let _ = sys::ioctl_s_ctrl(
            fd,
            &sys::v4l2_control {
                id: sys::V4L2_CID_MPEG_VIDEO_H264_LEVEL,
                value: config.level.to_v4l2(),
            },
        );

        // I フレーム間隔
        let _ = sys::ioctl_s_ctrl(
            fd,
            &sys::v4l2_control {
                id: sys::V4L2_CID_MPEG_VIDEO_H264_I_PERIOD,
                value: config.i_period as i32,
            },
        );

        // SPS/PPS 繰り返し
        let _ = sys::ioctl_s_ctrl(
            fd,
            &sys::v4l2_control {
                id: sys::V4L2_CID_MPEG_VIDEO_REPEAT_SEQ_HEADER,
                value: if config.repeat_sequence_header { 1 } else { 0 },
            },
        );

        // ビットレート
        if let Ok(value) = i32::try_from(config.bitrate_bps) {
            let _ = sys::ioctl_s_ctrl(
                fd,
                &sys::v4l2_control {
                    id: sys::V4L2_CID_MPEG_VIDEO_BITRATE,
                    value,
                },
            );
        }

        Ok(())
    }

    fn set_output_format(
        fd: RawFd,
        width: u32,
        height: u32,
        stride: u32,
        pixel_format: PixelFormat,
    ) -> crate::error::Result<ConfiguredOutputFormat> {
        let mut fmt = sys::zeroed_format(sys::V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE);
        let pix_mp = unsafe { &mut fmt.fmt.pix_mp };
        pix_mp.width = width;
        pix_mp.height = height;
        pix_mp.pixelformat = pixel_format.to_fourcc();
        pix_mp.field = sys::V4L2_FIELD_ANY;
        pix_mp.colorspace = sys::V4L2_COLORSPACE_DEFAULT;
        pix_mp.num_planes = 1;
        pix_mp.plane_fmt[0].bytesperline = stride;
        pix_mp.plane_fmt[0].sizeimage = Resolution {
            width,
            height,
            stride,
        }
        .yuv420_size() as u32;

        sys::ioctl_s_fmt(fd, &mut fmt)?;

        let pix_mp = unsafe { &fmt.fmt.pix_mp };
        Ok(ConfiguredOutputFormat {
            width: pix_mp.width,
            height: pix_mp.height,
            stride: pix_mp.plane_fmt[0].bytesperline,
        })
    }

    fn set_capture_format(fd: RawFd, width: u32, height: u32) -> crate::error::Result<()> {
        let mut fmt = sys::zeroed_format(sys::V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE);
        let pix_mp = unsafe { &mut fmt.fmt.pix_mp };
        pix_mp.width = width;
        pix_mp.height = height;
        pix_mp.pixelformat = sys::V4L2_PIX_FMT_H264;
        pix_mp.num_planes = 1;
        pix_mp.plane_fmt[0].sizeimage = 512 * 1024; // 512KB

        sys::ioctl_s_fmt(fd, &mut fmt)
    }
}

impl<H: EncodeHandler> Drop for H264Encoder<H> {
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
            let _ = sys::ioctl_streamoff(fd, sys::V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE);
            let _ = sys::ioctl_streamoff(fd, sys::V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE);
            runtime.started = false;
        }
    }
}
