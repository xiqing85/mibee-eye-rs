//! Entry point for the mibee-eye-raspi-rs ONVIF camera service.

#[cfg(feature = "ai")]
use mibee_eye_raspi_rs::ai::ortv::OrtDetector;
use mibee_eye_raspi_rs::camera::encoder_probe::{self as enc_probe, EncoderProbe, SelectedEncoder};
#[cfg(feature = "software-encoder")]
use mibee_eye_raspi_rs::camera::software::SoftwareCameraSource;
use mibee_eye_raspi_rs::camera::source::{
    CameraConfig, CameraSource, FrameType, H264Level, H264Profile,
};
#[cfg(feature = "v4l2-encoder")]
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

    println!("mibee-eye-raspi-rs v{}", env!("CARGO_PKG_VERSION"));
    println!("ONVIF / GB28181 camera service for Linux boards");

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

    // On-demand IDR request shared with the camera encoder threads
    // (raised by DeviceControl IFrameCmd Send, consumed per frame).
    let idr_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // GB RecordCmd runtime gate: StopRecord pauses segment writing,
    // Record resumes (platform-requested manual recording).
    let rec_pause = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // --- Start RTSP server ---
    let rtsp_config = RtspConfig {
        port: config.rtsp.port,
        username: config.rtsp.username.clone(),
        password: config.rtsp.password.clone(),
        realm: "MiBee Eye RTSP".to_string(),
    };
    let mut latest_yuv: Option<mibee_eye_raspi_rs::camera::v4l2_capture::LatestYuv> = None;
    // Live flip flags handle for the GB FrameMirror control (runtime).
    let mut gb_flips: Option<Arc<mibee_eye_raspi_rs::camera::v4l2_capture::Flips>> = None;
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

            if let Some((yuv, flips)) =
                start_camera_pipeline(&config, au_hub_internal, Arc::clone(&idr_flag)).await
            {
                latest_yuv = Some(yuv);
                gb_flips = Some(flips);
            }
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

    // Pull-Point events service (onvif-device-rs 0.7): AI motion alarms
    // publish as MotionAlarm while an NVR holds a subscription. The
    // publish seam must be taken before start; None = disabled by config.
    let onvif_events = config
        .onvif
        .events_enabled
        .then(|| onvif_server.enable_events());

    // Device service handlers. onvif-device-rs 0.6 fail-closes on the
    // neutral identity placeholders (issue #20); Config::load backfills
    // the documented defaults, so an error here means the host explicitly
    // configured placeholder/empty identity — keep the remaining ONVIF
    // services up and say so instead of dying.
    match DeviceServiceHandlers::new(
        config.device.clone(),
        config.onvif.port,
        device_ip.clone(),
    )
    // Advertise the events service exactly when its routes are served
    // (enable_events above) — the pair must not disagree.
    .map(|svc| svc.with_events_support(config.onvif.events_enabled))
    {
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

    // Media service handlers — dimensions are the post-rotation effective
    // ones so Profile S matches the actual stream aspect (SPEC A #19).
    let (onvif_w, onvif_h) = config.camera.effective_dims();
    let media_cfg = Arc::new(OnvifMediaConfig {
        camera_width: onvif_w,
        camera_height: onvif_h,
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

    // --- AI detections → alarm fan-out (one rising-edge bridge) ---
    // Accepted edges (edge + AlarmReport gate + cooldown) feed three
    // sinks: GB alarm NOTIFY (§9.5, only while a platform subscribes),
    // the SPEC v1 §6 `alarm` SSE event, and the ONVIF MotionAlarm
    // pull-point event. Created outside the GB branch so SSE/ONVIF
    // alarms do not require GB28181; the GB server task attaches its
    // notifier below.
    let alarm_bridge = Arc::new(mibee_eye_raspi_rs::gb28181_alarm::AlarmBridge::new(
        config.gb28181.alarm_notify_enabled,
        Duration::from_secs(config.gb28181.alarm_cooldown_secs),
    ));
    // SPEC v1 §6 `alarm` SSE: the forwarder formats accepted rising
    // edges onto the web event hub.
    let (alarm_tx, mut alarm_rx) = tokio::sync::mpsc::unbounded_channel::<(u64, usize)>();
    alarm_bridge.set_sse_sink(Some(alarm_tx));
    tokio::spawn(async move {
        while let Some((ms, targets)) = alarm_rx.recv().await {
            mibee_eye_raspi_rs::web::events::global_hub().broadcast(
                "alarm",
                &serde_json::json!({
                    "camera_id": "0",
                    "active": true,
                    "source": "ai",
                    "targets": targets,
                    "timestamp": ms,
                }),
            );
        }
    });
    // ONVIF MotionAlarm: same accepted edge, published to every live
    // pull-point subscription (no subscriber = no-op).
    if let Some(events) = onvif_events.clone() {
        alarm_bridge.set_onvif_sink(Some(Arc::new(move |_ms, targets| {
            events.publish_event(mibee_eye_raspi_rs::onvif_alarm::motion_alarm_event(targets));
        })));
    }

    // AI detections feed the bridge from the pipeline event bus
    // (independent of GB28181 being enabled).
    if let Some(bus) = ai_event_bus.clone() {
        let bridge = Arc::clone(&alarm_bridge);
        tokio::spawn(async move {
            let mut rx = bus.subscribe();
            loop {
                match rx.recv().await {
                    Ok(mibee_eye_raspi_rs::pipeline::bus::PipelineEvent::AiDetection {
                        detections,
                        ..
                    }) => {
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        bridge.on_detections(now_ms, detections.len());
                    }
                    // Lagged batches are fine — the next one
                    // carries fresh state.
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => break,
                }
            }
        });
    }

    // --- Start GB28181 server (if enabled) ---
    // Coordinates process exits (SIGTERM / web restart) with the GB
    // supervisor below: exit paths request, the supervisor deregisters
    // (REGISTER Expires: 0) and only then acks.
    let gb_exit = GracefulExit::new();
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
            // The shared alarm bridge (created above the GB branch —
            // the SSE/ONVIF sinks do not depend on GB28181) receives
            // the live notifier per server restart; DeviceConfig(AlarmReport)
            // keeps gating all three sinks at runtime.
            let alarm_bridge = Arc::clone(&alarm_bridge);
            // Static surveyed coordinates → MobilePosition NOTIFYs
            // (§9.5.3) while a platform subscribes; unset = no source.
            let static_position: Option<
                Arc<dyn mibee_eye_raspi_rs::gb28181::subscribe::MobilePositionSource>,
            > = if gb_config.longitude.is_empty() || gb_config.latitude.is_empty() {
                None
            } else {
                Some(Arc::new(
                    mibee_eye_raspi_rs::gb28181_position::StaticPosition::new(
                        &gb_config.longitude,
                        &gb_config.latitude,
                    ),
                ))
            };
            println!("gb28181: starting on port {}", gb_config.local_sip_port);
            // Snapshot executor input: the same shared latest-YUV slot
            // the /snapshot endpoint serves (an empty slot when the
            // camera is absent — exchanges then fail cleanly).
            let snapshot_yuv = latest_yuv
                .clone()
                .unwrap_or_else(|| Arc::new(std::sync::Mutex::new(None)));
            let rec_pause_gb = Arc::clone(&rec_pause);
            let recording_enabled_gb = config.recording.enabled;
            let gb_exit_sup = gb_exit.clone();
            tokio::spawn(async move {
                let mut want_rx = gb_exit_sup.want.subscribe();
                loop {
                    // At boot the interface may still be coming up ("Network
                    // is unreachable") — retry with backoff instead of
                    // abandoning the protocol task until the next restart.
                    let server = retry_start(
                        || async {
                            let built = Gb28181Server::with_recording_index(
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
                            .with_config_handler(Some(Arc::new(
                                mibee_eye_raspi_rs::gb28181_alarm::DeviceConfigGlue {
                                    alarm: Arc::clone(&alarm_bridge),
                                    flips: gb_flips
                                        .clone()
                                        .unwrap_or_else(|| Arc::new(Default::default())),
                                },
                            )))
                            .with_control_handler(Some(Arc::new(ControlGlue {
                                idr_flag: Arc::clone(&idr_flag),
                                rec_pause: Arc::clone(&rec_pause_gb),
                                recording_enabled: recording_enabled_gb,
                            })))
                            .with_position_source(static_position.clone());
                            // Hand the live notifier to the alarm bridge
                            // before the task owns the server.
                            alarm_bridge.update_notifier(Some(built.notifier()));
                            match built.spawn().await {
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
                    if *want_rx.borrow() {
                        // Exit requested while retrying — nothing to
                        // deregister (the server never came up).
                        let _ = gb_exit_sup.done.send(true);
                        break;
                    }
                    let mut server = Box::pin(server);
                    // SIP-Date drift observation (§9.10.2): poll the
                    // platform clock carried by the last REGISTER response
                    // on the same select — observation only, the clock is
                    // never adjusted (see gb28181_date). The inner loop
                    // keeps observing on the SAME server; only a server
                    // stop or the exit request leaves it.
                    let mut date_tick = tokio::time::interval(
                        mibee_eye_raspi_rs::gb28181_date::DATE_OBSERVER_INTERVAL,
                    );
                    date_tick.tick().await; // first tick is immediate — skip
                    let mut last_warned: Option<u64> = None;
                    loop {
                        tokio::select! {
                            _ = &mut server => {
                                eprintln!("gb28181: server stopped — restarting");
                                break;
                            }
                            _ = date_tick.tick() => {
                                if let Some(platform) = server.platform_date_unix() {
                                    let local = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_secs() as i64)
                                        .unwrap_or_default();
                                    use mibee_eye_raspi_rs::gb28181_date::DateDriftOutcome;
                                    match mibee_eye_raspi_rs::gb28181_date::evaluate_date_drift(
                                        platform, local, last_warned,
                                    ) {
                                        DateDriftOutcome::Warn(drift) => {
                                            eprintln!(
                                                "gb28181: WARN platform clock drifts {drift}s from local \
                                                 (SIP Date, observation only — clock not adjusted)"
                                            );
                                            last_warned = Some(drift.unsigned_abs());
                                        }
                                        DateDriftOutcome::Recovered => {
                                            eprintln!("gb28181: platform clock drift back within threshold");
                                            last_warned = None;
                                        }
                                        DateDriftOutcome::Stable => {}
                                    }
                                }
                            }
                            _ = want_rx.changed() => {
                                if !*want_rx.borrow() { continue; }
                                // Graceful exit: deregister first (REGISTER
                                // Expires: 0, 401 dance, 2s timeouts inside the
                                // library). Every failure only logs — the exit
                                // must always proceed.
                                match tokio::time::timeout(
                                    Duration::from_secs(8),
                                    server.as_mut().shutdown_with_deregister(),
                                ).await {
                                    Ok(Ok(())) => println!("gb28181: deregistered before exit"),
                                    Ok(Err(e)) => eprintln!("gb28181: deregister error — {e}"),
                                    Err(_) => {
                                        eprintln!("gb28181: deregister timed out; aborting server task");
                                        server.as_mut().abort();
                                    }
                                }
                                let _ = gb_exit_sup.done.send(true);
                                break;
                            }
                        }
                    }
                }
            });
        } else {
            eprintln!("gb28181: enabled but RTSP server failed to create au_hub - skipping");
            gb_exit.mark_done();
        }
    } else {
        // GB disabled — exit paths must not wait for a deregister that
        // will never happen.
        gb_exit.mark_done();
    }

    // --- Start local recording (if enabled) ---
    if config.recording.enabled {
        if let Some(rec_hub) = au_hub.clone() {
            let rec_config = config.recording.clone();
            let rec_pause_gate = Arc::clone(&rec_pause);
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
                if let Err(e) = mibee_eye_raspi_rs::recording::writer::run_gated(
                    rec_hub,
                    rec_config,
                    rec_pause_gate,
                )
                .await
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
    // POST /api/system/restart (SPEC §5.1): deregister GB28181 first so the
    // platform notices the restart immediately, then exit; the systemd unit
    // (Restart=always) brings the service back with the newly persisted
    // config applied. Spawned (not inline) so the HTTP handler returns and
    // the response flushes before the process dies.
    let gb_exit_restart = gb_exit.clone();
    web = web.with_restart_action(std::sync::Arc::new(move || {
        let exit = gb_exit_restart.clone();
        tokio::spawn(async move {
            graceful_exit(exit).await;
        });
    }));
    // SIGTERM (systemd stop/restart) gets the same sequence: request the
    // GB supervisor to deregister, then exit once it acks (10s ceiling).
    {
        let exit = gb_exit.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
            let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
            tokio::select! {
                _ = term.recv() => println!("shutdown: SIGTERM received"),
                _ = int.recv() => println!("shutdown: SIGINT received"),
                _ = tokio::signal::ctrl_c() => println!("shutdown: ctrl_c received"),
            }
            graceful_exit(exit).await;
        });
    }
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
// Graceful exit coordinator (SIGTERM / web restart → GB deregister → exit)
// ---------------------------------------------------------------------------

/// Two-phase shutdown handshake between process exit paths and the GB28181
/// supervisor task:
///
/// - exit paths (SIGTERM/SIGINT handler, `POST /api/system/restart`) raise
///   `want` and then block on `done` (10s ceiling — exit must always win);
/// - the supervisor selects on `want` between restarts, deregisters the
///   live server (REGISTER `Expires: 0`) and raises `done`;
/// - when GB is disabled (or never started) `mark_done` short-circuits the
///   handshake so exits do not stall.
#[derive(Clone)]
struct GracefulExit {
    want: std::sync::Arc<tokio::sync::watch::Sender<bool>>,
    done: std::sync::Arc<tokio::sync::watch::Sender<bool>>,
    /// Hold the initial receivers for the process lifetime: `watch`
    /// senders refuse to store a value while no receiver exists, and the
    /// first supervisor/waiter may subscribe only after an exit path has
    /// already fired.
    _keep_alive: (
        std::sync::Arc<tokio::sync::watch::Receiver<bool>>,
        std::sync::Arc<tokio::sync::watch::Receiver<bool>>,
    ),
}

impl GracefulExit {
    fn new() -> Self {
        let (want, want_rx) = tokio::sync::watch::channel(false);
        let (done, done_rx) = tokio::sync::watch::channel(false);
        Self {
            want: std::sync::Arc::new(want),
            done: std::sync::Arc::new(done),
            _keep_alive: (std::sync::Arc::new(want_rx), std::sync::Arc::new(done_rx)),
        }
    }

    /// Nothing will deregister — release exit paths immediately.
    fn mark_done(&self) {
        let _ = self.done.send(true);
    }

    fn request(&self) {
        let _ = self.want.send(true);
    }

    /// Resolve once the supervisor acked (or was marked done up front).
    async fn wait_done(&self) {
        let mut rx = self.done.subscribe();
        if *rx.borrow() {
            return;
        }
        let _ = rx.changed().await;
    }
}

/// Exit sequence shared by SIGTERM and the web restart action: request the
/// GB supervisor to deregister, wait for its ack (bounded), exit.
async fn graceful_exit(exit: GracefulExit) {
    exit.request();
    if tokio::time::timeout(Duration::from_secs(10), exit.wait_done())
        .await
        .is_err()
    {
        eprintln!("shutdown: GB deregister did not finish in time — exiting anyway");
    }
    println!("shutdown: exiting");
    std::process::exit(0);
}

#[cfg(test)]
mod graceful_exit_tests {
    use super::*;

    #[tokio::test]
    async fn request_then_done_unblocks_waiter() {
        let exit = GracefulExit::new();
        let waiter = exit.clone();
        let task = tokio::spawn(async move { waiter.wait_done().await });
        // Not done yet — the waiter must still be pending.
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!task.is_finished());
        exit.request(); // irrelevant to done, but must not unblock
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!task.is_finished());
        exit.mark_done();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn marked_done_waiter_returns_immediately() {
        let exit = GracefulExit::new();
        exit.mark_done();
        // Bounded wait — completes without another send (no hang = pass).
        tokio::time::timeout(Duration::from_millis(100), exit.wait_done())
            .await
            .expect("marked-done handshake resolves immediately");
    }

    #[test]
    fn request_is_idempotent() {
        let exit = GracefulExit::new();
        exit.request();
        exit.request();
        let rx = exit.want.subscribe();
        assert!(*rx.borrow());
    }
}

// ---------------------------------------------------------------------------
// AI → WebSocket bridge
// ---------------------------------------------------------------------------
// GB DeviceControl: force IDR
// ---------------------------------------------------------------------------

/// `DeviceControl(IFrameCmd Send)` (§9.3.2): raise the on-demand IDR
/// flag the camera encoder threads consume before the next frame.
/// Platforms send this when starting a pull or after loss — answering it
/// cuts the platform's wait from up to one GOP (2s) to one frame.
///
/// Note: installing a control handler accepts the whole DeviceControl
/// family (library semantics — no-op methods ack-only). PTZ commands
/// have no motor to drive on this hardware and TeleBoot stays a logged
/// no-op.
struct ControlGlue {
    idr_flag: Arc<std::sync::atomic::AtomicBool>,
    rec_pause: Arc<std::sync::atomic::AtomicBool>,
    recording_enabled: bool,
}

impl mibee_eye_raspi_rs::gb28181::server::DeviceControlHandler for ControlGlue {
    fn on_force_iframe(&self) {
        mibee_eye_raspi_rs::camera::raise_idr_request(&self.idr_flag);
    }

    /// GB/T 28181 RecordCmd (§9.3.2): platform-requested manual
    /// recording. StopRecord pauses the (config-enabled) recorder's
    /// segment writing; Record resumes it at a fresh segment boundary.
    /// With recording disabled in config there is no writer to gate —
    /// logged, not silently ignored.
    fn on_record(&self, start: bool) {
        if !self.recording_enabled {
            eprintln!("gb28181: RecordCmd ignored — recording disabled in config");
            return;
        }
        self.rec_pause
            .store(!start, std::sync::atomic::Ordering::Relaxed);
        println!(
            "gb28181: RecordCmd — recording {}",
            if start { "resumed" } else { "paused" }
        );
    }
}

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
    idr_flag: Arc<std::sync::atomic::AtomicBool>,
) -> Option<(
    mibee_eye_raspi_rs::camera::v4l2_capture::LatestYuv,
    Arc<mibee_eye_raspi_rs::camera::v4l2_capture::Flips>,
)> {
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
    // captured frames before encoding — every consumer sees them. Rotation
    // (camera.rotation, SPEC appendix A #19) bakes in first; 90/270 swap
    // the effective stream dimensions (encoder + ONVIF announcements).
    producer.set_flips(config.camera.hflip, config.camera.vflip);
    producer.set_rotation(config.camera.rotation);
    let flips_handle = producer.flips_arc();
    let (effective_w, effective_h) = config.camera.effective_dims();
    if config.camera.rotation != 0 {
        println!(
            "camera: rotation {}° baked in — effective resolution {effective_w}x{effective_h}",
            config.camera.rotation
        );
    }

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

    // Resolve hardware vs software encoder (camera.encoder /
    // camera.encoder_device; see camera::encoder_probe for the matrix).
    let availability = {
        #[cfg(feature = "v4l2-encoder")]
        {
            Some(enc_probe::RealEncoderProbe.probe(&config.camera.encoder_device))
        }
        #[cfg(not(feature = "v4l2-encoder"))]
        {
            None
        }
    };
    let (selection, note) = match enc_probe::decide(&config.camera.encoder, availability) {
        Ok(res) => res,
        Err(e) => {
            eprintln!("camera: encoder selection failed — {e}");
            return None;
        }
    };
    println!("camera: {note}");

    // H.264 encoder configuration shared by both paths. The encoder sees
    // the post-rotation frame, so 90/270 swap its configured dimensions
    // (both the M2M S_FMT and the openh264 view derive from these).
    let camera_config = CameraConfig {
        width: effective_w,
        height: effective_h,
        fps,
        bitrate_bps: bitrate,
        device_path: config.camera.encoder_device.clone(),
        profile: H264Profile::High,
        level: H264Level::Level4_0,
        i_period: fps * 2, // IDR every 2 seconds
    };

    let mut camera: Box<dyn CameraSource> = match selection {
        SelectedEncoder::Hardware => {
            #[cfg(feature = "v4l2-encoder")]
            {
                Box::new(V4l2CameraSource::new(camera_config, producer).with_idr_flag(idr_flag))
            }
            // decide() never selects Hardware when the feature is off.
            #[cfg(not(feature = "v4l2-encoder"))]
            {
                unreachable!("hardware encoder selected in a software-only build")
            }
        }
        SelectedEncoder::Software => {
            #[cfg(feature = "software-encoder")]
            {
                Box::new(SoftwareCameraSource::new(camera_config, producer).with_idr_flag(idr_flag))
            }
            #[cfg(not(feature = "software-encoder"))]
            {
                let _ = camera_config;
                unreachable!("software encoder selected in a hardware-only build")
            }
        }
    };

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

    Some((latest_yuv, flips_handle))
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
            // UDP-connect probe target for local-IP detection (no packets
            // are sent); not a deployment address.
            s.connect("8.8.8.8:80")?; // hardcode-ok: local-IP probe, never sends
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
