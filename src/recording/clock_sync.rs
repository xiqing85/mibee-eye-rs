//! Boot-time clock synchronization gate for recording.
//!
//! Recording targets (Pi-class devices) have no RTC: after an unclean power
//! loss the restored clock can be minutes off until NTP corrects it, and the
//! writer names segments (`YYYY-MM-DD/HH/MMSS.h264`) and stamps the index in
//! local wall-clock time. Starting to record before the clock is
//! synchronized therefore produces misfiled segments and wrong RecordInfo
//! answers. The recording task waits (bounded) for sync first; if sync never
//! arrives it records anyway — losing footage is worse than skewed stamps.

/// Default systemd-timesyncd marker (systemd ≥ 246).
#[must_use]
pub fn systemd_clock_synced() -> bool {
    std::path::Path::new("/run/systemd/timesync/synchronized").exists()
}

/// Wait until `is_synced` returns true, polling every `poll` up to `max_wait`.
///
/// Returns true when synchronized, false when `max_wait` elapsed without
/// sync (caller decides to proceed anyway).
pub async fn wait_for_clock_sync<F>(
    mut is_synced: F,
    poll: std::time::Duration,
    max_wait: std::time::Duration,
) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + max_wait;
    loop {
        if is_synced() {
            return true;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return false;
        }
        tokio::time::sleep(poll.min(deadline - now)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Already synchronized: return immediately without sleeping.
    #[tokio::test(start_paused = true)]
    async fn returns_immediately_when_already_synced() {
        let began = tokio::time::Instant::now();
        let synced = wait_for_clock_sync(
            || true,
            std::time::Duration::from_secs(2),
            std::time::Duration::from_secs(120),
        )
        .await;
        assert!(synced);
        assert_eq!(began.elapsed(), std::time::Duration::ZERO);
    }

    /// Never synchronizes: give up exactly at the deadline and report false.
    #[tokio::test(start_paused = true)]
    async fn gives_up_at_deadline_when_never_synced() {
        let began = tokio::time::Instant::now();
        let synced = wait_for_clock_sync(
            || false,
            std::time::Duration::from_secs(2),
            std::time::Duration::from_secs(10),
        )
        .await;
        assert!(!synced);
        assert_eq!(began.elapsed(), std::time::Duration::from_secs(10));
    }

    /// Sync arrives mid-wait: stop polling as soon as it does.
    #[tokio::test(start_paused = true)]
    async fn stops_polling_once_synced() {
        let calls = std::sync::atomic::AtomicU32::new(0);
        let began = tokio::time::Instant::now();
        let synced = wait_for_clock_sync(
            || {
                let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                n >= 3 // syncs on the 4th poll
            },
            std::time::Duration::from_secs(2),
            std::time::Duration::from_secs(120),
        )
        .await;
        assert!(synced);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 4);
        assert_eq!(began.elapsed(), std::time::Duration::from_secs(6));
    }
}
