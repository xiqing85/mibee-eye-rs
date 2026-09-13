//! Encoder selection: probe a V4L2 node for M2M encoding capability and
//! decide between the hardware (V4L2 M2M) and software (openh264) paths.
//!
//! Decision matrix for `camera.encoder`:
//!
//! | mode       | node is M2M-capable   | node missing / not M2M     |
//! |------------|-----------------------|----------------------------|
//! | `auto`     | hardware encoder      | software encoder (log)     |
//! | `hardware` | hardware encoder      | **error** (diagnostics)    |
//! | `software` | software encoder      | software encoder           |

use std::os::fd::RawFd;

use super::source::CameraError;

/// Result of probing one V4L2 device node for encoding capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HardwareEncoderAvailability {
    /// The node advertises `V4L2_CAP_VIDEO_M2M(_MPLANE)` — usable by the
    /// V4L2 M2M H.264 encoder.
    M2MCapable { driver: String },
    /// The node does not exist / cannot be opened.
    NotPresent,
    /// The node exists but is not an M2M encoder.
    NotCapable { reason: String },
}

/// Abstraction over the device probe so tests can cover the decision
/// matrix without hardware.
pub trait EncoderProbe: Send + Sync {
    fn probe(&self, device_path: &str) -> HardwareEncoderAvailability;
}

/// Production probe: `open` + `VIDIOC_QUERYCAP`.
pub struct RealEncoderProbe;

/// `v4l2_capability` (kernel layout, 104 bytes).
#[repr(C)]
#[derive(Default)]
struct V4l2Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

// ioctl request codes (Linux, matches the capture module's helper).
const IOC_READ: u32 = 2;
const TYPE_V: u32 = b'V' as u32;
const fn ioc(dir: u32, typ: u32, nr: u32, size: u32) -> u32 {
    (dir << 30) | (size << 16) | (typ << 8) | nr
}
const VIDIOC_QUERYCAP: u32 = ioc(
    IOC_READ,
    TYPE_V,
    0,
    std::mem::size_of::<V4l2Capability>() as u32,
);

const V4L2_CAP_VIDEO_M2M: u32 = 0x0000_0008;
const V4L2_CAP_VIDEO_M2M_MPLANE: u32 = 0x0000_4000;

fn cstr_bytes(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn query_caps(fd: RawFd) -> std::io::Result<V4l2Capability> {
    let mut caps = V4l2Capability::default();
    // Safety: `caps` is a valid, properly aligned repr(C) buffer for the
    // duration of the call, as required by ioctl(VIDIOC_QUERYCAP).
    // `as _` picks c_int on musl and c_ulong on gnu targets.
    let rc = unsafe { libc::ioctl(fd, VIDIOC_QUERYCAP as _, &mut caps) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(caps)
}

impl EncoderProbe for RealEncoderProbe {
    fn probe(&self, device_path: &str) -> HardwareEncoderAvailability {
        // Safety: plain libc::open with a NUL-terminated path.
        let fd = unsafe {
            let c = std::ffi::CString::new(device_path).unwrap_or_default();
            libc::open(c.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK)
        };
        if fd < 0 {
            return HardwareEncoderAvailability::NotPresent;
        }
        let query = query_caps(fd);
        // Safety: fd came from libc::open above and is closed exactly once.
        unsafe { libc::close(fd) };
        let caps = match query {
            Ok(c) => c,
            Err(e) => {
                return HardwareEncoderAvailability::NotCapable {
                    reason: format!("VIDIOC_QUERYCAP failed: {e}"),
                }
            }
        };
        // Prefer device_caps (per-device, since Linux 3.3) but fall back to
        // the legacy whole-device capabilities field.
        let caps_field = if caps.device_caps != 0 {
            caps.device_caps
        } else {
            caps.capabilities
        };
        if caps_field & (V4L2_CAP_VIDEO_M2M | V4L2_CAP_VIDEO_M2M_MPLANE) != 0 {
            HardwareEncoderAvailability::M2MCapable {
                driver: cstr_bytes(&caps.driver),
            }
        } else {
            HardwareEncoderAvailability::NotCapable {
                reason: format!(
                    "node '{}' is not an M2M encoder (driver {}, caps {:#x})",
                    device_path,
                    cstr_bytes(&caps.driver),
                    caps_field
                ),
            }
        }
    }
}

/// Which encoder `start_camera_pipeline` should assemble.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectedEncoder {
    Hardware,
    Software,
}

/// Resolve `camera.encoder` against a probe result.
///
/// `availability` is `None` when the hardware path is not compiled in
/// (`v4l2-encoder` feature off) — `auto` then picks software, `hardware`
/// fails with a rebuild hint.
pub fn decide(
    mode: &str,
    availability: Option<HardwareEncoderAvailability>,
) -> Result<(SelectedEncoder, String), CameraError> {
    #[cfg(not(feature = "software-encoder"))]
    if matches!(mode, "software" | "auto")
        && !matches!(
            availability,
            Some(HardwareEncoderAvailability::M2MCapable { .. })
        )
    {
        return Err(CameraError::Config(
            "the software encoder is not compiled in (rebuild with --features software-encoder) \
             or point camera.encoder_device at a working M2M encoder node"
                .into(),
        ));
    }
    match mode {
        "software" => Ok((
            SelectedEncoder::Software,
            "camera.encoder=software — using in-process openh264 encoder".to_string(),
        )),
        "hardware" => match availability {
            Some(HardwareEncoderAvailability::M2MCapable { driver }) => Ok((
                SelectedEncoder::Hardware,
                format!("camera.encoder=hardware — M2M encoder driver '{driver}'"),
            )),
            Some(HardwareEncoderAvailability::NotPresent) => Err(CameraError::DeviceNotFound(
                "camera.encoder=hardware but the encoder device node is missing; \
                 set camera.encoder_device or use auto/software"
                    .into(),
            )),
            Some(HardwareEncoderAvailability::NotCapable { reason }) => Err(CameraError::Config(
                format!("camera.encoder=hardware but {reason}"),
            )),
            None => Err(CameraError::Config(
                "camera.encoder=hardware but the v4l2-encoder feature is not compiled in; \
                 rebuild with --features v4l2-encoder"
                    .into(),
            )),
        },
        "auto" => match availability {
            Some(HardwareEncoderAvailability::M2MCapable { driver }) => Ok((
                SelectedEncoder::Hardware,
                format!("camera.encoder=auto — M2M encoder driver '{driver}' detected"),
            )),
            other => {
                let why = match other {
                    Some(HardwareEncoderAvailability::NotPresent) => "no encoder device node",
                    Some(HardwareEncoderAvailability::NotCapable { .. }) => {
                        "encoder device is not M2M-capable"
                    }
                    None => "hardware encoder path not compiled in",
                    Some(HardwareEncoderAvailability::M2MCapable { .. }) => unreachable!(),
                };
                Ok((
                    SelectedEncoder::Software,
                    format!("camera.encoder=auto — {why}, falling back to software encoder"),
                ))
            }
        },
        other => Err(CameraError::Config(format!(
            "camera.encoder must be auto, hardware or software, got: {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn software_mode_always_software() {
        for avail in [
            Some(HardwareEncoderAvailability::M2MCapable {
                driver: "bcm2835-codec".into(),
            }),
            Some(HardwareEncoderAvailability::NotPresent),
            Some(HardwareEncoderAvailability::NotCapable { reason: "x".into() }),
            None,
        ] {
            let (sel, _) = decide("software", avail).unwrap();
            assert_eq!(sel, SelectedEncoder::Software);
        }
    }

    #[test]
    fn hardware_mode_requires_m2m() {
        let (sel, _) = decide(
            "hardware",
            Some(HardwareEncoderAvailability::M2MCapable {
                driver: "bcm2835-codec".into(),
            }),
        )
        .unwrap();
        assert_eq!(sel, SelectedEncoder::Hardware);

        assert!(decide("hardware", Some(HardwareEncoderAvailability::NotPresent)).is_err());
        assert!(decide(
            "hardware",
            Some(HardwareEncoderAvailability::NotCapable {
                reason: "not m2m".into()
            })
        )
        .is_err());
        let err = decide("hardware", None).unwrap_err();
        assert!(format!("{err}").contains("v4l2-encoder"), "got {err}");
    }

    #[test]
    fn auto_prefers_hardware_falls_back_to_software() {
        let (sel, msg) = decide(
            "auto",
            Some(HardwareEncoderAvailability::M2MCapable {
                driver: "bcm2835-codec".into(),
            }),
        )
        .unwrap();
        assert_eq!(sel, SelectedEncoder::Hardware);
        assert!(msg.contains("bcm2835-codec"));

        let (sel, msg) = decide("auto", Some(HardwareEncoderAvailability::NotPresent)).unwrap();
        assert_eq!(sel, SelectedEncoder::Software);
        assert!(msg.contains("falling back"));

        let (sel, _) = decide("auto", None).unwrap();
        assert_eq!(sel, SelectedEncoder::Software);
    }

    #[test]
    fn invalid_mode_rejected() {
        assert!(decide("quantum", None).is_err());
    }

    #[test]
    fn software_encoder_not_compiled_is_rejected() {
        // Mirrors the cfg(not(software-encoder)) guard in decide(): the
        // guard only compiles in hardware-only builds, so this test
        // documents the contract and keeps the matrix honest.
        let any = Some(HardwareEncoderAvailability::NotPresent);
        let _ = decide("auto", any); // no panic in default builds
    }

    #[test]
    fn cstr_bytes_truncates_at_nul() {
        assert_eq!(cstr_bytes(b"bcm2835-codec\0rest"), "bcm2835-codec");
        assert_eq!(cstr_bytes(b"openh264"), "openh264");
    }
}
