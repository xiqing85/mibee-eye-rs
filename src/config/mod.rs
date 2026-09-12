use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur during configuration loading / validation.
#[derive(Debug)]
pub enum ConfigError {
    /// I/O error reading the config file.
    Io(std::io::Error),
    /// TOML parse error.
    Parse(String),
    /// Validation constraint violation.
    Validation(String),
    /// Environment variable error.
    EnvVar(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "I/O error: {e}"),
            ConfigError::Parse(e) => write!(f, "TOML parse error: {e}"),
            ConfigError::Validation(e) => write!(f, "Validation error: {e}"),
            ConfigError::EnvVar(e) => write!(f, "Environment variable error: {e}"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::Io(e)
    }
}

impl From<toml::de::Error> for ConfigError {
    fn from(e: toml::de::Error) -> Self {
        ConfigError::Parse(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Default-value helper functions
// ---------------------------------------------------------------------------

fn default_device() -> String {
    "/dev/video0".to_string()
}
fn default_mode() -> String {
    "mtxrpicam".to_string()
}
fn default_camera_width() -> u32 {
    1280
}
fn default_camera_height() -> u32 {
    720
}
fn default_fps() -> u32 {
    15
}
fn default_codec() -> String {
    "h264".to_string()
}
fn default_bitrate() -> u64 {
    2_000_000
}
fn default_brightness() -> f64 {
    0.0
}
fn default_contrast() -> f64 {
    1.0
}
fn default_saturation() -> f64 {
    1.0
}
fn default_sharpness() -> f64 {
    1.0
}
fn default_rtsp_port() -> u16 {
    8554
}
fn default_onvif_port() -> u16 {
    8080
}
fn default_onvif_username() -> String {
    "admin".to_string()
}
fn default_web_enabled() -> bool {
    true
}
fn default_web_port() -> u16 {
    8088
}
fn default_tls_enabled() -> bool {
    false
}

fn default_logging_level() -> String {
    "info".to_string()
}
fn default_retention_days() -> u32 {
    30
}
fn default_motion_sensitivity() -> f64 {
    0.5
}
fn default_motion_min_area() -> u32 {
    100
}
fn default_cooldown_ms() -> u64 {
    1000
}
pub(crate) fn default_ai_model_path() -> String {
    "/var/lib/mibee-eye/models/nanodet-m.onnx".to_string()
}
fn default_ai_model() -> String {
    "nanodet-plus-m-320".to_string()
}
fn default_ai_confidence_threshold() -> f32 {
    0.5
}
fn default_ai_interval_frames() -> u32 {
    5
}
fn default_ai_max_memory_mb() -> u32 {
    256
}
fn default_ai_cpu_cores() -> Vec<u32> {
    vec![2, 3]
}
fn default_recording_enabled() -> bool {
    false
}
fn default_recording_storage_path() -> String {
    "recordings".to_string()
}
fn default_recording_segment_secs() -> u64 {
    600
}
fn default_recording_retention_days() -> u32 {
    3
}
fn default_recording_max_storage_mb() -> u64 {
    8192
}
fn default_watermark_show_timestamp() -> bool {
    true
}
fn default_watermark_timestamp_format() -> String {
    "%Y-%m-%d %H:%M:%S".to_string()
}
fn default_watermark_position() -> Position {
    Position::TopLeft
}
fn default_watermark_font_size() -> u32 {
    24
}

// ---------------------------------------------------------------------------
// Section structs
// ---------------------------------------------------------------------------

/// Camera capture settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraConfig {
    #[serde(default = "default_device")]
    pub device: String,
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default)]
    pub rtsp_url: String,
    #[serde(default = "default_camera_width")]
    pub width: u32,
    #[serde(default = "default_camera_height")]
    pub height: u32,
    #[serde(default = "default_fps")]
    pub fps: u32,
    #[serde(default = "default_codec")]
    pub codec: String,
    #[serde(default = "default_bitrate")]
    pub bitrate: u64,
    #[serde(default = "default_brightness")]
    pub brightness: f64,
    #[serde(default = "default_contrast")]
    pub contrast: f64,
    #[serde(default = "default_saturation")]
    pub saturation: f64,
    #[serde(default = "default_sharpness")]
    pub sharpness: f64,
    /// Image rotation in degrees (0 or 180). Applied via CSS transform on the web UI.
    /// Does NOT touch V4L2 pipeline — avoids Bayer pattern corruption.
    #[serde(default)]
    pub rotation: u32,
    /// Device-level horizontal mirror applied to the captured YUV frames
    /// before encoding — affects every consumer (RTSP, ONVIF, GB28181,
    /// recordings, snapshots, AI) on every client, persistently.
    #[serde(default)]
    pub hflip: bool,
    /// Device-level vertical flip (upside-down mount compensation); applied
    /// like `hflip` on the post-ISP YUV planes, so it is safe for
    /// Bayer-based sensors.
    #[serde(default)]
    pub vflip: bool,
}

/// RTSP server settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RTSPConfig {
    #[serde(default = "default_rtsp_port")]
    pub port: u16,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
}

/// ONVIF server settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ONVIFConfig {
    #[serde(default = "default_onvif_port")]
    pub port: u16,
    #[serde(default = "default_onvif_username")]
    pub username: String,
    #[serde(default)]
    pub password: String,
}

// GB28181 device configuration lives in the `gb28181-rs` crate. This
// wrapper flattens the library's struct (TOML shape unchanged for every
// existing key) and adds the product-side `gb28181.gb35114` section;
// `Deref` keeps plain field access (`config.gb28181.device_id`) working.
pub use gb28181_rs::config::Transport;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Gb28181Config {
    #[serde(flatten)]
    pub lib: gb28181_rs::config::Gb28181Config,
    /// GB 35114 A-level security (default off; needs the `gb35114` cargo
    /// feature and pre-provisioned SM2 certificates).
    #[serde(default)]
    pub gb35114: Gb35114Config,
}

impl std::ops::Deref for Gb28181Config {
    type Target = gb28181_rs::config::Gb28181Config;
    fn deref(&self) -> &Self::Target {
        &self.lib
    }
}

impl std::ops::DerefMut for Gb28181Config {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.lib
    }
}

/// GB 35114 A-level security settings. When enabled, REGISTER
/// authentication switches from SIP Digest to SM2-certificate mutual
/// authentication; a build without the `gb35114` feature logs a warning
/// and falls back to Digest.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Gb35114Config {
    pub enabled: bool,
    /// Device SM2 signing certificate (PEM).
    pub device_cert_file: String,
    /// Device SM2 private key (SEC1 or PKCS#8 PEM).
    pub device_key_file: String,
    /// Platform signing certificate — verifies sign2 (Bidirection).
    pub platform_cert_file: String,
    /// 20-digit SIP server ID being authenticated to.
    pub server_id: String,
}

/// Web UI server settings.
///
/// When username / password are empty the web server reuses ONVIF credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebConfig {
    #[serde(default = "default_web_enabled")]
    pub enabled: bool,
    #[serde(default = "default_web_port")]
    pub port: u16,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default = "default_tls_enabled")]
    pub tls_enabled: bool,
}

// ONVIF device identity lives in the `onvif-rs` crate; re-exported so
// `config::DeviceConfig` keeps resolving and TOML shapes are unchanged.
pub use onvif_device_rs::DeviceConfig;

/// Logging settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    #[serde(default = "default_logging_level")]
    pub level: String,
}

/// Local storage path settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalStorageConfig {
    #[serde(default)]
    pub path: String,
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
}

/// Storage (recording) settings.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StorageConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub local: LocalStorageConfig,
}

/// Local recording settings (GB28181 playback).
///
/// Segments are written as bare Annex-B H.264 files under
/// `storage_path/YYYY-MM-DD/HH/MMSS.h264` with an append-only
/// `index.jsonl` — disk layout identical to the Go repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingConfig {
    #[serde(default = "default_recording_enabled")]
    pub enabled: bool,
    #[serde(default = "default_recording_storage_path")]
    pub storage_path: String,
    #[serde(default = "default_recording_segment_secs")]
    pub segment_secs: u64,
    #[serde(default = "default_recording_retention_days")]
    pub retention_days: u32,
    #[serde(default = "default_recording_max_storage_mb")]
    pub max_storage_mb: u64,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            enabled: default_recording_enabled(),
            storage_path: default_recording_storage_path(),
            segment_secs: default_recording_segment_secs(),
            retention_days: default_recording_retention_days(),
            max_storage_mb: default_recording_max_storage_mb(),
        }
    }
}

/// Watermark position on the frame (SPEC §5.2; kebab-case wire format).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Position {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// Video watermark settings (SPEC §5.2). Burned into the pre-encode I420
/// frames like the device-level flips — every consumer (RTSP, ONVIF,
/// GB28181, recordings, snapshots, AI) sees it. Restart to apply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatermarkConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Custom text (≤128 chars); CJK requires a `font_path` with CJK glyphs.
    #[serde(default)]
    pub text: String,
    #[serde(default = "default_watermark_show_timestamp")]
    pub show_timestamp: bool,
    /// strftime subset whitelist: %Y %m %d %H %M %S %F %T %% + literals.
    #[serde(default = "default_watermark_timestamp_format")]
    pub timestamp_format: String,
    #[serde(default = "default_watermark_position")]
    pub position: Position,
    /// Pixel height, 12..96.
    #[serde(default = "default_watermark_font_size")]
    pub font_size: u32,
    /// Optional TTF/OTF (e.g. CJK); empty = embedded ASCII subset font.
    /// Load failures fall back to the embedded font with a warning.
    #[serde(default)]
    pub font_path: String,
}

impl Default for WatermarkConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            text: String::new(),
            show_timestamp: default_watermark_show_timestamp(),
            timestamp_format: default_watermark_timestamp_format(),
            position: default_watermark_position(),
            font_size: default_watermark_font_size(),
            font_path: String::new(),
        }
    }
}

/// Motion detection settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MotionConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_motion_sensitivity")]
    pub sensitivity: f64,
    #[serde(default = "default_motion_min_area")]
    pub min_area: u32,
    #[serde(default = "default_cooldown_ms")]
    pub cooldown_ms: u64,
}

/// RTMP push settings.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RtmpConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub url: String,
}

// Feature sub-configs (all default to disabled).

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiFeatureConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Registry id of the model to load at startup (SPEC §4.6; resolved via
    /// `ai::registry`). A non-default `model_path` overrides it.
    #[serde(default = "default_ai_model")]
    pub model: String,
    /// Allow runtime model uploads (SPEC §4.6 capability `ai_upload`).
    /// Model files are untrusted input to the inference engine — keep off
    /// unless needed.
    #[serde(default)]
    pub allow_upload: bool,
    #[serde(default = "default_ai_model_path")]
    pub model_path: String,
    #[serde(default = "default_ai_confidence_threshold")]
    pub confidence_threshold: f32,
    #[serde(default = "default_ai_interval_frames")]
    pub interval_frames: u32,
    #[serde(default = "default_ai_max_memory_mb")]
    pub max_memory_mb: u32,
    #[serde(default = "default_ai_cpu_cores")]
    pub cpu_cores: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MultiCameraFeatureConfig {
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WebRtcFeatureConfig {
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct H265FeatureConfig {
    #[serde(default)]
    pub enabled: bool,
}

/// Feature flags (all disabled by default).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FeaturesConfig {
    #[serde(default)]
    pub ai: AiFeatureConfig,
    #[serde(default)]
    pub multi_camera: MultiCameraFeatureConfig,
    #[serde(default)]
    pub webrtc: WebRtcFeatureConfig,
    #[serde(default)]
    pub h265: H265FeatureConfig,
}

// ---------------------------------------------------------------------------
// Top-level Config
// ---------------------------------------------------------------------------

/// Top-level application configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub camera: CameraConfig,
    #[serde(default)]
    pub rtsp: RTSPConfig,
    #[serde(default)]
    pub onvif: ONVIFConfig,
    #[serde(default)]
    pub gb28181: Gb28181Config,

    #[serde(default)]
    pub web: WebConfig,
    #[serde(default = "default_device_config")]
    pub device: DeviceConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub recording: RecordingConfig,
    #[serde(default)]
    pub watermark: WatermarkConfig,
    #[serde(default)]
    pub motion: MotionConfig,
    #[serde(default)]
    pub rtmp: RtmpConfig,
    #[serde(default)]
    pub features: FeaturesConfig,
}

// ---------------------------------------------------------------------------
// Default implementations
// ---------------------------------------------------------------------------

impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            device: default_device(),
            mode: default_mode(),
            rtsp_url: String::new(),
            width: default_camera_width(),
            height: default_camera_height(),
            fps: default_fps(),
            codec: default_codec(),
            bitrate: default_bitrate(),
            brightness: default_brightness(),
            contrast: default_contrast(),
            saturation: default_saturation(),
            sharpness: default_sharpness(),
            rotation: 0,
            hflip: false,
            vflip: false,
        }
    }
}

impl Default for RTSPConfig {
    fn default() -> Self {
        Self {
            port: default_rtsp_port(),
            username: String::new(),
            password: String::new(),
        }
    }
}

impl Default for ONVIFConfig {
    fn default() -> Self {
        Self {
            port: default_onvif_port(),
            username: default_onvif_username(),
            password: String::new(),
        }
    }
}
impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: default_web_enabled(),
            port: default_web_port(),
            username: String::new(),
            password: String::new(),
            tls_enabled: default_tls_enabled(),
        }
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_logging_level(),
        }
    }
}

impl Default for LocalStorageConfig {
    fn default() -> Self {
        Self {
            path: String::new(),
            retention_days: default_retention_days(),
        }
    }
}

impl Default for MotionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sensitivity: default_motion_sensitivity(),
            min_area: default_motion_min_area(),
            cooldown_ms: default_cooldown_ms(),
        }
    }
}
impl Default for AiFeatureConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: default_ai_model(),
            allow_upload: false,
            model_path: default_ai_model_path(),
            confidence_threshold: default_ai_confidence_threshold(),
            interval_frames: default_ai_interval_frames(),
            max_memory_mb: default_ai_max_memory_mb(),
            cpu_cores: default_ai_cpu_cores(),
        }
    }
}
// ---------------------------------------------------------------------------
// Config methods
// ---------------------------------------------------------------------------

impl Config {
    /// Create a new `Config` with default values.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Load configuration from a TOML file.
    ///
    /// 1. Reads the TOML file (returns an error if the file does not exist).
    /// 2. Merges file values over the built-in defaults.
    /// 3. Applies `MIBEE_EYE_` prefixed environment variable overrides.
    /// 4. Validates all fields.
    /// 5. Issues a warning if the ONVIF password is empty.
    ///
    /// # Errors
    ///
    /// Returns `ConfigError::Io` if the file cannot be read,
    /// `ConfigError::Parse` if the TOML is invalid,
    /// or `ConfigError::Validation` if any constraint is violated.
    pub fn load(path: &str) -> Result<Config, ConfigError> {
        let contents = fs::read_to_string(path)?;
        let mut config: Config = toml::from_str(&contents)?;
        apply_env_overrides(&mut config);
        backfill_device_identity(&mut config.device);

        if config.onvif.password.is_empty() {
            eprintln!(
                "WARNING: ONVIF password is empty. \
                 Set `onvif.password` in config or `MIBEE_EYE_ONVIF_PASSWORD` env var"
            );
        }

        config.validate()?;
        Ok(config)
    }

    /// Validate all configuration fields.
    ///
    /// # Errors
    ///
    /// Returns `ConfigError::Validation` if any constraint is violated.
    pub fn validate(&self) -> Result<(), ConfigError> {
        // --- camera ---
        if self.camera.fps == 0 {
            return Err(ConfigError::Validation(
                "camera.fps must be positive".into(),
            ));
        }
        if self.camera.width == 0 {
            return Err(ConfigError::Validation(
                "camera.width must be positive".into(),
            ));
        }
        if self.camera.height == 0 {
            return Err(ConfigError::Validation(
                "camera.height must be positive".into(),
            ));
        }
        if self.camera.bitrate == 0 {
            return Err(ConfigError::Validation(
                "camera.bitrate must be positive".into(),
            ));
        }
        let b = self.camera.brightness;
        if !(-1.0..=1.0).contains(&b) {
            return Err(ConfigError::Validation(format!(
                "camera.brightness out of range [-1.0, 1.0]: {b}"
            )));
        }
        let c = self.camera.contrast;
        if !(0.0..=32.0).contains(&c) {
            return Err(ConfigError::Validation(format!(
                "camera.contrast out of range [0.0, 32.0]: {c}"
            )));
        }
        let s = self.camera.saturation;
        if !(0.0..=32.0).contains(&s) {
            return Err(ConfigError::Validation(format!(
                "camera.saturation out of range [0.0, 32.0]: {s}"
            )));
        }
        let s = self.camera.sharpness;
        if !(0.0..=16.0).contains(&s) {
            return Err(ConfigError::Validation(format!(
                "camera.sharpness out of range [0.0, 16.0]: {s}"
            )));
        }
        match self.camera.codec.as_str() {
            "h264" | "h265" => {}
            _ => {
                return Err(ConfigError::Validation(format!(
                    "camera.codec must be h264 or h265, got: {}",
                    self.camera.codec
                )));
            }
        }

        // --- rtsp ---
        if self.rtsp.port == 0 {
            return Err(ConfigError::Validation("rtsp.port must be positive".into()));
        }

        // --- onvif ---
        if self.onvif.port == 0 {
            return Err(ConfigError::Validation(
                "onvif.port must be positive".into(),
            ));
        }

        // --- web ---
        if self.web.enabled && self.web.port == 0 {
            return Err(ConfigError::Validation(
                "web.port must be positive when web is enabled".into(),
            ));
        }

        // --- features.ai ---
        // Id EXISTENCE is deferred to the runtime registry (uploaded models
        // only live there, and config validation runs before the registry
        // loads); malformed ids are rejected outright.
        if self.features.ai.model_path == default_ai_model_path()
            && !crate::ai::registry::valid_model_id(&self.features.ai.model)
        {
            return Err(ConfigError::Validation(format!(
                "features.ai.model must match ^[a-z0-9][a-z0-9-]{{0,63}}$, got '{}'",
                self.features.ai.model
            )));
        }

        // --- logging ---
        match self.logging.level.as_str() {
            "debug" | "info" | "warn" | "error" => {}
            _ => {
                return Err(ConfigError::Validation(format!(
                    "logging.level must be debug, info, warn, or error, got: {}",
                    self.logging.level
                )));
            }
        }

        // --- recording ---
        if self.recording.enabled && self.recording.segment_secs < 60 {
            return Err(ConfigError::Validation(format!(
                "recording.segment_secs must be >= 60, got: {}",
                self.recording.segment_secs
            )));
        }

        // --- watermark (SPEC §5.2) ---
        let wm = &self.watermark;
        if !(12..=96).contains(&wm.font_size) {
            return Err(ConfigError::Validation(format!(
                "watermark.font_size must be in 12..=96, got: {}",
                wm.font_size
            )));
        }
        if wm.text.chars().count() > 128 {
            return Err(ConfigError::Validation(
                "watermark.text must be at most 128 characters".into(),
            ));
        }
        if wm.enabled && wm.text.is_empty() && !wm.show_timestamp {
            return Err(ConfigError::Validation(
                "watermark.enabled requires text or show_timestamp".into(),
            ));
        }
        if !crate::watermark::valid_timestamp_format(&wm.timestamp_format) {
            return Err(ConfigError::Validation(format!(
                "watermark.timestamp_format only allows %Y %m %d %H %M %S %F %T %% and literals, got: {}",
                wm.timestamp_format
            )));
        }

        Ok(())
    }

    /// Short-hand: returns a reference to the ONVIF password.
    #[must_use]
    pub fn onvif_password(&self) -> &str {
        &self.onvif.password
    }
}

// ---------------------------------------------------------------------------
// Environment variable overrides (MIBEE_EYE_ prefix)
// ---------------------------------------------------------------------------

/// Apply `MIBEE_EYE_` prefixed environment variable overrides to the config.
// Manual Default (not derived): `device` must go through the backfilled
// default so every construction path — Config::default() (the no-config
// fallback in main), load(), and bare toml::from_str — advertises the
// documented identity, which onvif-device-rs 0.6 validates.
impl Default for Config {
    fn default() -> Self {
        Self {
            camera: CameraConfig::default(),
            rtsp: RTSPConfig::default(),
            onvif: ONVIFConfig::default(),
            gb28181: Gb28181Config::default(),
            web: WebConfig::default(),
            device: default_device_config(),
            logging: LoggingConfig::default(),
            storage: StorageConfig::default(),
            recording: RecordingConfig::default(),
            watermark: WatermarkConfig::default(),
            motion: MotionConfig::default(),
            rtmp: RtmpConfig::default(),
            features: FeaturesConfig::default(),
        }
    }
}

/// The documented `[device]` defaults ("Pi Camera V1" / "Raspberry Pi" /
/// "OV5647") — the library's own serde defaults are the neutral
/// placeholders, which its 0.6 identity validation rejects.
fn default_device_config() -> DeviceConfig {
    let mut device = DeviceConfig::default();
    backfill_device_identity(&mut device);
    device
}

/// Restore the `[device]` defaults config.example.toml has always
/// documented ("Pi Camera V1" / "Raspberry Pi" / "OV5647"). onvif-device-rs
/// 0.3.x shipped exactly these as its serde defaults; the library later
/// neutralized them ("unknown" / "ONVIF Device") and 0.6 fail-closes on
/// the neutral placeholders (issue #20). Hosts that omit `[device]` — or
/// leave fields at the library defaults — keep the documented identity,
/// which the 0.6 identity validation accepts.
fn backfill_device_identity(device: &mut DeviceConfig) {
    if device.name == "ONVIF Device" {
        device.name = "Pi Camera V1".to_string();
    }
    if device.manufacturer == "unknown" {
        device.manufacturer = "Raspberry Pi".to_string();
    }
    if device.model == "unknown" {
        device.model = "OV5647".to_string();
    }
    if device.hardware_id == "unknown" {
        device.hardware_id = "OV5647".to_string();
    }
}

fn apply_env_overrides(config: &mut Config) {
    // --- camera ---
    override_str("MIBEE_EYE_CAMERA_DEVICE", &mut config.camera.device);
    override_str("MIBEE_EYE_CAMERA_MODE", &mut config.camera.mode);
    override_str("MIBEE_EYE_CAMERA_RTSP_URL", &mut config.camera.rtsp_url);
    override_int("MIBEE_EYE_CAMERA_WIDTH", &mut config.camera.width);
    override_int("MIBEE_EYE_CAMERA_HEIGHT", &mut config.camera.height);
    override_int("MIBEE_EYE_CAMERA_FPS", &mut config.camera.fps);
    override_str("MIBEE_EYE_CAMERA_CODEC", &mut config.camera.codec);
    override_int("MIBEE_EYE_CAMERA_BITRATE", &mut config.camera.bitrate);
    override_float("MIBEE_EYE_CAMERA_BRIGHTNESS", &mut config.camera.brightness);
    override_float("MIBEE_EYE_CAMERA_CONTRAST", &mut config.camera.contrast);
    override_float("MIBEE_EYE_CAMERA_SATURATION", &mut config.camera.saturation);
    override_float("MIBEE_EYE_CAMERA_SHARPNESS", &mut config.camera.sharpness);

    // --- rtsp ---
    override_int("MIBEE_EYE_RTSP_PORT", &mut config.rtsp.port);
    override_str("MIBEE_EYE_RTSP_USERNAME", &mut config.rtsp.username);
    override_str("MIBEE_EYE_RTSP_PASSWORD", &mut config.rtsp.password);

    // --- onvif ---
    override_int("MIBEE_EYE_ONVIF_PORT", &mut config.onvif.port);
    override_str("MIBEE_EYE_ONVIF_USERNAME", &mut config.onvif.username);
    override_str("MIBEE_EYE_ONVIF_PASSWORD", &mut config.onvif.password);
    // --- gb28181 ---
    override_bool("MIBEE_EYE_GB28181_ENABLED", &mut config.gb28181.enabled);
    override_str(
        "MIBEE_EYE_GB28181_PLATFORM_SIP_ADDRESS",
        &mut config.gb28181.platform_sip_address,
    );
    override_int(
        "MIBEE_EYE_GB28181_PLATFORM_SIP_PORT",
        &mut config.gb28181.platform_sip_port,
    );
    override_str("MIBEE_EYE_GB28181_DEVICE_ID", &mut config.gb28181.device_id);
    override_str(
        "MIBEE_EYE_GB28181_CHANNEL_ID",
        &mut config.gb28181.channel_id,
    );
    override_str(
        "MIBEE_EYE_GB28181_SIP_DOMAIN",
        &mut config.gb28181.sip_domain,
    );
    override_str("MIBEE_EYE_GB28181_PASSWORD", &mut config.gb28181.password);
    override_bool(
        "MIBEE_EYE_GB28181_GB35114_ENABLED",
        &mut config.gb28181.gb35114.enabled,
    );
    override_str(
        "MIBEE_EYE_GB28181_GB35114_DEVICE_CERT_FILE",
        &mut config.gb28181.gb35114.device_cert_file,
    );
    override_str(
        "MIBEE_EYE_GB28181_GB35114_DEVICE_KEY_FILE",
        &mut config.gb28181.gb35114.device_key_file,
    );
    override_str(
        "MIBEE_EYE_GB28181_GB35114_PLATFORM_CERT_FILE",
        &mut config.gb28181.gb35114.platform_cert_file,
    );
    override_str(
        "MIBEE_EYE_GB28181_GB35114_SERVER_ID",
        &mut config.gb28181.gb35114.server_id,
    );
    override_int(
        "MIBEE_EYE_GB28181_LOCAL_SIP_PORT",
        &mut config.gb28181.local_sip_port,
    );
    override_int(
        "MIBEE_EYE_GB28181_REGISTER_INTERVAL_SECS",
        &mut config.gb28181.register_interval_secs,
    );
    override_int(
        "MIBEE_EYE_GB28181_HEARTBEAT_INTERVAL_SECS",
        &mut config.gb28181.heartbeat_interval_secs,
    );
    override_int(
        "MIBEE_EYE_GB28181_HEARTBEAT_TIMEOUT_COUNT",
        &mut config.gb28181.heartbeat_timeout_count,
    );
    if let Ok(v) = std::env::var("MIBEE_EYE_GB28181_TRANSPORT") {
        config.gb28181.transport = match v.to_lowercase().as_str() {
            "udp" => Transport::Udp,
            "tcp" => Transport::Tcp,
            _ => {
                eprintln!(
                    "warning: invalid MIBEE_EYE_GB28181_TRANSPORT value '{}', using udp",
                    v
                );
                Transport::Udp
            }
        };
    }

    // --- recording ---
    override_bool("MIBEE_EYE_RECORDING_ENABLED", &mut config.recording.enabled);
    override_str(
        "MIBEE_EYE_RECORDING_STORAGE_PATH",
        &mut config.recording.storage_path,
    );
    override_int(
        "MIBEE_EYE_RECORDING_SEGMENT_SECS",
        &mut config.recording.segment_secs,
    );
    override_int(
        "MIBEE_EYE_RECORDING_RETENTION_DAYS",
        &mut config.recording.retention_days,
    );
    override_int(
        "MIBEE_EYE_RECORDING_MAX_STORAGE_MB",
        &mut config.recording.max_storage_mb,
    );

    // --- watermark ---
    override_bool("MIBEE_EYE_WATERMARK_ENABLED", &mut config.watermark.enabled);
    override_str("MIBEE_EYE_WATERMARK_TEXT", &mut config.watermark.text);
    override_bool(
        "MIBEE_EYE_WATERMARK_SHOW_TIMESTAMP",
        &mut config.watermark.show_timestamp,
    );
    override_str(
        "MIBEE_EYE_WATERMARK_TIMESTAMP_FORMAT",
        &mut config.watermark.timestamp_format,
    );
    override_int(
        "MIBEE_EYE_WATERMARK_FONT_SIZE",
        &mut config.watermark.font_size,
    );
    override_str(
        "MIBEE_EYE_WATERMARK_FONT_PATH",
        &mut config.watermark.font_path,
    );
    if let Ok(v) = std::env::var("MIBEE_EYE_WATERMARK_POSITION") {
        config.watermark.position = match v.as_str() {
            "top-left" => Position::TopLeft,
            "top-right" => Position::TopRight,
            "bottom-left" => Position::BottomLeft,
            "bottom-right" => Position::BottomRight,
            _ => {
                eprintln!(
                    "warning: invalid MIBEE_EYE_WATERMARK_POSITION value '{v}', using top-left"
                );
                Position::TopLeft
            }
        };
    }

    // --- web ---
    override_bool("MIBEE_EYE_WEB_ENABLED", &mut config.web.enabled);
    override_int("MIBEE_EYE_WEB_PORT", &mut config.web.port);
    override_str("MIBEE_EYE_WEB_USERNAME", &mut config.web.username);
    override_str("MIBEE_EYE_WEB_PASSWORD", &mut config.web.password);
    override_bool("MIBEE_EYE_WEB_TLS_ENABLED", &mut config.web.tls_enabled);

    // --- device ---
    override_str("MIBEE_EYE_DEVICE_NAME", &mut config.device.name);
    override_str(
        "MIBEE_EYE_DEVICE_MANUFACTURER",
        &mut config.device.manufacturer,
    );
    override_str("MIBEE_EYE_DEVICE_MODEL", &mut config.device.model);
    override_str("MIBEE_EYE_DEVICE_FIRMWARE", &mut config.device.firmware);
    override_str(
        "MIBEE_EYE_DEVICE_HARDWAREID",
        &mut config.device.hardware_id,
    );
    override_str(
        "MIBEE_EYE_DEVICE_SERIALNUMBER",
        &mut config.device.serial_number,
    );

    // --- logging ---
    override_str("MIBEE_EYE_LOGGING_LEVEL", &mut config.logging.level);
}

fn override_str(name: &str, dest: &mut String) {
    if let Ok(val) = std::env::var(name) {
        *dest = val;
    }
}

fn override_int<T: std::str::FromStr>(name: &str, dest: &mut T) {
    if let Ok(val) = std::env::var(name) {
        if let Ok(parsed) = val.parse::<T>() {
            *dest = parsed;
        }
    }
}

fn override_float(name: &str, dest: &mut f64) {
    if let Ok(val) = std::env::var(name) {
        if let Ok(parsed) = val.parse::<f64>() {
            *dest = parsed;
        }
    }
}

fn override_bool(name: &str, dest: &mut bool) {
    if let Ok(val) = std::env::var(name) {
        if let Ok(parsed) = val.parse::<bool>() {
            *dest = parsed;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::LazyLock;
    use std::sync::Mutex;

    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// Helper: write a TOML string to a temporary file and return its path.
    fn temp_config(content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mibee_eye_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("config_{}.toml", content.len()));
        let mut f = std::fs::File::create(&path).unwrap();
        write!(f, "{content}").unwrap();
        path
    }

    // ------------------------------------------------------------------
    // Defaults
    // ------------------------------------------------------------------

    #[test]
    fn test_default_values() {
        let cfg = Config::default();
        // camera
        assert_eq!(cfg.camera.device, "/dev/video0");
        assert_eq!(cfg.camera.mode, "mtxrpicam");
        assert_eq!(cfg.camera.rtsp_url, "");
        assert_eq!(cfg.camera.width, 1280);
        assert_eq!(cfg.camera.height, 720);
        assert_eq!(cfg.camera.fps, 15);
        assert_eq!(cfg.camera.codec, "h264");
        assert_eq!(cfg.camera.bitrate, 2_000_000);
        assert!((cfg.camera.brightness - 0.0).abs() < 1e-9);
        assert!((cfg.camera.contrast - 1.0).abs() < 1e-9);
        assert!((cfg.camera.saturation - 1.0).abs() < 1e-9);
        assert!((cfg.camera.sharpness - 1.0).abs() < 1e-9);
        assert!(!cfg.camera.hflip);
        assert!(!cfg.camera.vflip);
        // rtsp
        assert_eq!(cfg.rtsp.port, 8554);
        assert_eq!(cfg.rtsp.username, "");
        assert_eq!(cfg.rtsp.password, "");
        // onvif
        assert_eq!(cfg.onvif.port, 8080);
        assert_eq!(cfg.onvif.username, "admin");
        assert_eq!(cfg.onvif.password, "");
        // gb28181
        assert!(!cfg.gb28181.enabled);
        assert_eq!(cfg.gb28181.platform_sip_address, "192.168.1.1");
        assert_eq!(cfg.gb28181.platform_sip_port, 5060);
        assert_eq!(cfg.gb28181.device_id, "34020000001320000001");
        assert_eq!(cfg.gb28181.channel_id, "34020000001320000001");
        assert_eq!(cfg.gb28181.sip_domain, "3402000000");
        // gb28181-rs 0.11 ships no default SIP password (its #26) —
        // unset stays empty; deployment hosts set real values in config.toml.
        assert_eq!(cfg.gb28181.password, "");
        assert_eq!(cfg.gb28181.local_sip_port, 5060);
        assert_eq!(cfg.gb28181.register_interval_secs, 60);
        assert_eq!(cfg.gb28181.heartbeat_interval_secs, 60);
        assert_eq!(cfg.gb28181.heartbeat_timeout_count, 3);

        // web
        assert!(cfg.web.enabled);
        assert_eq!(cfg.web.port, 8088);
        assert_eq!(cfg.web.username, "");
        assert_eq!(cfg.web.password, "");
        // device
        assert_eq!(cfg.device.name, "Pi Camera V1");
        assert_eq!(cfg.device.manufacturer, "Raspberry Pi");
        assert_eq!(cfg.device.model, "OV5647");
        assert_eq!(cfg.device.firmware, "1.0.0");
        assert_eq!(cfg.device.hardware_id, "OV5647");
        assert_eq!(cfg.device.serial_number, "");
        // logging
        assert_eq!(cfg.logging.level, "info");
        // storage (optional section, defaults)
        assert!(!cfg.storage.enabled);
        assert_eq!(cfg.storage.local.path, "");
        assert_eq!(cfg.storage.local.retention_days, 30);
        // motion (optional section, defaults)
        assert!(!cfg.motion.enabled);
        assert!((cfg.motion.sensitivity - 0.5).abs() < 1e-9);
        assert_eq!(cfg.motion.min_area, 100);
        assert_eq!(cfg.motion.cooldown_ms, 1000);
        // rtmp (optional section, defaults)
        assert!(!cfg.rtmp.enabled);
        assert_eq!(cfg.rtmp.url, "");
        // features (optional section, all disabled)
        assert!(!cfg.features.ai.enabled);
        assert!(!cfg.features.multi_camera.enabled);
        assert!(!cfg.features.webrtc.enabled);
        assert!(!cfg.features.h265.enabled);
    }

    #[test]
    fn config_features_disabled_by_default() {
        let cfg = Config::default();
        assert!(!cfg.features.ai.enabled, "AI should be disabled by default");
        assert!(
            !cfg.features.multi_camera.enabled,
            "Multi-camera should be disabled by default"
        );
        assert!(
            !cfg.features.webrtc.enabled,
            "WebRTC should be disabled by default"
        );
        assert!(
            !cfg.features.h265.enabled,
            "H265 should be disabled by default"
        );
    }

    #[test]
    fn test_ai_feature_config_default() {
        let ai = AiFeatureConfig::default();

        assert!(!ai.enabled, "AI should be disabled by default");
        assert_eq!(ai.model, "nanodet-plus-m-320");
        assert_eq!(ai.model_path, "/var/lib/mibee-eye/models/nanodet-m.onnx");
        assert!((ai.confidence_threshold - 0.5f32).abs() < 1e-9);
        assert_eq!(ai.interval_frames, 5);
        assert_eq!(ai.max_memory_mb, 256);
        assert_eq!(ai.cpu_cores, vec![2, 3]);
    }

    #[test]
    fn test_ai_config_malformed_model_id_rejected() {
        let mut cfg = Config::default();
        cfg.features.ai.model = "Bad_ID!".to_string();
        let err = cfg
            .validate()
            .expect_err("malformed ai.model must fail validation");
        assert!(err.to_string().contains("Bad_ID!"));
    }

    #[test]
    fn test_ai_config_unknown_but_wellformed_id_allowed() {
        // Uploaded models only exist in the runtime registry; a well-formed
        // unknown id passes config validation (fail-open at boot).
        let mut cfg = Config::default();
        cfg.features.ai.model = "yolo-9000".to_string();
        cfg.validate().expect("well-formed unknown id must pass");
    }

    #[test]
    fn test_ai_config_any_model_allowed_with_custom_path() {
        let mut cfg = Config::default();
        cfg.features.ai.model = "whatever!".to_string();
        cfg.features.ai.model_path = "/opt/custom.onnx".to_string();
        cfg.validate()
            .expect("custom model_path bypasses the id check");
    }
    #[test]
    fn test_gb28181_config_default() {
        let gb = Gb28181Config::default();
        assert!(!gb.enabled);
        assert_eq!(gb.platform_sip_address, "192.168.1.1");
        assert_eq!(gb.platform_sip_port, 5060);
        assert_eq!(gb.device_id, "34020000001320000001");
        assert_eq!(gb.channel_id, "34020000001320000001");
        assert_eq!(gb.sip_domain, "3402000000");
        // Library default since gb28181-rs 0.11: no default password.
        assert_eq!(gb.password, "");
        assert_eq!(gb.local_sip_port, 5060);
        assert_eq!(gb.register_interval_secs, 60);
        assert_eq!(gb.heartbeat_interval_secs, 60);
        assert_eq!(gb.heartbeat_timeout_count, 3);
    }
    #[test]
    fn test_recording_config_default() {
        let rec = RecordingConfig::default();
        assert!(!rec.enabled);
        assert_eq!(rec.storage_path, "recordings");
        assert_eq!(rec.segment_secs, 600);
        assert_eq!(rec.retention_days, 3);
        assert_eq!(rec.max_storage_mb, 8192);
    }
    #[test]
    fn test_recording_config_toml_parse() {
        let toml_str = r#"
[recording]
enabled = true
storage_path = "/mnt/recordings"
segment_secs = 300
retention_days = 7
max_storage_mb = 4096
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert!(cfg.recording.enabled);
        assert_eq!(cfg.recording.storage_path, "/mnt/recordings");
        assert_eq!(cfg.recording.segment_secs, 300);
        assert_eq!(cfg.recording.retention_days, 7);
        assert_eq!(cfg.recording.max_storage_mb, 4096);
    }
    #[test]
    fn test_recording_validate_segment_secs_min() {
        let mut cfg = Config::default();
        cfg.recording.enabled = true;
        cfg.recording.segment_secs = 30;
        assert!(cfg.validate().is_err());
        cfg.recording.segment_secs = 60;
        assert!(cfg.validate().is_ok());
    }
    #[test]
    fn test_watermark_config_default() {
        let wm = WatermarkConfig::default();
        assert!(!wm.enabled);
        assert_eq!(wm.text, "");
        assert!(wm.show_timestamp);
        assert_eq!(wm.timestamp_format, "%Y-%m-%d %H:%M:%S");
        assert_eq!(wm.position, Position::TopLeft);
        assert_eq!(wm.font_size, 24);
        assert_eq!(wm.font_path, "");
    }
    #[test]
    fn test_watermark_config_toml_parse() {
        let toml_str = r#"
[watermark]
enabled = true
text = "前门"
show_timestamp = true
timestamp_format = "%F %T"
position = "bottom-right"
font_size = 32
font_path = "/usr/local/share/fonts/NotoSansSC-Common.otf"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert!(cfg.watermark.enabled);
        assert_eq!(cfg.watermark.text, "前门");
        assert_eq!(cfg.watermark.position, Position::BottomRight);
        assert_eq!(cfg.watermark.font_size, 32);
        assert!(cfg.validate().is_ok());
        // Round-trips through serialization with kebab-case position.
        let out = toml::to_string(&cfg).unwrap();
        assert!(out.contains(r#"position = "bottom-right""#));
    }
    #[test]
    fn test_watermark_invalid_position_rejected() {
        let toml_str = r#"
[watermark]
position = "middle"
"#;
        let res: Result<Config, _> = toml::from_str(toml_str);
        assert!(res.is_err());
    }
    #[test]
    fn test_watermark_validate_constraints() {
        let mut cfg = Config::default();
        assert!(cfg.validate().is_ok());

        cfg.watermark.font_size = 10;
        assert!(cfg.validate().is_err());
        cfg.watermark.font_size = 96;
        assert!(cfg.validate().is_ok());
        cfg.watermark.font_size = 97;
        assert!(cfg.validate().is_err());
        cfg.watermark.font_size = 24;

        // enabled with nothing to render is rejected
        cfg.watermark.enabled = true;
        cfg.watermark.show_timestamp = false;
        assert!(cfg.validate().is_err());
        cfg.watermark.text = "cam".to_string();
        assert!(cfg.validate().is_ok());

        // timestamp format whitelist
        cfg.watermark.timestamp_format = "%y".to_string();
        assert!(cfg.validate().is_err());
        cfg.watermark.timestamp_format = "%F %T".to_string();
        assert!(cfg.validate().is_ok());

        // text length cap counts chars, not bytes
        cfg.watermark.text = "米".repeat(129);
        assert!(cfg.validate().is_err());
        cfg.watermark.text = "米".repeat(128);
        assert!(cfg.validate().is_ok());
    }
    #[test]
    fn test_gb28181_config_default_loads() {
        // Parse TOML with [gb28181] section and assert defaults populate
        let toml_str = r#"
[gb28181]
enabled = false
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert!(!cfg.gb28181.enabled);
        assert_eq!(cfg.gb28181.platform_sip_address, "192.168.1.1");
        assert_eq!(cfg.gb28181.platform_sip_port, 5060);
        assert_eq!(cfg.gb28181.device_id, "34020000001320000001");
        assert_eq!(cfg.gb28181.channel_id, "34020000001320000001");
        assert_eq!(cfg.gb28181.sip_domain, "3402000000");
        // Library default since gb28181-rs 0.11: no default password.
        assert_eq!(cfg.gb28181.password, "");
        assert_eq!(cfg.gb28181.local_sip_port, 5060);
        assert_eq!(cfg.gb28181.register_interval_secs, 60);
        assert_eq!(cfg.gb28181.heartbeat_interval_secs, 60);
        assert_eq!(cfg.gb28181.heartbeat_timeout_count, 3);
    }

    #[test]
    fn test_transport_default_is_udp() {
        let gb = Gb28181Config::default();
        assert_eq!(gb.transport, Transport::Udp);
    }

    #[test]
    fn test_transport_tcp_parses() {
        let toml_str = r#"
[gb28181]
transport = "tcp"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.gb28181.transport, Transport::Tcp);
    }

    #[test]
    fn test_transport_invalid_value() {
        let toml_str = r#"
[gb28181]
transport = "sctp"
"#;
        assert!(toml::from_str::<Config>(toml_str).is_err());
    }

    #[test]
    fn config_example_toml_features_disabled() {
        // Verify the example config has features disabled
        let toml_str = r#"
[features.ai]
enabled = false
[features.multi_camera]
enabled = false
[features.webrtc]
enabled = false
[features.h265]
enabled = false
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert!(!cfg.features.ai.enabled);
        assert!(!cfg.features.multi_camera.enabled);
        assert!(!cfg.features.webrtc.enabled);
        assert!(!cfg.features.h265.enabled);
    }
    #[test]
    fn test_new_equals_default() {
        let n = Config::new();
        let d = Config::default();
        assert_eq!(n.camera.device, d.camera.device);
        assert_eq!(n.camera.width, d.camera.width);
        assert_eq!(n.rtsp.port, d.rtsp.port);
        assert_eq!(n.web.enabled, d.web.enabled);
        assert_eq!(n.logging.level, d.logging.level);
    }

    // ------------------------------------------------------------------
    // TOML deserialisation (happy path with all fields)
    // ------------------------------------------------------------------

    #[test]
    fn test_toml_parse_full() {
        let toml_str = r#"
[camera]
device = "/dev/video2"
mode = "rtsp"
rtsp_url = "rtsp://192.168.1.100:554/stream"
width = 1920
height = 1080
fps = 30
codec = "h265"
bitrate = 4000000
brightness = 0.5
contrast = 1.5
saturation = 1.2
sharpness = 2.0
hflip = true
vflip = true

[rtsp]
port = 8555
username = "rtsp_user"
password = "rtsp_pass"

[onvif]
port = 8081
username = "onvif_admin"
password = "onvif_secret"

[web]
enabled = false
port = 9090
username = "web_user"
password = "web_pass"

[device]
name = "Test Cam"
manufacturer = "TestCorp"
model = "TC-2000"
firmware = "2.1.0"
hardware_id = "TC2000"
serial_number = "SN-ABC-123"

[logging]
level = "debug"

[storage]
enabled = true

[storage.local]
path = "/mnt/recordings"
retention_days = 14

[motion]
enabled = true
sensitivity = 0.8
min_area = 500
cooldown_ms = 500

[rtmp]
enabled = true
url = "rtmp://live.example.com/stream"

[features]

[features.ai]
enabled = true

[features.multi_camera]
enabled = true

[features.webrtc]
enabled = true

[features.h265]
enabled = true
"#;

        let cfg: Config = toml::from_str(toml_str).unwrap();

        // camera
        assert_eq!(cfg.camera.device, "/dev/video2");
        assert_eq!(cfg.camera.mode, "rtsp");
        assert_eq!(cfg.camera.rtsp_url, "rtsp://192.168.1.100:554/stream");
        assert_eq!(cfg.camera.width, 1920);
        assert_eq!(cfg.camera.height, 1080);
        assert_eq!(cfg.camera.fps, 30);
        assert_eq!(cfg.camera.codec, "h265");
        assert_eq!(cfg.camera.bitrate, 4_000_000);
        assert!((cfg.camera.brightness - 0.5).abs() < 1e-9);
        assert!((cfg.camera.contrast - 1.5).abs() < 1e-9);
        assert!((cfg.camera.saturation - 1.2).abs() < 1e-9);
        assert!((cfg.camera.sharpness - 2.0).abs() < 1e-9);
        assert!(cfg.camera.hflip);
        assert!(cfg.camera.vflip);

        // rtsp
        assert_eq!(cfg.rtsp.port, 8555);
        assert_eq!(cfg.rtsp.username, "rtsp_user");
        assert_eq!(cfg.rtsp.password, "rtsp_pass");

        // onvif
        assert_eq!(cfg.onvif.port, 8081);
        assert_eq!(cfg.onvif.username, "onvif_admin");
        assert_eq!(cfg.onvif.password, "onvif_secret");

        // web
        assert!(!cfg.web.enabled);
        assert_eq!(cfg.web.port, 9090);
        assert_eq!(cfg.web.username, "web_user");
        assert_eq!(cfg.web.password, "web_pass");

        // device
        assert_eq!(cfg.device.name, "Test Cam");
        assert_eq!(cfg.device.manufacturer, "TestCorp");
        assert_eq!(cfg.device.model, "TC-2000");
        assert_eq!(cfg.device.firmware, "2.1.0");
        assert_eq!(cfg.device.hardware_id, "TC2000");
        assert_eq!(cfg.device.serial_number, "SN-ABC-123");

        // logging
        assert_eq!(cfg.logging.level, "debug");

        // storage
        assert!(cfg.storage.enabled);
        assert_eq!(cfg.storage.local.path, "/mnt/recordings");
        assert_eq!(cfg.storage.local.retention_days, 14);

        // motion
        assert!(cfg.motion.enabled);
        assert!((cfg.motion.sensitivity - 0.8).abs() < 1e-9);
        assert_eq!(cfg.motion.min_area, 500);
        assert_eq!(cfg.motion.cooldown_ms, 500);

        // rtmp
        assert!(cfg.rtmp.enabled);
        assert_eq!(cfg.rtmp.url, "rtmp://live.example.com/stream");

        // features
        assert!(cfg.features.ai.enabled);
        assert!(cfg.features.multi_camera.enabled);
        assert!(cfg.features.webrtc.enabled);
        assert!(cfg.features.h265.enabled);
    }

    // ------------------------------------------------------------------
    // Defaults fill in for missing optional fields
    // ------------------------------------------------------------------

    #[test]
    fn test_toml_parse_partial() {
        let toml_str = r#"
[camera]
device = "/dev/video1"
# width, height, fps, etc. are missing — should get defaults

[rtsp]
port = 8554
# username/password missing

[onvif]
# only port
port = 8080
"#;

        let cfg: Config = toml::from_str(toml_str).unwrap();

        // Fields from TOML
        assert_eq!(cfg.camera.device, "/dev/video1");
        assert_eq!(cfg.rtsp.port, 8554);
        assert_eq!(cfg.onvif.port, 8080);

        // Defaults for missing fields
        assert_eq!(cfg.camera.width, 1280);
        assert_eq!(cfg.camera.height, 720);
        assert_eq!(cfg.camera.fps, 15);
        assert_eq!(cfg.camera.codec, "h264");
        assert_eq!(cfg.camera.bitrate, 2_000_000);

        // Empty strings
        assert_eq!(cfg.rtsp.username, "");
        assert_eq!(cfg.onvif.password, "");

        // Sections not in TOML at all get full defaults
        assert!(cfg.web.enabled);
        assert_eq!(cfg.web.port, 8088);
        assert_eq!(cfg.device.name, "Pi Camera V1");
        assert_eq!(cfg.logging.level, "info");
        assert!(!cfg.storage.enabled);
        assert!(!cfg.motion.enabled);
        assert!(!cfg.features.ai.enabled);
    }

    // ------------------------------------------------------------------
    // Validation: valid config passes
    // ------------------------------------------------------------------

    #[test]
    fn test_validate_ok() {
        let cfg = Config::default();
        assert!(cfg.validate().is_ok());
    }

    // ------------------------------------------------------------------
    // Validation: invalid port (zero / positive check)
    // ------------------------------------------------------------------

    #[test]
    fn test_validate_rtsp_port_zero() {
        let mut cfg = Config::default();
        cfg.rtsp.port = 0;
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
        assert!(err.to_string().contains("rtsp.port"));
    }

    #[test]
    fn test_validate_onvif_port_zero() {
        let mut cfg = Config::default();
        cfg.onvif.port = 0;
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
        assert!(err.to_string().contains("onvif.port"));
    }

    #[test]
    fn test_validate_web_port_zero_when_enabled() {
        let mut cfg = Config::default();
        cfg.web.enabled = true;
        cfg.web.port = 0;
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
        assert!(err.to_string().contains("web.port"));
    }

    #[test]
    fn test_validate_web_port_zero_when_disabled() {
        // When web is disabled, port zero is fine (server won't start).
        let mut cfg = Config::default();
        cfg.web.enabled = false;
        cfg.web.port = 0;
        assert!(cfg.validate().is_ok());
    }

    // ------------------------------------------------------------------
    // Validation: invalid resolution
    // ------------------------------------------------------------------

    #[test]
    fn test_validate_width_zero() {
        let mut cfg = Config::default();
        cfg.camera.width = 0;
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("width"));
    }

    #[test]
    fn test_validate_height_zero() {
        let mut cfg = Config::default();
        cfg.camera.height = 0;
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("height"));
    }

    #[test]
    fn test_validate_fps_zero() {
        let mut cfg = Config::default();
        cfg.camera.fps = 0;
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("fps"));
    }

    #[test]
    fn test_validate_bitrate_zero() {
        let mut cfg = Config::default();
        cfg.camera.bitrate = 0;
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("bitrate"));
    }

    // ------------------------------------------------------------------
    // Validation: invalid codec
    // ------------------------------------------------------------------

    #[test]
    fn test_validate_codec_invalid() {
        let mut cfg = Config::default();
        cfg.camera.codec = "vp9".to_string();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("codec"));
    }

    #[test]
    fn test_validate_codec_h264() {
        let mut cfg = Config::default();
        cfg.camera.codec = "h264".to_string();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_validate_codec_h265() {
        let mut cfg = Config::default();
        cfg.camera.codec = "h265".to_string();
        assert!(cfg.validate().is_ok());
    }

    // ------------------------------------------------------------------
    // Validation: invalid log level
    // ------------------------------------------------------------------

    #[test]
    fn test_validate_log_level_invalid() {
        let mut cfg = Config::default();
        cfg.logging.level = "trace".to_string();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("logging.level"));
    }

    #[test]
    fn test_validate_log_level_valid_variants() {
        for level in &["debug", "info", "warn", "error"] {
            let mut cfg = Config::default();
            cfg.logging.level = level.to_string();
            assert!(cfg.validate().is_ok(), "level={level} should be valid");
        }
    }

    // ------------------------------------------------------------------
    // Validation: image-control ranges
    // ------------------------------------------------------------------

    #[test]
    fn test_validate_brightness_range() {
        let mut cfg = Config::default();
        cfg.camera.brightness = -1.5;
        assert!(cfg.validate().is_err());
        cfg.camera.brightness = 1.5;
        assert!(cfg.validate().is_err());
        cfg.camera.brightness = -1.0;
        assert!(cfg.validate().is_ok());
        cfg.camera.brightness = 1.0;
        assert!(cfg.validate().is_ok());
        cfg.camera.brightness = 0.0;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_validate_contrast_range() {
        let mut cfg = Config::default();
        cfg.camera.contrast = -0.1;
        assert!(cfg.validate().is_err());
        cfg.camera.contrast = 32.1;
        assert!(cfg.validate().is_err());
        cfg.camera.contrast = 0.0;
        assert!(cfg.validate().is_ok());
        cfg.camera.contrast = 32.0;
        assert!(cfg.validate().is_ok());
        cfg.camera.contrast = 1.0;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_validate_saturation_range() {
        let mut cfg = Config::default();
        cfg.camera.saturation = -1.0;
        assert!(cfg.validate().is_err());
        cfg.camera.saturation = 33.0;
        assert!(cfg.validate().is_err());
        cfg.camera.saturation = 0.0;
        assert!(cfg.validate().is_ok());
        cfg.camera.saturation = 32.0;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_validate_sharpness_range() {
        let mut cfg = Config::default();
        cfg.camera.sharpness = -1.0;
        assert!(cfg.validate().is_err());
        cfg.camera.sharpness = 17.0;
        assert!(cfg.validate().is_err());
        cfg.camera.sharpness = 0.0;
        assert!(cfg.validate().is_ok());
        cfg.camera.sharpness = 16.0;
        assert!(cfg.validate().is_ok());
    }

    // ------------------------------------------------------------------
    // Environment variable overrides
    // ------------------------------------------------------------------

    #[test]
    fn test_env_override_string() {
        let _guard = ENV_LOCK.lock();
        unsafe { std::env::set_var("MIBEE_EYE_CAMERA_DEVICE", "/dev/video99") };
        let mut cfg = Config::default();
        apply_env_overrides(&mut cfg);
        assert_eq!(cfg.camera.device, "/dev/video99");
        unsafe { std::env::remove_var("MIBEE_EYE_CAMERA_DEVICE") };
    }

    #[test]
    fn test_env_override_int() {
        let _guard = ENV_LOCK.lock();
        unsafe { std::env::set_var("MIBEE_EYE_RTSP_PORT", "9999") };
        let mut cfg = Config::default();
        apply_env_overrides(&mut cfg);
        assert_eq!(cfg.rtsp.port, 9999);
        unsafe { std::env::remove_var("MIBEE_EYE_RTSP_PORT") };
    }

    #[test]
    fn test_env_override_float() {
        let _guard = ENV_LOCK.lock();
        unsafe { std::env::set_var("MIBEE_EYE_CAMERA_BRIGHTNESS", "0.75") };
        let mut cfg = Config::default();
        apply_env_overrides(&mut cfg);
        assert!((cfg.camera.brightness - 0.75).abs() < 1e-9);
        unsafe { std::env::remove_var("MIBEE_EYE_CAMERA_BRIGHTNESS") };
    }

    #[test]
    fn test_env_override_bool() {
        let _guard = ENV_LOCK.lock();
        unsafe { std::env::set_var("MIBEE_EYE_WEB_ENABLED", "false") };
        let mut cfg = Config::default();
        apply_env_overrides(&mut cfg);
        assert!(!cfg.web.enabled);
        unsafe { std::env::remove_var("MIBEE_EYE_WEB_ENABLED") };
    }

    #[test]
    fn test_env_override_onvif_password() {
        let _guard = ENV_LOCK.lock();
        unsafe { std::env::set_var("MIBEE_EYE_ONVIF_PASSWORD", "super_secret") };
        let mut cfg = Config::default();
        apply_env_overrides(&mut cfg);
        assert_eq!(cfg.onvif.password, "super_secret");
        unsafe { std::env::remove_var("MIBEE_EYE_ONVIF_PASSWORD") };
    }

    #[test]
    fn test_env_override_invalid_int_ignored() {
        // If the env var is set but not parseable to the target type,
        // the override must be silently ignored (matching Go behaviour).
        let _guard = ENV_LOCK.lock();
        unsafe { std::env::set_var("MIBEE_EYE_RTSP_PORT", "not_a_number") };
        let mut cfg = Config::default();
        apply_env_overrides(&mut cfg);
        assert_eq!(cfg.rtsp.port, 8554); // unchanged
        unsafe { std::env::remove_var("MIBEE_EYE_RTSP_PORT") };
    }

    #[test]
    fn test_env_override_unset_does_nothing() {
        // Ensure the env var is NOT set
        let _guard = ENV_LOCK.lock();
        unsafe { std::env::remove_var("MIBEE_EYE_RTSP_PORT") };
        let mut cfg = Config::default();
        cfg.rtsp.port = 42; // arbitrary
        apply_env_overrides(&mut cfg);
        assert_eq!(cfg.rtsp.port, 42); // still arbitrary, not overridden
    }

    // ------------------------------------------------------------------
    // Load from file (integration)
    // ------------------------------------------------------------------

    #[test]
    fn test_load_from_file() {
        let toml_str = r#"
[camera]
device = "/dev/video0"
width = 640
height = 480
fps = 10
codec = "h264"
bitrate = 1000000

[rtsp]
port = 8554

[onvif]
port = 8080
username = "admin"
password = "test123"

[web]
enabled = true
port = 8088
"#;
        let path = temp_config(toml_str);
        let cfg = Config::load(path.to_str().unwrap()).unwrap();
        assert_eq!(cfg.camera.device, "/dev/video0");
        assert_eq!(cfg.camera.width, 640);
        assert_eq!(cfg.camera.height, 480);
        assert_eq!(cfg.camera.fps, 10);
        assert_eq!(cfg.onvif.password, "test123");
        // unspecified fields have defaults
        assert_eq!(cfg.camera.codec, "h264");
        assert_eq!(cfg.rtsp.port, 8554);
        assert!(cfg.web.enabled);
    }

    /// onvif-device-rs 0.6 neutralized the 0.3.x serde defaults and
    /// fail-closes on them; hosts without an explicit [device] section
    /// must still get the identity config.example.toml documents.
    #[test]
    fn test_load_device_identity_backfilled_to_documented_defaults() {
        let toml_str = r#"
[camera]
device = "/dev/video0"
width = 640
height = 480
fps = 10
"#;
        let path = temp_config(toml_str);
        let cfg = Config::load(path.to_str().unwrap()).unwrap();
        assert_eq!(cfg.device.name, "Pi Camera V1");
        assert_eq!(cfg.device.manufacturer, "Raspberry Pi");
        assert_eq!(cfg.device.model, "OV5647");
        assert_eq!(cfg.device.hardware_id, "OV5647");
        // Backfilled identity must pass the onvif-device-rs 0.6 validation.
        assert!(cfg.device.validate().is_ok());
    }

    /// Explicit identity survives the backfill untouched.
    #[test]
    fn test_load_device_identity_explicit_values_not_clobbered() {
        let toml_str = r#"
[device]
name = "Gate Cam"
manufacturer = "Acme"
model = "Cam-X"
firmware = "9.9.9"
hardware_id = "cam-x-1"
"#;
        let path = temp_config(toml_str);
        let cfg = Config::load(path.to_str().unwrap()).unwrap();
        assert_eq!(cfg.device.name, "Gate Cam");
        assert_eq!(cfg.device.manufacturer, "Acme");
        assert_eq!(cfg.device.model, "Cam-X");
        assert_eq!(cfg.device.firmware, "9.9.9");
        assert_eq!(cfg.device.hardware_id, "cam-x-1");
    }

    #[test]
    fn test_load_file_not_found() {
        let err = Config::load("/nonexistent/path/for/config.toml").unwrap_err();
        assert!(matches!(err, ConfigError::Io(_)));
    }

    #[test]
    fn test_load_invalid_toml() {
        let path = temp_config("this is not valid toml [[[");
        let err = Config::load(path.to_str().unwrap()).unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)));
    }

    #[test]
    fn test_load_validation_failure() {
        let path = temp_config(
            r#"
[camera]
width = 0
height = 0

[rtsp]
port = 0

[onvif]
port = 0
"#,
        );
        let err = Config::load(path.to_str().unwrap()).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
    }

    // ------------------------------------------------------------------
    // Empty ONVIF password warning (test that it loads fine)
    // ------------------------------------------------------------------

    #[test]
    fn test_empty_onvif_password_ok() {
        // Empty password should not be an error, just a warning
        let mut cfg = Config::default();
        cfg.onvif.password = String::new();
        assert!(cfg.validate().is_ok());
    }

    // ------------------------------------------------------------------
    // Error Display / Debug
    // ------------------------------------------------------------------

    #[test]
    fn test_config_error_display() {
        let err = ConfigError::Validation("test error".into());
        let msg = err.to_string();
        assert!(msg.contains("test error"));
    }

    #[test]
    fn test_config_error_debug() {
        let err = ConfigError::Validation("debug me".into());
        let msg = format!("{err:?}");
        assert!(msg.contains("Validation"));
    }

    // ------------------------------------------------------------------
    // Clone / serialisation round-trip (sanity)
    // ------------------------------------------------------------------

    #[test]
    fn test_config_clone() {
        let cfg = Config::default();
        let cloned = cfg.clone();
        assert_eq!(cfg.camera.device, cloned.camera.device);
    }

    #[test]
    fn test_serialize_roundtrip() {
        let cfg = Config::default();
        let toml_str = toml::to_string(&cfg).unwrap();
        let deserialized: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(cfg.camera.device, deserialized.camera.device);
        assert_eq!(cfg.camera.width, deserialized.camera.width);
        assert_eq!(cfg.rtsp.port, deserialized.rtsp.port);
    }

    // ------------------------------------------------------------------
    // Edge: all-optional sections absent from TOML
    // ------------------------------------------------------------------

    #[test]
    fn test_optional_sections_default() {
        let toml_str = r#"
[camera]
device = "/dev/video0"
width = 640
height = 480

[rtsp]
port = 8554

[onvif]
port = 8080
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        // storage section entirely absent
        assert!(!cfg.storage.enabled);
        assert_eq!(cfg.storage.local.path, "");
        assert_eq!(cfg.storage.local.retention_days, 30);
        // motion absent
        assert!(!cfg.motion.enabled);
        // features absent
        assert!(!cfg.features.ai.enabled);
    }

    #[test]
    fn test_gb35114_section_parsing() {
        let toml_str = r#"
[gb28181]
enabled = true
password = "12345678"

[gb28181.gb35114]
enabled = true
device_cert_file = "/etc/mibee-eye/gb35114/device_cert.pem"
device_key_file = "/etc/mibee-eye/gb35114/device_key.pem"
platform_cert_file = "/etc/mibee-eye/gb35114/platform_cert.pem"
server_id = "34020000002000000001"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert!(cfg.gb28181.gb35114.enabled);
        assert_eq!(
            cfg.gb28181.gb35114.device_cert_file,
            "/etc/mibee-eye/gb35114/device_cert.pem"
        );
        assert_eq!(cfg.gb28181.gb35114.server_id, "34020000002000000001");
        // Existing keys still land on the flattened library struct.
        assert_eq!(cfg.gb28181.password, "12345678");
    }

    #[test]
    fn test_gb35114_disabled_by_default() {
        let cfg: Config = toml::from_str(
            "[gb28181]
enabled = true
",
        )
        .unwrap();
        assert!(!cfg.gb28181.gb35114.enabled);
    }
}
