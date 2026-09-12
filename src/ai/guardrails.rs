//! Runtime guardrails for AI detection: memory cap and thermal throttle.

use crate::config::AiFeatureConfig;
use std::fs;
use std::io;
use std::time::Duration;

/// Action to take after checking guardrails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardrailAction {
    /// Proceed with normal detection loop.
    Continue,
    /// Skip this frame's detection (e.g., memory cap exceeded).
    SkipThisFrame,
    /// Pause for a duration (e.g., thermal throttle).
    Pause(Duration),
}

/// Abstraction for reading system metrics, mockable in tests.
pub trait SystemMetricsReader: Send + Sync {
    /// Read RSS (Resident Set Size) in bytes.
    fn read_rss_bytes(&self) -> io::Result<u64>;

    /// Read temperature in degrees Celsius.
    fn read_temp_celsius(&self) -> io::Result<f32>;
}

/// Production implementation that reads from actual system files.
pub struct RealSystemMetricsReader;

impl SystemMetricsReader for RealSystemMetricsReader {
    fn read_rss_bytes(&self) -> io::Result<u64> {
        let content = fs::read_to_string("/proc/self/statm")?;
        // Format: <size> <resident> <share> <text> <data> <dt>
        // Field 1 (index 0) is total program size in pages
        // Field 2 (index 1) is resident set size (RSS) in pages
        let parts: Vec<&str> = content.split_whitespace().collect();
        if parts.len() < 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid /proc/self/statm format",
            ));
        }
        let pages = parts[1]
            .parse::<u64>()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(pages * 4096) // Convert pages to bytes
    }

    fn read_temp_celsius(&self) -> io::Result<f32> {
        let content = fs::read_to_string("/sys/class/thermal/thermal_zone0/temp")?;
        let millidegrees = content
            .trim()
            .parse::<i32>()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(millidegrees as f32 / 1000.0)
    }
}

/// Check guardrails based on system metrics and configuration.
///
/// Returns a `GuardrailAction` indicating what the AI loop should do next.
///
/// # Fail-open behavior
/// If reading system metrics fails, this function returns `Continue` to avoid
/// crashing the AI loop due to monitoring errors.
///
/// # Arguments
///
/// * `reader` - The system metrics reader (production or mock).
/// * `config` - AI feature configuration containing `max_memory_mb`.
///
/// # Examples
///
/// ```
/// use std::time::Duration;
/// use mibee_eye_raspi_rs::ai::guardrails::{GuardrailAction, check_guardrails, SystemMetricsReader};
/// use mibee_eye_raspi_rs::config::AiFeatureConfig;
///
/// struct MockReader;
/// impl SystemMetricsReader for MockReader {
///     fn read_rss_bytes(&self) -> std::io::Result<u64> { Ok(100_000_000) }
///     fn read_temp_celsius(&self) -> std::io::Result<f32> { Ok(45.0) }
/// }
///
/// let reader = MockReader;
/// let config = AiFeatureConfig::default();
/// let action = check_guardrails(&reader, &config);
/// assert_eq!(action, GuardrailAction::Continue);
/// ```
pub fn check_guardrails(
    reader: &dyn SystemMetricsReader,
    config: &AiFeatureConfig,
) -> GuardrailAction {
    // Check memory cap
    let memory_action = check_memory_cap(reader, config);
    if matches!(
        memory_action,
        GuardrailAction::SkipThisFrame | GuardrailAction::Pause(_)
    ) {
        return memory_action;
    }

    // Check thermal limits
    check_thermal(reader)
}

/// Check if memory usage exceeds the configured cap.
fn check_memory_cap(reader: &dyn SystemMetricsReader, config: &AiFeatureConfig) -> GuardrailAction {
    let rss_bytes = match reader.read_rss_bytes() {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("guardrails: failed to read RSS: {e}, continuing");
            return GuardrailAction::Continue;
        }
    };

    let max_bytes = config.max_memory_mb as u64 * 1024 * 1024;
    if rss_bytes > max_bytes {
        eprintln!(
            "guardrails: memory cap exceeded: {} MB > {} MB, skipping frame",
            rss_bytes / 1024 / 1024,
            config.max_memory_mb
        );
        // Skip this frame to reduce memory pressure
        return GuardrailAction::SkipThisFrame;
    }

    GuardrailAction::Continue
}

/// Check thermal limits and return appropriate throttling action.
fn check_thermal(reader: &dyn SystemMetricsReader) -> GuardrailAction {
    let temp_c = match reader.read_temp_celsius() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("guardrails: failed to read temperature: {e}, continuing");
            return GuardrailAction::Continue;
        }
    };

    // Critical thermal: pause for 30 seconds
    if temp_c > 85.0 {
        eprintln!(
            "guardrails: critical temperature: {:.1}°C, pausing for 30s",
            temp_c
        );
        return GuardrailAction::Pause(Duration::from_secs(30));
    }

    // High thermal: double the detection interval (pause for extra 200ms)
    if temp_c > 80.0 {
        eprintln!("guardrails: high temperature: {:.1}°C, throttling", temp_c);
        // Pause for extra 200ms on top of the normal 200ms sleep
        return GuardrailAction::Pause(Duration::from_millis(200));
    }

    GuardrailAction::Continue
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mock reader that returns fixed values.
    struct MockReader {
        rss_bytes: u64,
        temp_celsius: f32,
    }

    impl SystemMetricsReader for MockReader {
        fn read_rss_bytes(&self) -> io::Result<u64> {
            Ok(self.rss_bytes)
        }

        fn read_temp_celsius(&self) -> io::Result<f32> {
            Ok(self.temp_celsius)
        }
    }

    /// Mock reader that simulates I/O errors.
    struct FailingReader;

    impl SystemMetricsReader for FailingReader {
        fn read_rss_bytes(&self) -> io::Result<u64> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "access denied",
            ))
        }

        fn read_temp_celsius(&self) -> io::Result<f32> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "access denied",
            ))
        }
    }

    #[test]
    fn test_guardrail_action_continue() {
        let reader = MockReader {
            rss_bytes: 100_000_000, // 100 MB
            temp_celsius: 45.0,
        };
        let config = AiFeatureConfig::default();
        let action = check_guardrails(&reader, &config);
        assert_eq!(action, GuardrailAction::Continue);
    }

    #[test]
    fn test_memory_cap_exceeded() {
        let reader = MockReader {
            rss_bytes: 300_000_000, // 300 MB > default 256 MB
            temp_celsius: 45.0,
        };
        let config = AiFeatureConfig::default();
        let action = check_guardrails(&reader, &config);
        assert_eq!(action, GuardrailAction::SkipThisFrame);
    }

    #[test]
    fn test_memory_cap_not_exceeded() {
        let reader = MockReader {
            rss_bytes: 200_000_000, // 200 MB < 256 MB
            temp_celsius: 45.0,
        };
        let config = AiFeatureConfig::default();
        let action = check_guardrails(&reader, &config);
        assert_eq!(action, GuardrailAction::Continue);
    }

    #[test]
    fn test_thermal_high_throttle() {
        let reader = MockReader {
            rss_bytes: 100_000_000,
            temp_celsius: 82.0, // > 80°C
        };
        let config = AiFeatureConfig::default();
        let action = check_guardrails(&reader, &config);
        assert_eq!(action, GuardrailAction::Pause(Duration::from_millis(200)));
    }

    #[test]
    fn test_thermal_critical_pause() {
        let reader = MockReader {
            rss_bytes: 100_000_000,
            temp_celsius: 86.0, // > 85°C
        };
        let config = AiFeatureConfig::default();
        let action = check_guardrails(&reader, &config);
        assert_eq!(action, GuardrailAction::Pause(Duration::from_secs(30)));
    }

    #[test]
    fn test_thermal_normal() {
        let reader = MockReader {
            rss_bytes: 100_000_000,
            temp_celsius: 75.0, // < 80°C
        };
        let config = AiFeatureConfig::default();
        let action = check_guardrails(&reader, &config);
        assert_eq!(action, GuardrailAction::Continue);
    }

    #[test]
    fn test_failing_reader_continues() {
        let reader = FailingReader;
        let config = AiFeatureConfig::default();
        let action = check_guardrails(&reader, &config);
        assert_eq!(action, GuardrailAction::Continue);
    }

    #[test]
    fn test_custom_memory_cap() {
        let reader = MockReader {
            rss_bytes: 500_000_000, // 500 MB
            temp_celsius: 45.0,
        };
        let config = AiFeatureConfig {
            max_memory_mb: 512, // 512 MB cap
            ..AiFeatureConfig::default()
        };
        let action = check_guardrails(&reader, &config);
        assert_eq!(action, GuardrailAction::Continue);
    }

    #[test]
    fn test_memory_and_thermal_both_exceeded() {
        // Memory cap takes precedence
        let reader = MockReader {
            rss_bytes: 300_000_000, // > 256 MB
            temp_celsius: 90.0,     // > 85°C
        };
        let config = AiFeatureConfig::default();
        let action = check_guardrails(&reader, &config);
        // Memory check happens first, so we skip the frame
        assert_eq!(action, GuardrailAction::SkipThisFrame);
    }

    #[test]
    fn test_real_system_metrics_reader_rss() {
        let reader = RealSystemMetricsReader;
        // This should not fail on a real Linux system
        let rss = reader.read_rss_bytes();
        // We can't assert the exact value, but it should be non-zero
        if let Ok(bytes) = rss {
            assert!(bytes > 0);
        }
    }

    #[test]
    fn test_real_system_metrics_reader_temp() {
        let reader = RealSystemMetricsReader;
        // This might fail if thermal_zone0 doesn't exist
        let temp = reader.read_temp_celsius();
        // If successful, temp should be reasonable
        if let Ok(celsius) = temp {
            assert!((0.0..150.0).contains(&celsius));
        }
    }
}
