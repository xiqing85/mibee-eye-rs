//! V4L2 OUTPUT / CAPTURE キューの管理。

use std::collections::VecDeque;
use std::os::fd::{AsRawFd, RawFd};

use crate::buffer::{BufferSet, PlaneMapping};
use crate::sys;

/// OUTPUT キュー (エンコーダー/デコーダーへの入力)。
pub(crate) struct OutputQueue {
    fd: RawFd,
    buf_type: u32,
    memory: u32,
    buffers: BufferSet,
    available: VecDeque<u32>,
}

impl OutputQueue {
    /// OUTPUT キューを作成する。
    pub fn new(fd: RawFd, buf_type: u32, memory: u32, buffers: BufferSet) -> Self {
        let count = buffers.count();
        let mut available = VecDeque::with_capacity(count as usize);
        for i in 0..count {
            available.push_back(i);
        }
        OutputQueue {
            fd,
            buf_type,
            memory,
            buffers,
            available,
        }
    }

    /// 利用可能なバッファインデックスを取得する。
    pub fn dequeue_available(&mut self) -> Option<u32> {
        self.available.pop_front()
    }

    /// バッファインデックスを利用可能リストに戻す。
    pub fn return_buffer(&mut self, index: u32) {
        self.available.push_back(index);
    }

    /// mmap バッファを直接初期化して QBUF する。
    pub fn enqueue(
        &mut self,
        index: u32,
        fill: &mut dyn FnMut(&mut [u8]) -> Option<usize>,
        timestamp_us: i64,
    ) -> crate::error::Result<()> {
        let plane_length = self.buffers.plane(index, 0).length;
        let bytesused = {
            let buf = self.buffers.mmap_slice_mut(index, 0).ok_or(
                crate::error::Error::InvalidFormat {
                    reason: "output queue does not provide MMAP buffer".to_string(),
                },
            )?;
            fill(buf).ok_or(crate::error::Error::MmapInputNotProduced)?
        };

        if bytesused > plane_length as usize {
            return Err(crate::error::Error::InputTooLarge {
                size: bytesused,
                capacity: plane_length as usize,
            });
        }

        self.enqueue_with_plane(
            index,
            bytesused as u32,
            plane_length,
            sys::v4l2_plane_m { mem_offset: 0 },
            self.memory,
            timestamp_us,
        )
    }

    fn enqueue_with_plane(
        &mut self,
        index: u32,
        bytesused: u32,
        length: u32,
        plane_m: sys::v4l2_plane_m,
        memory: u32,
        timestamp_us: i64,
    ) -> crate::error::Result<()> {
        if bytesused > length {
            return Err(crate::error::Error::InputTooLarge {
                size: bytesused as usize,
                capacity: length as usize,
            });
        }

        let timestamp = sys::timestamp_us_to_timeval(timestamp_us);

        let mut plane_info = sys::v4l2_plane {
            bytesused,
            length,
            m: plane_m,
            data_offset: 0,
            reserved: [0; 11],
        };

        let mut buf = sys::zeroed_buffer(self.buf_type, memory);
        buf.index = index;
        buf.length = 1;
        buf.flags = sys::V4L2_BUF_FLAG_TIMESTAMP_COPY;
        buf.timestamp = timestamp;
        buf.m = sys::v4l2_buffer_m {
            planes: &mut plane_info as *mut _,
        };

        sys::ioctl_qbuf(self.fd, &mut buf)
    }

    /// DMABUF fd を設定して QBUF する (ゼロコピー)。
    pub fn enqueue_dmabuf(
        &mut self,
        index: u32,
        dmabuf_fd: RawFd,
        bytesused: u32,
        length: u32,
        timestamp_us: i64,
    ) -> crate::error::Result<()> {
        self.enqueue_with_plane(
            index,
            bytesused,
            length,
            sys::v4l2_plane_m { fd: dmabuf_fd },
            sys::V4L2_MEMORY_DMABUF,
            timestamp_us,
        )
    }
}

/// CAPTURE キュー (エンコーダー/デコーダーからの出力)。
pub(crate) struct CaptureQueue {
    fd: RawFd,
    buf_type: u32,
    memory: u32,
    buffers: BufferSet,
}

impl CaptureQueue {
    /// CAPTURE キューを作成する。
    pub fn new(fd: RawFd, buf_type: u32, memory: u32, buffers: BufferSet) -> Self {
        CaptureQueue {
            fd,
            buf_type,
            memory,
            buffers,
        }
    }

    /// 全バッファを QBUF する (初期化時に使用)。
    pub fn enqueue_all(&self) -> crate::error::Result<()> {
        for i in 0..self.buffers.count() {
            self.enqueue(i)?;
        }
        Ok(())
    }

    /// 指定インデックスのバッファを QBUF する。
    pub fn enqueue(&self, index: u32) -> crate::error::Result<()> {
        let plane = self.buffers.plane(index, 0);

        let mut plane_info = sys::v4l2_plane {
            bytesused: 0,
            length: plane.length,
            m: match &plane.mapping {
                PlaneMapping::Mmap(_) => sys::v4l2_plane_m { mem_offset: 0 },
                PlaneMapping::DmaBuf(fd) => sys::v4l2_plane_m { fd: fd.as_raw_fd() },
                PlaneMapping::None => unreachable!("CAPTURE バッファに NoMapping は使用されない"),
            },
            data_offset: 0,
            reserved: [0; 11],
        };

        let mut buf = sys::zeroed_buffer(self.buf_type, self.memory);
        buf.index = index;
        buf.length = 1;
        buf.m = sys::v4l2_buffer_m {
            planes: &mut plane_info as *mut _,
        };

        sys::ioctl_qbuf(self.fd, &mut buf)
    }

    /// バッファセットへの参照を取得する。
    pub fn buffers(&self) -> &BufferSet {
        &self.buffers
    }
}
