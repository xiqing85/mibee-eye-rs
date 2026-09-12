//! V4L2 ポーリングスレッド。
//!
//! C++ の `V4L2Runner` に相当する。
//! `poll()` でイベントを監視し、エンコーダー/デコーダーへ直接通知する。

use std::os::fd::RawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use crate::sys;

/// タイムスタンプ情報。
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timestamp {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

/// ポーリングスレッドからのイベント。
pub(crate) enum PollEvent {
    /// OUTPUT バッファがデキューされた。
    OutputDequeued { index: u32 },
    /// CAPTURE バッファがデキューされた。
    CaptureDequeued {
        index: u32,
        bytesused: u32,
        flags: u32,
        timestamp: Timestamp,
    },
    /// ソース変更イベント (解像度変更)。
    SourceChanged,
    /// エラーが発生した。
    Error(crate::error::Error),
}

/// ポーリングスレッドの設定。
pub(crate) struct PollerConfig {
    pub fd: RawFd,
    pub output_buf_type: u32,
    pub output_memory: u32,
    pub capture_buf_type: u32,
    pub capture_memory: u32,
    pub subscribe_events: bool,
}

/// ポーリングスレッド。
pub(crate) struct Poller {
    thread: Option<JoinHandle<()>>,
    abort: Arc<AtomicBool>,
}

impl Poller {
    /// ポーリングスレッドを起動する。
    pub fn start<F>(config: PollerConfig, on_event: F) -> Self
    where
        F: FnMut(PollEvent) + Send + 'static,
    {
        let abort = Arc::new(AtomicBool::new(false));
        let abort_clone = abort.clone();

        let thread = thread::spawn(move || {
            Self::poll_loop(config, on_event, abort_clone);
        });

        Poller {
            thread: Some(thread),
            abort,
        }
    }

    /// ポーリングスレッドを停止する。
    pub fn stop(&mut self) {
        self.abort.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    fn poll_loop<F>(config: PollerConfig, mut on_event: F, abort: Arc<AtomicBool>)
    where
        F: FnMut(PollEvent),
    {
        let mut poll_events = libc::POLLIN | libc::POLLOUT;
        if config.subscribe_events {
            poll_events |= libc::POLLPRI;
        }

        loop {
            if abort.load(Ordering::Acquire) {
                return;
            }

            let mut pollfd = libc::pollfd {
                fd: config.fd,
                events: poll_events,
                revents: 0,
            };

            let ret = unsafe { libc::poll(&mut pollfd, 1, 500) };

            if abort.load(Ordering::Acquire) {
                return;
            }

            if ret < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                on_event(PollEvent::Error(crate::error::Error::Poll { source: err }));
                return;
            }

            if ret == 0 {
                continue;
            }

            if pollfd.revents & libc::POLLPRI != 0
                && let Err(err) = Self::handle_source_event(&config, &mut on_event, &abort)
            {
                on_event(PollEvent::Error(err));
                return;
            }

            // OUTPUT は POLLOUT のときのみ DQBUF する。
            if pollfd.revents & libc::POLLOUT != 0
                && let Err(err) = Self::process_output(&config, &mut on_event, &abort)
            {
                on_event(PollEvent::Error(err));
                return;
            }

            // CAPTURE は POLLIN のときのみ DQBUF する。
            if pollfd.revents & libc::POLLIN != 0
                && let Err(err) = Self::process_capture(&config, &mut on_event, &abort)
            {
                on_event(PollEvent::Error(err));
                return;
            }
        }
    }

    fn handle_source_event<F>(
        config: &PollerConfig,
        on_event: &mut F,
        abort: &AtomicBool,
    ) -> crate::error::Result<()>
    where
        F: FnMut(PollEvent),
    {
        if abort.load(Ordering::Acquire) {
            return Ok(());
        }

        let mut event: sys::v4l2_event = unsafe { std::mem::zeroed() };
        match sys::ioctl_dqevent(config.fd, &mut event) {
            Ok(()) => {
                if event.r#type == sys::V4L2_EVENT_SOURCE_CHANGE {
                    // event.u の先頭 4 バイトが v4l2_event_src_change.changes
                    let changes = u32::from_ne_bytes([
                        event.u.data[0],
                        event.u.data[1],
                        event.u.data[2],
                        event.u.data[3],
                    ]);
                    if changes & sys::V4L2_EVENT_SRC_CH_RESOLUTION != 0 {
                        on_event(PollEvent::SourceChanged);
                    }
                }
                Ok(())
            }
            Err(crate::error::Error::Ioctl { source, .. })
                if source.raw_os_error() == Some(libc::EAGAIN) =>
            {
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    fn process_output<F>(
        config: &PollerConfig,
        on_event: &mut F,
        abort: &AtomicBool,
    ) -> crate::error::Result<()>
    where
        F: FnMut(PollEvent),
    {
        if abort.load(Ordering::Acquire) {
            return Ok(());
        }

        if let Some(event) = Self::try_dequeue_output(config)? {
            on_event(event);
        }

        Ok(())
    }

    fn process_capture<F>(
        config: &PollerConfig,
        on_event: &mut F,
        abort: &AtomicBool,
    ) -> crate::error::Result<()>
    where
        F: FnMut(PollEvent),
    {
        if abort.load(Ordering::Acquire) {
            return Ok(());
        }

        if let Some(event) = Self::try_dequeue_capture(config)? {
            on_event(event);
        }

        Ok(())
    }

    fn try_dequeue_output(config: &PollerConfig) -> crate::error::Result<Option<PollEvent>> {
        let mut plane = sys::v4l2_plane {
            bytesused: 0,
            length: 0,
            m: sys::v4l2_plane_m { mem_offset: 0 },
            data_offset: 0,
            reserved: [0; 11],
        };

        let mut buf = sys::zeroed_buffer(config.output_buf_type, config.output_memory);
        buf.length = 1;
        buf.m = sys::v4l2_buffer_m {
            planes: &mut plane as *mut _,
        };

        match sys::ioctl_dqbuf(config.fd, &mut buf) {
            Ok(()) => Ok(Some(PollEvent::OutputDequeued { index: buf.index })),
            Err(crate::error::Error::Ioctl { source, .. })
                if source.raw_os_error() == Some(libc::EAGAIN) =>
            {
                Ok(None)
            }
            Err(err) => Err(err),
        }
    }

    fn try_dequeue_capture(config: &PollerConfig) -> crate::error::Result<Option<PollEvent>> {
        let mut plane = sys::v4l2_plane {
            bytesused: 0,
            length: 0,
            m: sys::v4l2_plane_m { mem_offset: 0 },
            data_offset: 0,
            reserved: [0; 11],
        };

        let mut buf = sys::zeroed_buffer(config.capture_buf_type, config.capture_memory);
        buf.length = 1;
        buf.m = sys::v4l2_buffer_m {
            planes: &mut plane as *mut _,
        };

        match sys::ioctl_dqbuf(config.fd, &mut buf) {
            Ok(()) => Ok(Some(PollEvent::CaptureDequeued {
                index: buf.index,
                bytesused: plane.bytesused,
                flags: buf.flags,
                timestamp: Timestamp {
                    tv_sec: buf.timestamp.tv_sec,
                    tv_usec: buf.timestamp.tv_usec,
                },
            })),
            Err(crate::error::Error::Ioctl { source, .. })
                if source.raw_os_error() == Some(libc::EAGAIN) =>
            {
                Ok(None)
            }
            Err(err) => Err(err),
        }
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        self.stop();
    }
}
