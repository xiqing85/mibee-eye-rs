//! V4L2 バッファの管理。
//!
//! mmap されたメモリ領域と DMABUF の管理を行う。
//! unsafe はこのモジュールに集約する。

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crate::sys;

/// mmap されたメモリ領域。
///
/// `Drop` で自動的に munmap される。
pub(crate) struct MmapRegion {
    ptr: *mut u8,
    length: usize,
}

// SAFETY: MmapRegion の ptr はプロセス全体から見える共有メモリだが、
// 排他アクセスはキュー管理によって保証する。
unsafe impl Send for MmapRegion {}
// SAFETY: 共有参照では読み取りのみを許可し、可変アクセスは &mut が必要なため競合しない。
unsafe impl Sync for MmapRegion {}

impl MmapRegion {
    /// 新しい mmap 領域を作成する。
    pub fn new(fd: RawFd, length: usize, offset: u32) -> crate::error::Result<Self> {
        let ptr = sys::mmap_buffer(fd, length, offset)?;
        Ok(MmapRegion { ptr, length })
    }

    /// mmap 領域をスライスとして取得する。
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: ptr は mmap で確保された有効なメモリ領域で、
        // length バイトの読み取りアクセスが保証されている。
        unsafe { std::slice::from_raw_parts(self.ptr, self.length) }
    }

    /// mmap 領域を可変スライスとして取得する。
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: ptr は mmap で確保された有効なメモリ領域で、
        // &mut self により排他アクセスが保証されている。
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.length) }
    }
}

impl Drop for MmapRegion {
    fn drop(&mut self) {
        sys::munmap_buffer(self.ptr, self.length);
    }
}

/// プレーンのマッピング方式。
pub(crate) enum PlaneMapping {
    /// mmap による直接メモリマッピング。
    Mmap(MmapRegion),
    /// DMABUF ファイルディスクリプタ (export_dmabuf=true 時)。
    DmaBuf(OwnedFd),
    /// マッピングなし (V4L2_MEMORY_DMABUF OUTPUT バッファ: fd はエンキュー時に指定)。
    None,
}

/// 単一バッファのプレーン情報。
pub(crate) struct BufferPlane {
    pub mapping: PlaneMapping,
    pub length: u32,
}

/// REQBUFS で確保された一連のバッファ。
pub(crate) struct BufferSet {
    fd: RawFd,
    buf_type: u32,
    memory: u32,
    buffers: Vec<Vec<BufferPlane>>,
}

impl BufferSet {
    /// バッファセットを確保し、mmap する。
    ///
    /// `export_dmabuf` が true の場合、mmap の代わりに DMABUF FD をエクスポートする。
    pub fn allocate(
        fd: RawFd,
        buf_type: u32,
        memory: u32,
        count: u32,
        export_dmabuf: bool,
    ) -> crate::error::Result<Self> {
        let mut req = sys::v4l2_requestbuffers {
            count,
            r#type: buf_type,
            memory,
            capabilities: 0,
            flags: 0,
            reserved: [0; 3],
        };
        sys::ioctl_reqbufs(fd, &mut req)?;

        let actual_count = req.count;
        let mut buffers = Vec::with_capacity(actual_count as usize);

        for i in 0..actual_count {
            let mut plane = sys::v4l2_plane {
                bytesused: 0,
                length: 0,
                m: sys::v4l2_plane_m { mem_offset: 0 },
                data_offset: 0,
                reserved: [0; 11],
            };

            let mut buf = sys::zeroed_buffer(buf_type, memory);
            buf.index = i;
            buf.length = 1; // num_planes
            buf.m = sys::v4l2_buffer_m {
                planes: &mut plane as *mut _,
            };

            sys::ioctl_querybuf(fd, &mut buf)?;

            let plane_info = if export_dmabuf {
                let mut expbuf = sys::v4l2_exportbuffer {
                    r#type: buf_type,
                    index: i,
                    plane: 0,
                    flags: libc::O_RDWR as u32,
                    fd: -1,
                    reserved: [0; 11],
                };
                sys::ioctl_expbuf(fd, &mut expbuf)?;

                let dmabuf_fd = unsafe { OwnedFd::from_raw_fd(expbuf.fd) };
                BufferPlane {
                    mapping: PlaneMapping::DmaBuf(dmabuf_fd),
                    length: plane.length,
                }
            } else if memory == sys::V4L2_MEMORY_DMABUF {
                // DMABUF メモリタイプのバッファは mmap できない。
                // fd はエンキュー時 (enqueue_dmabuf) に指定されるためマッピング不要。
                BufferPlane {
                    mapping: PlaneMapping::None,
                    length: plane.length,
                }
            } else {
                let offset = unsafe { plane.m.mem_offset };
                let region = MmapRegion::new(fd, plane.length as usize, offset)?;
                BufferPlane {
                    mapping: PlaneMapping::Mmap(region),
                    length: plane.length,
                }
            };

            buffers.push(vec![plane_info]);
        }

        Ok(BufferSet {
            fd,
            buf_type,
            memory,
            buffers,
        })
    }

    /// バッファ数を取得する。
    pub fn count(&self) -> u32 {
        self.buffers.len() as u32
    }

    /// 指定インデックスのバッファプレーンへの参照を取得する。
    pub fn plane(&self, index: u32, plane_index: usize) -> &BufferPlane {
        &self.buffers[index as usize][plane_index]
    }

    /// mmap 領域をスライスとして取得する。
    ///
    /// DMABUF の場合は None を返す。
    pub fn mmap_slice(&self, index: u32, plane_index: usize) -> Option<&[u8]> {
        match &self.buffers[index as usize][plane_index].mapping {
            PlaneMapping::Mmap(region) => Some(region.as_slice()),
            PlaneMapping::DmaBuf(_) | PlaneMapping::None => None,
        }
    }

    /// mmap 領域を可変スライスとして取得する。
    ///
    /// DMABUF の場合は None を返す。
    pub fn mmap_slice_mut(&mut self, index: u32, plane_index: usize) -> Option<&mut [u8]> {
        match &mut self.buffers[index as usize][plane_index].mapping {
            PlaneMapping::Mmap(region) => Some(region.as_mut_slice()),
            PlaneMapping::DmaBuf(_) | PlaneMapping::None => None,
        }
    }

    /// DMABUF の fd を取得する。
    ///
    /// Mmap の場合は None を返す。
    pub fn dmabuf_fd(&self, index: u32, plane_index: usize) -> Option<RawFd> {
        match &self.buffers[index as usize][plane_index].mapping {
            PlaneMapping::DmaBuf(fd) => Some(fd.as_raw_fd()),
            PlaneMapping::Mmap(_) | PlaneMapping::None => None,
        }
    }
}

impl Drop for BufferSet {
    fn drop(&mut self) {
        // バッファ (mmap 領域/DMABUF fd) を先に解放
        self.buffers.clear();

        // REQBUFS count=0 でカーネル側のバッファを解放
        let mut req = sys::v4l2_requestbuffers {
            count: 0,
            r#type: self.buf_type,
            memory: self.memory,
            capabilities: 0,
            flags: 0,
            reserved: [0; 3],
        };
        let _ = sys::ioctl_reqbufs(self.fd, &mut req);
    }
}
