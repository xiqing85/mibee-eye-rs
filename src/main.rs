//! Entry point for the mibee-eye-raspi-rs ONVIF camera service.

#[cfg(feature = "ai")]
use mibee_eye_raspi_rs::ai::ortv::OrtDetector;
use mibee_eye_raspi_rs::camera::source::{
    CameraConfig, CameraSource, FrameType, H264Level, H264Profile,
};
use mibee_eye_raspi_rs::camera::v4l2::V4l2CameraSource;
use mibee_eye_raspi_rs::camera::v4l2_capture::V4l2CaptureProducer;
use mibee_eye_raspi_rs::config::Config;
use mibee_eye_raspi_rs::features::ai::{AiDetector, Detection};
#[cfg(feature = "ai")]
use mibee_eye_raspi_rs::features::FeatureError;
use mibee_eye_raspi_rs::gb28181::server::Gb28181Server;
use mibee_eye_raspi_rs::h264::hub::{AccessUnit, AuHub};
use mibee_eye_raspi_rs::h264::parser::Parser;
use mibee_eye_raspi_rs::hardware::capability::{CapabilityGate, HardwareCapability};
use mibee_eye_raspi_rs::onvif::device::{DeviceHandler, DeviceServiceHandlers};
use mibee_eye_raspi_rs::onvif::discovery::DiscoveryServer;
use mibee_eye_raspi_rs::onvif::media::{
    GetProfilesHandler, GetSnapshotUriHandler, GetStreamUriHandler, GetVideoSourcesHandler,
    OnvifMediaConfig, VideoEncoding,
};
use mibee_eye_raspi_rs::onvif::ptz::PtzHandler;
use mibee_eye_raspi_rs::onvif::server::{OnvifConfig, OnvifServer};
use mibee_eye_raspi_rs::pipeline::bus::{EventBus, PipelineEvent};
use mibee_eye_raspi_rs::ptz::state::PtzState;
use mibee_eye_raspi_rs::streaming::rtsp::{RtspConfig, RtspServer};
use mibee_eye_raspi_rs::web::events::global_hub;
use mibee_eye_raspi_rs::web::server::WebServer;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mibee_eye_raspi_rs::gb28181::{FrameSource, FrameSubscription, RecordingSource, SegmentMeta};
use mibee_eye_raspi_rs::recording::{index::index_path, RecordingIndex};

/// Adapter that serves RecordInfo queries from the on-disk recording index.
///
/// Reloads `index.jsonl` on every lookup so queries see freshly-written
/// segments (the writer owns its index privately and does not share it).
struct DiskRecordingSource {
    root: PathBuf,
}

impl RecordingSource for DiskRecordingSource {
    fn lookup(&self, start_ms: u64, end_ms: u64) -> Vec<SegmentMeta> {
        let index = RecordingIndex::load(&index_path(&self.root));
        index
            .lookup(start_ms, end_ms)
            .into_iter()
            .map(|s| SegmentMeta {
                file: s.file,
                start_ms: s.start_ms,
                end_ms: s.end_ms,
            })
            .collect()
    }
    fn resolve_path(&self, file: &str) -> PathBuf {
        self.root.join(file)
    }
}

/// Adapts the host `h264::hub::AuHub` to the gb28181 library's `FrameSource`
/// seam. Each subscription spawns a bridge thread converting host access
/// units into library units; when the hub unsubscribes (upstream channel
/// closes), the thread exits and the downstream channel disconnects in turn.
struct AuHubFrameSource(
    Arc<AuHub>,
    std::sync::Arc<mibee_eye_raspi_rs::web::observe::Observe>,
);

impl FrameSource for AuHubFrameSource {
    fn subscribe_with_capacity(&self, capacity: usize) -> FrameSubscription {
        let sub = self.0.subscribe_with_capacity(capacity);
        let observe = self.1.clone();
        let host_id = sub.id;
        let (tx, rx) = std::sync::mpsc::sync_channel(capacity);
        std::thread::Builder::new()
            .name("gb28181-frame-bridge".to_string())
            .spawn(move || {
                for au in sub.receiver.iter() {
                    let converted = mibee_eye_raspi_rs::gb28181::AccessUnit {
                        nalus: au
                            .nalus
                            .into_iter()
                            .map(|n| mibee_eye_raspi_rs::gb28181::Nalu {
                                nalu_type: n.nalu_type,
                                data: n.data,
                                is_idr: n.is_idr,
                                is_sps: n.is_sps,
                                is_pps: n.is_pps,
                                is_aud: n.is_aud,
                            })
                            .collect(),
                        timestamp: au.timestamp,
                        is_key_frame: au.is_key_frame,
                    };
                    // App-attributed traffic (SPEC §3.2): bytes handed to the
                    // GB28181 stack (RTP payload + framing added downstream).
                    observe.traffic.gb28181_tx.fetch_add(
                        converted
                            .nalus
                            .iter()
                            .map(|n| n.data.len() + 4)
                            .sum::<usize>() as u64,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    // Mirror hub semantics: drop for a slow consumer instead of
                    // blocking the capture pipeline.
                    match tx.try_send(converted) {
                        Ok(()) => {}
                        Err(std::sync::mpsc::TrySendError::Full(_)) => {}
                        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break,
                    }
                }
            })
            .expect("spawn gb28181-frame-bridge");
        FrameSubscription {
            id: host_id as u64,
            receiver: rx,
        }
    }

    fn unsubscribe(&self, id: u64) {
        self.0.unsubscribe(id as usize);
    }
}

#[tokio::main]
async fn main() {
    // Observability first: the process-wide tee logger captures `log`-facade
    // records (protocol libraries) into the /api/logs ring while still
    // rendering them to stderr / journald. Default to info so library
    // diagnostics stay as visible as they were pre-extraction; RUST_LOG
    // overrides.
    let observe = std::sync::Arc::new(mibee_eye_raspi_rs::web::observe::Observe::new());
    mibee_eye_raspi_rs::web::observe::init_logger(observe.clone());
    // Separate handle for the web server — the GB28181 task below moves its
    // own clone into the spawned supervision loop.
    let web_observe = observe.clone();

    println!("mibee-eye-raspi-rs v0.1.0");
    println!("ONVIF camera service for Raspberry Pi");

    // --- Load configuration ---
    let config_path = resolve_config_path();
    let config = Config::load(&config_path).unwrap_or_else(|e| {
        eprintln!("config: {e} (path: {config_path}) â using built-in defaults");
        Config::default()
    });

    println!(
        "config: {} {}x{}@{} fps | web:{} onvif:{} rtsp:{}",
        config.camera.device,
        config.camera.width,
        config.camera.height,
        config.camera.fps,
        config.web.port,
        config.onvif.port,
        config.rtsp.port,
    );

    let device_ip = detect_local_ip();

    // --- Start RTSP server ---
    let rtsp_config = RtspConfig {
        port: config.rtsp.port,
        username: config.rtsp.username.clone(),
        password: config.rtsp.password.clone(),
        realm: "MiBee Eye RTSP".to_string(),
    };
    let mut latest_yuv: Option<mibee_eye_raspi_rs::camera::v4l2_capture::LatestYuv> = None;
    let mut au_hub_for_web: Option<Arc<mibee_eye_raspi_rs::h264::hub::AuHub>> = None;
    let mut au_hub: Option<Arc<mibee_eye_raspi_rs::h264::hub::AuHub>> = None;
    match RtspServer::new(rtsp_config).await {
        Ok(server) => {
            // Clone the AuHub before moving the server into its task.
            let au_hub_internal = server.au_hub().clone();
            au_hub = Some(au_hub_internal.clone());
            au_hub_for_web = Some(au_hub_internal.clone());
            println!("rtsp: listening on :{}", config.rtsp.port);
            tokio::spawn(async move {
                if let Err(e) = server.start().await {
                    eprintln!("rtsp: {e}");
                }
            });

            latest_yuv = start_camera_pipeline(&config, au_hub_internal).await;
        }
        Err(e) => eprintln!("rtsp: failed to bind :{} â {e}", config.rtsp.port),
    }

    // --- Start AI detection module (if enabled and hardware allows) ---
    // Shared with the web API (T5): latest detections + pipeline event bus.
    // The runtime registry (builtin + uploaded overlay, SPEC §4.6) loads
    // before anything resolves models against it.
    let registry = Arc::new(std::sync::RwLock::new(
        mibee_eye_raspi_rs::ai::registry::Registry::load(std::path::Path::new(
            mibee_eye_raspi_rs::ai::registry::MODELS_DIR,
        )),
    ));
    let mut ai_last_detections: Option<Arc<tokio::sync::RwLock<Vec<Detection>>>> = None;
    let mut ai_model_name: Option<String> = None;
    let mut ai_event_bus: Option<EventBus> = None;
    let mut ai_module_handle: Option<Arc<mibee_eye_raspi_rs::ai::AiModule>> = None;
    if config.features.ai.enabled {
        if let Some(ref yuv) = latest_yuv {
            let capability = HardwareCapability::detect();
            let gate = CapabilityGate::new(capability);
            if gate.enable_feature("ai") {
                println!("ai: hardware capability check passed");
                // Resolve the model through the registry (SPEC §4.6): the
                // config's model id names a registry entry; a custom
                // model_path still overrides it for special deployments.
                // Validation normally rejects bad ids at load time; a bad
                // override here just disables AI (fail-open, SPEC §4.6).
                let model = registry
                    .read()
                    .expect("registry lock")
                    .resolve_active(&config.features.ai)
                    .map_err(|e| eprintln!("ai: invalid model configuration — {e}"))
                    .ok();
                // Prefer the real ONNX detector when built with the `ai` feature;
                // fall back to the mock detector for plain builds (dev/testing).
                #[cfg(feature = "ai")]
                let detector: Option<Arc<dyn AiDetector>> = match model
                    .as_ref()
                    .map(|m| {
                        OrtDetector::new(&m.path, mibee_eye_raspi_rs::ai::registry::family_of(m))
                    })
                    .unwrap_or(Err(FeatureError::Runtime(
                        "model configuration invalid".to_string(),
                    ))) {
                    Ok(d) => {
                        println!("ai: loaded ONNX model: {}", d.model_name());
                        Some(Arc::new(d))
                    }
                    Err(e) => {
                        eprintln!("ai: failed to load model: {e}");
                        None
                    }
                };
                #[cfg(not(feature = "ai"))]
                let detector: Option<Arc<dyn AiDetector>> = match &model {
                    Some(_) => Some(Arc::new(mibee_eye_raspi_rs::ai::mock::MockAiDetector::new())),
                    None => None,
                };

                if let (Some(detector), Some(model)) = (detector, model) {
                    let bus = mibee_eye_raspi_rs::pipeline::bus::EventBus::new(64);
                    let ai_module = mibee_eye_raspi_rs::ai::AiModule::new(
                        detector,
                        model.id.clone(),
                        yuv.clone(),
                        Some(bus.clone()),
                        config.features.ai.clone(),
                    );
                    let last_detections = ai_module.last_detections_arc();
                    let model_name = ai_module.model_name();
                    let ai_module = Arc::new(ai_module);
                    let _ai_handle = ai_module.start();
                    // T7: EventBus â WsHub bridge â forwards AiDetection events to
                    // connected WebSocket clients. Singleton, lives for process lifetime.
                    spawn_ai_event_bridge(Some(bus.clone()));
                    ai_last_detections = Some(last_detections);
                    ai_model_name = Some(model_name.clone());
                    ai_event_bus = Some(bus);
                    ai_module_handle = Some(ai_module);
                    println!("ai: started with model {} ({})", model.id, model_name);
                } else {
                    println!("ai: disabled - detector load failed");
                }
            } else {
                println!("ai: disabled - hardware capability gate denied");
            }
        } else {
            println!("ai: disabled - camera pipeline not available");
        }
    } else {
        println!("ai: disabled in config");
    }
    // Detector factory for SPEC §4.6 model hot-switching. Only real builds
    // (feature `ai`) can load ONNX sessions at runtime; plain builds leave
    // capability `ai_models` false and activation answers 501.
    #[cfg(feature = "ai")]
    let ai_loader: Option<mibee_eye_raspi_rs::ai::registry::AiLoader> = Some(Arc::new(
        |path: &str, family: mibee_eye_raspi_rs::ai::registry::Family| {
            if !mibee_eye_raspi_rs::ai::registry::is_available(path) {
                Err(
                    mibee_eye_raspi_rs::ai::registry::ActivateError::Unavailable(format!(
                        "model file not available: {path}"
                    )),
                )
            } else {
                OrtDetector::new(path, family)
                    .map(|d| {
                        println!("ai: loaded ONNX model: {}", d.model_name());
                        Arc::new(d) as Arc<dyn AiDetector>
                    })
                    .map_err(|e| {
                        mibee_eye_raspi_rs::ai::registry::ActivateError::LoadFailed(format!("{e}"))
                    })
            }
        },
    ));
    #[cfg(not(feature = "ai"))]
    let ai_loader: Option<mibee_eye_raspi_rs::ai::registry::AiLoader> = None;

    // --- Set up and start ONVIF SOAP server ---
    let onvif_cfg = OnvifConfig {
        port: config.onvif.port,
        username: config.onvif.username.clone(),
        password: config.onvif.password.clone(),
        // An empty password has always meant "auth off" for this host
        // (Config::load only warns); onvif-device-rs 0.3.0 is fail-closed
        // unless that is stated explicitly.
        allow_no_auth: config.onvif.password.is_empty(),
        ..Default::default()
    };
    let mut onvif_server = OnvifServer::new(&onvif_cfg);

    // Device service handlers. onvif-device-rs 0.6 fail-closes on the
    // neutral identity placeholders (issue #20); Config::load backfills
    // the documented defaults, so an error here means the host explicitly
    // configured placeholder/empty identity — keep the remaining ONVIF
    // services up and say so instead of dying.
    match DeviceServiceHandlers::new(
        config.device.clone(),
        config.onvif.port,
        device_ip.clone(),
    ) {
        Ok(svc) => {
            let device_svc = Arc::new(svc);
            for action in [
                "GetSystemDateAndTime",
                "GetDeviceInformation",
                "GetCapabilities",
                "GetServices",
                "GetScopes",
            ] {
                onvif_server.register_handler(
                    action,
                    Box::new(DeviceHandler(Arc::clone(&device_svc))),
                );
            }
        }
        Err(e) => eprintln!(
            "onvif: device identity config rejected ({e}) — device service actions stay unregistered; set real values in [device]"
        ),
    }

    // Pre-auth actions per ONVIF Core spec â reachable without authentication:
    //   GetCapabilities / GetServices: needed during NVR discovery so clients can
    //     read service endpoints before they have credentials to compute a digest.
    //   GetSystemDateAndTime: clients sync the clock before computing WS-Security
    //     username-token digests.
    for action in ["GetSystemDateAndTime", "GetCapabilities", "GetServices"] {
        onvif_server.register_anonymous_action(action);
    }

    // Media service handlers
    let media_cfg = Arc::new(OnvifMediaConfig {
        camera_width: config.camera.width,
        camera_height: config.camera.height,
        camera_fps: config.camera.fps,
        camera_bitrate: config.camera.bitrate as u32,
        rtsp_port: config.rtsp.port,
        device_ip: device_ip.clone(),
        stream_path: "/stream".to_string(),
        // GetSnapshotUri → the legacy public JPEG endpoint (SPEC appendix A).
        snapshot_port: config.web.port,
        snapshot_path: "/snapshot".to_string(),
        profile_token: "main".to_string(),
        video_source_token: "videoSrc0".to_string(),
        encoder_token: "enc0".to_string(),
        encoding: VideoEncoding::H264,
        video_source_name: "Video Source".to_string(),
    });
    onvif_server.register_handler(
        "GetProfiles",
        Box::new(GetProfilesHandler::new(Arc::clone(&media_cfg))),
    );
    onvif_server.register_handler(
        "GetSnapshotUri",
        Box::new(GetSnapshotUriHandler::new(Arc::clone(&media_cfg))),
    );
    onvif_server.register_handler(
        "GetStreamUri",
        Box::new(GetStreamUriHandler::new(Arc::clone(&media_cfg))),
    );
    onvif_server.register_handler(
        "GetVideoSources",
        Box::new(GetVideoSourcesHandler::new(Arc::clone(&media_cfg))),
    );

    // PTZ service handler (dispatches internally based on body content)
    let ptz_state = Arc::new(PtzState::new());
    for action in [
        "ContinuousMove",
        "AbsoluteMove",
        "RelativeMove",
        "Stop",
        "GetStatus",
        "GetPresets",
        "SetPreset",
        "GotoPreset",
        "RemovePreset",
        "GetNodes",
        "GetConfigurations",
    ] {
        onvif_server.register_handler(action, Box::new(PtzHandler(Arc::clone(&ptz_state))));
    }

    println!("onvif: listening on :{}", config.onvif.port);
    tokio::spawn(async move {
        // start() returns a handle whose Drop stops the server — awaiting it
        // keeps the spawn alive for the process lifetime.
        match onvif_server.start().await {
            Ok(handle) => {
                let _ = handle.await;
            }
            Err(e) => eprintln!("onvif: {e}"),
        }
    });

    // --- Start WS-Discovery ---
    // with_identity restores the device's own scopes (onvif-device-rs 0.3.0
    // defaults to the neutral spec profile scope only).
    let discovery = DiscoveryServer::with_identity(
        &device_ip,
        config.onvif.port,
        &config.device.name,
        &config.device.hardware_id,
    );
    tokio::spawn(async move {
        // Same Drop-stops-server contract as the SOAP server above.
        match discovery.start().await {
            Ok(handle) => {
                let _ = handle.await;
            }
            Err(e) => eprintln!("discovery: {e}"),
        }
    });

    // --- Start GB28181 server (if enabled) ---
    if config.gb28181.enabled {
        if let Some(gb_hub) = au_hub.clone() {
            let mut gb_config = config.gb28181.clone();
            // Keep the device's self-description identical across the ONVIF
            // and GB28181 interfaces (gb28181-rs 0.6.0 defaults to neutral
            // identity so it never advertises a vendor by accident).
            gb_config.device_name = Some(config.device.name.clone());
            gb_config.manufacturer = Some(config.device.manufacturer.clone());
            gb_config.model = Some(config.device.model.clone());
            gb_config.firmware = Some(config.device.firmware.clone());
            // Serve RecordInfo queries from the recording index when recording is on.
            let rec_source: Option<Arc<dyn RecordingSource>> = if config.recording.enabled {
                Some(Arc::new(DiskRecordingSource {
                    root: PathBuf::from(&config.recording.storage_path),
                }))
            } else {
                None
            };
            // GB 35114 A-level: replaces Digest auth when configured (and
            // the binary carries the gb35114 feature). A misconfigured
            // security identity is fatal — it must not silently downgrade.
            let authenticator = match mibee_eye_raspi_rs::gb35114_glue::build(
                &config.gb28181.gb35114,
                &gb_config.device_id,
            ) {
                Ok(auth) => auth,
                // A misconfigured security identity must not silently
                // downgrade to Digest — refuse to start (Go twin: log.Fatalf).
                Err(e) => {
                    eprintln!("gb28181: {e:#}");
                    std::process::exit(1);
                }
            };
            println!("gb28181: starting on port {}", gb_config.local_sip_port);
            // Snapshot executor input: the same shared latest-YUV slot
            // the /snapshot endpoint serves (an empty slot when the
            // camera is absent — exchanges then fail cleanly).
            let snapshot_yuv = latest_yuv
                .clone()
                .unwrap_or_else(|| Arc::new(std::sync::Mutex::new(None)));
            tokio::spawn(async move {
                loop {
                    // At boot the interface may still be coming up ("Network
                    // is unreachable") — retry with backoff instead of
                    // abandoning the protocol task until the next restart.
                    let server = retry_start(
                        || async {
                            match Gb28181Server::with_recording_index(
                                gb_config.lib.clone(),
                                Arc::new(AuHubFrameSource(gb_hub.clone(), observe.clone())),
                                rec_source.clone(),
                            )
                            .with_register_authenticator(authenticator.clone())
                            .with_snapshot_executor(Some(Arc::new(
                                mibee_eye_raspi_rs::gb28181_snapshot::YuvSnapshotUploader {
                                    latest_yuv: snapshot_yuv.clone(),
                                },
                            )))
                            .spawn()
                            .await
                            {
                                Ok(handle) => Some(handle),
                                Err(e) => {
                                    eprintln!("gb28181: {e} — retrying");
                                    None
                                }
                            }
                        },
                        Duration::from_secs(1),
                        Duration::from_secs(30),
                    )
                    .await;
                    let _ = server.await;
                    eprintln!("gb28181: server stopped — restarting");
                }
            });
        } else {
            eprintln!("gb28181: enabled but RTSP server failed to create au_hub - skipping");
        }
    }

    // --- Start local recording (if enabled) ---
    if config.recording.enabled {
        if let Some(rec_hub) = au_hub.clone() {
            let rec_config = config.recording.clone();
            println!(
                "recording: enabled, root={} segment_secs={} retention_days={} max_storage_mb={}",
                rec_config.storage_path,
                rec_config.segment_secs,
                rec_config.retention_days,
                rec_config.max_storage_mb
            );
            tokio::spawn(async move {
                // RTC-less boot: wait (bounded) for NTP sync so segment names
                // and index timestamps are not minutes off; record anyway if
                // sync never arrives — losing footage is worse.
                if !mibee_eye_raspi_rs::recording::clock_sync::wait_for_clock_sync(
                    mibee_eye_raspi_rs::recording::clock_sync::systemd_clock_synced,
                    std::time::Duration::from_secs(2),
                    std::time::Duration::from_secs(120),
                )
                .await
                {
                    eprintln!(
                        "recording: clock not NTP-synced after 120s — starting anyway (timestamps may be skewed)"
                    );
                }
                if let Err(e) =
                    mibee_eye_raspi_rs::recording::writer::run(rec_hub, rec_config).await
                {
                    eprintln!("recording: {e}");
                }
            });
            // Retention/capacity sweep task.
            let ret_config = config.recording.clone();
            tokio::spawn(async move {
                mibee_eye_raspi_rs::recording::retention::run(ret_config).await;
            });
        } else {
            eprintln!("recording: enabled but RTSP server failed to create au_hub - skipping");
        }
    }

    // --- Start Web management server (blocks until Ctrl-C) ---
    let mut web = WebServer::new(&config.web).with_observe(web_observe);
    if let Some(ref yuv) = latest_yuv {
        web = web.with_camera_yuv(yuv.clone());
    }
    if let Some(ref hub) = au_hub_for_web {
        web = web.with_au_hub(hub.clone());
    }
    if let Some(detections) = ai_last_detections {
        web = web.with_latest_detections(detections);
    }
    if let Some(model) = ai_model_name {
        web = web.with_ai_model(model);
    }
    if let Some(module) = ai_module_handle {
        web = web.with_ai_module(module);
    }
    if let Some(loader) = ai_loader {
        web = web.with_ai_loader(loader);
    }
    web = web.with_registry(registry);
    // POST /api/system/restart (SPEC §5.1): exit after a grace period; the
    // systemd unit (Restart=always) brings the service back with the newly
    // persisted config applied.
    web = web.with_restart_action(std::sync::Arc::new(|| {
        std::process::exit(0);
    }));
    if let Some(bus) = ai_event_bus {
        web = web.with_event_bus(bus);
    }
    println!("web: listening on :{}", config.web.port);
    if let Err(e) = web.start(config, config_path.clone()).await {
        eprintln!("web: fatal â {e}");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// AI â WebSocket bridge
// ---------------------------------------------------------------------------

/// Spawn the singleton EventBus â WsHub bridge task (T7).
///
/// Subscribes to the AI event bus and forwards `AiDetection` events to all
/// connected WebSocket clients. No-op when AI is disabled (`None`).
fn spawn_ai_event_bridge(event_bus: Option<EventBus>) {
    let Some(bus) = event_bus else {
        return;
    };
    tokio::spawn(async move {
        let mut rx = bus.subscribe();
        loop {
            match rx.recv().await {
                Ok(PipelineEvent::AiDetection {
                    detections,
                    frame_number,
                }) => {
                    global_hub().broadcast(
                        "ai_detection",
                        &serde_json::json!({
                            "camera_id": "0",
                            "detections": detections,
                            "frame_number": frame_number,
                        }),
                    );
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!("sse-bridge: lagged {n} events");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    eprintln!("sse-bridge: event bus closed, exiting");
                    break;
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Camera capture pipeline
// ---------------------------------------------------------------------------

/// Create the camera source and spawn a background task that captures
// H.264 frames and pushes them to the RTSP server's AuHub.
async fn start_camera_pipeline(
    config: &Config,
    au_hub: Arc<mibee_eye_raspi_rs::h264::hub::AuHub>,
) -> Option<mibee_eye_raspi_rs::camera::v4l2_capture::LatestYuv> {
    let device = config.camera.device.clone();
    let width = config.camera.width;
    let height = config.camera.height;
    let fps = config.camera.fps;
    let bitrate = config.camera.bitrate as u32;

    // V4L2 capture producer â device is opened lazily on the encoder thread
    // because the libcamera v4l2-compat layer is thread-local.
    let mut producer = V4l2CaptureProducer::new(&device, width, height, fps);
    let latest_yuv = producer.latest_yuv.clone();

    // Device-level flips (camera.hflip / camera.vflip) are baked into the
    // captured frames before encoding — every consumer sees them.
    producer.set_flips(config.camera.hflip, config.camera.vflip);

    // Video watermark (watermark.* config, SPEC §5.2) — same bake-in point.
    // Fail-open: a broken font_path falls back to the embedded font inside
    // Watermark::new; only a total init failure disables the watermark.
    if config.watermark.enabled {
        match mibee_eye_raspi_rs::watermark::Watermark::new(&config.watermark) {
            Ok(wm) => {
                println!(
                    "watermark: enabled (position={:?}, font_size={})",
                    config.watermark.position, config.watermark.font_size
                );
                producer.set_watermark(wm);
            }
            Err(e) => eprintln!("watermark: init failed — {e}; continuing without watermark"),
        }
    }

    // AI mode requires more frequent YUV frame sharing for inference.
    if config.features.ai.enabled {
        producer.set_yuv_share_interval(3);
    }
    println!("camera: will open {device} {width}x{height}@{fps}fps");

    // Build the V4L2 M2M H.264 encoder configuration.
    let camera_config = CameraConfig {
        width,
        height,
        fps,
        bitrate_bps: bitrate,
        device_path: "/dev/video11".to_string(), // bcm2835-codec-encode
        profile: H264Profile::High,
        level: H264Level::Level4_0,
        i_period: fps * 2, // IDR every 2 seconds
    };

    let mut camera = V4l2CameraSource::new(camera_config, producer);

    match camera.start().await {
        Ok(()) => println!("camera: H.264 encoder started"),
        Err(e) => {
            eprintln!("camera: encoder failed to start â {e}");
            return None;
        }
    }

    // Spawn the capture â AuHub pump.
    tokio::spawn(async move {
        println!("camera: streaming to RTSP AuHub");
        let mut frame_count = 0u64;
        loop {
            match camera.next_frame().await {
                Ok(frame) => {
                    frame_count += 1;
                    if frame_count == 1 || frame_count.is_multiple_of(150) || frame.is_key_frame {
                        println!(
                            "camera: frame {frame_count} ({} bytes, key={}, nalus={})",
                            frame.data.len(),
                            frame.is_key_frame,
                            Parser::parse(&frame.data).len()
                        );
                    }
                    if frame.frame_type != FrameType::H264AnnexB {
                        continue;
                    }
                    let nalus = Parser::parse(&frame.data);
                    if nalus.is_empty() {
                        continue;
                    }
                    let au = AccessUnit {
                        nalus,
                        timestamp: Instant::now(),
                        is_key_frame: frame.is_key_frame,
                    };
                    au_hub.write(au);
                }
                Err(e) => {
                    eprintln!("camera: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
    });

    Some(latest_yuv)
}

/// Resolve the configuration file path.
///
/// Search order:
/// 1. `--config <path>` command-line argument (used by the systemd unit).
/// 2. `config.toml` in the current working directory (local dev).
/// 3. `/etc/mibee-eye/config.toml` (production install).
fn resolve_config_path() -> String {
    let args: Vec<String> = std::env::args().collect();
    for i in 0..args.len() {
        if args[i] == "--config" {
            if let Some(p) = args.get(i + 1) {
                return p.clone();
            }
        } else if let Some(p) = args[i].strip_prefix("--config=") {
            return p.to_string();
        }
    }
    if std::path::Path::new("config.toml").exists() {
        return "config.toml".to_string();
    }
    "/etc/mibee-eye/config.toml".to_string()
}

/// Best-effort local IP detection by opening a UDP socket toward a public
/// address (no packets are actually sent for UDP `connect`).
fn detect_local_ip() -> String {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:80")?;
            s.local_addr().map(|a| a.ip().to_string())
        })
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}

/// Keep calling `start` until it returns `Some`, sleeping with exponential
/// backoff between failures.
///
/// Boot-race resilience: the GB28181 SIP stack starts with the service, but
/// on a fresh boot (Pi, no RTC) the network interface can still be coming
/// up — `Gb28181Server::start` then fails with "Network is unreachable".
/// The camera/web side is already serving, so instead of abandoning the
/// protocol task, keep retrying until the interface is ready.
async fn retry_start<F, Fut, T>(mut start: F, base: Duration, cap: Duration) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let mut delay = base;
    loop {
        if let Some(value) = start().await {
            return value;
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(cap);
    }
}

#[cfg(test)]
mod retry_start_tests {
    use super::*;

    /// Fails twice ("network unreachable"-style), then starts — the helper
    /// must keep calling and finally return the started value.
    #[tokio::test(start_paused = true)]
    async fn retries_until_start_succeeds() {
        let attempts = std::sync::atomic::AtomicU32::new(0);
        let started = retry_start(
            || async {
                let n = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n < 2 {
                    None
                } else {
                    Some("server-up")
                }
            },
            Duration::from_millis(100),
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(started, "server-up");
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    /// Backoff doubles per failure and is capped: 3 failures with
    /// base=100ms cap=150ms sleep 100 + 150 + 150 = 400ms total.
    #[tokio::test(start_paused = true)]
    async fn backoff_doubles_then_caps() {
        let attempts = std::sync::atomic::AtomicU32::new(0);
        let began = tokio::time::Instant::now();
        retry_start(
            || async {
                let n = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n < 3 {
                    None
                } else {
                    Some(())
                }
            },
            Duration::from_millis(100),
            Duration::from_millis(150),
        )
        .await;
        assert_eq!(began.elapsed(), Duration::from_millis(400));
    }

    /// First-attempt success must not sleep at all.
    #[tokio::test(start_paused = true)]
    async fn immediate_success_no_delay() {
        let began = tokio::time::Instant::now();
        let v = retry_start(
            || async { Some(7u32) },
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(v, 7);
        assert_eq!(began.elapsed(), Duration::ZERO);
    }
}
