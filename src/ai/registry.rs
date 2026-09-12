//! Built-in AI model registry + runtime uploaded overlay (SPEC v1 §4.6).
//!
//! Maps stable model ids to on-device ONNX files plus the metadata the API
//! reports (`family`, `input`, `source`). The registry is the single place a
//! new model enters the device:
//!
//! - **builtin** entries ship with the binary (families the pre/post-
//!   processing implements: NanoDet GFL, YOLOX);
//! - **uploaded** entries arrive at runtime through `POST /api/ai/models/{id}`
//!   (capability `ai_upload`), are validated by fully loading an ONNX
//!   session BEFORE entering the registry, and persist in an
//!   `uploaded.json` manifest next to the model files so they survive
//!   reboots;
//! - a non-default `model_path` in the config stays a custom override.

use crate::config::AiFeatureConfig;
use crate::features::ai::AiDetector;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Canonical model directory (also where uploads land and the manifest
/// lives).
pub const MODELS_DIR: &str = "/var/lib/mibee-eye/models";

/// Upload size cap (SPEC §4.6 `max_bytes`).
pub const UPLOAD_MAX_BYTES: usize = 32 * 1024 * 1024;

/// Why a model activation failed before the running detector was touched.
#[derive(Debug, Clone)]
pub enum ActivateError {
    /// Model file missing on this device (HTTP 409, SPEC §4.6).
    Unavailable(String),
    /// Detector construction failed (HTTP 500); the old model keeps running.
    LoadFailed(String),
}

/// Builds a detector for a model file (path + family). Production builds
/// (feature `ai`) construct an [`crate::ai::ortv::OrtDetector`] — the full
/// load doubles as upload validation (session build + family shape check);
/// tests inject mocks.
pub type AiLoader =
    Arc<dyn Fn(&str, Family) -> Result<Arc<dyn AiDetector>, ActivateError> + Send + Sync>;

/// Decoder families the pre/post-processing implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// NanoDet GFL: BGR stretch, ImageNet mean/std, 4-level ceil grid.
    NanoDet,
    /// YOLOX: RGB letterbox /255, raw deltas + obj×cls, 3-level floor grid.
    Yolox,
}

impl Family {
    /// Parse a registry `family` string.
    #[must_use]
    pub fn parse_family(s: &str) -> Option<Self> {
        match s {
            "nanodet" => Some(Self::NanoDet),
            "yolox" => Some(Self::Yolox),
            _ => None,
        }
    }

    /// The registry `family` string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NanoDet => "nanodet",
            Self::Yolox => "yolox",
        }
    }
}

/// One entry of the model registry.
#[derive(Debug, Clone)]
pub struct ModelSpec {
    /// Registry id — the `"model"` value of SPEC §4.6.
    pub id: String,
    /// Decoder family; selects the pre/post-processing pair.
    pub family: String,
    /// Square model input size in pixels (informational — the authoritative
    /// size is read from the ONNX session at load time).
    pub input: u32,
    /// On-device path of the ONNX file.
    pub path: String,
    /// `builtin` | `uploaded`.
    pub source: String,
}

/// The built-in entries (families this build implements).
pub fn builtin() -> Vec<ModelSpec> {
    vec![
        ModelSpec {
            id: "nanodet-plus-m-320".into(),
            family: "nanodet".into(),
            input: 320,
            path: "/var/lib/mibee-eye/models/nanodet-m.onnx".into(),
            source: "builtin".into(),
        },
        ModelSpec {
            id: "nanodet-plus-m-416".into(),
            family: "nanodet".into(),
            input: 416,
            path: "/var/lib/mibee-eye/models/nanodet-m-416.onnx".into(),
            source: "builtin".into(),
        },
        ModelSpec {
            id: "yolox-nano-416".into(),
            family: "yolox".into(),
            input: 416,
            path: "/var/lib/mibee-eye/models/yolox-nano.onnx".into(),
            source: "builtin".into(),
        },
    ]
}

/// Validate a model id's syntax (`^[a-z0-9][a-z0-9-]{0,63}$`). Existence is
/// deferred to the runtime registry — uploaded models only exist there, and
/// config validation runs before the registry loads.
#[must_use]
pub fn valid_model_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The model a configuration resolves to.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveModel {
    pub id: String,
    pub path: String,
    /// `builtin` | `uploaded` | `custom`.
    pub source: &'static str,
    pub family: String,
    /// 0 = unknown until the ONNX session is loaded (custom paths only).
    pub input: u32,
}

impl From<&ModelSpec> for ActiveModel {
    fn from(spec: &ModelSpec) -> Self {
        Self {
            id: spec.id.clone(),
            path: spec.path.clone(),
            source: if spec.source == "uploaded" {
                "uploaded"
            } else {
                "builtin"
            },
            family: spec.family.clone(),
            input: spec.input,
        }
    }
}

/// Whether the model file exists on this device (drives `available` in the
/// API and the 409 gate on activation).
#[must_use]
pub fn is_available(path: &str) -> bool {
    Path::new(path).exists()
}

/// Manifest record for an uploaded model (persisted next to the files).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct UploadEntry {
    id: String,
    family: String,
    input: u32,
    file: String,
}

fn manifest_entry(m: &ModelSpec) -> UploadEntry {
    UploadEntry {
        id: m.id.clone(),
        family: m.family.clone(),
        input: m.input,
        file: Path::new(&m.path)
            .file_name()
            .map_or_else(|| m.id.clone(), |f| f.to_string_lossy().into_owned()),
    }
}

/// Runtime registry: builtin entries plus the uploaded overlay persisted
/// in `<dir>/uploaded.json`. Shared behind an `Arc<RwLock<…>>` between the
/// web handlers and the detector loader.
#[derive(Debug, Default)]
pub struct Registry {
    entries: Vec<ModelSpec>,
    dir: Option<PathBuf>,
}

impl Registry {
    /// Registry without persistence (tests, plain builds).
    #[must_use]
    pub fn builtin_only() -> Self {
        Self {
            entries: builtin(),
            dir: None,
        }
    }

    /// Load the registry from `dir`: builtin entries + the uploaded
    /// manifest. Manifest entries whose files vanished are pruned (and the
    /// manifest rewritten best-effort).
    #[must_use]
    pub fn load(dir: &Path) -> Self {
        let mut uploaded: Vec<UploadEntry> = std::fs::read(dir.join("uploaded.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let before = uploaded.len();
        uploaded.retain(|e| dir.join(&e.file).exists());
        let mut entries = builtin();
        entries.extend(uploaded.iter().map(|e| ModelSpec {
            id: e.id.clone(),
            family: e.family.clone(),
            input: e.input,
            path: dir.join(&e.file).to_string_lossy().into_owned(),
            source: "uploaded".into(),
        }));
        let reg = Self {
            entries,
            dir: Some(dir.to_path_buf()),
        };
        if uploaded.len() != before {
            let _ = reg.save_manifest(&uploaded);
        }
        reg
    }

    /// All entries (builtin first, then uploaded).
    #[must_use]
    pub fn list(&self) -> &[ModelSpec] {
        &self.entries
    }

    /// The models directory uploads land in (None for builtin-only
    /// registries — tests, plain builds).
    #[must_use]
    pub fn models_dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// Look up an entry by id.
    #[must_use]
    pub fn find(&self, id: &str) -> Option<ModelSpec> {
        self.entries.iter().find(|m| m.id == id).cloned()
    }

    /// Resolve the AI config into the model that should be loaded. A
    /// non-default `model_path` is a legacy/custom deployment override and
    /// wins over the registry id.
    ///
    /// # Errors
    ///
    /// `Err` when `model_path` is default and `ai.model` names no entry.
    pub fn resolve_active(&self, cfg: &AiFeatureConfig) -> Result<ActiveModel, String> {
        if cfg.model_path != crate::config::default_ai_model_path() {
            return Ok(ActiveModel {
                id: "custom".to_string(),
                path: cfg.model_path.clone(),
                source: "custom",
                // The decode path predates families; custom files are by
                // convention NanoDet graphs.
                family: "nanodet".to_string(),
                input: 0,
            });
        }
        self.find(&cfg.model).map_or_else(
            || {
                Err(format!(
                    "unknown ai.model '{}' (known: {})",
                    cfg.model,
                    self.entries
                        .iter()
                        .map(|m| m.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            },
            |spec| Ok(ActiveModel::from(&spec)),
        )
    }

    /// Add an uploaded entry (after validation) and persist the manifest.
    ///
    /// # Errors
    ///
    /// `io::Error` when the registry has no models dir or the manifest
    /// cannot be written.
    pub fn insert_uploaded(&mut self, spec: ModelSpec) -> std::io::Result<()> {
        let uploaded: Vec<UploadEntry> = self
            .entries
            .iter()
            .filter(|m| m.source == "uploaded")
            .map(manifest_entry)
            .chain(std::iter::once(manifest_entry(&spec)))
            .collect();
        self.save_manifest(&uploaded)?;
        self.entries.push(spec);
        Ok(())
    }

    /// Remove an uploaded entry and persist the manifest. Returns the
    /// removed spec; `None` when the id is unknown or not uploaded.
    pub fn remove_uploaded(&mut self, id: &str) -> Option<ModelSpec> {
        let idx = self
            .entries
            .iter()
            .position(|m| m.id == id && m.source == "uploaded")?;
        let spec = self.entries.remove(idx);
        let uploaded: Vec<UploadEntry> = self
            .entries
            .iter()
            .filter(|m| m.source == "uploaded")
            .map(manifest_entry)
            .collect();
        if let Some(dir) = self.dir.as_ref() {
            let _ = std::fs::write(
                dir.join("uploaded.json"),
                serde_json::to_vec_pretty(&uploaded).unwrap_or_default(),
            );
        }
        Some(spec)
    }

    fn save_manifest(&self, uploaded: &[UploadEntry]) -> std::io::Result<()> {
        let dir = self
            .dir
            .as_ref()
            .ok_or_else(|| std::io::Error::other("registry has no models dir"))?;
        std::fs::create_dir_all(dir)?;
        std::fs::write(
            dir.join("uploaded.json"),
            serde_json::to_vec_pretty(uploaded).unwrap_or_default(),
        )
    }
}

/// The decoder family of a resolved model (custom paths default to NanoDet,
/// matching the escape-hatch behavior before families existed).
#[must_use]
pub fn family_of(model: &ActiveModel) -> Family {
    Family::parse_family(&model.family).unwrap_or(Family::NanoDet)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(model: &str, model_path: &str) -> AiFeatureConfig {
        AiFeatureConfig {
            model: model.to_string(),
            model_path: model_path.to_string(),
            ..AiFeatureConfig::default()
        }
    }

    #[test]
    fn resolve_default_is_registry_320() {
        let reg = Registry::builtin_only();
        let m = reg
            .resolve_active(&cfg(
                "nanodet-plus-m-320",
                crate::config::default_ai_model_path().as_str(),
            ))
            .expect("default config must resolve");
        assert_eq!(m.id, "nanodet-plus-m-320");
        assert_eq!(m.source, "builtin");
        assert_eq!(m.family, "nanodet");
        assert_eq!(m.input, 320);
    }

    #[test]
    fn resolve_custom_model_path_wins() {
        let reg = Registry::builtin_only();
        let m = reg
            .resolve_active(&cfg("nanodet-plus-m-320", "/opt/my-nanodet.onnx"))
            .expect("custom path must resolve");
        assert_eq!(m.id, "custom");
        assert_eq!(m.source, "custom");
        assert_eq!(m.path, "/opt/my-nanodet.onnx");
    }

    #[test]
    fn resolve_unknown_id_is_error() {
        let reg = Registry::builtin_only();
        let err = reg
            .resolve_active(&cfg(
                "yolo-9000",
                crate::config::default_ai_model_path().as_str(),
            ))
            .expect_err("unknown id must be rejected");
        assert!(err.contains("yolo-9000"));
    }

    #[test]
    fn builtin_ids_are_unique() {
        let mut ids: Vec<_> = builtin().iter().map(|m| m.id.clone()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), builtin().len());
    }

    #[test]
    fn model_id_syntax() {
        assert!(valid_model_id("nanodet-plus-m-320"));
        assert!(valid_model_id("my-model-1"));
        assert!(!valid_model_id(""));
        assert!(!valid_model_id("-leading"));
        assert!(!valid_model_id("Upper"));
        assert!(!valid_model_id("has_underscore"));
        assert!(!valid_model_id("spaces in id"));
        assert!(!valid_model_id(&"x".repeat(65)));
    }

    #[test]
    fn uploaded_entries_roundtrip_through_manifest() {
        let dir = std::env::temp_dir().join(format!("mibee-reg-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("custom-yolox.onnx");
        std::fs::write(&file, b"onnx").unwrap();

        let mut reg = Registry::load(&dir);
        assert_eq!(reg.list().len(), builtin().len());
        reg.insert_uploaded(ModelSpec {
            id: "custom-yolox".into(),
            family: "yolox".into(),
            input: 416,
            path: file.to_string_lossy().into_owned(),
            source: "uploaded".into(),
        })
        .expect("insert must persist");
        assert_eq!(reg.list().len(), builtin().len() + 1);
        assert_eq!(reg.find("custom-yolox").expect("entry").source, "uploaded");

        // A fresh load picks the manifest entry back up.
        let reloaded = Registry::load(&dir);
        assert_eq!(
            reloaded
                .find("custom-yolox")
                .expect("survives reload")
                .input,
            416
        );

        // Removing the file prunes the entry on the next load.
        std::fs::remove_file(&file).unwrap();
        let pruned = Registry::load(&dir);
        assert!(pruned.find("custom-yolox").is_none());

        // remove_uploaded refuses builtin entries.
        let mut pruned = pruned;
        assert!(pruned.remove_uploaded("nanodet-plus-m-320").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
