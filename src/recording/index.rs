//! Recording index: append-only `index.jsonl` of segment metadata.
//!
//! Each segment is one JSON line:
//! `{"file":"...","start_ms":...,"end_ms":...,"size":...,"frames":N,"keyframes":N}`.
//! Queries read the file and filter in memory (a day of segments is well under
//! 1 MB on the Pi). Retention deletes rewrite the file synchronously.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// Metadata for a single recorded segment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SegmentInfo {
    /// Path to the `.h264` segment file, relative to the recording root.
    pub file: String,
    /// Wall-clock start time in milliseconds since the Unix epoch.
    pub start_ms: u64,
    /// Wall-clock end time in milliseconds since the Unix epoch.
    pub end_ms: u64,
    /// Size of the segment file in bytes.
    pub size: u64,
    /// Total number of frames (access units) in the segment.
    pub frames: u64,
    /// Number of key frames (IDR) in the segment.
    pub keyframes: u64,
}

impl SegmentInfo {
    /// True if this segment overlaps the inclusive `[start_ms, end_ms]` range.
    #[must_use]
    pub fn overlaps(&self, start_ms: u64, end_ms: u64) -> bool {
        self.start_ms <= end_ms && self.end_ms >= start_ms
    }
}

/// In-memory view of the recording index.
///
/// Loaded from `index.jsonl` on demand; `append` and `remove` mutate the
/// in-memory state and persist to disk. Corrupt lines are skipped on load.
#[derive(Debug, Default)]
pub struct RecordingIndex {
    segments: Vec<SegmentInfo>,
}

impl RecordingIndex {
    /// Load the index from `path` (the `index.jsonl` file).
    ///
    /// A missing file yields an empty index. Corrupt or unparseable lines are
    /// skipped. I/O errors other than "not found" are logged and yield an
    /// empty index rather than failing the caller.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        let mut segments = Vec::new();
        match fs::File::open(path) {
            Ok(file) => {
                let reader = BufReader::new(file);
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<SegmentInfo>(trimmed) {
                        Ok(info) => segments.push(info),
                        Err(_) => {
                            eprintln!("recording: skipping corrupt index line: {trimmed}");
                        }
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                eprintln!("recording: failed to read index {}: {e}", path.display());
            }
        }
        Self { segments }
    }

    /// Append a segment to the in-memory index and persist it to `path`.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the index file cannot be opened for appending.
    pub fn append(&mut self, path: &Path, info: &SegmentInfo) -> std::io::Result<()> {
        self.segments.push(info.clone());
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let line = serde_json::to_string(info).unwrap_or_default();
        writeln!(file, "{line}")?;
        Ok(())
    }

    /// Remove a segment (by its `file` path) from the index and rewrite the
    /// index file synchronously. Returns true if the segment was present.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the index file cannot be rewritten.
    pub fn remove(&mut self, path: &Path, file: &str) -> std::io::Result<bool> {
        let before = self.segments.len();
        self.segments.retain(|s| s.file != file);
        let removed = self.segments.len() != before;
        if removed {
            self.rewrite(path)?;
        }
        Ok(removed)
    }

    /// Rewrite the whole index file from the in-memory state.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the file cannot be written.
    fn rewrite(&self, path: &Path) -> std::io::Result<()> {
        let mut file = fs::File::create(path)?;
        for info in &self.segments {
            let line = serde_json::to_string(info).unwrap_or_default();
            writeln!(file, "{line}")?;
        }
        file.sync_all()?;
        Ok(())
    }

    /// Return all segments overlapping the inclusive `[start_ms, end_ms]` range.
    #[must_use]
    pub fn lookup(&self, start_ms: u64, end_ms: u64) -> Vec<SegmentInfo> {
        self.segments
            .iter()
            .filter(|s| s.overlaps(start_ms, end_ms))
            .cloned()
            .collect()
    }

    /// Return all segments, sorted by start time ascending.
    #[must_use]
    pub fn all(&self) -> Vec<SegmentInfo> {
        let mut all = self.segments.clone();
        all.sort_by_key(|s| s.start_ms);
        all
    }

    /// Number of segments currently tracked.
    #[must_use]
    pub fn len(&self) -> usize {
        self.segments.len()
    }

    /// True if the index tracks no segments.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }
}

/// Resolve the index file path for a recording root.
#[must_use]
pub fn index_path(root: &Path) -> PathBuf {
    root.join("index.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// Create a unique temp directory for a test (process id + counter).
    fn temp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("mibee_rec_index_{}_{}", std::process::id(), n));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    fn seg(file: &str, start_ms: u64, end_ms: u64) -> SegmentInfo {
        SegmentInfo {
            file: file.to_string(),
            start_ms,
            end_ms,
            size: 100,
            frames: 10,
            keyframes: 1,
        }
    }

    #[test]
    fn test_load_missing_file_is_empty() {
        let dir = temp_dir();
        let idx = RecordingIndex::load(&index_path(&dir));
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn test_append_then_lookup_hit() {
        let dir = temp_dir();
        let path = index_path(&dir);
        let mut idx = RecordingIndex::load(&path);
        let info = seg("2026-08-15/10/0000.h264", 1000, 2000);
        idx.append(&path, &info).unwrap();

        let hits = idx.lookup(1500, 2500);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].file, "2026-08-15/10/0000.h264");
    }

    #[test]
    fn test_lookup_overlap_inclusive() {
        let dir = temp_dir();
        let path = index_path(&dir);
        let mut idx = RecordingIndex::load(&path);
        idx.append(&path, &seg("a.h264", 1000, 2000)).unwrap();
        idx.append(&path, &seg("b.h264", 3000, 4000)).unwrap();

        // Exact boundary touch counts as overlap (inclusive).
        assert_eq!(idx.lookup(2000, 2000).len(), 1);
        assert_eq!(idx.lookup(2000, 3000).len(), 2);
        // Range fully inside a segment.
        assert_eq!(idx.lookup(1200, 1800).len(), 1);
        // Range spanning both.
        assert_eq!(idx.lookup(1500, 3500).len(), 2);
    }

    #[test]
    fn test_lookup_empty_range() {
        let dir = temp_dir();
        let path = index_path(&dir);
        let mut idx = RecordingIndex::load(&path);
        idx.append(&path, &seg("a.h264", 1000, 2000)).unwrap();
        // Range before all segments.
        assert!(idx.lookup(0, 500).is_empty());
        // Range after all segments.
        assert!(idx.lookup(5000, 6000).is_empty());
    }

    #[test]
    fn test_remove_and_index_sync() {
        let dir = temp_dir();
        let path = index_path(&dir);
        let mut idx = RecordingIndex::load(&path);
        idx.append(&path, &seg("a.h264", 1000, 2000)).unwrap();
        idx.append(&path, &seg("b.h264", 3000, 4000)).unwrap();

        assert!(idx.remove(&path, "a.h264").unwrap());
        assert_eq!(idx.len(), 1);
        assert!(idx.lookup(1500, 2500).is_empty());

        // Reload from disk: the rewrite must have persisted.
        let reloaded = RecordingIndex::load(&path);
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded.all()[0].file, "b.h264");
    }

    #[test]
    fn test_remove_missing_returns_false() {
        let dir = temp_dir();
        let path = index_path(&dir);
        let mut idx = RecordingIndex::load(&path);
        idx.append(&path, &seg("a.h264", 1000, 2000)).unwrap();
        assert!(!idx.remove(&path, "nope.h264").unwrap());
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn test_load_skips_corrupt_lines() {
        let dir = temp_dir();
        let path = index_path(&dir);
        let mut idx = RecordingIndex::load(&path);
        idx.append(&path, &seg("a.h264", 1000, 2000)).unwrap();

        // Append a corrupt line directly to the file.
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "this is not json").unwrap();
        drop(f);

        let reloaded = RecordingIndex::load(&path);
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded.all()[0].file, "a.h264");
    }
}
