//! V4L2 デバイスファイルの管理。

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// V4L2 デバイスファイルを `OwnedFd` で管理する。
///
/// `Drop` で自動的にファイルディスクリプタが閉じられる。
pub(crate) struct Device {
    fd: OwnedFd,
}

impl Device {
    /// デバイスファイルをオープンする。
    pub fn open(path: &str) -> crate::error::Result<Self> {
        let c_path = std::ffi::CString::new(path).map_err(|_| crate::error::Error::DeviceOpen {
            path: path.to_string(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "パスに NULL バイトが含まれています",
            ),
        })?;

        let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(crate::error::Error::DeviceOpen {
                path: path.to_string(),
                source: std::io::Error::last_os_error(),
            });
        }

        let owned_fd = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Device { fd: owned_fd })
    }

    /// 生のファイルディスクリプタを取得する。
    pub fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}
