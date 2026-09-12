//! ピクセルフォーマットと解像度の定義。

use crate::sys;

/// V4L2 バッファのメモリ方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Memory {
    /// mmap によるコピー入出力。
    Mmap,
    /// DMABUF によるゼロコピー入出力。
    DmaBuf,
}

impl Memory {
    pub(crate) fn to_v4l2(self) -> u32 {
        match self {
            Memory::Mmap => sys::V4L2_MEMORY_MMAP,
            Memory::DmaBuf => sys::V4L2_MEMORY_DMABUF,
        }
    }
}

/// ピクセルフォーマット。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// I420 (YUV420 planar)。
    Yuv420,
    /// NV12 (YUV420 semi-planar)。
    Nv12,
    /// H.264。
    H264,
}

impl PixelFormat {
    /// V4L2 fourcc 値に変換する。
    pub fn to_fourcc(self) -> u32 {
        match self {
            PixelFormat::Yuv420 => sys::V4L2_PIX_FMT_YUV420,
            PixelFormat::Nv12 => sys::V4L2_PIX_FMT_NV12,
            PixelFormat::H264 => sys::V4L2_PIX_FMT_H264,
        }
    }

    /// V4L2 fourcc 値から変換する。
    pub fn from_fourcc(fourcc: u32) -> Option<Self> {
        match fourcc {
            sys::V4L2_PIX_FMT_YUV420 => Some(PixelFormat::Yuv420),
            sys::V4L2_PIX_FMT_NV12 => Some(PixelFormat::Nv12),
            sys::V4L2_PIX_FMT_H264 => Some(PixelFormat::H264),
            _ => None,
        }
    }
}

/// 映像の解像度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

impl Resolution {
    /// YUV420 (I420) フレームのバイトサイズを計算する。
    ///
    /// Y plane: stride * height
    /// U plane: (stride/2) * (height/2)
    /// V plane: (stride/2) * (height/2)
    pub fn yuv420_size(&self) -> usize {
        let w = self.stride as usize;
        let h = self.height as usize;
        w * h * 3 / 2
    }
}
