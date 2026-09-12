/// Hardware capability detection for Raspberry Pi.
///
/// Reads system files (`/proc/cpuinfo`, `/proc/device-tree/model`,
/// `/proc/meminfo`) to detect the RPi model and infer hardware capabilities
/// such as H.264/H.265 encoder support and available memory for AI feature
/// gating.
///
/// The `ProcReader` trait allows mocking these file reads in tests.
use std::fmt;
use std::fs;
use std::io;

/// Raspberry Pi model identification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpiModel {
    Pi3B,
    Pi4,
    Pi5,
    Unknown(String),
}

impl fmt::Display for RpiModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RpiModel::Pi3B => write!(f, "Raspberry Pi 3 Model B"),
            RpiModel::Pi4 => write!(f, "Raspberry Pi 4 Model B"),
            RpiModel::Pi5 => write!(f, "Raspberry Pi 5"),
            RpiModel::Unknown(s) => write!(f, "Unknown Raspberry Pi model ({})", s),
        }
    }
}

/// Hardware capabilities detected from the system.
///
/// All encoder and AI capability flags are inferred from the detected model
/// and available memory — no V4L2 ioctl queries are performed.
#[derive(Debug, Clone, PartialEq)]
pub struct HardwareCapability {
    pub model: RpiModel,
    pub has_h264_encoder: bool,
    pub has_h265_encoder: bool,
    pub memory_mb: u64,
    pub ai_lite_capable: bool,
    pub ai_full_capable: bool,
}

/// Abstraction for reading system files, mockable in tests.
pub trait ProcReader: Send + Sync {
    fn read_cpuinfo(&self) -> io::Result<String>;
    fn read_device_tree_model(&self) -> io::Result<String>;
    fn read_meminfo(&self) -> io::Result<String>;
}

/// Production implementation that reads from actual `/proc` filesystem.
pub struct RealProcReader;

impl ProcReader for RealProcReader {
    fn read_cpuinfo(&self) -> io::Result<String> {
        fs::read_to_string("/proc/cpuinfo")
    }

    fn read_device_tree_model(&self) -> io::Result<String> {
        // Device-tree model files often have a trailing NUL byte.
        let bytes = fs::read("/proc/device-tree/model")?;
        let trimmed: Vec<u8> = bytes.iter().take_while(|&&b| b != 0).copied().collect();
        Ok(String::from_utf8_lossy(&trimmed).trim().to_string())
    }

    fn read_meminfo(&self) -> io::Result<String> {
        fs::read_to_string("/proc/meminfo")
    }
}

/// Mock implementation of `ProcReader` for unit tests, using canned strings.
pub struct MockProcReader {
    pub cpuinfo: String,
    pub device_tree_model: String,
    pub meminfo: String,
}

impl ProcReader for MockProcReader {
    fn read_cpuinfo(&self) -> io::Result<String> {
        Ok(self.cpuinfo.clone())
    }

    fn read_device_tree_model(&self) -> io::Result<String> {
        Ok(self.device_tree_model.clone())
    }

    fn read_meminfo(&self) -> io::Result<String> {
        Ok(self.meminfo.clone())
    }
}

impl HardwareCapability {
    /// Detect hardware capabilities by reading system files.
    ///
    /// Uses the real filesystem (`RealProcReader`). In tests, use
    /// `detect_with_reader` instead.
    pub fn detect() -> Self {
        Self::detect_with_reader(&RealProcReader)
    }

    /// Detect capabilities using a custom `ProcReader` (for testing).
    pub fn detect_with_reader(reader: &dyn ProcReader) -> Self {
        let model = detect_model(reader);
        let has_h264_encoder = model_has_h264(&model);
        let has_h265_encoder = model_has_h265(&model);
        let memory_mb = parse_memory_mb(&reader.read_meminfo().unwrap_or_default());
        // Known RPi boards ship with >= 1 GB physical RAM; MemTotal under-
        // reports because the GPU carve-out is excluded (Pi 3B: ~905 MB of
        // 1024 MB). The design doc gates on physical RAM, so a known model
        // passes the lite tier regardless of MemTotal.
        let ai_lite_capable =
            memory_mb >= 1024 || matches!(model, RpiModel::Pi3B | RpiModel::Pi4 | RpiModel::Pi5);
        let ai_full_capable = memory_mb >= 2048;

        HardwareCapability {
            model,
            has_h264_encoder,
            has_h265_encoder,
            memory_mb,
            ai_lite_capable,
            ai_full_capable,
        }
    }
}

// ---------------------------------------------------------------------------
// Detection helpers
// ---------------------------------------------------------------------------

/// Determine the RPi model by first trying `/proc/device-tree/model` (more
/// descriptive), then falling back to the `Revision` field in `/proc/cpuinfo`.
fn detect_model(reader: &dyn ProcReader) -> RpiModel {
    // Primary: /proc/device-tree/model
    if let Ok(model_str) = reader.read_device_tree_model() {
        let model_str = model_str.trim();
        if !model_str.is_empty() {
            if model_str.contains("3 Model B") || model_str.contains("Pi 3") {
                return RpiModel::Pi3B;
            }
            if model_str.contains("4 Model B") || model_str.contains("Pi 4") {
                return RpiModel::Pi4;
            }
            if model_str.contains("Pi 5") {
                return RpiModel::Pi5;
            }
        }
    }

    // Fallback: parse cpuinfo revision
    if let Ok(cpuinfo) = reader.read_cpuinfo() {
        if let Some(rev) = parse_revision(&cpuinfo) {
            return match_revision(&rev);
        }
    }

    RpiModel::Unknown("could not determine model".to_string())
}

/// Extract the `Revision` value from `/proc/cpuinfo`.
fn parse_revision(cpuinfo: &str) -> Option<String> {
    for line in cpuinfo.lines() {
        let line = line.trim();
        // Split on first ':' — handles both tab and space separators
        if let Some(pos) = line.find(':') {
            let key = line[..pos].trim();
            if key == "Revision" {
                let value = line[pos + 1..].trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

/// Map a revision string to a known `RpiModel`.
///
/// Revision codes from Raspberry Pi documentation:
/// <https://www.raspberrypi.com/documentation/computers/raspberry-pi.html>
fn match_revision(rev: &str) -> RpiModel {
    let rev_lower = rev.trim().to_lowercase();
    let rev_stripped = rev_lower.strip_prefix("0x").unwrap_or(&rev_lower);

    match rev_stripped {
        // Pi 3B
        "a02082" | "a22082" | "a32082" => RpiModel::Pi3B,
        // Pi 4B (all sub-revisions)
        "a03111" | "b03111" | "c03111" | "b03112" | "d03114" => RpiModel::Pi4,
        // Pi 5 (exact)
        "902120" => RpiModel::Pi5,
        _ => {
            // Pi 5 wildcard: 0xa?2120 (any hex digit for ?)
            if rev_stripped.len() == 6
                && rev_stripped.starts_with('a')
                && rev_stripped.ends_with("2120")
            {
                return RpiModel::Pi5;
            }
            RpiModel::Unknown(rev.to_string())
        }
    }
}

/// Parse the `MemTotal` line from `/proc/meminfo` and return megabytes.
fn parse_memory_mb(meminfo: &str) -> u64 {
    for line in meminfo.lines() {
        let line = line.trim();
        if line.starts_with("MemTotal:") {
            // Extract contiguous digits
            let digits: String = line.chars().filter(|c| c.is_ascii_digit()).collect();
            if let Ok(kb) = digits.parse::<u64>() {
                return kb / 1024;
            }
        }
    }
    0
}

/// Infer H.264 encoder presence from model.
fn model_has_h264(model: &RpiModel) -> bool {
    matches!(model, RpiModel::Pi3B | RpiModel::Pi4 | RpiModel::Pi5)
}

/// Infer H.265 encoder presence from model (Pi4+ have it, Pi3B does not).
fn model_has_h265(model: &RpiModel) -> bool {
    matches!(model, RpiModel::Pi4 | RpiModel::Pi5)
}

// ---------------------------------------------------------------------------
// Capability Gate
// ---------------------------------------------------------------------------

/// Feature gate that uses detected hardware capabilities to decide whether a
/// feature should be enabled.
pub struct CapabilityGate {
    capability: HardwareCapability,
}

impl CapabilityGate {
    pub fn new(capability: HardwareCapability) -> Self {
        CapabilityGate { capability }
    }

    /// Returns `true` if the named feature should be enabled based on detected
    /// hardware capabilities.
    ///
    /// Feature rules:
    /// - `"h264"` — always true (all supported models have H.264 encoder)
    /// - `"h265"` — only on Pi4/5 (NOT Pi3B)
    /// - `"ai"` — when memory >= 1 GB, or the model is known (all known
    ///   boards ship with >= 1 GB physical RAM)
    /// - `"ai_full"` — when memory >= 2 GB (full AI features)
    /// - `"multi_camera"` — always true (CPU-bounded, no hw requirement)
    /// - `"webrtc"` — always true (no hardware requirement)
    pub fn enable_feature(&self, name: &str) -> bool {
        match name {
            "h264" => self.capability.has_h264_encoder,
            "h265" => self.capability.has_h265_encoder,
            "ai" => self.capability.ai_lite_capable,
            "ai_full" => self.capability.ai_full_capable,
            "multi_camera" => true,
            "webrtc" => true,
            _ => false,
        }
    }

    /// Returns a reference to the underlying capability.
    pub fn capability(&self) -> &HardwareCapability {
        &self.capability
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Helpers -----------------------------------------------------------

    fn pi3b_cpuinfo() -> String {
        "Hardware\t: BCM2835\nRevision\t: a02082\n".to_string()
    }

    fn pi3b_device_tree() -> String {
        "Raspberry Pi 3 Model B Rev 1.2\0".to_string()
    }

    fn pi4_cpuinfo() -> String {
        "Hardware\t: BCM2835\nRevision\t: c03111\n".to_string()
    }

    fn pi5_cpuinfo() -> String {
        "Hardware\t: BCM2835\nRevision\t: 902120\n".to_string()
    }

    fn pi5_cpuinfo_variant() -> String {
        "Hardware\t: BCM2835\nRevision\t: a02120\n".to_string()
    }

    fn meminfo_1024mb() -> String {
        "MemTotal:        1048576 kB\nMemFree:          512000 kB\nMemAvailable:    756000 kB\n"
            .to_string()
    }

    fn meminfo_4096mb() -> String {
        "MemTotal:        4194304 kB\nMemFree:         2048000 kB\nMemAvailable:    3780000 kB\n"
            .to_string()
    }

    fn meminfo_512mb() -> String {
        "MemTotal:         524288 kB\nMemFree:          256000 kB\n".to_string()
    }

    // -- Model detection ---------------------------------------------------

    #[test]
    fn detect_pi3b() {
        let reader = MockProcReader {
            cpuinfo: pi3b_cpuinfo(),
            device_tree_model: String::new(),
            meminfo: meminfo_1024mb(),
        };
        let cap = HardwareCapability::detect_with_reader(&reader);
        assert_eq!(cap.model, RpiModel::Pi3B);
    }

    #[test]
    fn detect_pi3b_from_device_tree() {
        let reader = MockProcReader {
            cpuinfo: String::new(),
            device_tree_model: pi3b_device_tree(),
            meminfo: meminfo_1024mb(),
        };
        let cap = HardwareCapability::detect_with_reader(&reader);
        assert_eq!(cap.model, RpiModel::Pi3B);
    }

    #[test]
    fn detect_pi4() {
        let reader = MockProcReader {
            cpuinfo: pi4_cpuinfo(),
            device_tree_model: String::new(),
            meminfo: meminfo_4096mb(),
        };
        let cap = HardwareCapability::detect_with_reader(&reader);
        assert_eq!(cap.model, RpiModel::Pi4);
    }

    #[test]
    fn detect_pi5() {
        let reader = MockProcReader {
            cpuinfo: pi5_cpuinfo(),
            device_tree_model: String::new(),
            meminfo: meminfo_4096mb(),
        };
        let cap = HardwareCapability::detect_with_reader(&reader);
        assert_eq!(cap.model, RpiModel::Pi5);
    }

    #[test]
    fn detect_pi5_variant() {
        let reader = MockProcReader {
            cpuinfo: pi5_cpuinfo_variant(),
            device_tree_model: String::new(),
            meminfo: meminfo_4096mb(),
        };
        let cap = HardwareCapability::detect_with_reader(&reader);
        assert_eq!(cap.model, RpiModel::Pi5);
    }

    // -- Memory parsing ----------------------------------------------------

    #[test]
    fn memory_parsing_1024mb() {
        let cap = HardwareCapability::detect_with_reader(&MockProcReader {
            cpuinfo: pi3b_cpuinfo(),
            device_tree_model: String::new(),
            meminfo: meminfo_1024mb(),
        });
        assert_eq!(cap.memory_mb, 1024);
    }

    #[test]
    fn memory_parsing_4096mb() {
        let cap = HardwareCapability::detect_with_reader(&MockProcReader {
            cpuinfo: pi4_cpuinfo(),
            device_tree_model: String::new(),
            meminfo: meminfo_4096mb(),
        });
        assert_eq!(cap.memory_mb, 4096);
    }

    #[test]
    fn memory_parsing_512mb() {
        let cap = HardwareCapability::detect_with_reader(&MockProcReader {
            cpuinfo: pi3b_cpuinfo(),
            device_tree_model: String::new(),
            meminfo: meminfo_512mb(),
        });
        assert_eq!(cap.memory_mb, 512);
    }

    // -- AI capability gating ---------------------------------------------

    #[test]
    fn ai_lite_capable_at_1gb() {
        // 1 GB → ai_lite_capable = true, ai_full_capable = false
        let cap = HardwareCapability::detect_with_reader(&MockProcReader {
            cpuinfo: pi3b_cpuinfo(),
            device_tree_model: String::new(),
            meminfo: meminfo_1024mb(),
        });
        assert!(cap.ai_lite_capable);
        assert!(!cap.ai_full_capable);

        let gate = CapabilityGate::new(cap);
        assert!(gate.enable_feature("ai"));
        assert!(!gate.enable_feature("ai_full"));
    }

    #[test]
    fn ai_full_capable_at_4gb() {
        // 4 GB → both ai_lite_capable and ai_full_capable = true
        let cap = HardwareCapability::detect_with_reader(&MockProcReader {
            cpuinfo: pi4_cpuinfo(),
            device_tree_model: String::new(),
            meminfo: meminfo_4096mb(),
        });
        assert!(cap.ai_lite_capable);
        assert!(cap.ai_full_capable);

        let gate = CapabilityGate::new(cap);
        assert!(gate.enable_feature("ai"));
        assert!(gate.enable_feature("ai_full"));
    }

    #[test]
    fn ai_not_capable_at_512mb() {
        // 512 MB on an unknown board -> both tiers false.
        // (A Pi 3B is always 1 GB physical, so the memory floor is exercised
        // with an unknown model instead of the impossible Pi3B+512MB combo.)
        let cap = HardwareCapability::detect_with_reader(&MockProcReader {
            cpuinfo: String::new(),
            device_tree_model: String::new(),
            meminfo: meminfo_512mb(),
        });
        assert!(!cap.ai_lite_capable);
        assert!(!cap.ai_full_capable);
    }

    #[test]
    fn ai_lite_capable_on_pi3b_with_low_memtotal() {
        // Real Pi 3B: MemTotal (~905 MB) excludes the GPU carve-out, but the
        // board physically has 1 GB -> must still pass the lite gate.
        let cap = HardwareCapability::detect_with_reader(&MockProcReader {
            cpuinfo: pi3b_cpuinfo(),
            device_tree_model: String::new(),
            meminfo: "MemTotal:         926820 kB\nMemFree:          512000 kB\n".to_string(),
        });
        assert!(cap.ai_lite_capable);
        assert!(!cap.ai_full_capable);
    }
    // -- Encoder capability -----------------------------------------------

    #[test]
    fn h264_always_enabled() {
        let cap = HardwareCapability::detect_with_reader(&MockProcReader {
            cpuinfo: pi3b_cpuinfo(),
            device_tree_model: String::new(),
            meminfo: meminfo_1024mb(),
        });
        assert!(cap.has_h264_encoder);

        let gate = CapabilityGate::new(cap);
        assert!(gate.enable_feature("h264"));
    }

    #[test]
    fn h265_disabled_on_pi3b() {
        let cap = HardwareCapability::detect_with_reader(&MockProcReader {
            cpuinfo: pi3b_cpuinfo(),
            device_tree_model: String::new(),
            meminfo: meminfo_1024mb(),
        });
        assert!(!cap.has_h265_encoder);

        let gate = CapabilityGate::new(cap);
        assert!(!gate.enable_feature("h265"));
    }

    #[test]
    fn h265_enabled_on_pi4() {
        let cap = HardwareCapability::detect_with_reader(&MockProcReader {
            cpuinfo: pi4_cpuinfo(),
            device_tree_model: String::new(),
            meminfo: meminfo_4096mb(),
        });
        assert!(cap.has_h265_encoder);

        let gate = CapabilityGate::new(cap);
        assert!(gate.enable_feature("h265"));
    }

    #[test]
    fn h265_enabled_on_pi5() {
        let cap = HardwareCapability::detect_with_reader(&MockProcReader {
            cpuinfo: pi5_cpuinfo(),
            device_tree_model: String::new(),
            meminfo: meminfo_4096mb(),
        });
        assert!(cap.has_h265_encoder);
    }

    // -- Always-on features -----------------------------------------------

    #[test]
    fn multi_camera_always_enabled() {
        let cap = HardwareCapability::detect_with_reader(&MockProcReader {
            cpuinfo: pi3b_cpuinfo(),
            device_tree_model: String::new(),
            meminfo: meminfo_1024mb(),
        });
        let gate = CapabilityGate::new(cap);
        assert!(gate.enable_feature("multi_camera"));
    }

    #[test]
    fn webrtc_always_enabled() {
        let cap = HardwareCapability::detect_with_reader(&MockProcReader {
            cpuinfo: pi3b_cpuinfo(),
            device_tree_model: String::new(),
            meminfo: meminfo_1024mb(),
        });
        let gate = CapabilityGate::new(cap);
        assert!(gate.enable_feature("webrtc"));
    }

    // -- Unknown feature --------------------------------------------------

    #[test]
    fn unknown_feature_returns_false() {
        let cap = HardwareCapability::detect_with_reader(&MockProcReader {
            cpuinfo: pi3b_cpuinfo(),
            device_tree_model: String::new(),
            meminfo: meminfo_1024mb(),
        });
        let gate = CapabilityGate::new(cap);
        assert!(!gate.enable_feature("nonexistent_feature"));
    }

    // -- RpiModel Display -------------------------------------------------

    #[test]
    fn model_display() {
        assert_eq!(format!("{}", RpiModel::Pi3B), "Raspberry Pi 3 Model B");
        assert_eq!(format!("{}", RpiModel::Pi4), "Raspberry Pi 4 Model B");
        assert_eq!(format!("{}", RpiModel::Pi5), "Raspberry Pi 5");
        assert_eq!(
            format!("{}", RpiModel::Unknown("test".to_string())),
            "Unknown Raspberry Pi model (test)"
        );
    }
}
