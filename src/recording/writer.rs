//! Recording writer: subscribes to the AuHub and writes Annex-B H.264
//! segments to disk, maintaining the recording index and per-frame sidecars.
//!
//! Disk layout is identical to the Go repo (binding for cross-repo behavioral
//! parity): `storage_path/YYYY-MM-DD/HH/MMSS.h264` plus `index.jsonl` and a
//! per-segment `<segment>.ts.jsonl` sidecar, with civil names in
//! device-local wall-clock time (matching the Go repo's `time.Local`
//! segment naming; epoch-ms values in the index stay UTC). Files are
//! written directly via `std::fs` (not through the `StorageBackend`
//! trait, which would buffer whole segments in RAM and use `.m4v` naming).

use crate::config::RecordingConfig;
use crate::gb28181::RECORD_ACTIVE;
use crate::h264::hub::{AccessUnit, AuHub};
use crate::h264::parser::Nalu;
use crate::recording::index::{index_path, RecordingIndex, SegmentInfo};

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Annex-B start code prefix prepended to every NALU payload.
const START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

/// An in-progress recording segment.
struct Segment {
    /// Wall-clock time when the segment started (first IDR).
    start: SystemTime,
    /// Wall-clock start in ms since the Unix epoch.
    start_ms: u64,
    /// Full path to the `.h264` file being written.
    path: PathBuf,
    /// Open file handle for the segment.
    file: File,
    /// Total frames (access units) written.
    frames: u64,
    /// Key frames (IDR) written.
    keyframes: u64,
    /// Bytes written so far.
    size: u64,
    /// Per-frame wall-clock-relative ms offsets from segment start.
    pts_ms: Vec<u64>,
}

impl Segment {
    /// Open a new segment starting at `start` (wall-clock) for the given AU.
    fn open(root: &Path, start: SystemTime, start_ms: u64) -> std::io::Result<Self> {
        let (date, hour, minute, second) = civil_parts(start_ms);
        let dir = root.join(format!("{date}/{hour}"));
        fs::create_dir_all(&dir)?;
        let name = format!("{minute}{second}.h264");
        let path = dir.join(&name);
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)?;
        Ok(Segment {
            start,
            start_ms,
            path,
            file,
            frames: 0,
            keyframes: 0,
            size: 0,
            pts_ms: Vec::new(),
        })
    }

    /// Append an access unit's NALUs (Annex-B) and record its frame offset.
    fn append_au(&mut self, au: &AccessUnit, now_ms: u64) -> std::io::Result<()> {
        for nalu in &au.nalus {
            self.write_nalu(nalu)?;
        }
        self.frames += 1;
        if au.is_key_frame {
            self.keyframes += 1;
        }
        self.pts_ms.push(now_ms.saturating_sub(self.start_ms));
        Ok(())
    }

    /// Write a single NALU as Annex-B (start code + payload).
    fn write_nalu(&mut self, nalu: &Nalu) -> std::io::Result<()> {
        self.file.write_all(&START_CODE)?;
        self.file.write_all(&nalu.data)?;
        self.size += (START_CODE.len() + nalu.data.len()) as u64;
        Ok(())
    }

    /// Close the segment: fsync the data file, write the per-frame sidecar,
    /// and append the index entry.
    fn close(self, root: &Path, end_ms: u64, index: &mut RecordingIndex) -> std::io::Result<()> {
        self.file.sync_all()?;
        drop(self.file);

        // Per-frame sidecar: one `{"pts_ms":N}` line per frame.
        let sidecar = sidecar_path(&self.path);
        let mut ts = File::create(&sidecar)?;
        for pts in &self.pts_ms {
            writeln!(ts, "{{\"pts_ms\":{pts}}}")?;
        }
        ts.sync_all()?;

        let rel = self
            .path
            .strip_prefix(root)
            .unwrap_or(&self.path)
            .to_string_lossy()
            .replace('\\', "/");
        let info = SegmentInfo {
            file: rel,
            start_ms: self.start_ms,
            end_ms,
            size: self.size,
            frames: self.frames,
            keyframes: self.keyframes,
        };
        index.append(&index_path(root), &info)
    }
}

/// Run the recording writer task until the channel closes or a fatal error
/// occurs. Sets `RECORD_ACTIVE` true while running and false on exit.
///
/// # Errors
///
/// Returns an error only if the recording root cannot be created. Runtime
/// write failures (e.g. disk full) are logged and stop recording cleanly
/// without panicking or killing the process.
pub async fn run(hub: Arc<AuHub>, config: RecordingConfig) -> anyhow::Result<()> {
    run_inner(hub, config, None).await
}

/// Like [`run`], but stops when `shutdown` fires (used by tests).
async fn run_inner(
    hub: Arc<AuHub>,
    config: RecordingConfig,
    mut shutdown: Option<tokio::sync::oneshot::Receiver<()>>,
) -> anyhow::Result<()> {
    let root = PathBuf::from(&config.storage_path);
    fs::create_dir_all(&root).map_err(|e| {
        anyhow::anyhow!(
            "recording: failed to create storage root {}: {e}",
            root.display()
        )
    })?;

    // Subscribe to the AuHub (sync receiver) and bridge to async, mirroring
    // the gb28181 server's spawn_blocking + mpsc pattern.
    let subscriber = hub.subscribe_with_capacity(64);
    let sub_id = subscriber.id;
    let sync_rx = subscriber.receiver;
    let (async_tx, mut async_rx) = tokio::sync::mpsc::channel::<AccessUnit>(64);
    tokio::task::spawn_blocking(move || {
        while let Ok(au) = sync_rx.recv() {
            if async_tx.blocking_send(au).is_err() {
                break;
            }
        }
    });

    RECORD_ACTIVE.store(true, Ordering::SeqCst);
    println!("recording: started, root={}", root.display());

    let mut index = RecordingIndex::load(&index_path(&root));
    // Seed the app-wide storage footprint from the existing index so the
    // metrics summary reports the real footprint immediately after start.
    for seg in index.all() {
        crate::recording::add_recorded_bytes(seg.size);
    }
    let mut segment: Option<Segment> = None;
    let segment_secs = config.segment_secs;

    let mut stopping = false;
    loop {
        let au = if stopping {
            // Shutdown requested: drain any remaining AUs, then stop when
            // the channel closes (the bridge drops the sender after the hub
            // unsubscribes us).
            async_rx.recv().await
        } else {
            tokio::select! {
                au = async_rx.recv() => au,
                _ = async {
                    if let Some(rx) = shutdown.as_mut() {
                        let _ = rx.await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    // Shutdown fired: unsubscribe so the bridge's blocking
                    // `recv()` returns `Disconnected` and it stops delivering.
                    // We then drain the AUs already buffered in `async_rx`
                    // before stopping, so nothing written is lost.
                    hub.unsubscribe(sub_id);
                    stopping = true;
                    async_rx.recv().await
                }
            }
        };

        let Some(au) = au else { break };

        let now = SystemTime::now();
        let now_ms = epoch_ms(now);

        if au.is_key_frame {
            let should_roll = match &segment {
                Some(seg) => {
                    let elapsed = now.duration_since(seg.start).unwrap_or_default().as_secs();
                    elapsed >= segment_secs
                }
                None => false,
            };
            if should_roll {
                if let Some(seg) = segment.take() {
                    let size = seg.size;
                    if let Err(e) = seg.close(&root, now_ms, &mut index) {
                        eprintln!("recording: failed to close segment: {e}");
                        RECORD_ACTIVE.store(false, Ordering::SeqCst);
                        return Ok(());
                    }
                    crate::recording::add_recorded_bytes(size);
                }
            }

            // Start a new segment if none is open (first IDR or after roll).
            if segment.is_none() {
                match Segment::open(&root, now, now_ms) {
                    Ok(seg) => segment = Some(seg),
                    Err(e) => {
                        eprintln!("recording: failed to open segment: {e}");
                        RECORD_ACTIVE.store(false, Ordering::SeqCst);
                        return Ok(());
                    }
                }
            }
        }

        // Append the AU to the open segment (if any). Non-IDR AUs before the
        // first IDR are dropped — segments must start on a key frame.
        if let Some(seg) = segment.as_mut() {
            if let Err(e) = seg.append_au(&au, now_ms) {
                eprintln!("recording: write failed (disk full?): {e}");
                RECORD_ACTIVE.store(false, Ordering::SeqCst);
                return Ok(());
            }
        }
    }

    // Channel closed or shutdown: close any open segment.
    if let Some(seg) = segment.take() {
        let now_ms = epoch_ms(SystemTime::now());
        let size = seg.size;
        if let Err(e) = seg.close(&root, now_ms, &mut index) {
            eprintln!("recording: failed to close final segment: {e}");
        } else {
            crate::recording::add_recorded_bytes(size);
        }
    }

    // Unsubscribe so the bridge task's blocking `recv()` returns
    // `Disconnected` and the blocking thread can exit (otherwise the
    // runtime drop would wait on it forever).
    hub.unsubscribe(sub_id);
    RECORD_ACTIVE.store(false, Ordering::SeqCst);
    println!("recording: stopped");
    Ok(())
}

/// Path to the per-frame sidecar for a segment file.
#[must_use]
pub fn sidecar_path(segment_path: &Path) -> PathBuf {
    let mut os = segment_path.as_os_str().to_owned();
    os.push(".ts.jsonl");
    PathBuf::from(os)
}

/// Milliseconds since the Unix epoch for a `SystemTime`.
#[must_use]
pub fn epoch_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

/// Split an epoch-ms timestamp into device-local `(YYYY-MM-DD, HH, MM, SS)`
/// civil parts. Segment naming matches the Go repo, which names files with
/// `time.Local`.
#[must_use]
pub fn civil_parts(ms: u64) -> (String, String, String, String) {
    civil_parts_with_offset(ms, crate::gb28181::manscdp::device_local_offset_secs())
}

/// Like [`civil_parts`], but with an explicit UTC offset in seconds so
/// unit tests stay deterministic on any machine timezone.
#[must_use]
pub fn civil_parts_with_offset(ms: u64, offset_secs: i64) -> (String, String, String, String) {
    let secs = (ms / 1000) as i64 + offset_secs;
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    // Civil date from days since epoch (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    let hour = rem / 3600;
    let minute = (rem % 3600) / 60;
    let second = rem % 60;
    (
        format!("{year:04}-{month:02}-{day:02}"),
        format!("{hour:02}"),
        format!("{minute:02}"),
        format!("{second:02}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::parser::Nalu;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::time::Instant;

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, AtomicOrdering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("mibee_rec_writer_{}_{}", std::process::id(), n));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    fn nalu(nalu_type: u8, data: Vec<u8>) -> Nalu {
        Nalu {
            nalu_type,
            data,
            is_idr: nalu_type == 5,
            is_sps: nalu_type == 7,
            is_pps: nalu_type == 8,
            is_aud: nalu_type == 9,
        }
    }

    fn au(is_key: bool, nalus: Vec<Nalu>) -> AccessUnit {
        AccessUnit {
            nalus,
            timestamp: Instant::now(),
            is_key_frame: is_key,
        }
    }

    /// Block until the writer has registered a subscriber on the hub, so
    /// writes issued afterwards are not dropped (the hub only fans out to
    /// registered subscribers).
    fn wait_for_subscriber(hub: &Arc<AuHub>) {
        for _ in 0..1000 {
            if hub.subscriber_count() >= 1 {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("writer never subscribed to the hub");
    }

    #[test]
    fn test_civil_parts_utc() {
        // 1_786_791_180 s = 2026-08-15T10:53:00Z (verified via date -d).
        let (date, hour, minute, second) = civil_parts_with_offset(1_786_791_180_000, 0);
        assert_eq!(date, "2026-08-15");
        assert_eq!(hour, "10");
        assert_eq!(minute, "53");
        assert_eq!(second, "00");
    }

    #[test]
    fn test_civil_parts_positive_offset() {
        // Same instant under +08:00 → 18:53 local (CST segment naming,
        // matching the Go repo's time.Local layout).
        let (date, hour, minute, second) = civil_parts_with_offset(1_786_791_180_000, 8 * 3600);
        assert_eq!(date, "2026-08-15");
        assert_eq!(hour, "18");
        assert_eq!(minute, "53");
        assert_eq!(second, "00");
    }

    #[test]
    fn test_civil_parts_offset_day_rollover() {
        // 1_786_836_600 s = 2026-08-15T23:30:00Z; under +08:00 the civil
        // date rolls over to the next day: 2026-08-16 07:30.
        let (date, hour, minute, second) = civil_parts_with_offset(1_786_836_600_000, 8 * 3600);
        assert_eq!(date, "2026-08-16");
        assert_eq!(hour, "07");
        assert_eq!(minute, "30");
        assert_eq!(second, "00");
    }

    #[test]
    fn test_civil_parts_negative_offset() {
        // Same base instant under -05:00 → 05:53 same day.
        let (date, hour, minute, _) = civil_parts_with_offset(1_786_791_180_000, -5 * 3600);
        assert_eq!(date, "2026-08-15");
        assert_eq!(hour, "05");
        assert_eq!(minute, "53");
    }

    #[test]
    fn test_segment_rolls_on_keyframe_at_boundary() {
        // Build a synthetic AU stream: keyframe, then non-key frames, then a
        // keyframe after the segment boundary. Verify the writer produces two
        // segments, each starting with an IDR.
        let dir = temp_dir();
        let root = dir.join("rec");
        fs::create_dir_all(&root).unwrap();

        let hub = Arc::new(AuHub::new());

        // Use a tiny segment_secs so the boundary is crossed quickly.
        let config = RecordingConfig {
            enabled: true,
            storage_path: root.to_string_lossy().to_string(),
            segment_secs: 1,
            retention_days: 3,
            max_storage_mb: 8192,
        };

        // Spawn the writer with a shutdown signal.
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let hub_w = Arc::clone(&hub);
        let cfg = config.clone();
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(run_inner(hub_w, cfg, Some(shutdown_rx)))
        });

        // Wait until the writer has subscribed so the writes below are not
        // dropped (the hub only fans out to registered subscribers).
        wait_for_subscriber(&hub);

        // Feed: IDR (segment 1 start), 2 non-key, sleep past boundary, IDR (roll).
        hub.write(au(true, vec![nalu(5, vec![0x65, 0x88])]));
        hub.write(au(false, vec![nalu(1, vec![0x61, 0x88])]));
        hub.write(au(false, vec![nalu(1, vec![0x61, 0x89])]));
        std::thread::sleep(std::time::Duration::from_millis(2000));
        hub.write(au(true, vec![nalu(5, vec![0x65, 0x99])]));
        // A trailing non-key frame for segment 2.
        hub.write(au(false, vec![nalu(1, vec![0x61, 0xaa])]));

        // Signal shutdown to stop the writer cleanly.
        let _ = shutdown_tx.send(());
        let result = handle.join().unwrap();
        assert!(result.is_ok());

        // Verify two segments exist, each starting with an IDR.
        let index = RecordingIndex::load(&index_path(&root));
        let all = index.all();
        assert_eq!(all.len(), 2, "expected 2 segments, got {}", all.len());

        // Segment 1: IDR + 2 non-key = 3 frames, 1 keyframe.
        assert_eq!(all[0].frames, 3);
        assert_eq!(all[0].keyframes, 1);
        // Segment 2: IDR + 1 non-key = 2 frames, 1 keyframe.
        assert_eq!(all[1].frames, 2);
        assert_eq!(all[1].keyframes, 1);

        // Each segment file must start with an Annex-B start code + IDR NALU.
        for info in &all {
            let data = fs::read(root.join(&info.file)).unwrap();
            assert_eq!(&data[0..4], &START_CODE);
            assert_eq!(data[4] & 0x1F, 5, "segment must start with IDR");
        }

        // Sidecar files exist with one line per frame.
        for info in &all {
            let sidecar = sidecar_path(&root.join(&info.file));
            let content = fs::read_to_string(&sidecar).unwrap();
            let lines: Vec<&str> = content.lines().collect();
            assert_eq!(lines.len() as u64, info.frames);
        }
    }

    #[test]
    fn test_drops_non_idr_before_first_keyframe() {
        let dir = temp_dir();
        let root = dir.join("rec");
        fs::create_dir_all(&root).unwrap();

        let hub = Arc::new(AuHub::new());
        let config = RecordingConfig {
            enabled: true,
            storage_path: root.to_string_lossy().to_string(),
            segment_secs: 600,
            retention_days: 3,
            max_storage_mb: 8192,
        };

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let hub_w = Arc::clone(&hub);
        let cfg = config.clone();
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(run_inner(hub_w, cfg, Some(shutdown_rx)))
        });

        // Wait until the writer has subscribed so the writes below are not
        // dropped (the hub only fans out to registered subscribers).
        wait_for_subscriber(&hub);

        // Non-IDR frames first — must be dropped (no segment yet).
        hub.write(au(false, vec![nalu(1, vec![0x61, 0x88])]));
        hub.write(au(false, vec![nalu(1, vec![0x61, 0x89])]));
        // Then an IDR — starts the segment.
        hub.write(au(true, vec![nalu(5, vec![0x65, 0x88])]));

        // Signal shutdown to stop the writer cleanly.
        let _ = shutdown_tx.send(());
        let result = handle.join().unwrap();
        assert!(result.is_ok());

        let index = RecordingIndex::load(&index_path(&root));
        let all = index.all();
        assert_eq!(all.len(), 1);
        // Only the IDR frame was recorded (non-IDR before it dropped).
        assert_eq!(all[0].frames, 1);
        assert_eq!(all[0].keyframes, 1);
    }
}
