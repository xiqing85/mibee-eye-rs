//! V4L2 camera parameter controls.
//!
//! Provides [`ParamManager`] which maps ONVIF parameter names to V4L2 control
//! IDs and handles normalized `[0.0, 1.0]` value conversion to / from V4L2
//! integer ranges.
//!
//! ## Architecture
//!
//! ```text
//!  ONVIF name ──► ParamManager ──► V4l2ControlIo trait ──► MockV4l2Control
//!  (e.g. "Brightness")              (real: ioctl)            (testing)
//! ```
//!
//! All V4L2 interactions go through a [`V4l2ControlIo`] trait object so tests
//! never require real hardware.

use std::collections::HashMap;
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// V4L2 control ID constants
// ---------------------------------------------------------------------------

/// V4L2 CID for brightness (`V4L2_CID_BRIGHTNESS = 0x00980900`).
pub const V4L2_CID_BRIGHTNESS: u32 = 9963776;
/// V4L2 CID for contrast (`V4L2_CID_CONTRAST = 0x00980901`).
pub const V4L2_CID_CONTRAST: u32 = 9963777;
/// V4L2 CID for saturation (`V4L2_CID_SATURATION = 0x00980902`).
pub const V4L2_CID_SATURATION: u32 = 9963778;
/// V4L2 CID for sharpness (`V4L2_CID_SHARPNESS = 0x0098091b`).
pub const V4L2_CID_SHARPNESS: u32 = 9963803;
/// V4L2 CID for auto white balance (`V4L2_CID_AUTO_WHITE_BALANCE = 0x0098090c`).
pub const V4L2_CID_AUTO_WHITE_BALANCE: u32 = 9963788;
/// V4L2 CID for exposure auto (`V4L2_CID_EXPOSURE_AUTO = 0x009a0901`).
pub const V4L2_CID_EXPOSURE_AUTO: u32 = 10094849;

// ---------------------------------------------------------------------------
// ControlQuery
// ---------------------------------------------------------------------------

/// Result of a V4L2 `VIDIOC_QUERYCTRL` ioctl.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ControlQuery {
    /// Minimum value (inclusive).
    pub min: i32,
    /// Maximum value (inclusive).
    pub max: i32,
    /// Default value.
    pub default: i32,
    /// Step increment.
    pub step: i32,
}

// ---------------------------------------------------------------------------
// V4l2ControlIo trait
// ---------------------------------------------------------------------------

/// Abstract interface for V4L2 control I/O.
///
/// Implementations wrap `ioctl(VIDIOC_QUERYCTRL / VIDIOC_S_CTRL / VIDIOC_G_CTRL)`
/// for real hardware, or use an in-memory [`HashMap`] for tests.
pub trait V4l2ControlIo: Send + Sync {
    /// Query control attributes (range, default, step).
    fn queryctrl(&self, id: u32) -> Result<ControlQuery, ParamError>;
    /// Set a control value (`VIDIOC_S_CTRL`).
    fn s_ctrl(&self, id: u32, value: i32) -> Result<(), ParamError>;
    /// Get a control value (`VIDIOC_G_CTRL`).
    fn g_ctrl(&self, id: u32) -> Result<i32, ParamError>;
}

// ---------------------------------------------------------------------------
// ParamError
// ---------------------------------------------------------------------------

/// Errors that can occur during parameter operations.
#[derive(Debug, Clone, PartialEq)]
pub enum ParamError {
    /// The parameter name is not recognised.
    InvalidName(String),
    /// The requested value falls outside the valid range.
    OutOfRange {
        /// The value that was attempted.
        value: f64,
        /// Minimum allowed value.
        min: f64,
        /// Maximum allowed value.
        max: f64,
    },
    /// An I/O error talking to the V4L2 device.
    IoError(String),
    /// The requested operation is not implemented.
    NotImplemented(String),
}

impl std::fmt::Display for ParamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParamError::InvalidName(name) => write!(f, "unknown parameter: {name}"),
            ParamError::OutOfRange { value, min, max } => {
                write!(f, "value {value} out of range [{min}, {max}]")
            }
            ParamError::IoError(msg) => write!(f, "V4L2 I/O error: {msg}"),
            ParamError::NotImplemented(msg) => write!(f, "not implemented: {msg}"),
        }
    }
}

impl std::error::Error for ParamError {}

// ---------------------------------------------------------------------------
// ParamInfo
// ---------------------------------------------------------------------------

/// Descriptive information about a camera parameter.
#[derive(Debug, Clone, PartialEq)]
pub struct ParamInfo {
    /// ONVIF-style parameter name (e.g. `"Brightness"`).
    pub name: String,
    /// Minimum value (in the normalized range).
    pub min: f64,
    /// Maximum value (in the normalized range).
    pub max: f64,
    /// Default value (normalised).
    pub default: f64,
    /// Current value (normalised).
    pub current: f64,
}

// ---------------------------------------------------------------------------
// Internal parameter descriptor
// ---------------------------------------------------------------------------

struct ParamDef {
    v4l2_id: u32,
    onvif_name: &'static str,
    range: ControlQuery,
}

// ---------------------------------------------------------------------------
// ONVIF bridge: ONVIF name → V4L2 control ID
// ---------------------------------------------------------------------------

fn onvif_to_v4l2_id(name: &str) -> Option<u32> {
    match name {
        "Brightness" => Some(V4L2_CID_BRIGHTNESS),
        "Contrast" => Some(V4L2_CID_CONTRAST),
        "Saturation" => Some(V4L2_CID_SATURATION),
        "Sharpness" => Some(V4L2_CID_SHARPNESS),
        "WhiteBalance" => Some(V4L2_CID_AUTO_WHITE_BALANCE),
        "Exposure" => Some(V4L2_CID_EXPOSURE_AUTO),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Normalised ↔ V4L2 conversion
// ---------------------------------------------------------------------------

/// Convert a normalised [0.0, 1.0] value to a V4L2 integer for the given
/// control range.
fn normalized_to_v4l2(norm: f64, range: &ControlQuery) -> i32 {
    let norm = norm.clamp(0.0, 1.0);
    let val = range.min as f64 + norm * (range.max - range.min) as f64;
    val.round() as i32
}

/// Convert a V4L2 integer value back to the [0.0, 1.0] normalised range.
fn v4l2_to_normalized(v4l2_val: i32, range: &ControlQuery) -> f64 {
    let span = (range.max - range.min) as f64;
    if span.abs() <= f64::EPSILON {
        return 0.5;
    }
    ((v4l2_val - range.min) as f64 / span).clamp(0.0, 1.0)
}

// ---------------------------------------------------------------------------
// ParamManager
// ---------------------------------------------------------------------------

/// Manages camera parameters with ONVIF name bridging and normalised-value
/// conversion.
///
/// All V4L2 operations go through a [`V4l2ControlIo`] trait object, enabling
/// transparent testing with [`MockV4l2Control`].
pub struct ParamManager {
    device_path: String,
    io: Box<dyn V4l2ControlIo>,
    param_defs: Vec<ParamDef>,
}

impl ParamManager {
    /// Create a new `ParamManager` for the given device path.
    ///
    /// The control abstraction **must** be supplied — use [`MockV4l2Control`]
    /// in tests and a real ioctl wrapper in production.
    pub fn new(device_path: &str, io: Box<dyn V4l2ControlIo>) -> Self {
        Self {
            device_path: device_path.to_string(),
            io,
            param_defs: Self::default_param_defs(),
        }
    }

    /// Build the list of built-in parameter definitions.
    fn default_param_defs() -> Vec<ParamDef> {
        vec![
            ParamDef {
                v4l2_id: V4L2_CID_BRIGHTNESS,
                onvif_name: "Brightness",
                range: ControlQuery {
                    min: -255,
                    max: 255,
                    default: 0,
                    step: 1,
                },
            },
            ParamDef {
                v4l2_id: V4L2_CID_CONTRAST,
                onvif_name: "Contrast",
                range: ControlQuery {
                    min: 0,
                    max: 255,
                    default: 127,
                    step: 1,
                },
            },
            ParamDef {
                v4l2_id: V4L2_CID_SATURATION,
                onvif_name: "Saturation",
                range: ControlQuery {
                    min: 0,
                    max: 255,
                    default: 127,
                    step: 1,
                },
            },
            ParamDef {
                v4l2_id: V4L2_CID_SHARPNESS,
                onvif_name: "Sharpness",
                range: ControlQuery {
                    min: 0,
                    max: 255,
                    default: 0,
                    step: 1,
                },
            },
            ParamDef {
                v4l2_id: V4L2_CID_AUTO_WHITE_BALANCE,
                onvif_name: "WhiteBalance",
                range: ControlQuery {
                    min: 0,
                    max: 1,
                    default: 1,
                    step: 1,
                },
            },
            ParamDef {
                v4l2_id: V4L2_CID_EXPOSURE_AUTO,
                onvif_name: "Exposure",
                range: ControlQuery {
                    min: 0,
                    max: 3,
                    default: 0,
                    step: 1,
                },
            },
        ]
    }

    /// Find a parameter definition by ONVIF name.
    fn find_param(&self, name: &str) -> Option<&ParamDef> {
        self.param_defs.iter().find(|p| p.onvif_name == name)
    }

    /// Get the current value of a parameter, returned as a [0.0, 1.0]
    /// normalised `f64`.
    ///
    /// # Errors
    ///
    /// Returns [`ParamError::InvalidName`] if `name` is not recognised.
    pub fn get_param(&self, name: &str) -> Result<f64, ParamError> {
        let param = self
            .find_param(name)
            .ok_or_else(|| ParamError::InvalidName(name.to_string()))?;
        let v4l2_val = self.io.g_ctrl(param.v4l2_id)?;
        Ok(v4l2_to_normalized(v4l2_val, &param.range))
    }

    /// Set a parameter from a normalised [0.0, 1.0] value.
    ///
    /// # Errors
    ///
    /// Returns [`ParamError::InvalidName`] if `name` is not recognised, or
    /// [`ParamError::OutOfRange`] if `value` is outside [0.0, 1.0].
    pub fn set_param(&self, name: &str, value: f64) -> Result<(), ParamError> {
        let param = self
            .find_param(name)
            .ok_or_else(|| ParamError::InvalidName(name.to_string()))?;

        if !(0.0..=1.0).contains(&value) {
            return Err(ParamError::OutOfRange {
                value,
                min: 0.0,
                max: 1.0,
            });
        }

        let v4l2_val = normalized_to_v4l2(value, &param.range);
        self.io.s_ctrl(param.v4l2_id, v4l2_val)
    }

    /// Get the normalised [min, max] range for a parameter.
    ///
    /// The returned range is always `[0.0, 1.0]` since all public values are
    /// normalised.
    ///
    /// # Errors
    ///
    /// Returns [`ParamError::InvalidName`] if `name` is not recognised.
    pub fn get_param_range(&self, name: &str) -> Result<(f64, f64), ParamError> {
        let _param = self
            .find_param(name)
            .ok_or_else(|| ParamError::InvalidName(name.to_string()))?;
        Ok((0.0, 1.0))
    }

    /// List all supported parameters with their current values.
    pub fn list_params(&self) -> Vec<ParamInfo> {
        self.param_defs
            .iter()
            .map(|p| {
                let current = self
                    .io
                    .g_ctrl(p.v4l2_id)
                    .map(|v| v4l2_to_normalized(v, &p.range))
                    .unwrap_or(0.5);
                ParamInfo {
                    name: p.onvif_name.to_string(),
                    min: 0.0,
                    max: 1.0,
                    default: v4l2_to_normalized(p.range.default, &p.range),
                    current,
                }
            })
            .collect()
    }

    /// The device path this manager is bound to.
    #[must_use]
    pub fn device_path(&self) -> &str {
        &self.device_path
    }

    /// Resolve an ONVIF parameter name to its V4L2 control ID.
    ///
    /// This is a static bridge — no I/O required.
    #[must_use]
    pub fn resolve_onvif_name(name: &str) -> Option<u32> {
        onvif_to_v4l2_id(name)
    }
}

// ---------------------------------------------------------------------------
// MockV4l2Control
// ---------------------------------------------------------------------------

/// In-memory mock of the V4L2 control interface for testing.
///
/// Stores current values in a [`HashMap`] behind a [`Mutex`] and returns
/// pre-configured [`ControlQuery`] ranges. No real hardware required.
pub struct MockV4l2Control {
    values: Mutex<HashMap<u32, i32>>,
    queries: HashMap<u32, ControlQuery>,
}

impl MockV4l2Control {
    /// Create a new mock initialised with the six standard controls at their
    /// default values.
    #[must_use]
    pub fn new() -> Self {
        let mut values = HashMap::new();
        let mut queries = HashMap::new();

        let controls = [
            (
                V4L2_CID_BRIGHTNESS,
                ControlQuery {
                    min: -255,
                    max: 255,
                    default: 0,
                    step: 1,
                },
            ),
            (
                V4L2_CID_CONTRAST,
                ControlQuery {
                    min: 0,
                    max: 255,
                    default: 127,
                    step: 1,
                },
            ),
            (
                V4L2_CID_SATURATION,
                ControlQuery {
                    min: 0,
                    max: 255,
                    default: 127,
                    step: 1,
                },
            ),
            (
                V4L2_CID_SHARPNESS,
                ControlQuery {
                    min: 0,
                    max: 255,
                    default: 0,
                    step: 1,
                },
            ),
            (
                V4L2_CID_AUTO_WHITE_BALANCE,
                ControlQuery {
                    min: 0,
                    max: 1,
                    default: 1,
                    step: 1,
                },
            ),
            (
                V4L2_CID_EXPOSURE_AUTO,
                ControlQuery {
                    min: 0,
                    max: 3,
                    default: 0,
                    step: 1,
                },
            ),
        ];

        for &(id, q) in &controls {
            values.insert(id, q.default);
            queries.insert(id, q);
        }

        Self {
            values: Mutex::new(values),
            queries,
        }
    }

    /// Directly set the raw V4L2 value for a control (bypasses range check).
    /// Useful for setting up specific test scenarios.
    pub fn set_raw(&self, id: u32, value: i32) {
        let mut values = self.values.lock().unwrap();
        values.insert(id, value);
    }
}

impl Default for MockV4l2Control {
    fn default() -> Self {
        Self::new()
    }
}

impl V4l2ControlIo for MockV4l2Control {
    fn queryctrl(&self, id: u32) -> Result<ControlQuery, ParamError> {
        self.queries
            .get(&id)
            .copied()
            .ok_or_else(|| ParamError::InvalidName(format!("V4L2 control ID {id}")))
    }

    fn s_ctrl(&self, id: u32, value: i32) -> Result<(), ParamError> {
        let q = self
            .queries
            .get(&id)
            .ok_or_else(|| ParamError::InvalidName(format!("V4L2 control ID {id}")))?;
        if value < q.min || value > q.max {
            return Err(ParamError::OutOfRange {
                value: value as f64,
                min: q.min as f64,
                max: q.max as f64,
            });
        }
        let mut values = self.values.lock().unwrap();
        values.insert(id, value);
        Ok(())
    }

    fn g_ctrl(&self, id: u32) -> Result<i32, ParamError> {
        let values = self.values.lock().unwrap();
        values
            .get(&id)
            .copied()
            .ok_or_else(|| ParamError::InvalidName(format!("V4L2 control ID {id}")))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a `ParamManager` backed by a fresh mock.
    fn make_manager() -> ParamManager {
        let mock: Box<dyn V4l2ControlIo> = Box::new(MockV4l2Control::new());
        ParamManager::new("/dev/video0", mock)
    }

    // 1. set_and_get_param — set brightness to 0.5, get returns 0.5
    #[test]
    fn set_and_get_param() {
        let pm = make_manager();
        pm.set_param("Brightness", 0.5).unwrap();
        let val = pm.get_param("Brightness").unwrap();
        assert!((val - 0.5).abs() < 0.01, "expected ~0.5, got {val}");
    }

    // 2. param_range — get_range returns correct min/max
    #[test]
    fn param_range() {
        let pm = make_manager();
        let (min, max) = pm.get_param_range("Brightness").unwrap();
        assert!((min - 0.0).abs() < f64::EPSILON);
        assert!((max - 1.0).abs() < f64::EPSILON);
    }

    // 3. invalid_param_name — get "Invalid" returns Err
    #[test]
    fn invalid_param_name() {
        let pm = make_manager();
        let err = pm.get_param("Invalid").unwrap_err();
        assert!(matches!(err, ParamError::InvalidName(_)));
    }

    // 4. out_of_range — set brightness to 2.0 returns Err
    #[test]
    fn out_of_range() {
        let pm = make_manager();
        let err = pm.set_param("Brightness", 2.0).unwrap_err();
        assert!(matches!(err, ParamError::OutOfRange { .. }));
    }

    // 5. list_params — returns all 6 params
    #[test]
    fn list_params() {
        let pm = make_manager();
        let params = pm.list_params();
        assert_eq!(params.len(), 6);

        let names: Vec<&str> = params.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"Brightness"));
        assert!(names.contains(&"Contrast"));
        assert!(names.contains(&"Saturation"));
        assert!(names.contains(&"Sharpness"));
        assert!(names.contains(&"WhiteBalance"));
        assert!(names.contains(&"Exposure"));
    }

    // 6. normalized_mapping — set 0.0 → min, 1.0 → max, 0.5 → midpoint
    #[test]
    fn normalized_mapping() {
        let mock = MockV4l2Control::new();
        let range = ControlQuery {
            min: -255,
            max: 255,
            default: 0,
            step: 1,
        };

        // 0.0 → min (-255)
        assert_eq!(normalized_to_v4l2(0.0, &range), -255);
        // 1.0 → max (255)
        assert_eq!(normalized_to_v4l2(1.0, &range), 255);
        // 0.5 → midpoint (0)
        assert_eq!(normalized_to_v4l2(0.5, &range), 0);

        // Round-trip through the mock
        let pm = ParamManager::new("/dev/video0", Box::new(mock));

        // 0.0 on a [-255,255] range
        pm.set_param("Brightness", 0.0).unwrap();
        let v = pm.get_param("Brightness").unwrap();
        assert!((v - 0.0).abs() < 0.01, "expected ~0.0, got {v}");

        // 1.0 on a [-255,255] range
        pm.set_param("Brightness", 1.0).unwrap();
        let v = pm.get_param("Brightness").unwrap();
        assert!((v - 1.0).abs() < 0.01, "expected ~1.0, got {v}");

        // 0.5 → midpoint
        pm.set_param("Brightness", 0.5).unwrap();
        let v = pm.get_param("Brightness").unwrap();
        assert!((v - 0.5).abs() < 0.01, "expected ~0.5, got {v}");
    }

    // 7. onvif_name_bridging — "Brightness" works, "ColorBrightness" fails
    #[test]
    fn onvif_name_bridging() {
        // Valid ONVIF names resolve correctly
        assert_eq!(
            ParamManager::resolve_onvif_name("Brightness"),
            Some(V4L2_CID_BRIGHTNESS)
        );
        assert_eq!(
            ParamManager::resolve_onvif_name("Contrast"),
            Some(V4L2_CID_CONTRAST)
        );
        assert_eq!(
            ParamManager::resolve_onvif_name("Saturation"),
            Some(V4L2_CID_SATURATION)
        );
        assert_eq!(
            ParamManager::resolve_onvif_name("Sharpness"),
            Some(V4L2_CID_SHARPNESS)
        );
        assert_eq!(
            ParamManager::resolve_onvif_name("WhiteBalance"),
            Some(V4L2_CID_AUTO_WHITE_BALANCE)
        );
        assert_eq!(
            ParamManager::resolve_onvif_name("Exposure"),
            Some(V4L2_CID_EXPOSURE_AUTO)
        );

        // Invalid name returns None
        assert_eq!(ParamManager::resolve_onvif_name("ColorBrightness"), None);
        assert_eq!(ParamManager::resolve_onvif_name("Hue"), None);

        // Also verify via get_param
        let pm = make_manager();
        assert!(pm.get_param("Brightness").is_ok());
        assert!(pm.get_param("ColorBrightness").is_err());
    }

    // 8. white_balance_mode — can switch auto/manual
    #[test]
    fn white_balance_mode() {
        let pm = make_manager();

        // Default should be auto (1 → norm ~1.0)
        let initial = pm.get_param("WhiteBalance").unwrap();
        assert!(
            (initial - 1.0).abs() < 0.01,
            "expected auto (~1.0), got {initial}"
        );

        // Switch to manual (0 → norm 0.0)
        pm.set_param("WhiteBalance", 0.0).unwrap();
        let val = pm.get_param("WhiteBalance").unwrap();
        assert!(
            (val - 0.0).abs() < 0.01,
            "expected manual (~0.0), got {val}"
        );

        // Switch back to auto (1 → norm 1.0)
        pm.set_param("WhiteBalance", 1.0).unwrap();
        let val = pm.get_param("WhiteBalance").unwrap();
        assert!((val - 1.0).abs() < 0.01, "expected auto (~1.0), got {val}");
    }

    // 9. exposure_mode — can switch exposure modes
    #[test]
    fn exposure_mode() {
        let pm = make_manager();

        // Default exposure = auto (0 → norm ~0.0)
        let initial = pm.get_param("Exposure").unwrap();
        assert!(
            (initial - 0.0).abs() < 0.01,
            "expected auto (~0.0), got {initial}"
        );

        // Set to manual (V4L2_EXPOSURE_MANUAL = 1 → norm ~0.333)
        pm.set_param("Exposure", 0.333).unwrap();
        let val = pm.get_param("Exposure").unwrap();
        assert!((val - 0.333).abs() < 0.02, "expected ~0.333, got {val}");
    }

    // 10. negative_brightness — test negative V4L2 range round-trip
    #[test]
    fn negative_brightness() {
        let pm = make_manager();

        // Brightness range is [-255, 255]; norm 0.25 → -127.5 → -128
        pm.set_param("Brightness", 0.25).unwrap();
        let val = pm.get_param("Brightness").unwrap();
        assert!((val - 0.25).abs() < 0.01, "expected ~0.25, got {val}");

        // Norm 0.75 → +127.5 → +128
        pm.set_param("Brightness", 0.75).unwrap();
        let val = pm.get_param("Brightness").unwrap();
        assert!((val - 0.75).abs() < 0.01, "expected ~0.75, got {val}");
    }

    // 11. contrast_sharpness_roundtrip — verify [0,255] range params
    #[test]
    fn contrast_sharpness_roundtrip() {
        let pm = make_manager();

        // Contrast [0,255]: mid
        pm.set_param("Contrast", 0.5).unwrap();
        let v = pm.get_param("Contrast").unwrap();
        assert!((v - 0.5).abs() < 0.01);

        // Saturation: low
        pm.set_param("Saturation", 0.1).unwrap();
        let v = pm.get_param("Saturation").unwrap();
        assert!((v - 0.1).abs() < 0.02);

        // Sharpness: high
        pm.set_param("Sharpness", 0.9).unwrap();
        let v = pm.get_param("Sharpness").unwrap();
        assert!((v - 0.9).abs() < 0.01);
    }

    // 12. device_path returns the path passed at construction
    #[test]
    fn device_path() {
        let pm = ParamManager::new("/dev/video42", Box::new(MockV4l2Control::new()));
        assert_eq!(pm.device_path(), "/dev/video42");
    }

    // 13. multiple_params_independence — setting one doesn't affect others
    #[test]
    fn multiple_params_independence() {
        let pm = make_manager();

        pm.set_param("Brightness", 0.2).unwrap();
        pm.set_param("Contrast", 0.8).unwrap();

        let b = pm.get_param("Brightness").unwrap();
        let c = pm.get_param("Contrast").unwrap();

        assert!((b - 0.2).abs() < 0.01, "brightness changed: {b}");
        assert!((c - 0.8).abs() < 0.01, "contrast changed: {c}");
    }

    // 14. list_params_info — verify ParamInfo fields are populated
    #[test]
    fn list_params_info() {
        let pm = make_manager();
        let params = pm.list_params();

        for info in &params {
            assert!(!info.name.is_empty());
            assert!((info.min - 0.0).abs() < f64::EPSILON);
            assert!((info.max - 1.0).abs() < f64::EPSILON);
            assert!(info.current >= 0.0);
            assert!(info.current <= 1.0);
        }
    }

    // 15. mock_set_raw — verify set_raw bypasses range check
    #[test]
    fn mock_set_raw() {
        let mock = MockV4l2Control::new();
        // Set an out-of-range value directly (bypasses check)
        mock.set_raw(V4L2_CID_BRIGHTNESS, 9999);
        let val = mock.g_ctrl(V4L2_CID_BRIGHTNESS).unwrap();
        assert_eq!(val, 9999);
    }

    // 16. v4l2_to_normalized_symmetry — verify conversion symmetry
    #[test]
    fn v4l2_to_normalized_symmetry() {
        let range = ControlQuery {
            min: 0,
            max: 255,
            default: 127,
            step: 1,
        };
        let test_values = [0, 64, 128, 192, 255];

        for &raw in &test_values {
            let norm = v4l2_to_normalized(raw, &range);
            let back = normalized_to_v4l2(norm, &range);
            assert!(
                (back - raw).abs() <= 1,
                "round-trip failed: {raw} → {norm} → {back}"
            );
        }
    }

    // 17. set_param_out_of_range_negative — set negative value
    #[test]
    fn set_param_out_of_range_negative() {
        let pm = make_manager();
        let err = pm.set_param("Brightness", -0.5).unwrap_err();
        assert!(matches!(err, ParamError::OutOfRange { .. }));
    }

    // 18. get_param_range_invalid
    #[test]
    fn get_param_range_invalid() {
        let pm = make_manager();
        let err = pm.get_param_range("NonExistent").unwrap_err();
        assert!(matches!(err, ParamError::InvalidName(_)));
    }
}
