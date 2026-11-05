//! Thermal subsystem error type (module-local — the camera pipeline has
//! its own `CameraError`; this one never crosses into it).

use std::fmt;

#[derive(Debug)]
pub enum ThermalError {
    /// Underlying bus transport failed (errno or protocol NAK).
    Bus(String),
    /// Sensor returned data that violates the MLX90640 register map.
    Sensor(String),
    /// Bad usage / configuration.
    Usage(String),
    Io(std::io::Error),
}

impl fmt::Display for ThermalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ThermalError::Bus(m) => write!(f, "i2c bus: {m}"),
            ThermalError::Sensor(m) => write!(f, "mlx90640: {m}"),
            ThermalError::Usage(m) => write!(f, "usage: {m}"),
            ThermalError::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for ThermalError {}

impl From<std::io::Error> for ThermalError {
    fn from(e: std::io::Error) -> Self {
        ThermalError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, ThermalError>;

/// The ported MLX90640 stack references `Error::…` — keep that spelling.
pub use ThermalError as Error;
