//! AI detection implementations.
//!
//! This module contains concrete implementations of the [`AiDetector`] trait
//! from the `features::ai` module. The trait itself is defined in `features/ai.rs`
//! because it's part of the feature abstraction layer.

use crate::config::AiFeatureConfig;
use crate::features::ai::{AiDetector, Detection};
use crate::pipeline::bus::{EventBus, PipelineEvent};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::RwLock as AsyncRwLock;

pub mod guardrails;
pub mod mock;
pub mod ortv;
pub mod postprocess;
pub mod preprocess;
pub mod registry;
pub mod yolox;

/// Shared YUV frame from the camera pipeline (width, height, data).
type LatestYuv = Arc<Mutex<Option<(u32, u32, Vec<u8>)>>>;
/// Swappable detector slot (SPEC §4.6 activate): the inference loop clones
/// the `Arc` out per iteration, so `set_detector` swaps without restart.
type DetectorSlot = Arc<RwLock<Arc<dyn AiDetector>>>;
/// AI detection module that runs inference in a background tokio task.
///
/// The module shares YUV frames from the camera pipeline via `latest_yuv`,
/// runs detection at a configurable interval, and publishes results to the
/// event bus.
pub struct AiModule {
    /// The detector implementation (e.g., MockAiDetector, ONNXRuntime) in a
    /// swappable slot — see [`AiModule::set_detector`].
    detector: DetectorSlot,
    /// Active model id (SPEC §4.6 model identifier, e.g. "nanodet-plus-m-320").
    active_model: Arc<RwLock<String>>,
    /// Shared YUV frame from the camera pipeline (width, height, data).
    latest_yuv: LatestYuv,
    /// Optional event bus for publishing detection events.
    event_bus: Option<EventBus>,
    /// AI feature configuration.
    config: AiFeatureConfig,
    /// Frame counter for interval-based detection.
    frame_counter: Arc<AtomicU64>,
    /// Latest detection results, shared with the web API through the
    /// same `Arc` so the UI can read results without a copy.
    last_detections: Arc<AsyncRwLock<Vec<Detection>>>,
}

impl AiModule {
    /// Create a new AI module.
    ///
    /// # Arguments
    ///
    /// * `detector` - The detector implementation to use.
    /// * `model_id` - Registry id of the model the detector runs (SPEC §4.6).
    /// * `latest_yuv` - Shared YUV frame from the camera pipeline.
    /// * `event_bus` - Optional event bus for publishing detection events.
    /// * `config` - AI feature configuration.
    #[must_use]
    pub fn new(
        detector: Arc<dyn AiDetector>,
        model_id: String,
        latest_yuv: LatestYuv,
        event_bus: Option<EventBus>,
        config: AiFeatureConfig,
    ) -> Self {
        Self {
            detector: Arc::new(RwLock::new(detector)),
            active_model: Arc::new(RwLock::new(model_id)),
            latest_yuv,
            event_bus,
            config,
            frame_counter: Arc::new(AtomicU64::new(0)),
            last_detections: Arc::new(AsyncRwLock::new(Vec::new())),
        }
    }

    /// Hot-swap the detector (SPEC §4.6 `POST /api/ai/models/{id}/activate`).
    ///
    /// Callers must only invoke this after the new detector has been fully
    /// constructed, so a failed load never disturbs the running one. Stale
    /// detections from the previous model are cleared.
    pub async fn set_detector(&self, model_id: &str, detector: Arc<dyn AiDetector>) {
        *self.active_model.write().expect("active_model lock") = model_id.to_string();
        let old = std::mem::replace(
            &mut *self.detector.write().expect("detector slot lock"),
            detector,
        );
        drop(old);
        // Results from the previous model must not be attributed to the new one.
        self.last_detections.write().await.clear();
    }

    /// The active model id (SPEC §4.6 model identifier).
    #[must_use]
    pub fn active_model(&self) -> String {
        self.active_model.read().expect("active_model lock").clone()
    }

    /// Start the AI inference loop in a background tokio task.
    ///
    /// Returns a join handle that can be used to monitor or cancel the task.
    #[must_use]
    pub fn start(&self) -> tokio::task::JoinHandle<()> {
        let detector = Arc::clone(&self.detector);
        let latest_yuv = Arc::clone(&self.latest_yuv);
        let event_bus = self.event_bus.clone();
        let config = self.config.clone();
        let frame_counter = Arc::clone(&self.frame_counter);
        let last_detections = Arc::clone(&self.last_detections);

        tokio::spawn(async move {
            // Pin this task to configured CPU cores (fail open on error).
            #[cfg(feature = "ai")]
            {
                let cores: Vec<u32> = config.cpu_cores.clone();
                if !cores.is_empty() {
                    // Convert u32 core IDs to core_affinity::CoreId
                    let core_ids: Vec<core_affinity::CoreId> = cores
                        .iter()
                        .filter_map(|&id| id.try_into().ok().map(|id| core_affinity::CoreId { id }))
                        .collect();
                    if !core_ids.is_empty() {
                        let success = core_ids.into_iter().all(core_affinity::set_for_current);
                        if !success {
                            eprintln!("ai: failed to set CPU affinity to cores {:?}", cores);
                        }
                    } else {
                        eprintln!("ai: no valid core IDs in {:?}, skipping affinity", cores);
                    }
                }
            }

            use guardrails::{check_guardrails, GuardrailAction, RealSystemMetricsReader};
            let metrics_reader = RealSystemMetricsReader;
            loop {
                // Always sleep between iterations to prevent busy-spin.
                tokio::time::sleep(Duration::from_millis(200)).await;

                // Check guardrails: memory cap and thermal limits.
                match check_guardrails(&metrics_reader, &config) {
                    GuardrailAction::Continue => {
                        // Proceed with normal detection.
                    }
                    GuardrailAction::SkipThisFrame => {
                        // Skip this frame's detection to reduce pressure.
                        continue;
                    }
                    GuardrailAction::Pause(duration) => {
                        // Pause for thermal throttling.
                        tokio::time::sleep(duration).await;
                        continue;
                    }
                }

                // Clone the latest YUV frame if available.
                let (width, height, data) = {
                    let guard = latest_yuv.lock().unwrap();
                    match guard.as_ref() {
                        Some((w, h, d)) => (*w, *h, d.clone()),
                        None => continue,
                    }
                };

                // Increment frame counter and check if we should run detection.
                let count = frame_counter.fetch_add(1, Ordering::Relaxed) + 1;
                if !count.is_multiple_of(config.interval_frames as u64) {
                    continue;
                }

                // Run detection with the CURRENT slot contents (don't hold any
                // lock during inference) so model swaps apply from the next
                // iteration on.
                let detector = detector.read().expect("detector slot lock").clone();
                match detector.detect(&data, width, height).await {
                    Ok(mut detections) => {
                        ::metrics::counter!("mibee_ai_inferences_total").increment(1);
                        // Filter detections by confidence threshold.
                        detections.retain(|d| d.confidence >= config.confidence_threshold);

                        // Store latest detections.
                        {
                            let mut guard = last_detections.write().await;
                            *guard = detections.clone();
                        }

                        // Publish detection event to event bus.
                        if let Some(ref bus) = event_bus {
                            let event = PipelineEvent::AiDetection {
                                detections,
                                frame_number: count,
                            };
                            if let Err(e) = bus.publish(event) {
                                eprintln!("ai: failed to publish event: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        // Log error but don't crash - store empty detections.
                        eprintln!("ai: inference error: {e}");
                        let mut guard = last_detections.write().await;
                        guard.clear();
                    }
                }
            }
        })
    }

    /// Get the latest detection results.
    ///
    /// Returns a clone of the current detection vector.
    #[must_use]
    pub async fn get_detections(&self) -> Vec<Detection> {
        self.last_detections.read().await.clone()
    }

    /// Share the lock holding the latest detections, so other components
    /// (e.g. the web API) can read results without a copy.
    #[must_use]
    pub fn last_detections_arc(&self) -> Arc<AsyncRwLock<Vec<Detection>>> {
        Arc::clone(&self.last_detections)
    }

    /// Human-readable identifier of the active detector / model.
    #[must_use]
    pub fn model_name(&self) -> String {
        self.detector
            .read()
            .expect("detector slot lock")
            .model_name()
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn test_ai_module_creation() {
        let detector = Arc::new(mock::MockAiDetector::new());
        let latest_yuv = Arc::new(Mutex::new(None));
        let event_bus = Some(EventBus::new(16));
        let config = AiFeatureConfig::default();

        let module = AiModule::new(
            detector,
            "nanodet-plus-m-320".to_string(),
            latest_yuv,
            event_bus,
            config,
        );
        let detections = module.get_detections().await;
        assert!(detections.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_ai_module_without_event_bus() {
        let detector = Arc::new(mock::MockAiDetector::new());
        let latest_yuv = Arc::new(Mutex::new(None));
        let config = AiFeatureConfig::default();

        let module = AiModule::new(
            detector,
            "nanodet-plus-m-320".to_string(),
            latest_yuv,
            None,
            config,
        );
        assert!(module.get_detections().await.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_set_detector_swaps_active_model() {
        let module = AiModule::new(
            Arc::new(mock::MockAiDetector::new()),
            "nanodet-plus-m-320".to_string(),
            Arc::new(Mutex::new(None)),
            None,
            AiFeatureConfig::default(),
        );
        assert_eq!(module.active_model(), "nanodet-plus-m-320");
        assert!(module.model_name().contains("mock"));

        module
            .set_detector("nanodet-plus-m-416", Arc::new(mock::MockAiDetector::new()))
            .await;
        assert_eq!(module.active_model(), "nanodet-plus-m-416");

        // Stale detections are cleared on swap.
        assert!(module.get_detections().await.is_empty());
    }
}
