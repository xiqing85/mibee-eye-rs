//! V4L2 低レベル定数・構造体・ioctl ラッパー。
//!
//! `linux/videodev2.h` を参照し、必要な定数と構造体を手動定義する。
//! unsafe はこのモジュールに集約する。

#![allow(non_camel_case_types)]

use std::os::fd::RawFd;

// ---------------------------------------------------------------------------
// ioctl リクエスト番号
// ---------------------------------------------------------------------------

// ioctl direction bits
const _IOC_NRBITS: u32 = 8;
const _IOC_TYPEBITS: u32 = 8;
const _IOC_SIZEBITS: u32 = 14;

const _IOC_NRSHIFT: u32 = 0;
const _IOC_TYPESHIFT: u32 = _IOC_NRSHIFT + _IOC_NRBITS;
const _IOC_SIZESHIFT: u32 = _IOC_TYPESHIFT + _IOC_TYPEBITS;
const _IOC_DIRSHIFT: u32 = _IOC_SIZESHIFT + _IOC_SIZEBITS;

const _IOC_WRITE: u32 = 1;
const _IOC_READ: u32 = 2;

const fn _ioc(dir: u32, ty: u32, nr: u32, size: u32) -> u64 {
    ((dir << _IOC_DIRSHIFT)
        | (ty << _IOC_TYPESHIFT)
        | (nr << _IOC_NRSHIFT)
        | (size << _IOC_SIZESHIFT)) as u64
}

const fn _iowr(ty: u32, nr: u32, size: u32) -> u64 {
    _ioc(_IOC_READ | _IOC_WRITE, ty, nr, size)
}

const fn _iow(ty: u32, nr: u32, size: u32) -> u64 {
    _ioc(_IOC_WRITE, ty, nr, size)
}

const fn _ior(ty: u32, nr: u32, size: u32) -> u64 {
    _ioc(_IOC_READ, ty, nr, size)
}

const V4L2_TYPE: u32 = b'V' as u32;

// IOCTL request codes are 32-bit values. Use u32 for compatibility with both glibc (unsigned long)
// and musl (int) ioctl signatures. On glibc: unsigned long = u64 on aarch64. On musl: int = i32.
// Both accept u32 values without truncation since the ioctl constants are within 32-bit range.
pub(crate) const VIDIOC_S_FMT: u32 = _iowr(V4L2_TYPE, 5, V4L2_FORMAT_SIZE) as u32;
pub(crate) const VIDIOC_G_FMT: u32 = _iowr(V4L2_TYPE, 4, V4L2_FORMAT_SIZE) as u32;
pub(crate) const VIDIOC_REQBUFS: u32 = _iowr(V4L2_TYPE, 8, V4L2_REQUESTBUFFERS_SIZE) as u32;
pub(crate) const VIDIOC_QUERYBUF: u32 = _iowr(V4L2_TYPE, 9, V4L2_BUFFER_SIZE) as u32;
pub(crate) const VIDIOC_QBUF: u32 = _iowr(V4L2_TYPE, 15, V4L2_BUFFER_SIZE) as u32;
pub(crate) const VIDIOC_DQBUF: u32 = _iowr(V4L2_TYPE, 17, V4L2_BUFFER_SIZE) as u32;
pub(crate) const VIDIOC_STREAMON: u32 = _iow(V4L2_TYPE, 18, 4) as u32;
pub(crate) const VIDIOC_STREAMOFF: u32 = _iow(V4L2_TYPE, 19, 4) as u32;
pub(crate) const VIDIOC_S_CTRL: u32 = _iowr(V4L2_TYPE, 28, V4L2_CONTROL_SIZE) as u32;
pub(crate) const VIDIOC_EXPBUF: u32 = _iowr(V4L2_TYPE, 16, V4L2_EXPORTBUFFER_SIZE) as u32;
pub(crate) const VIDIOC_SUBSCRIBE_EVENT: u32 = _iow(V4L2_TYPE, 90, V4L2_EVENT_SUBSCRIPTION_SIZE) as u32;
pub(crate) const VIDIOC_DQEVENT: u32 = _ior(V4L2_TYPE, 89, V4L2_EVENT_SIZE) as u32;

// ---------------------------------------------------------------------------
// バッファタイプ
// ---------------------------------------------------------------------------

pub(crate) const V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE: u32 = 10;
pub(crate) const V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE: u32 = 9;

// ---------------------------------------------------------------------------
// メモリタイプ
// ---------------------------------------------------------------------------

pub(crate) const V4L2_MEMORY_MMAP: u32 = 1;
pub(crate) const V4L2_MEMORY_DMABUF: u32 = 4;

// ---------------------------------------------------------------------------
// フィールド
// ---------------------------------------------------------------------------

pub(crate) const V4L2_FIELD_ANY: u32 = 0;

// ---------------------------------------------------------------------------
// カラースペース
// ---------------------------------------------------------------------------

pub(crate) const V4L2_COLORSPACE_DEFAULT: u32 = 0;

// ---------------------------------------------------------------------------
// ピクセルフォーマット (fourcc)
// ---------------------------------------------------------------------------

const fn v4l2_fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

pub(crate) const V4L2_PIX_FMT_YUV420: u32 = v4l2_fourcc(b'Y', b'U', b'1', b'2');
pub(crate) const V4L2_PIX_FMT_NV12: u32 = v4l2_fourcc(b'N', b'V', b'1', b'2');
pub(crate) const V4L2_PIX_FMT_H264: u32 = v4l2_fourcc(b'H', b'2', b'6', b'4');

// ---------------------------------------------------------------------------
// バッファフラグ
// ---------------------------------------------------------------------------

pub(crate) const V4L2_BUF_FLAG_TIMESTAMP_COPY: u32 = 0x00004000;
pub(crate) const V4L2_BUF_FLAG_KEYFRAME: u32 = 0x00000008;
// ---------------------------------------------------------------------------
// コントロール ID
// ---------------------------------------------------------------------------

const V4L2_CTRL_CLASS_CODEC: u32 = 0x00990000;
const V4L2_CID_CODEC_BASE: u32 = V4L2_CTRL_CLASS_CODEC | 0x900;

pub(crate) const V4L2_CID_MPEG_VIDEO_H264_PROFILE: u32 = V4L2_CID_CODEC_BASE + 363;
pub(crate) const V4L2_CID_MPEG_VIDEO_H264_LEVEL: u32 = V4L2_CID_CODEC_BASE + 359;
pub(crate) const V4L2_CID_MPEG_VIDEO_H264_I_PERIOD: u32 = V4L2_CID_CODEC_BASE + 358;
pub(crate) const V4L2_CID_MPEG_VIDEO_REPEAT_SEQ_HEADER: u32 = V4L2_CID_CODEC_BASE + 226;
pub(crate) const V4L2_CID_MPEG_VIDEO_FORCE_KEY_FRAME: u32 = V4L2_CID_CODEC_BASE + 229;
pub(crate) const V4L2_CID_MPEG_VIDEO_BITRATE: u32 = V4L2_CID_CODEC_BASE + 207;

// ---------------------------------------------------------------------------
// H.264 プロファイル定数
// ---------------------------------------------------------------------------

pub(crate) const V4L2_MPEG_VIDEO_H264_PROFILE_BASELINE: i32 = 0;
pub(crate) const V4L2_MPEG_VIDEO_H264_PROFILE_CONSTRAINED_BASELINE: i32 = 1;
pub(crate) const V4L2_MPEG_VIDEO_H264_PROFILE_MAIN: i32 = 2;
pub(crate) const V4L2_MPEG_VIDEO_H264_PROFILE_HIGH: i32 = 4;

// ---------------------------------------------------------------------------
// H.264 レベル定数
// ---------------------------------------------------------------------------

pub(crate) const V4L2_MPEG_VIDEO_H264_LEVEL_3_0: i32 = 8;
pub(crate) const V4L2_MPEG_VIDEO_H264_LEVEL_3_1: i32 = 9;
pub(crate) const V4L2_MPEG_VIDEO_H264_LEVEL_3_2: i32 = 10;
pub(crate) const V4L2_MPEG_VIDEO_H264_LEVEL_4_0: i32 = 11;
pub(crate) const V4L2_MPEG_VIDEO_H264_LEVEL_4_1: i32 = 12;
pub(crate) const V4L2_MPEG_VIDEO_H264_LEVEL_4_2: i32 = 13;
pub(crate) const V4L2_MPEG_VIDEO_H264_LEVEL_5_0: i32 = 14;
pub(crate) const V4L2_MPEG_VIDEO_H264_LEVEL_5_1: i32 = 15;

// ---------------------------------------------------------------------------
// イベント
// ---------------------------------------------------------------------------

pub(crate) const V4L2_EVENT_SOURCE_CHANGE: u32 = 5;
pub(crate) const V4L2_EVENT_SRC_CH_RESOLUTION: u32 = 1;

// ---------------------------------------------------------------------------
// 構造体サイズ定数 (ioctl マクロ用)
// ---------------------------------------------------------------------------

const V4L2_FORMAT_SIZE: u32 = 208;
const V4L2_REQUESTBUFFERS_SIZE: u32 = 20;
const V4L2_BUFFER_SIZE: u32 = 88;
const V4L2_CONTROL_SIZE: u32 = 8;
const V4L2_EXPORTBUFFER_SIZE: u32 = 64;
const V4L2_EVENT_SUBSCRIPTION_SIZE: u32 = 32;
const V4L2_EVENT_SIZE: u32 = 136;

// ---------------------------------------------------------------------------
// V4L2 構造体 (#[repr(C)])
// ---------------------------------------------------------------------------

/// `v4l2_pix_format_mplane` のプレーン情報。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct v4l2_plane_pix_format {
    pub sizeimage: u32,
    pub bytesperline: u32,
    pub reserved: [u16; 6],
}

/// `v4l2_pix_format_mplane`。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct v4l2_pix_format_mplane {
    pub width: u32,
    pub height: u32,
    pub pixelformat: u32,
    pub field: u32,
    pub colorspace: u32,
    pub plane_fmt: [v4l2_plane_pix_format; 8],
    pub num_planes: u8,
    pub flags: u8,
    pub _encoding_or_ycbcr: u8,
    pub quantization: u8,
    pub xfer_func: u8,
    pub reserved: [u8; 7],
}

/// `v4l2_format`。
///
/// `v4l2_format_union` の実際のアライメントは `v4l2_window`（ポインタを含む）により
/// 8 バイトになるため、`type` の直後に 4 バイトのパディングが入る。
/// Rust 側の union には `v4l2_window` を定義しないため明示的に追加する。
#[repr(C)]
pub(crate) struct v4l2_format {
    pub r#type: u32,
    pub _pad: u32, // v4l2_format_union の 8 バイトアライメントに合わせるパディング
    pub fmt: v4l2_format_union,
}

/// `v4l2_format` の union 部分。pix_mp のみ使用。
#[repr(C)]
pub(crate) union v4l2_format_union {
    pub pix_mp: v4l2_pix_format_mplane,
    pub raw: [u8; 200],
}

/// `v4l2_requestbuffers`。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct v4l2_requestbuffers {
    pub count: u32,
    pub r#type: u32,
    pub memory: u32,
    pub capabilities: u32,
    pub flags: u8,
    pub reserved: [u8; 3],
}

/// `v4l2_plane` (バッファのプレーン情報)。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct v4l2_plane {
    pub bytesused: u32,
    pub length: u32,
    pub m: v4l2_plane_m,
    pub data_offset: u32,
    pub reserved: [u32; 11],
}

/// `v4l2_plane` の m union。
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) union v4l2_plane_m {
    pub mem_offset: u32,
    pub userptr: u64,
    pub fd: i32,
}

impl std::fmt::Debug for v4l2_plane_m {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("v4l2_plane_m")
            .field("mem_offset", &unsafe { self.mem_offset })
            .finish()
    }
}

/// `v4l2_timeval` (タイムスタンプ)。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct v4l2_timeval {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

/// `v4l2_buffer`。
#[repr(C)]
pub(crate) struct v4l2_buffer {
    pub index: u32,
    pub r#type: u32,
    pub bytesused: u32,
    pub flags: u32,
    pub field: u32,
    pub timestamp: v4l2_timeval,
    pub timecode: [u8; 16], // v4l2_timecode
    pub sequence: u32,
    pub memory: u32,
    pub m: v4l2_buffer_m,
    pub length: u32,
    pub reserved2: u32,
    pub reserved: u32,
}

/// `v4l2_buffer` の m union。
#[repr(C)]
pub(crate) union v4l2_buffer_m {
    pub offset: u32,
    pub userptr: u64,
    pub planes: *mut v4l2_plane,
    pub fd: i32,
}

/// `v4l2_control`。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct v4l2_control {
    pub id: u32,
    pub value: i32,
}

/// `v4l2_exportbuffer`。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct v4l2_exportbuffer {
    pub r#type: u32,
    pub index: u32,
    pub plane: u32,
    pub flags: u32,
    pub fd: i32,
    pub reserved: [u32; 11],
}

/// `v4l2_event_subscription`。
#[repr(C)]
pub(crate) struct v4l2_event_subscription {
    pub r#type: u32,
    pub id: u32,
    pub flags: u32,
    pub reserved: [u32; 5],
}

/// `v4l2_event` の union 部分。
///
/// カーネル側の union は `v4l2_event_ctrl` 内の `__s64` により 8 バイトアライメントを持つ。
/// `[u8; 64]` ではアライメント 1 になり、`type` と `u` 間のパディングが欠落するため、
/// `#[repr(C, align(8))]` でカーネルと一致させる。
#[repr(C, align(8))]
#[derive(Clone, Copy)]
pub(crate) struct v4l2_event_data {
    pub data: [u8; 64],
}

/// `v4l2_event`。
#[repr(C)]
pub(crate) struct v4l2_event {
    pub r#type: u32,
    // v4l2_event_data のアライメント 8 により 4 バイトのパディングが自動挿入される
    pub u: v4l2_event_data,
    pub pending: u32,
    pub sequence: u32,
    pub timestamp: [u8; 16], // struct timespec
    pub id: u32,
    pub reserved: [u32; 8],
}

// ---------------------------------------------------------------------------
// ioctl ラッパー関数
// ---------------------------------------------------------------------------

pub(crate) fn ioctl_s_fmt(fd: RawFd, fmt: &mut v4l2_format) -> crate::error::Result<()> {
    let ret = unsafe { libc::ioctl(fd, VIDIOC_S_FMT as _, fmt as *mut _) };
    if ret < 0 {
        return Err(crate::error::Error::Ioctl {
            request: "VIDIOC_S_FMT",
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

pub(crate) fn ioctl_g_fmt(fd: RawFd, fmt: &mut v4l2_format) -> crate::error::Result<()> {
    let ret = unsafe { libc::ioctl(fd, VIDIOC_G_FMT as _, fmt as *mut _) };
    if ret < 0 {
        return Err(crate::error::Error::Ioctl {
            request: "VIDIOC_G_FMT",
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

pub(crate) fn ioctl_reqbufs(fd: RawFd, req: &mut v4l2_requestbuffers) -> crate::error::Result<()> {
    let ret = unsafe { libc::ioctl(fd, VIDIOC_REQBUFS as _, req as *mut _) };
    if ret < 0 {
        return Err(crate::error::Error::Ioctl {
            request: "VIDIOC_REQBUFS",
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

pub(crate) fn ioctl_querybuf(fd: RawFd, buf: &mut v4l2_buffer) -> crate::error::Result<()> {
    let ret = unsafe { libc::ioctl(fd, VIDIOC_QUERYBUF as _, buf as *mut _) };
    if ret < 0 {
        return Err(crate::error::Error::Ioctl {
            request: "VIDIOC_QUERYBUF",
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

pub(crate) fn ioctl_qbuf(fd: RawFd, buf: &mut v4l2_buffer) -> crate::error::Result<()> {
    let ret = unsafe { libc::ioctl(fd, VIDIOC_QBUF as _, buf as *mut _) };
    if ret < 0 {
        return Err(crate::error::Error::Ioctl {
            request: "VIDIOC_QBUF",
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

pub(crate) fn ioctl_dqbuf(fd: RawFd, buf: &mut v4l2_buffer) -> crate::error::Result<()> {
    let ret = unsafe { libc::ioctl(fd, VIDIOC_DQBUF as _, buf as *mut _) };
    if ret < 0 {
        return Err(crate::error::Error::Ioctl {
            request: "VIDIOC_DQBUF",
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

pub(crate) fn ioctl_streamon(fd: RawFd, buf_type: u32) -> crate::error::Result<()> {
    let ret = unsafe { libc::ioctl(fd, VIDIOC_STREAMON as _, &buf_type as *const _) };
    if ret < 0 {
        return Err(crate::error::Error::StreamOn {
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

pub(crate) fn ioctl_streamoff(fd: RawFd, buf_type: u32) -> crate::error::Result<()> {
    let ret = unsafe { libc::ioctl(fd, VIDIOC_STREAMOFF as _, &buf_type as *const _) };
    if ret < 0 {
        return Err(crate::error::Error::StreamOff {
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

pub(crate) fn ioctl_s_ctrl(fd: RawFd, ctrl: &v4l2_control) -> crate::error::Result<()> {
    let ret = unsafe { libc::ioctl(fd, VIDIOC_S_CTRL as _, ctrl as *const _) };
    if ret < 0 {
        return Err(crate::error::Error::Ioctl {
            request: "VIDIOC_S_CTRL",
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

pub(crate) fn ioctl_expbuf(fd: RawFd, expbuf: &mut v4l2_exportbuffer) -> crate::error::Result<()> {
    let ret = unsafe { libc::ioctl(fd, VIDIOC_EXPBUF as _, expbuf as *mut _) };
    if ret < 0 {
        return Err(crate::error::Error::Ioctl {
            request: "VIDIOC_EXPBUF",
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

pub(crate) fn ioctl_subscribe_event(
    fd: RawFd,
    sub: &v4l2_event_subscription,
) -> crate::error::Result<()> {
    let ret = unsafe { libc::ioctl(fd, VIDIOC_SUBSCRIBE_EVENT as _, sub as *const _) };
    if ret < 0 {
        return Err(crate::error::Error::Ioctl {
            request: "VIDIOC_SUBSCRIBE_EVENT",
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

pub(crate) fn ioctl_dqevent(fd: RawFd, event: &mut v4l2_event) -> crate::error::Result<()> {
    let ret = unsafe { libc::ioctl(fd, VIDIOC_DQEVENT as _, event as *mut _) };
    if ret < 0 {
        return Err(crate::error::Error::Ioctl {
            request: "VIDIOC_DQEVENT",
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// mmap / munmap ラッパー
// ---------------------------------------------------------------------------

pub(crate) fn mmap_buffer(fd: RawFd, length: usize, offset: u32) -> crate::error::Result<*mut u8> {
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            length,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            offset as libc::off_t,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(crate::error::Error::Mmap {
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(ptr as *mut u8)
}

pub(crate) fn munmap_buffer(ptr: *mut u8, length: usize) {
    unsafe {
        libc::munmap(ptr as *mut libc::c_void, length);
    }
}

// ---------------------------------------------------------------------------
// ヘルパー
// ---------------------------------------------------------------------------

/// `v4l2_format` をゼロ初期化する。
pub(crate) fn zeroed_format(buf_type: u32) -> v4l2_format {
    v4l2_format {
        r#type: buf_type,
        _pad: 0,
        fmt: v4l2_format_union { raw: [0u8; 200] },
    }
}

/// `v4l2_buffer` をゼロ初期化する。
pub(crate) fn zeroed_buffer(buf_type: u32, memory: u32) -> v4l2_buffer {
    v4l2_buffer {
        index: 0,
        r#type: buf_type,
        bytesused: 0,
        flags: 0,
        field: 0,
        timestamp: v4l2_timeval::default(),
        timecode: [0u8; 16],
        sequence: 0,
        memory,
        m: v4l2_buffer_m { offset: 0 },
        length: 0,
        reserved2: 0,
        reserved: 0,
    }
}

/// マイクロ秒を `v4l2_timeval` に変換する。
pub(crate) fn timestamp_us_to_timeval(timestamp_us: i64) -> v4l2_timeval {
    v4l2_timeval {
        tv_sec: timestamp_us / 1_000_000,
        tv_usec: timestamp_us % 1_000_000,
    }
}
