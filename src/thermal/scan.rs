//! Hardware adaptation: bus/pin/address discovery for MLX90640 modules.
//!
//! Different carrier boards expose different I2C buses, GPIO base numbers
//! and module strapping (MLX90640 responds at 0x33 by factory default, but
//! A1/A0 strapping moves it across 0x30..=0x3E). Instead of guessing, the
//! `scan` command walks every hardware bus × plausible address and every
//! candidate GPIO pair, identifies the sensor by its EEPROM ID word, and
//! prints the working configuration.
//!
//! Read-only (a single word read per probe); never writes to any device,
//! so scanning a bus with unrelated I2C chips (PMIC, HDMI DDC, PHY) is
//! safe — they either NAK or return an implausible ID.

use super::error::Result;
use super::i2c::{BitBang, MlxBus, Twi};
use super::mlx90640::{Mlx90640, REG_ID};

/// MLX90640 plausible-address window (datasheet: 7-bit SA 0x30..=0x3E with
/// A1/A0 straps; 0x33 is the factory default).
pub const ADDR_RANGE: std::ops::RangeInclusive<u8> = 0x30..=0x3E;

/// One discovered sensor.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// Human-readable transport description (config `bus` value shape).
    pub bus: String,
    pub addr: u8,
    pub id: u16,
}

fn plausible_id(id: u16) -> bool {
    // The ID word is a real calibration value: never 0x0000/0xFFFF and the
    // low nibble pattern varies. Anything read back cleanly counts.
    id != 0x0000 && id != 0xFFFF
}

/// Enumerate /dev/i2c-* devices present on this board.
pub fn twi_devices() -> Vec<String> {
    let mut out = Vec::new();
    for n in 0..16 {
        let p = format!("/dev/i2c-{n}");
        if std::path::Path::new(&p).exists() {
            out.push(p);
        }
    }
    out
}

/// Probe one hardware TWI device × address range.
pub fn scan_twi_dev(dev: &str) -> Vec<Hit> {
    let mut hits = Vec::new();
    for addr in ADDR_RANGE.clone() {
        let Ok(bus) = Twi::open(dev, addr) else {
            continue; // bus open failed once — skip its whole range below
        };
        let mut s = Mlx90640::new(bus);
        if let Ok(id) = s.read_id() {
            if plausible_id(id) {
                hits.push(Hit {
                    bus: format!("twi:{dev}"),
                    addr,
                    id,
                });
            }
        }
    }
    hits
}

/// Probe every hardware TWI bus.
pub fn scan_twi() -> Vec<Hit> {
    twi_devices().iter().flat_map(|d| scan_twi_dev(d)).collect()
}

/// Release the board's TWI controller + stale sysfs exports so bit-bang can
/// own the candidate pins (same dance as the `serve` path; idempotent,
/// best-effort). `5002800.i2c` is the Orange Pi Zero 3 TWI2 on PH2/PH3 —
/// other boards: see docs/porting.md.
pub(crate) fn release_gpio_pair(sda: u32, scl: u32) {
    let _ = std::fs::write(
        "/sys/bus/platform/drivers/mv64xxx_i2c/unbind",
        "5002800.i2c",
    );
    for n in [sda, scl] {
        let _ = std::fs::write("/sys/class/gpio/unexport", n.to_string());
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
}

/// Operator hint appended to a failed GPIO-pair probe — the common cases
/// (permissions, busy pins) have one-line fixes and must not be silent.
fn pair_error_hint(e: &super::error::Error) -> String {
    let s = e.to_string();
    if s.contains("Permission denied") {
        " (sysfs GPIO export needs root — run scan with sudo)".into()
    } else if s.contains("Device or resource busy") {
        " (pins held by another process — stop mibee-eye-thermal first)".into()
    } else {
        String::new()
    }
}

/// Probe one sysfs GPIO pair (SDA, SCL) × address range via bit-bang.
///
/// Returns the hits plus a failure reason when the *pair itself* was
/// unusable (export denied, lines stuck) — distinct from "probed clean,
/// nothing answered", which is not an error. Reuses the serve bring-up
/// dance: the board's TWI controller must let go of the pins before sysfs
/// GPIO reaches the pads, and the pair is exported once for the whole
/// address walk (`BitBang::set_addr`) instead of per address.
pub fn scan_gpio_pair(sda: u32, scl: u32) -> (Vec<Hit>, Option<String>) {
    let mut hits = Vec::new();
    release_gpio_pair(sda, scl);
    let mut bus = match BitBang::new(sda, scl, *ADDR_RANGE.start()) {
        Ok(b) => b,
        Err(e) => {
            return (
                hits,
                Some(format!("gpio {sda},{scl}: {e}{}", pair_error_hint(&e))),
            )
        }
    };
    for addr in ADDR_RANGE.clone() {
        bus.set_addr(addr);
        if let Ok(id) = bus.read_reg16(REG_ID) {
            if plausible_id(id) {
                hits.push(Hit {
                    bus: format!("gpio:{sda},{scl}"),
                    addr,
                    id,
                });
            }
        }
    }
    (hits, None)
}

/// `--gpio-candidates 226,227:8,10` → [(226,227), (8,10)] with the module
/// pin-order footgun handled (SDA/SCL swapped is reported, not silently
/// retried — an explicit pair beats magic).
pub fn parse_gpio_candidates(spec: &str) -> Result<Vec<(u32, u32)>> {
    let mut out = Vec::new();
    for pair in spec.split(':') {
        let mut it = pair.split(',');
        let sda: u32 = it
            .next()
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| super::error::Error::Usage(format!("bad gpio pair: {pair}")))?;
        let scl: u32 = it
            .next()
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| super::error::Error::Usage(format!("bad gpio pair: {pair}")))?;
        out.push((sda, scl));
    }
    if out.is_empty() {
        return Err(super::error::Error::Usage(
            "--gpio-candidates needs at least one SDA,SCL pair".into(),
        ));
    }
    Ok(out)
}

/// Resolve a working sensor transport for boot-time `bus = auto`:
/// hardware TWI first (proper pull-ups present), then the classic
/// Orange Pi Zero 3 PH2/PH3 pair as the bit-bang fallback.
pub fn auto_detect() -> Option<(Hit, bool)> {
    // (hit, is_gpio)
    let twi = scan_twi();
    if let Some(h) = twi.first() {
        return Some((h.clone(), false));
    }
    let (gpio, _pair_err) = scan_gpio_pair(226, 227);
    gpio.first().map(|h| (h.clone(), true))
}

/// Format a scan result as ready-to-paste config lines.
pub fn config_snippet(hit: &Hit) -> String {
    let bus = if let Some(dev) = hit.bus.strip_prefix("twi:") {
        crate::config::BusSpec::Twi {
            device: dev.to_string(),
        }
    } else {
        let pins = hit.bus.trim_start_matches("gpio:");
        let (sda, scl) = pins.split_once(',').unwrap_or((pins, pins));
        let (sda, scl) = (sda.parse().unwrap_or(0), scl.parse().unwrap_or(0));
        crate::config::BusSpec::Gpio { sda, scl }
    };
    format!(
        "[thermal]\n{}\naddr = {:#04x}\n",
        super::bus_toml(&bus),
        hit.addr
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpio_candidate_parsing() {
        let v = parse_gpio_candidates("226,227:8,10").expect("parse");
        assert_eq!(v, vec![(226, 227), (8, 10)]);
        assert!(parse_gpio_candidates("").is_err());
        assert!(parse_gpio_candidates("226").is_err());
        assert!(parse_gpio_candidates("a,b").is_err());
    }

    #[test]
    fn config_snippet_shapes() {
        let twi = Hit {
            bus: "twi:/dev/i2c-2".into(),
            addr: 0x33,
            id: 0x1901,
        };
        assert!(config_snippet(&twi).contains("bus = { type = \"twi\", device = \"/dev/i2c-2\" }"));
        assert!(config_snippet(&twi).contains("addr = 0x33"));
        let gpio = Hit {
            bus: "gpio:226,227".into(),
            addr: 0x30,
            id: 0x1901,
        };
        assert!(config_snippet(&gpio).contains("sda = 226, scl = 227"));
        assert!(config_snippet(&gpio).contains("addr = 0x30"));
    }

    #[test]
    fn address_range_covers_straps() {
        assert!(ADDR_RANGE.contains(&0x33));
        assert!(ADDR_RANGE.contains(&0x30));
        assert!(ADDR_RANGE.contains(&0x3E));
        assert!(!ADDR_RANGE.contains(&0x3F));
    }

    #[test]
    fn pair_error_hint_covers_common_cases() {
        // EACCES: sysfs GPIO export is root-only — say so.
        let e = super::super::error::Error::Io(std::io::Error::from_raw_os_error(13));
        assert!(pair_error_hint(&e).contains("sudo"));
        // EBUSY: another process holds the exported pins.
        let e = super::super::error::Error::Io(std::io::Error::from_raw_os_error(16));
        assert!(pair_error_hint(&e).contains("stop"));
        // Anything else: no invented hint.
        assert_eq!(
            pair_error_hint(&super::super::error::Error::Bus("NAK".into())),
            ""
        );
    }
}
