//! Static position → MobilePosition NOTIFYs (§9.5.3).
//!
//! A fixed camera does not move: its GB/T 28181 position report is the
//! deployment's surveyed coordinates from config (`gb28181.longitude` /
//! `gb28181.latitude`, GB 度分秒 string form, e.g. `1163942.55E` /
//! `395436.30N` — formatting stays with the operator, the wire carries
//! the strings verbatim). Installed via `with_position_source` while a
//! platform holds a MobilePosition subscription; absent config installs
//! nothing.

use crate::gb28181::subscribe::{MobilePositionSource, PositionReport};

/// Fixed coordinates from config, reported verbatim on the
/// subscription's cadence.
pub struct StaticPosition {
    longitude: String,
    latitude: String,
}

impl StaticPosition {
    /// Both coordinates in GB 度分秒 string form; the report is only
    /// installed when both are configured.
    #[must_use]
    pub fn new(longitude: &str, latitude: &str) -> Self {
        Self {
            longitude: longitude.to_string(),
            latitude: latitude.to_string(),
        }
    }

    fn report(&self) -> PositionReport {
        PositionReport {
            time: crate::gb28181::client::format_gb_time_ms(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
            ),
            longitude: self.longitude.clone(),
            latitude: self.latitude.clone(),
            speed: String::new(),
            direction: String::new(),
            altitude: String::new(),
        }
    }
}

impl MobilePositionSource for StaticPosition {
    fn current_position(&self) -> Option<PositionReport> {
        Some(self.report())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_position_reports_configured_coordinates() {
        let p = StaticPosition::new("1163942.55E", "395436.30N");
        let r = p.current_position().expect("always reports");
        assert_eq!(r.longitude, "1163942.55E");
        assert_eq!(r.latitude, "395436.30N");
        assert!(r.time.contains('T'), "GB time format: {}", r.time);
        assert_eq!(r.speed, "");
    }
}
