//! ONVIF wire integration tests — the product's real server wiring over
//! a real socket.
//!
//! These tests drive `onvif_glue::wire_onvif_server`, the exact
//! registration the binary ships (Device family + hooks, Media store
//! migration, PTZ, Imaging, Media2, IP filter, HTTP Digest), so what is
//! pinned here is what an NVR sees on the network:
//!
//! - the byte-stable legacy responses (GetProfiles / GetStreamUri →
//!   MediaUri → Uri element names, snapshot URI, capabilities/services
//!   advertisement) — the MiBee NVR does raw SOAP local-name matching;
//! - the onvif-device-rs 0.8 completion batch: media encoder family +
//!   sync point (IDR latch), Media2 ver20 face, PTZ completion, the
//!   imaging face, DeviceHooks semantics (observe-only clock, refused
//!   reboots, real system log texts, user directory);
//! - the security seams: HTTP Digest challenge + MD5 handshake, IP
//!   filter enforcement, and the historical no-challenge default;
//! - the events push interface (wsnt:Subscribe → Notify delivery) the
//!   events service serves with zero extra product code.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use mibee_eye_raspi_rs::config::{DeviceConfig, ONVIFConfig};
use mibee_eye_raspi_rs::onvif::events::EVENTS_SERVICE_PATH;
use mibee_eye_raspi_rs::onvif::media::{MediaProfileConfig, OnvifMediaConfig};
use mibee_eye_raspi_rs::onvif::server::OnvifServerHandle;
use mibee_eye_raspi_rs::onvif_glue::{
    wire_onvif_server, DeviceHooksGlue, DeviceReport, ImagingGlue, OnvifWiringInput,
};

const USERNAME: &str = "admin";
const PASSWORD: &str = "password";
const MEDIA2_PATH: &str = "/onvif/media2_service";
/// The library's fixed Digest realm (challenge contract).
const REALM: &str = "onvif";

// ---------------------------------------------------------------------------
// Minimal ONVIF client (what an NVR implements)
// ---------------------------------------------------------------------------

/// POST a SOAP 1.2 envelope whose body is `body_xml` to `path`. With
/// `auth` a PasswordText UsernameToken is carried. Returns
/// `(status, headers, body)` — headers is the raw header section.
async fn post_soap_raw(port: u16, path: &str, body_xml: &str, auth: bool) -> (u16, String, String) {
    let header = if auth {
        format!(
            "<Header><Security xmlns=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd\">\
             <UsernameToken><Username>{USERNAME}</Username>\
             <Password Type=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordText\">{PASSWORD}</Password>\
             </UsernameToken></Security></Header>"
        )
    } else {
        String::new()
    };
    let envelope = format!(
        "<Envelope xmlns=\"http://www.w3.org/2005/soap-envelope\">{header}\
         <Body>{body_xml}</Body></Envelope>"
    );

    let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
         Content-Type: application/soap+xml; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{envelope}",
        envelope.len()
    );
    sock.write_all(request.as_bytes()).await.expect("write");
    let mut raw = Vec::new();
    sock.read_to_end(&mut raw).await.expect("read");
    let text = String::from_utf8_lossy(&raw).to_string();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .expect("status line");
    let split = text.find("\r\n\r\n").map(|p| p + 4).unwrap_or(text.len());
    let (headers, body) = text.split_at(split);
    (status, headers.to_string(), body.to_string())
}

/// Authenticated POST returning `(status, body)`.
async fn post_soap(port: u16, path: &str, body_xml: &str) -> (u16, String) {
    let (status, _, body) = post_soap_raw(port, path, body_xml, true).await;
    (status, body)
}

/// POST with an explicit raw `Authorization` header (Digest tests).
async fn post_soap_authorized(
    port: u16,
    path: &str,
    body_xml: &str,
    authorization: &str,
) -> (u16, String, String) {
    let envelope = format!(
        "<Envelope xmlns=\"http://www.w3.org/2005/soap-envelope\">\
         <Body>{body_xml}</Body></Envelope>"
    );
    let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{authorization}\
         Content-Type: application/soap+xml; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{envelope}",
        envelope.len()
    );
    sock.write_all(request.as_bytes()).await.expect("write");
    let mut raw = Vec::new();
    sock.read_to_end(&mut raw).await.expect("read");
    let text = String::from_utf8_lossy(&raw).to_string();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .expect("status line");
    let split = text.find("\r\n\r\n").map(|p| p + 4).unwrap_or(text.len());
    let (headers, body) = text.split_at(split);
    (status, headers.to_string(), body.to_string())
}

/// Text content of the first `<tag ...>` element (local-name tolerant).
fn xml_field<'a>(xml: &'a str, tag: &str) -> &'a str {
    let open = format!("<{tag}");
    let mut from = 0;
    while let Some(rel) = xml[from..].find(&open) {
        let after = from + rel + open.len();
        let next = xml[after..].chars().next().unwrap_or('>');
        if next == '>' || next == ' ' || next == '/' {
            let start = xml[after..].find('>').map_or(after, |g| after + g + 1);
            let end = xml[start..].find("</").map_or(start, |e| start + e);
            return xml[start..end].trim();
        }
        from = after;
    }
    ""
}

fn header_value<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    headers.lines().find_map(|l| {
        let lower = l.to_ascii_lowercase();
        lower
            .starts_with(&format!("{name}:").to_ascii_lowercase())
            .then(|| l.split_once(':').map(|(_, v)| v.trim()).unwrap_or(""))
    })
}

/// Extract a quoted param (`nonce="..."`) from a Digest header value.
fn digest_param(header_value: &str, key: &str) -> Option<String> {
    let (_, rest) = header_value.split_once(&format!("{key}=\""))?;
    let (value, _) = rest.split_once('"')?;
    Some(value.to_string())
}

fn md5hex(s: &str) -> String {
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    h.update(s.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Server scaffolding — the product wiring, verbatim
// ---------------------------------------------------------------------------

struct Wired {
    port: u16,
    idr_latch: Arc<AtomicBool>,
    hooks: Arc<DeviceHooksGlue>,
    events: Option<Arc<mibee_eye_raspi_rs::onvif::events::EventsService>>,
    media_store: mibee_eye_raspi_rs::onvif::media::SharedMediaConfig,
    handle: OnvifServerHandle,
}

fn test_host_config() -> ONVIFConfig {
    ONVIFConfig {
        username: USERNAME.to_string(),
        password: PASSWORD.to_string(),
        ..ONVIFConfig::default()
    }
}

/// Multi-profile media store shaped like the real deployment: primary
/// `main` 1280x720@15 + substream `sub`, snapshot on the web port.
fn test_media_config() -> OnvifMediaConfig {
    let mut cfg = OnvifMediaConfig::new(1280, 720, 15, 2_000_000, 8554, "127.0.0.1".into());
    cfg.snapshot_port = 8088;
    cfg.snapshot_path = "/snapshot".to_string();
    cfg.extra_profiles = vec![MediaProfileConfig::new(
        "sub", 640, 360, 15, 600_000, "/sub",
    )];
    cfg
}

fn test_device_config() -> DeviceConfig {
    DeviceConfig {
        name: "Wire Test Cam".into(),
        manufacturer: "MiBee".into(),
        model: "OV5647".into(),
        firmware: "1.0.0".into(),
        hardware_id: "HW-1".into(),
        serial_number: "SN-WIRE-1".into(),
    }
}

/// Start the product wiring on an ephemeral loopback port, with
/// `mutate` applied to the `[onvif]` config section first.
async fn start_wired(mutate: impl FnOnce(&mut ONVIFConfig)) -> Wired {
    let mut host = test_host_config();
    mutate(&mut host);
    let idr_latch = Arc::new(AtomicBool::new(false));
    let latch = Arc::clone(&idr_latch);
    let keyframe_hook: Arc<dyn Fn() + Send + Sync> =
        Arc::new(move || latch.store(true, Ordering::SeqCst));
    let hooks = Arc::new(DeviceHooksGlue::new(DeviceReport {
        version: "9.9.9-wire-test".to_string(),
        device_line: "Wire Test Cam — MiBee OV5647".to_string(),
        camera_line: "1280x720@15fps h264 2000000bps".to_string(),
        notes: vec!["gb28181: disabled".to_string()],
    }));
    let wired = wire_onvif_server(OnvifWiringInput {
        host,
        device: test_device_config(),
        device_ip: "127.0.0.1".to_string(),
        media: test_media_config(),
        keyframe_hook: Some(keyframe_hook),
        hooks: Arc::clone(&hooks) as Arc<dyn mibee_eye_raspi_rs::onvif::device::DeviceHooks>,
        imaging: Arc::new(ImagingGlue::new()),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let handle = wired.server.start_on(listener).await.expect("start");
    Wired {
        port,
        idr_latch,
        hooks,
        events: wired.events,
        media_store: wired.media_store,
        handle,
    }
}

/// A local HTTP consumer for wsnt Notify POSTs: answers `status` and
/// forwards each captured request over a channel.
async fn spawn_notify_consumer(status: u16) -> (u16, tokio::sync::mpsc::Receiver<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("consumer bind");
    let port = listener.local_addr().expect("consumer addr").port();
    let (tx, rx) = tokio::sync::mpsc::channel::<String>(16);
    let status_line = Arc::new(format!("HTTP/1.1 {status} Delivered\r\n"));
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let tx = tx.clone();
            let status_line = Arc::clone(&status_line);
            tokio::spawn(async move {
                let mut raw = Vec::new();
                let _ = sock.read_to_end(&mut raw).await;
                let captured = String::from_utf8_lossy(&raw).to_string();
                let response =
                    format!("{status_line}Content-Length: 0\r\nConnection: close\r\n\r\n");
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
                let _ = tx.send(captured).await;
            });
        }
    });
    (port, rx)
}

// ---------------------------------------------------------------------------
// Legacy byte-stable responses (the NVR contract)
// ---------------------------------------------------------------------------

/// The Media1 responses the MiBee NVR matches by raw local names keep
/// their historical shape after the store migration: profile tokens,
/// `GetStreamUriResponse → MediaUri → Uri`, the substream token mapping
/// and the snapshot endpoint.
#[tokio::test]
async fn media1_legacy_shape_is_byte_stable() {
    let mut wired = start_wired(|_| {}).await;

    let (status, body) = post_soap(wired.port, "/onvif/media_service", "<GetProfiles/>").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("GetProfilesResponse"), "{body}");
    // Primary profile first (Profile S consumers pick "the first").
    let main_at = body.find(r#"Profiles token="main""#).expect("main token");
    assert!(body.contains(r#"token="videoSrc0""#), "{body}");
    assert!(body.contains("<Width>1280</Width>"), "{body}");
    assert!(body.contains("<Height>720</Height>"), "{body}");
    assert!(body.contains("<Encoding>H264</Encoding>"), "{body}");
    assert!(
        body.contains("<FrameRateLimit>15</FrameRateLimit>"),
        "{body}"
    );
    assert!(
        body.contains("<BitrateLimit>2000000</BitrateLimit>"),
        "{body}"
    );
    // Substream advertised after the primary, on its own mount.
    let sub_at = body.find(r#"Profiles token="sub""#).expect("sub token");
    assert!(main_at < sub_at, "primary profile must come first");

    // GetStreamUri — the exact element chain the NVR matches on.
    let (status, body) = post_soap(
        wired.port,
        "/onvif/media_service",
        "<GetStreamUri><ProfileToken>main</ProfileToken></GetStreamUri>",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("GetStreamUriResponse"), "{body}");
    assert!(body.contains("MediaUri"), "{body}");
    assert!(body.contains("<Uri>"), "{body}");
    assert!(
        body.contains("rtsp://127.0.0.1:8554/stream"),
        "stream URI host must be the connection's server IP: {body}"
    );
    assert!(body.contains("InvalidAfterConnect"), "{body}");
    assert!(body.contains("InvalidAfterReboot"), "{body}");
    assert!(body.contains("Timeout"), "{body}");

    // The `sub` token maps to the /sub RTSP mount.
    let (status, body) = post_soap(
        wired.port,
        "/onvif/media_service",
        "<GetStreamUri><ProfileToken>sub</ProfileToken></GetStreamUri>",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("rtsp://127.0.0.1:8554/sub"), "{body}");

    // GetSnapshotUri — the legacy public JPEG endpoint.
    let (status, body) = post_soap(wired.port, "/onvif/media_service", "<GetSnapshotUri/>").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("GetSnapshotUriResponse"), "{body}");
    assert!(body.contains("MediaUri"), "{body}");
    assert!(body.contains("http://127.0.0.1:8088/snapshot"), "{body}");

    // GetVideoSources — the videoSrc0 token the profiles reference.
    let (status, body) = post_soap(wired.port, "/onvif/media_service", "<GetVideoSources/>").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#"token="videoSrc0""#), "{body}");

    // Auth stays enforced on the media reads (only the three pre-auth
    // actions are anonymous)…
    let (status, _, _) =
        post_soap_raw(wired.port, "/onvif/media_service", "<GetProfiles/>", false).await;
    assert_eq!(status, 401, "GetProfiles without credentials must 401");
    // …and the pre-auth trio answers unauthenticated.
    let (status, _, body) = post_soap_raw(
        wired.port,
        "/onvif/device_service",
        "<GetSystemDateAndTime/>",
        false,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("GetSystemDateAndTimeResponse"), "{body}");

    wired.handle.shutdown().await.expect("shutdown");
}

/// The GetCapabilities / GetServices advertisement keeps the historical
/// entries (Device/Media/PTZ/Imaging + gated Events) and adds the
/// Media2 entry exactly when the route is served.
#[tokio::test]
async fn capabilities_and_services_advertisement() {
    let mut wired = start_wired(|_| {}).await;
    let port = wired.port;

    let (status, _, body) =
        post_soap_raw(port, "/onvif/device_service", "<GetCapabilities/>", false).await;
    assert_eq!(status, 200, "{body}");
    for entry in ["tt:Device", "tt:Media", "tt:PTZ", "tt:Imaging"] {
        assert!(body.contains(entry), "missing {entry}: {body}");
    }
    // Events advertised exactly when served (default on here).
    assert!(body.contains("tt:Events"), "{body}");
    assert!(body.contains("WSPullPointSupport"), "{body}");
    // The XAddr port is the configured [onvif] port (production binds
    // exactly that port; the ephemeral test listener is a harness
    // artifact) — host is the connection's server IP.
    assert!(
        body.contains("http://127.0.0.1:8080/onvif/device_service"),
        "XAddr host is the connection's server IP: {body}"
    );

    let (status, _, body) =
        post_soap_raw(port, "/onvif/device_service", "<GetServices/>", false).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("GetServicesResponse"), "{body}");
    assert!(
        body.contains("http://www.onvif.org/ver10/device/wsdl"),
        "{body}"
    );
    assert!(
        body.contains("http://www.onvif.org/ver10/media/wsdl"),
        "{body}"
    );
    // media2 default on → ver20/media entry present.
    assert!(
        body.contains("http://www.onvif.org/ver20/media/wsdl"),
        "{body}"
    );
    // WSDL shape (onvif-rs #64/#65 fixes): Service elements are DIRECT
    // children of GetServicesResponse — the tds:Services wrapper that
    // broke cross-library clients is gone.
    assert!(body.contains("<tds:Service>"), "{body}");
    assert!(!body.contains("<tds:Services>"), "{body}");
    assert!(body.contains("<tds:Namespace>"), "{body}");
    assert!(body.contains("<tds:XAddr>"), "{body}");

    // GetScopes answers the WSDL tds:Scopes / tt:Scope form with the
    // three built-in scopes marked Fixed (onvif-rs #65).
    let (status, body) = post_soap(port, "/onvif/device_service", "<GetScopes/>").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("GetScopesResponse"), "{body}");
    assert!(body.contains("<tds:Scopes>"), "{body}");
    assert!(body.contains("<tt:ScopeDef>Fixed</tt:ScopeDef>"), "{body}");
    assert!(body.contains("<tt:ScopeItem>"), "{body}");
    assert!(
        body.contains(&format!("onvif://www.onvif.org/name/{}", "Wire Test Cam")),
        "{body}"
    );

    wired.handle.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// Media completion (onvif-device-rs 0.8): encoder family + sync point
// ---------------------------------------------------------------------------

#[tokio::test]
async fn media_encoder_family_and_sync_point() {
    let mut wired = start_wired(|_| {}).await;
    let port = wired.port;

    // Encoder options advertise the H264 block.
    let (status, body) = post_soap(
        port,
        "/onvif/media_service",
        "<GetVideoEncoderConfigurationOptions/>",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("GetVideoEncoderConfigurationOptionsResponse"),
        "{body}"
    );
    assert!(body.contains("H264"), "{body}");
    assert!(body.contains("ResolutionsAvailable"), "{body}");

    // The configuration listing and single-shot read share the store.
    let (status, body) = post_soap(
        port,
        "/onvif/media_service",
        "<GetVideoEncoderConfigurations/>",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#"token="enc0""#), "{body}");
    let (status, body) = post_soap(
        port,
        "/onvif/media_service",
        "<GetVideoEncoderConfiguration><ConfigurationToken>enc0</ConfigurationToken></GetVideoEncoderConfiguration>",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("<Encoding>H264</Encoding>"), "{body}");

    // One guaranteed encoder instance per source.
    let (status, body) = post_soap(
        port,
        "/onvif/media_service",
        "<GetGuaranteedNumberOfVideoEncoderInstances/>",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("<TotalNumber>1</TotalNumber>"), "{body}");

    // SetSynchronizationPoint fires the product's IDR latch — the next
    // encoded frame becomes a keyframe (the same latch the GB28181
    // IFrameCmd control raises).
    assert!(!wired.idr_latch.load(Ordering::SeqCst));
    let (status, body) = post_soap(
        port,
        "/onvif/media_service",
        "<SetSynchronizationPoint><ProfileToken>main</ProfileToken></SetSynchronizationPoint>",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("SetSynchronizationPointResponse"), "{body}");
    assert!(
        wired.idr_latch.load(Ordering::SeqCst),
        "keyframe hook must raise the IDR latch"
    );

    // SetVideoEncoderConfiguration: the write lands in the shared store
    // (readers observe it) — capture applies on restart, which the
    // wrapper logs instead of pretending the live encoder reconfigured.
    let (status, body) = post_soap(
        port,
        "/onvif/media_service",
        "<SetVideoEncoderConfiguration>\
         <Configuration token=\"enc0\">\
         <Encoding>H264</Encoding>\
         <Resolution><Width>640</Width><Height>360</Height></Resolution>\
         <RateControl><FrameRateLimit>10</FrameRateLimit><BitrateLimit>1000000</BitrateLimit><EncodingInterval>1</EncodingInterval></RateControl>\
         </Configuration></SetVideoEncoderConfiguration>",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("SetVideoEncoderConfigurationResponse"),
        "{body}"
    );
    {
        let store = wired
            .media_store
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(store.camera_bitrate, 1_000_000);
        assert_eq!(store.camera_width, 640);
        assert_eq!(store.camera_fps, 10);
    }
    // …and the read side observes the new value through the store.
    let (status, body) = post_soap(
        port,
        "/onvif/media_service",
        "<GetVideoEncoderConfiguration><ConfigurationToken>enc0</ConfigurationToken></GetVideoEncoderConfiguration>",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("<BitrateLimit>1000000</BitrateLimit>"),
        "{body}"
    );

    // The audio/OSD sets answer honestly empty (no audio hardware).
    let (status, body) = post_soap(port, "/onvif/media_service", "<GetAudioSources/>").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("GetAudioSourcesResponse"), "{body}");

    wired.handle.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// Media2 (ver20/media)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn media2_face_served_and_advertised() {
    let mut wired = start_wired(|_| {}).await;
    let port = wired.port;

    let (status, body) = post_soap(
        port,
        MEDIA2_PATH,
        "<GetProfiles xmlns=\"http://www.onvif.org/ver20/media/wsdl\"/>",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("tr2:GetProfilesResponse"), "{body}");
    assert!(body.contains(r#"tr2:Profiles token="main""#), "{body}");
    assert!(body.contains("<tr2:Configurations>"), "{body}");

    // Stream URI on the Media2 face (plain tr2:Uri form).
    let (status, body) = post_soap(
        port,
        MEDIA2_PATH,
        "<GetStreamUri xmlns=\"http://www.onvif.org/ver20/media/wsdl\"><ProfileToken>main</ProfileToken></GetStreamUri>",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("rtsp://127.0.0.1:8554/stream"), "{body}");

    // The sync point on Media2 drives the SAME IDR latch.
    wired.idr_latch.store(false, Ordering::SeqCst);
    let (status, body) = post_soap(
        port,
        MEDIA2_PATH,
        "<SetSynchronizationPoint xmlns=\"http://www.onvif.org/ver20/media/wsdl\"><ProfileToken>main</ProfileToken></SetSynchronizationPoint>",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(wired.idr_latch.load(Ordering::SeqCst));

    wired.handle.shutdown().await.expect("shutdown");
}

/// `media2_enabled = false` restores the pre-Media2 wire bytes: no
/// ver20/media GetServices entry and a 404 on the service path.
#[tokio::test]
async fn media2_disabled_matches_historical_bytes() {
    let mut wired = start_wired(|host| host.media2_enabled = false).await;
    let port = wired.port;

    let (status, _, body) =
        post_soap_raw(port, "/onvif/device_service", "<GetServices/>", false).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        !body.contains("http://www.onvif.org/ver20/media/wsdl"),
        "no Media2 advertisement when disabled: {body}"
    );

    let (status, body) = post_soap(
        port,
        MEDIA2_PATH,
        "<GetProfiles xmlns=\"http://www.onvif.org/ver20/media/wsdl\"/>",
    )
    .await;
    assert_eq!(status, 404, "media2 route must be unmounted: {body}");

    wired.handle.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// PTZ completion
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ptz_completion_family_answers() {
    let mut wired = start_wired(|_| {}).await;
    let port = wired.port;

    let (status, body) = post_soap(port, "/onvif/ptz_service", "<GetConfigurationOptions/>").await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("tptz:GetConfigurationOptionsResponse"),
        "{body}"
    );

    let (status, body) = post_soap(
        port,
        "/onvif/ptz_service",
        "<SetConfiguration><PTZConfiguration token=\"ptz0\"><Name>main</Name><NodeToken>node0</NodeToken></PTZConfiguration></SetConfiguration>",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("tptz:SetConfigurationResponse"), "{body}");

    let (status, body) = post_soap(port, "/onvif/ptz_service", "<GotoHomePosition/>").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("tptz:GotoHomePositionResponse"), "{body}");

    let (status, body) = post_soap(port, "/onvif/ptz_service", "<SetHomePosition/>").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("tptz:SetHomePositionResponse"), "{body}");

    let (status, body) = post_soap(
        port,
        "/onvif/ptz_service",
        "<SendAuxiliaryCommand><AuxiliaryData>wiper:on</AuxiliaryData></SendAuxiliaryCommand>",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("tptz:AuxiliaryCommandResponse"), "{body}");
    assert!(body.contains("wiper:on"), "{body}");

    // The historical PTZ answers keep working after the imaging takeover
    // of the shared names (non-imaging bodies fall back).
    let (status, body) = post_soap(port, "/onvif/ptz_service", "<Stop/>").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("tptz:StopResponse"), "{body}");
    let (status, body) = post_soap(port, "/onvif/ptz_service", "<GetStatus/>").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("tptz:GetStatusResponse"), "{body}");

    // Shared-name routing: the bare GetServiceCapabilities is owned by
    // the media face (library-recommended ordering — see the wiring
    // doc), imaging-shaped bodies get the imaging capabilities, and
    // every variant answers 200.
    let (status, body) =
        post_soap(port, "/onvif/device_service", "<GetServiceCapabilities/>").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("GetServiceCapabilitiesResponse"), "{body}");

    let (status, body) = post_soap(
        port,
        "/onvif/imaging_service",
        "<GetServiceCapabilities xmlns=\"http://www.onvif.org/ver20/imaging/wsdl\"><VideoSourceToken>videoSrc0</VideoSourceToken></GetServiceCapabilities>",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("timg:GetServiceCapabilitiesResponse"),
        "{body}"
    );

    wired.handle.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// Imaging face
// ---------------------------------------------------------------------------

#[tokio::test]
async fn imaging_face_answers() {
    let mut wired = start_wired(|_| {}).await;
    let port = wired.port;

    let (status, body) = post_soap(
        port,
        "/onvif/imaging_service",
        "<GetImagingSettings><VideoSourceToken>videoSrc0</VideoSourceToken></GetImagingSettings>",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("timg:GetImagingSettingsResponse"), "{body}");
    assert!(body.contains("timg:Settings"), "{body}");
    // AUTO exposure / white balance and the neutral 0.5 defaults (the
    // imaging schema carries values as attributes).
    assert_eq!(xml_field(&body, "tt:Mode"), "AUTO");
    assert!(body.contains("AUTO"), "{body}");
    assert!(body.contains(r#"<tt:Brightness Value="0.5""#), "{body}");

    // SetImagingSettings round-trips through the in-memory store.
    let (status, body) = post_soap(
        port,
        "/onvif/imaging_service",
        "<SetImagingSettings><VideoSourceToken>videoSrc0</VideoSourceToken>\
         <Settings xmlns=\"http://www.onvif.org/ver10/schema\">\
         <Brightness Value=\"0.25\"/>\
         </Settings></SetImagingSettings>",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("timg:SetImagingSettingsResponse"), "{body}");
    let (status, body) = post_soap(
        port,
        "/onvif/imaging_service",
        "<GetImagingSettings><VideoSourceToken>videoSrc0</VideoSourceToken></GetImagingSettings>",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains(r#"<tt:Brightness Value="0.25""#),
        "set value must be readable back: {body}"
    );

    // Focus Move / Stop acknowledge (fixed-focus sensor: no motor) and
    // GetMoveOptions renders the focus ranges.
    let (status, body) = post_soap(
        port,
        "/onvif/imaging_service",
        "<Move><VideoSourceToken>videoSrc0</VideoSourceToken>\
         <AbsoluteFocus><Position>0.5</Position><Speed>1.0</Speed></AbsoluteFocus>\
         </Move>",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("timg:MoveResponse"), "{body}");

    let (status, body) = post_soap(
        port,
        "/onvif/imaging_service",
        "<Stop><VideoSourceToken>videoSrc0</VideoSourceToken></Stop>",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("timg:StopResponse"), "{body}");

    let (status, body) = post_soap(port, "/onvif/imaging_service", "<GetMoveOptions/>").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("timg:GetMoveOptionsResponse"), "{body}");
    assert!(body.contains("tt:AbsoluteFocusOptions"), "{body}");

    // GetOptions (the settings ranges) serves the same face.
    let (status, body) = post_soap(
        port,
        "/onvif/imaging_service",
        "<GetOptions><VideoSourceToken>videoSrc0</VideoSourceToken></GetOptions>",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("timg:GetOptionsResponse"), "{body}");

    wired.handle.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// DeviceHooks semantics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn device_hooks_observe_without_executing() {
    let mut wired = start_wired(|_| {}).await;
    let port = wired.port;

    // SetSystemDateAndTime answers 200 and is RECORDED — but the system
    // clock is host territory; a network command never adjusts it.
    let (status, body) = post_soap(
        port,
        "/onvif/device_service",
        "<SetSystemDateAndTime>\
         <DateTimeType>Manual</DateTimeType><DaylightSavings>false</DaylightSavings>\
         <UTCDateTime><Time><Hour>12</Hour><Minute>34</Minute><Second>56</Second></Time>\
         <Date><Year>2026</Year><Month>9</Month><Day>29</Day></Date></UTCDateTime>\
         <TimeZone><TZ>CST-8</TZ></TimeZone>\
         </SetSystemDateAndTime>",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("SetSystemDateAndTimeResponse"), "{body}");
    assert_eq!(
        wired.hooks.last_clock_request(),
        Some(((2026, 9, 29, 12, 34, 56), "CST-8".to_string()))
    );

    // SystemReboot answers the protocol and is REFUSED as an effect —
    // the test process being alive to read the counter is the proof.
    let (status, body) = post_soap(port, "/onvif/device_service", "<SystemReboot/>").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("tds:SystemRebootResponse"), "{body}");
    assert_eq!(wired.hooks.reboot_requests(), 1);

    let (status, body) = post_soap(
        port,
        "/onvif/device_service",
        "<SetSystemFactoryDefault><FactoryDefault>Hard</FactoryDefault></SetSystemFactoryDefault>",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(wired.hooks.factory_default_requests(), 1);

    wired.handle.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn system_log_support_info_and_users_carry_real_data() {
    let mut wired = start_wired(|_| {}).await;
    let port = wired.port;

    let (status, body) = post_soap(port, "/onvif/device_service", "<GetSystemLog/>").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("GetSystemLogResponse"), "{body}");
    assert!(body.contains("9.9.9-wire-test"), "real version: {body}");
    assert!(body.contains("1280x720@15fps h264 2000000bps"), "{body}");
    assert!(body.contains("gb28181: disabled"), "{body}");

    let (status, body) = post_soap(
        port,
        "/onvif/device_service",
        "<GetSystemSupportInformation/>",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("GetSystemSupportInformationResponse"),
        "{body}"
    );
    assert!(
        !xml_field(&body, "tds:SupportInformation").is_empty(),
        "{body}"
    );

    // The user directory lists the configured WS-Security account.
    let (status, body) = post_soap(port, "/onvif/device_service", "<GetUsers/>").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("GetUsersResponse"), "{body}");
    assert!(body.contains("<tt:Username>admin</tt:Username>"), "{body}");
    assert!(body.contains("Administrator"), "{body}");

    // Hostname / network reads answer from the completed device family.
    let (status, body) = post_soap(port, "/onvif/device_service", "<GetHostname/>").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("GetHostnameResponse"), "{body}");
    let (status, body) = post_soap(port, "/onvif/device_service", "<GetNetworkInterfaces/>").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("GetNetworkInterfacesResponse"), "{body}");

    wired.handle.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// Security: HTTP Digest + IP filter
// ---------------------------------------------------------------------------

/// With `http_digest = true`: token-less requests get the 401 Digest
/// challenge over the wire and a valid MD5 response authenticates.
#[tokio::test]
async fn http_digest_challenge_and_handshake() {
    let mut wired = start_wired(|host| host.http_digest = true).await;
    let port = wired.port;

    let (status, headers, body) =
        post_soap_raw(port, "/onvif/media_service", "<GetProfiles/>", false).await;
    assert_eq!(status, 401, "{body}");
    let challenge = header_value(&headers, "WWW-Authenticate").expect("digest challenge header");
    assert!(
        challenge.starts_with("Digest realm=\"onvif\", nonce=\""),
        "{challenge}"
    );
    assert!(challenge.contains("qop=\"auth\""), "{challenge}");
    assert!(challenge.contains("algorithm=MD5"), "{challenge}");

    // Minimal RFC 7616 MD5/qop=auth client (the library's golden path).
    let nonce = digest_param(challenge, "nonce").expect("nonce");
    let uri = "/onvif/media_service";
    let ha1 = md5hex(&format!("{USERNAME}:{REALM}:{PASSWORD}"));
    let ha2 = md5hex(&format!("POST:{uri}"));
    let response = md5hex(&format!("{ha1}:{nonce}:00000001:c1:auth:{ha2}"));
    let authorization = format!(
        "Authorization: Digest username=\"{USERNAME}\", realm=\"{REALM}\", nonce=\"{nonce}\", \
         uri=\"{uri}\", qop=auth, nc=00000001, cnonce=\"c1\", \
         response=\"{response}\", opaque=\"x\", algorithm=MD5\r\n"
    );
    let (status, _, body) = post_soap_authorized(port, uri, "<GetProfiles/>", &authorization).await;
    assert_eq!(status, 200, "valid digest must authenticate: {body}");
    assert!(body.contains("GetProfilesResponse"), "{body}");

    wired.handle.shutdown().await.expect("shutdown");
}

/// Default (`http_digest = false`): the historical WSSE-only 401 — no
/// challenge header, existing deployments' bytes unchanged.
#[tokio::test]
async fn http_digest_off_keeps_historical_401() {
    let mut wired = start_wired(|_| {}).await;

    let (status, headers, body) =
        post_soap_raw(wired.port, "/onvif/media_service", "<GetProfiles/>", false).await;
    assert_eq!(status, 401, "{body}");
    assert!(
        header_value(&headers, "WWW-Authenticate").is_none(),
        "no challenge when digest is off: {headers}"
    );

    wired.handle.shutdown().await.expect("shutdown");
}

/// A configured allow-list admits the loopback peer (and the SOAP ops
/// read the same shared state); a non-matching list refuses with 403
/// before any auth processing.
#[tokio::test]
async fn ip_filter_gates_the_listener() {
    // Loopback listed → passes.
    let mut wired = start_wired(|host| {
        host.ip_filter = vec!["127.0.0.1".to_string()];
    })
    .await;
    let (status, body) = post_soap(wired.port, "/onvif/media_service", "<GetProfiles/>").await;
    assert_eq!(status, 200, "{body}");

    // The SOAP read answers the same shared filter state.
    let (status, body) =
        post_soap(wired.port, "/onvif/device_service", "<GetIPAddressFilter/>").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("GetIPAddressFilterResponse"), "{body}");
    assert!(body.contains("<tt:Type>Allow</tt:Type>"), "{body}");
    assert!(body.contains("127.0.0.1"), "{body}");
    wired.handle.shutdown().await.expect("shutdown");

    // Loopback NOT listed → 403 before auth.
    let mut wired = start_wired(|host| {
        host.ip_filter = vec!["10.99.0.0/16".to_string()];
    })
    .await;
    let (status, body) = post_soap(wired.port, "/onvif/media_service", "<GetProfiles/>").await;
    assert_eq!(status, 403, "{body}");
    assert!(body.contains("soap:Fault"), "{body}");
    wired.handle.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// Events push interface (wsnt:Subscribe → Notify) — zero product code
// ---------------------------------------------------------------------------

/// With the events service enabled (the product default), a wsnt:Subscribe
/// registers a push subscription and published MotionAlarm events are
/// delivered as wsnt:Notify POSTs to the consumer — the library's #50
/// interface, obtained by the product with no wiring beyond
/// `enable_events()`.
#[tokio::test]
async fn events_push_subscribe_and_notify() {
    let mut wired = start_wired(|_| {}).await;
    let port = wired.port;
    let events = wired.events.clone().expect("events seam present");

    let (consumer_port, mut rx) = spawn_notify_consumer(200).await;
    let (status, _, body) = post_soap_raw(
        port,
        EVENTS_SERVICE_PATH,
        &format!(
            "<Subscribe xmlns=\"http://docs.oasis-open.org/wsn/b-2\">\
             <ConsumerReference><wsa:Address xmlns:wsa=\"http://www.w3.org/2005/08/addressing\">http://127.0.0.1:{consumer_port}/notify</wsa:Address></ConsumerReference>\
             <TerminationTime>PT10M</TerminationTime>\
             </Subscribe>"
        ),
        false,
    )
    .await;
    assert_eq!(status, 200, "subscribe failed:\n{body}");
    assert!(body.contains("<wsnt:SubscribeResponse"), "{body}");
    assert!(
        xml_field(&body, "wsa:Address").contains("/onvif/events_service/sub/"),
        "SubscriptionReference: {}",
        xml_field(&body, "wsa:Address")
    );

    // Publish the product's MotionAlarm event; the consumer must receive
    // the Notify POST.
    events.publish_event(mibee_eye_raspi_rs::onvif_alarm::motion_alarm_event(3));
    let notify = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("notify delivered")
        .expect("consumer channel live");
    assert!(notify.contains("/notify"), "{notify}");
    assert!(notify.contains("MotionAlarm"), "{notify}");
    assert!(notify.contains("wsnt:Notify"), "{notify}");

    wired.handle.shutdown().await.expect("shutdown");
}
