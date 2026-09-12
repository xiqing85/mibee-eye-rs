//! Local recording subsystem (GB28181 playback).
//!
//! Writes bare Annex-B H.264 segments to disk with an append-only index and
//! per-frame PTS sidecars, enforces retention/capacity, and reads segments
//! back for playback. Disk layout is identical to the Go repo.

pub mod clock_sync;
pub mod index;
pub mod retention;
pub mod writer;

pub use index::{RecordingIndex, SegmentInfo};

use std::sync::atomic::{AtomicU64, Ordering};

/// Cumulative size of indexed recording segments — the app's on-disk
/// footprint as reported by `GET /api/metrics/summary` (`process.storage_bytes`).
/// Maintained by the writer (index load + segment close) and the retention
/// sweep (deletions).
static RECORDED_BYTES: AtomicU64 = AtomicU64::new(0);

/// Current on-disk recording footprint in bytes.
#[must_use]
pub fn recorded_bytes() -> u64 {
    RECORDED_BYTES.load(Ordering::Relaxed)
}

/// Add `n` bytes to the recording footprint (segment closed).
pub(crate) fn add_recorded_bytes(n: u64) {
    RECORDED_BYTES.fetch_add(n, Ordering::Relaxed);
}

/// Remove `n` bytes from the recording footprint (retention deleted data).
pub(crate) fn sub_recorded_bytes(n: u64) {
    RECORDED_BYTES
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |b| {
            Some(b.saturating_sub(n))
        })
        .ok();
}
