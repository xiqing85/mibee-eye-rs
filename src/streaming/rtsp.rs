//! RTSP server for H.264 streaming with TCP interleaved transport.
//!
//! Supports RTSP 1.0 (RFC 2326) with Basic and Digest authentication,
//! TCP interleaved mode (RTP over RTSP TCP), and H.264 RTP packetization
//! per RFC 6184 (single NALU and FU-A fragmentation).
//!
//! # Architecture
//!
//! Each client connection runs as a single async task that multiplexes
//! between reading RTSP requests and forwarding RTP frames. The AuHub
//! subscriber is bridged from a synchronous `std::sync::mpsc` channel
//! to a `tokio::sync::mpsc` channel via `spawn_blocking`.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rand::Rng;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

use crate::h264::hub::{AccessUnit, AuHub};
use crate::h264::parser::Nalu;

// ============================================================================
// Error type
// ============================================================================

/// Errors that can occur during RTSP server operation.
#[derive(Debug)]
pub enum RtspError {
    /// I/O error from the underlying TCP socket.
    Io(std::io::Error),
    /// Authentication failure.
    Auth(String),
    /// Protocol violation or parse error.
    Protocol(String),
    /// Referenced session does not exist.
    SessionNotFound(String),
}

impl fmt::Display for RtspError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RtspError::Io(e) => write!(f, "I/O error: {e}"),
            RtspError::Auth(e) => write!(f, "auth error: {e}"),
            RtspError::Protocol(e) => write!(f, "protocol error: {e}"),
            RtspError::SessionNotFound(e) => write!(f, "session not found: {e}"),
        }
    }
}

impl std::error::Error for RtspError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RtspError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for RtspError {
    fn from(e: std::io::Error) -> Self {
        RtspError::Io(e)
    }
}

// ============================================================================
// Configuration
// ============================================================================

/// Configuration for the RTSP server.
#[derive(Debug, Clone)]
pub struct RtspConfig {
    /// TCP port to listen on (default: 8554).
    pub port: u16,
    /// Username for authentication. Empty means no auth.
    pub username: String,
    /// Password for authentication.
    pub password: String,
    /// Realm for Digest authentication.
    pub realm: String,
}

impl Default for RtspConfig {
    fn default() -> Self {
        Self {
            port: 8554,
            username: String::new(),
            password: String::new(),
            realm: "MiBee Eye RTSP".to_string(),
        }
    }
}

// ============================================================================
// Session management
// ============================================================================

/// Represents a single RTSP session (one client stream).
#[allow(dead_code)]
pub struct Session {
    id: String,
    rtp_channel: u8,
    rtcp_channel: u8,
    is_playing: bool,
    ssrc: u32,
    seq: Arc<AtomicU16>,
    base_time: Instant,
    last_ts: Arc<AtomicU32>,
    is_udp: bool,
    udp_socket: Option<Arc<tokio::net::UdpSocket>>,
    udp_client_addr: Option<std::net::SocketAddr>,
}

impl Clone for Session {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            rtp_channel: self.rtp_channel,
            rtcp_channel: self.rtcp_channel,
            is_playing: self.is_playing,
            ssrc: self.ssrc,
            seq: self.seq.clone(),
            base_time: self.base_time,
            last_ts: self.last_ts.clone(),
            is_udp: self.is_udp,
            udp_socket: self.udp_socket.clone(),
            udp_client_addr: self.udp_client_addr,
        }
    }
}

/// Shared server state protected by a mutex.
pub struct Inner {
    sessions: HashMap<String, Session>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

// ============================================================================
// RTSP Server
// ============================================================================

/// RTSP server that serves H.264 frames from [`AuHub`] to RTSP clients.
///
/// Uses TCP interleaved mode (RTP over RTSP TCP connection) and supports
/// Basic and Digest authentication.
pub struct RtspServer {
    config: RtspConfig,
    listener: TcpListener,
    au_hub: Arc<AuHub>,
    inner: Arc<Mutex<Inner>>,
    shutdown: Arc<Notify>,
}

impl RtspServer {
    /// Creates a new [`RtspServer`] and binds it to the configured port.
    ///
    /// # Errors
    ///
    /// Returns [`RtspError::Io`] if the TCP listener cannot be bound.
    pub async fn new(config: RtspConfig) -> Result<Self, RtspError> {
        let addr = format!("0.0.0.0:{}", config.port);
        let listener = TcpListener::bind(&addr).await?;

        let realm = if config.realm.is_empty() {
            "MiBee Eye RTSP".to_string()
        } else {
            config.realm.clone()
        };

        Ok(Self {
            config: RtspConfig { realm, ..config },
            listener,
            au_hub: Arc::new(AuHub::new()),
            inner: Arc::new(Mutex::new(Inner {
                sessions: HashMap::new(),
                sps: None,
                pps: None,
            })),
            shutdown: Arc::new(Notify::new()),
        })
    }

    /// Returns a reference to the internal [`AuHub`] for frame injection.
    pub fn au_hub(&self) -> &Arc<AuHub> {
        &self.au_hub
    }

    /// Starts accepting RTSP client connections.
    ///
    /// Runs until the server is dropped or encounters a fatal error.
    ///
    /// # Errors
    ///
    /// Returns [`RtspError::Io`] on critical accept failures.
    pub async fn start(&self) -> Result<(), RtspError> {
        let shutdown = self.shutdown.clone();

        loop {
            let accept = tokio::select! {
                _ = shutdown.notified() => {
                    return Ok(());
                }
                result = self.listener.accept() => result,
            };

            match accept {
                Ok((stream, _addr)) => {
                    let inner = self.inner.clone();
                    let au_hub = self.au_hub.clone();
                    let config = self.config.clone();

                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream, inner, au_hub, config).await {
                            eprintln!("[RTSP] connection error: {e}");
                        }
                    });
                }
                Err(e) => {
                    eprintln!("[RTSP] accept error: {e}");
                }
            }
        }
    }
}

impl Drop for RtspServer {
    fn drop(&mut self) {
        self.shutdown.notify_one();
    }
}

// ============================================================================
// Per-connection handler
// ============================================================================

/// Handle a single RTSP client connection.
async fn handle_connection(
    mut stream: TcpStream,
    inner: Arc<Mutex<Inner>>,
    au_hub: Arc<AuHub>,
    config: RtspConfig,
) -> Result<(), RtspError> {
    let peer_addr = stream
        .peer_addr()
        .unwrap_or(std::net::SocketAddr::from(([0, 0, 0, 0], 0)));
    let (mut reader, mut writer) = stream.split();

    // Buffer for reading RTSP requests.
    let mut buf = vec![0u8; 65536];

    // Per-connection state.
    let mut session_id: Option<String> = None;
    let mut has_auth = false;
    let mut frame_bridge: Option<tokio::sync::mpsc::Receiver<AccessUnit>> = None;
    let mut stall_count: u32 = 0;
    let mut pframe_dropped: bool = false;
    let mut sent_keyframe: bool = false;

    loop {
        let n;
        // Race between RTSP commands and frame delivery.
        tokio::select! {

            // Deliver video frames when PLAY is active.
            au_opt = async {
                match frame_bridge.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => None,
                }
            }, if frame_bridge.is_some() => {
                if let Some(au) = au_opt {
                    let sid = session_id.clone().unwrap_or_default();
                    let session = {
                        let guard = inner.lock().unwrap_or_else(|e| e.into_inner());
                        guard.sessions.get(&sid).cloned()
                    };
                    if let Some(session) = session {
                        // Wait for first keyframe before sending anything.
                        // P-frames without a reference IDR cause decoder corruption.
                        if !sent_keyframe && !au.is_key_frame {
                            continue;
                        }
                        if au.is_key_frame { sent_keyframe = true; }
                        if session.is_udp {
                            // UDP mode: send raw RTP packets.
                            let packets = access_unit_to_frames(&au, &session, &inner);
                            if let (Some(sock), Some(addr)) = (&session.udp_socket, session.udp_client_addr) {
                                for pkt in &packets {
                                    // Strip 4-byte interleaving header for UDP.
                                    let rtp_data = if pkt.len() > 4 && pkt[0] == 0x24 { &pkt[4..] } else { pkt.as_slice() };
                                    let _ = sock.send_to(rtp_data, addr).await;
                                }
                            }
                                        } else {
                                            // TCP interleaved mode — write with timeout.
                                            // Skip P-frames after a drop to prevent decoder corruption
                                            // (P-frames reference previous frames; if previous was dropped,
                                            // decoder can't reconstruct macroblocks).
                                            if !au.is_key_frame && pframe_dropped {
                                                // Still advance seq to maintain contiguity.
                                                let _ = access_unit_to_frames(&au, &session, &inner);
                                                continue;
                                            }
                                            let seq_before = session.seq.load(Ordering::SeqCst);
                                            let frames = access_unit_to_frames(&au, &session, &inner);
                                            let mut wrote_any = false;
                                            let timeout_ms = if au.is_key_frame { 5000 } else { 2000 };
                                            let mut last_sent_seq: Option<u16> = None;
                                            // Combine all RTP packets into one write for atomic delivery.
                                            // Prevents partial P-frame sends that corrupt the decoder.
                                            let mut combined = Vec::new();
                                            for f in &frames { combined.extend_from_slice(f); }
                                            match tokio::time::timeout(
                                                std::time::Duration::from_millis(timeout_ms),
                                                writer.write_all(&combined),
                                            ).await {
                                                Ok(Ok(())) => {
                                                    wrote_any = true;
                                                    if combined.len() >= 8 {
                                                        last_sent_seq = Some(u16::from_be_bytes([combined[6], combined[7]]));
                                                    }
                                                }
                                                Ok(Err(e)) => {
                                                    eprintln!("[RTSP] write error: {e}");
                                                    return Ok(());
                                                }
                                                Err(_) => {
                                                    if au.is_key_frame {
                                                        stall_count += 1;
                                                        if stall_count >= 5 { return Ok(()); }
                                                    } else {
                                                        pframe_dropped = true;
                                                    }
                                                }
                                            }
                                            // Rewind RTP seq — NO GAPS even if entire AU was dropped.
                                            let rewind_to = last_sent_seq.map(|s| s.wrapping_add(1)).unwrap_or(seq_before);
                                            session.seq.store(rewind_to, Ordering::SeqCst);
                                            if wrote_any {
                                                stall_count = 0;
                                                if au.is_key_frame { pframe_dropped = false; }
                                            }
                                            let _ = writer.flush().await;
                                        }
                                    } else {
                                        // Bridge closed.
                                        frame_bridge = None;
                                    }
                                }
                                continue;
                            },

            // Read next RTSP request.
            result = reader.read(&mut buf) => {
                n = match result {
                    Ok(0) => return Ok(()),
                    Ok(read_n) => read_n,
                    Err(e) => return Err(RtspError::Io(e)),
                };
            }
        }

        let request_data = &buf[..n];

        // Check for interleaved data from client (we ignore it).
        if request_data[0] == 0x24 && n >= 4 {
            let payload_len = (request_data[2] as usize) << 8 | (request_data[3] as usize);
            let total_len = 4 + payload_len;
            if n >= total_len {
                // Interleaved data from client (e.g., RTCP). Skip it.
                continue;
            }
        }

        // Parse RTSP request.
        let request_str = match std::str::from_utf8(request_data) {
            Ok(s) => s,
            Err(_) => {
                // Try to find end of headers with raw bytes.
                if let Some(pos) = request_data.windows(4).position(|w| w == b"\r\n\r\n") {
                    let raw_headers = &request_data[..pos + 4];
                    let headers = match std::str::from_utf8(raw_headers) {
                        Ok(s) => s,
                        Err(_) => {
                            send_response(&mut writer, "400", "Bad Request", &[("CSeq", "0")], "")
                                .await
                                .ok();
                            continue;
                        }
                    };

                    let parsed = parse_rtsp_request(headers);
                    match parsed {
                        Some(req) => {
                            handle_rtsp_request(
                                req,
                                &mut writer,
                                &mut session_id,
                                &mut has_auth,
                                &mut frame_bridge,
                                &inner,
                                &au_hub,
                                &config,
                                peer_addr,
                            )
                            .await;
                        }
                        None => {
                            send_response(&mut writer, "400", "Bad Request", &[("CSeq", "0")], "")
                                .await
                                .ok();
                        }
                    }
                } else {
                    send_response(&mut writer, "400", "Bad Request", &[("CSeq", "0")], "")
                        .await
                        .ok();
                }
                continue;
            }
        };

        // Find end of headers.
        match request_str.find("\r\n\r\n") {
            Some(end) => {
                let headers = &request_str[..end + 4];

                if let Some(parsed) = parse_rtsp_request(headers) {
                    handle_rtsp_request(
                        parsed,
                        &mut writer,
                        &mut session_id,
                        &mut has_auth,
                        &mut frame_bridge,
                        &inner,
                        &au_hub,
                        &config,
                        peer_addr,
                    )
                    .await;
                } else {
                    send_response(&mut writer, "400", "Bad Request", &[("CSeq", "0")], "")
                        .await
                        .ok();
                }
            }
            None => {
                // Partial request — wait for more data.
                // In a real server we would buffer, but for simplicity skip.
                continue;
            }
        };
    }
}

// ============================================================================
// RTSP request parsing
// ============================================================================

/// A parsed RTSP request.
#[derive(Debug)]
struct RtspRequest {
    method: String,
    url: String,
    cseq: u32,
    headers: HashMap<String, String>,
}

/// Parse an RTSP request string (headers only, up to `\r\n\r\n`).
fn parse_rtsp_request(data: &str) -> Option<RtspRequest> {
    let mut lines = data.lines();

    // Request line: METHOD url RTSP/1.0
    let request_line = lines.next()?;
    let parts: Vec<&str> = request_line.splitn(3, ' ').collect();
    if parts.len() != 3 {
        return None;
    }
    let method = parts[0].to_string();
    let url_full = parts[1];

    // Strip RTSP URI to get path component.
    let url = if let Some(rest) = url_full.strip_prefix("rtsp://") {
        if let Some(slash) = rest.find('/') {
            rest[slash..].to_string()
        } else {
            "/".to_string()
        }
    } else {
        url_full.to_string()
    };

    // Parse headers.
    let mut headers = HashMap::new();
    let mut cseq = 0u32;

    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some(pos) = line.find(':') {
            let key = line[..pos].trim().to_string();
            let value = line[pos + 1..].trim().to_string();
            if key.eq_ignore_ascii_case("CSeq") {
                cseq = value.parse().unwrap_or(0);
            }
            headers.insert(key, value);
        }
    }

    Some(RtspRequest {
        method,
        url,
        cseq,
        headers,
    })
}

// ============================================================================
// RTSP request routing
// ============================================================================

/// Route and handle a parsed RTSP request.
#[allow(clippy::too_many_arguments)]
async fn handle_rtsp_request(
    req: RtspRequest,
    writer: &mut tokio::net::tcp::WriteHalf<'_>,
    session_id: &mut Option<String>,
    has_auth: &mut bool,
    frame_bridge: &mut Option<tokio::sync::mpsc::Receiver<AccessUnit>>,
    inner: &Arc<Mutex<Inner>>,
    au_hub: &Arc<AuHub>,
    config: &RtspConfig,
    peer_addr: std::net::SocketAddr,
) {
    let cseq = req.cseq;

    match req.method.as_str() {
        "GET_PARAMETER" | "SET_PARAMETER" => {
            send_response(writer, "200", "OK", &[("CSeq", &cseq.to_string())], "")
                .await
                .ok();
        }
        "OPTIONS" => handle_options(writer, cseq).await,
        "DESCRIBE" => {
            handle_describe(&req, writer, cseq, has_auth, inner, config).await;
        }
        "SETUP" => {
            if let Err(e) = handle_setup(
                &req, writer, session_id, cseq, has_auth, inner, config, peer_addr,
            )
            .await
            {
                eprintln!("[RTSP] SETUP error: {e}");
            }
        }
        "PLAY" => {
            if let Err(e) = handle_play(
                &req,
                writer,
                session_id,
                cseq,
                has_auth,
                frame_bridge,
                inner,
                au_hub,
                config,
            )
            .await
            {
                eprintln!("[RTSP] PLAY error: {e}");
            }
        }
        "TEARDOWN" => {
            handle_teardown(writer, session_id, cseq, frame_bridge, inner, config).await;
        }
        _ => {
            send_response(
                writer,
                "405",
                "Method Not Allowed",
                &[
                    ("CSeq", &cseq.to_string()),
                    ("Public", "OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN "),
                ],
                "",
            )
            .await
            .ok();
        }
    }
}

// ============================================================================
// Authentication
// ============================================================================

/// Check if authentication is configured.
fn is_auth_configured(config: &RtspConfig) -> bool {
    !config.username.is_empty()
}

/// Verify Basic authentication header.
fn check_basic_auth(header_value: &str, config: &RtspConfig) -> bool {
    let encoded = match header_value.strip_prefix("Basic ") {
        Some(val) => val.trim(),
        None => return false,
    };

    let decoded = match decode_base64(encoded) {
        Some(d) => d,
        None => return false,
    };

    let decoded_str = match std::str::from_utf8(&decoded) {
        Ok(s) => s,
        Err(_) => return false,
    };

    // Format is "username:password"
    if let Some(colon_pos) = decoded_str.find(':') {
        let user = &decoded_str[..colon_pos];
        let pass = &decoded_str[colon_pos + 1..];
        user == config.username && pass == config.password
    } else {
        false
    }
}

/// Verify Digest authentication header.
fn check_digest_auth(
    header_value: &str,
    method: &str,
    uri: &str,
    nonce: &str,
    config: &RtspConfig,
) -> bool {
    use md5::{Digest, Md5};

    let params = parse_digest_params(header_value);
    let response = match params.get("response") {
        Some(r) => r,
        None => return false,
    };
    let username = match params.get("username") {
        Some(u) => u,
        None => return false,
    };
    let client_nonce = params.get("cnonce").map(|s| s.as_str()).unwrap_or("");
    let nc = params.get("nc").map(|s| s.as_str()).unwrap_or("");
    let qop = params.get("qop").map(|s| s.as_str()).unwrap_or("");

    if username != &config.username {
        return false;
    }

    // HA1 = MD5(username:realm:password)
    let ha1 = {
        let mut hasher = Md5::new();
        hasher.update(format!("{}:{}:{}", username, config.realm, config.password));
        format!("{:x}", hasher.finalize())
    };

    // HA2 = MD5(method:uri)
    let ha2 = {
        let mut hasher = Md5::new();
        hasher.update(format!("{method}:{uri}"));
        format!("{:x}", hasher.finalize())
    };

    // response = MD5(HA1:nonce:nc:cnonce:qop:HA2) if qop is present
    // or MD5(HA1:nonce:HA2) if qop is absent
    let expected = if qop.is_empty() {
        let mut hasher = Md5::new();
        hasher.update(format!("{ha1}:{nonce}:{ha2}"));
        format!("{:x}", hasher.finalize())
    } else {
        let mut hasher = Md5::new();
        hasher.update(format!("{ha1}:{nonce}:{nc}:{client_nonce}:{qop}:{ha2}"));
        format!("{:x}", hasher.finalize())
    };

    // Constant-time comparison to avoid timing attacks.
    let expected_bytes = expected.as_bytes();
    let response_bytes = response.as_bytes();
    if expected_bytes.len() != response_bytes.len() {
        return false;
    }
    expected_bytes
        .iter()
        .zip(response_bytes)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// Parse Digest auth parameters from the Authorization header.
fn parse_digest_params(header: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();

    // Skip "Digest " prefix.
    let body = match header.strip_prefix("Digest ") {
        Some(val) => val.trim(),
        None => return params,
    };

    let mut remaining = body;
    while !remaining.is_empty() {
        remaining = remaining.trim();
        if remaining.is_empty() {
            break;
        }

        // Skip leading comma from previous value.
        if remaining.starts_with(',') {
            remaining = remaining[1..].trim();
            if remaining.is_empty() {
                break;
            }
        }

        // Find key.
        if let Some(eq_pos) = remaining.find('=') {
            let key = remaining[..eq_pos].trim().to_string();
            remaining = remaining[eq_pos + 1..].trim();

            if remaining.is_empty() {
                break;
            }

            let (value, rest) = if let Some(after_open) = remaining.strip_prefix('"') {
                // Quoted string: find closing ".
                if let Some(end) = after_open.find('"') {
                    // end is position in after_open
                    let val = &after_open[..end];
                    // +1 for the opening quote already consumed by strip_prefix
                    let rest = after_open[end + 1..].trim_start();
                    (val, rest)
                } else {
                    // No closing quote — consume rest.
                    (after_open, "")
                }
            } else {
                // Unquoted value (up to comma or end).
                let end = remaining.find(',').unwrap_or(remaining.len());
                let val = remaining[..end].trim();
                let rest = if end < remaining.len() {
                    &remaining[end + 1..]
                } else {
                    ""
                };
                (val, rest)
            };

            params.insert(key, value.to_string());
            remaining = rest;
        } else {
            break;
        }
    }
    params
}

/// Generate a random nonce for Digest auth.
fn generate_nonce() -> String {
    let mut rng = rand::thread_rng();
    let bytes: [u8; 16] = rng.gen();
    hex::encode(bytes)
}

/// Minimal base64 decode (for Basic auth).
fn decode_base64(input: &str) -> Option<Vec<u8>> {
    // Remove any whitespace.
    let input: String = input.chars().filter(|c| !c.is_whitespace()).collect();

    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    if !input.len().is_multiple_of(4) {
        return None;
    }

    let mut result = Vec::with_capacity(input.len() * 3 / 4);
    let bytes: Vec<u8> = input.bytes().collect();
    let mut i = 0;

    while i < bytes.len() {
        let mut vals = [0u8; 4];
        let mut padding = 0;

        for (j, slot) in vals.iter_mut().enumerate() {
            let c = *bytes.get(i + j)?;
            if c == b'=' {
                padding += 1;
                *slot = 0;
            } else {
                *slot = u8::try_from(CHARS.iter().position(|&x| x == c)?).unwrap_or(0);
            }
        }

        let triple = ((vals[0] as u32) << 18)
            | ((vals[1] as u32) << 12)
            | ((vals[2] as u32) << 6)
            | (vals[3] as u32);

        result.push((triple >> 16) as u8);
        if padding < 2 {
            result.push((triple >> 8) as u8);
        }
        if padding < 1 {
            result.push(triple as u8);
        }

        i += 4;
    }

    Some(result)
}

/// Minimal base64 encode (for SDP sprop-parameter-sets).
fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity(data.len().div_ceil(3) * 4);

    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;

        result.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        result.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);

        if chunk.len() > 1 {
            result.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
    }

    result
}

// ============================================================================
// SDP generation
// ============================================================================

/// Generate an SDP description for H.264 video.
fn generate_sdp(inner: &Arc<Mutex<Inner>>) -> String {
    let guard = inner.lock().unwrap_or_else(|e| e.into_inner());

    let profile_level_id = match &guard.sps {
        Some(sps) if sps.len() >= 4 => {
            format!("{:02x}{:02x}{:02x}", sps[1], sps[2], sps[3])
        }
        _ => "42e01f".to_string(), // Default: Baseline 3.1
    };

    let sprop = match (&guard.sps, &guard.pps) {
        (Some(sps), Some(pps)) => {
            format!(
                "; sprop-parameter-sets={},{}",
                base64_encode(sps),
                base64_encode(pps)
            )
        }
        _ => String::new(),
    };

    format!(
        "v=0\r\n\
         o=- 0 0 IN IP4 0.0.0.0\r\n\
         s=MiBee Eye Stream\r\n\
         t=0 0\r\n\
         m=video 0 RTP/AVP 96\r\n\
         a=control:streamid=0\r\n\
         a=rtpmap:96 H264/90000\r\n\
         a=fmtp:96 profile-level-id={profile_level_id}; packetization-mode=1{sprop}\r\n"
    )
}

// ============================================================================
// RTP packetization (RFC 6184)
// ============================================================================

/// Maximum RTP payload size (conservative for UDP compatibility).
const RTP_MTU: usize = 1400;

/// RTP header length in bytes.
const RTP_HEADER_LEN: usize = 12;

/// Build a 12-byte RTP header.
///
/// |V=2|P|X|CC| M|PT| sequence number | timestamp | SSRC |
fn build_rtp_header(marker: bool, seq: u16, timestamp: u32, ssrc: u32) -> [u8; 12] {
    let mut header = [0u8; 12];

    // Byte 0: V=2 (bits 6-7), P=0 (bit 5), X=0 (bit 4), CC=0 (bits 0-3)
    header[0] = 0x80;

    // Byte 1: marker (bit 7), payload type=96 (bits 0-6)
    header[1] = if marker { 0x80 | 96 } else { 96 };

    // Bytes 2-3: sequence number (big-endian)
    header[2] = (seq >> 8) as u8;
    header[3] = (seq & 0xFF) as u8;

    // Bytes 4-7: timestamp (big-endian)
    header[4] = (timestamp >> 24) as u8;
    header[5] = (timestamp >> 16) as u8;
    header[6] = (timestamp >> 8) as u8;
    header[7] = timestamp as u8;

    // Bytes 8-11: SSRC (big-endian)
    header[8] = (ssrc >> 24) as u8;
    header[9] = (ssrc >> 16) as u8;
    header[10] = (ssrc >> 8) as u8;
    header[11] = ssrc as u8;

    header
}

/// Construct an interleaved RTP frame: `$` + channel + 2-byte-length + RTP packet.
fn build_interleaved_rtp(channel: u8, rtp_packet: &[u8]) -> Vec<u8> {
    let len = rtp_packet.len();
    let mut frame = Vec::with_capacity(4 + len);
    frame.push(0x24); // '$' marker
    frame.push(channel);
    frame.push((len >> 8) as u8);
    frame.push(len as u8);
    frame.extend_from_slice(rtp_packet);
    frame
}

/// Packetize a single NALU into one or more RTP payloads (RFC 6184).
///
/// Small NALUs use single-NALU mode, large NALUs use FU-A fragmentation.
fn packetize_nalu(nalu: &Nalu, mtu: usize) -> Vec<Vec<u8>> {
    let nalu_data = &nalu.data;
    if nalu_data.is_empty() {
        return Vec::new();
    }

    // Single NALU packet: fits in MTU.
    if nalu_data.len() <= mtu {
        return vec![nalu_data.to_vec()];
    }

    // FU-A fragmentation: NALU is larger than MTU.
    let nal_header = nalu_data[0];
    let nal_type = nal_header & 0x1F;
    let payload = &nalu_data[1..]; // data without the NAL header byte

    // FU indicator: keep F and NRI bits from original header, set type=28 (FU-A).
    let fu_indicator = (nal_header & 0xE0) | 28;

    let mut packets = Vec::new();
    let mut offset = 0;
    let remaining = payload.len();
    let fragment_mtu = mtu.saturating_sub(2); // FU indicator + FU header = 2 bytes

    while offset < remaining {
        let fragment_size = std::cmp::min(fragment_mtu, remaining - offset);
        let is_first = offset == 0;
        let is_last = offset + fragment_size >= remaining;

        // FU header: S (bit 7), E (bit 6), R=0 (bit 5), Type (bits 0-4).
        let fu_header =
            (if is_first { 0x80u8 } else { 0u8 }) | (if is_last { 0x40u8 } else { 0u8 }) | nal_type;

        let mut fragment = Vec::with_capacity(2 + fragment_size);
        fragment.push(fu_indicator);
        fragment.push(fu_header);
        fragment.extend_from_slice(&payload[offset..offset + fragment_size]);

        packets.push(fragment);
        offset += fragment_size;
    }

    packets
}

/// Build a complete interleaved RTP packet (header + payload) for sending.
fn build_rtp_packet(
    channel: u8,
    marker: bool,
    seq: u16,
    timestamp: u32,
    ssrc: u32,
    payload: &[u8],
) -> Vec<u8> {
    let header = build_rtp_header(marker, seq, timestamp, ssrc);
    let mut rtp = Vec::with_capacity(RTP_HEADER_LEN + payload.len());
    rtp.extend_from_slice(&header);
    rtp.extend_from_slice(payload);
    build_interleaved_rtp(channel, &rtp)
}

// ============================================================================
// RTSP method handlers
// ============================================================================

/// Handle OPTIONS request.
async fn handle_options(writer: &mut tokio::net::tcp::WriteHalf<'_>, cseq: u32) {
    send_response(
        writer,
        "200",
        "OK",
        &[
            ("CSeq", &cseq.to_string()),
            ("Public", "OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN "),
        ],
        "",
    )
    .await
    .ok();
}

/// Handle DESCRIBE request — check auth, return SDP.
async fn handle_describe(
    req: &RtspRequest,
    writer: &mut tokio::net::tcp::WriteHalf<'_>,
    cseq: u32,
    has_auth: &mut bool,
    inner: &Arc<Mutex<Inner>>,
    config: &RtspConfig,
) {
    // Check authentication.
    if is_auth_configured(config) && !*has_auth {
        if let Some(auth_header) = req.headers.get("Authorization") {
            if check_basic_auth(auth_header, config)
                || check_digest_auth(auth_header, "DESCRIBE", &req.url, "", config)
            {
                *has_auth = true;
            }
        }
    }

    if is_auth_configured(config) && !*has_auth {
        let nonce = generate_nonce();
        let www_auth = format!(
            "Basic realm=\"{}\", Digest realm=\"{}\", nonce=\"{}\", algorithm=MD5",
            config.realm, config.realm, nonce
        );
        send_response(
            writer,
            "401",
            "Unauthorized",
            &[("CSeq", &cseq.to_string()), ("WWW-Authenticate", &www_auth)],
            "",
        )
        .await
        .ok();
        return;
    }

    let sdp = generate_sdp(inner);

    send_response(
        writer,
        "200",
        "OK",
        &[
            ("CSeq", &cseq.to_string()),
            ("Content-Type", "application/sdp"),
        ],
        &sdp,
    )
    .await
    .ok();
}

/// Handle SETUP request — create session, set up transport.
// Mirrors the full RTSP request + connection state; grouping would just
// re-bundle fields the single call site already holds individually.
#[allow(clippy::too_many_arguments)]
async fn handle_setup(
    req: &RtspRequest,
    writer: &mut tokio::net::tcp::WriteHalf<'_>,
    session_id: &mut Option<String>,
    cseq: u32,
    has_auth: &mut bool,
    inner: &Arc<Mutex<Inner>>,
    config: &RtspConfig,
    peer_addr: std::net::SocketAddr,
) -> Result<(), RtspError> {
    // Check authentication.
    if is_auth_configured(config) && !*has_auth {
        if let Some(auth_header) = req.headers.get("Authorization") {
            if check_basic_auth(auth_header, config)
                || check_digest_auth(auth_header, "SETUP", &req.url, "", config)
            {
                *has_auth = true;
            }
        }
    }

    if is_auth_configured(config) && !*has_auth {
        let nonce = generate_nonce();
        let www_auth = format!(
            "Basic realm=\"{}\", Digest realm=\"{}\", nonce=\"{}\", algorithm=MD5",
            config.realm, config.realm, nonce
        );
        send_response(
            writer,
            "401",
            "Unauthorized",
            &[("CSeq", &cseq.to_string()), ("WWW-Authenticate", &www_auth)],
            "",
        )
        .await
        .ok();
        return Ok(());
    }

    // Parse Transport header.
    let transport = req
        .headers
        .get("Transport")
        .map(|s| s.as_str())
        .unwrap_or("");

    // Detect transport mode: TCP (interleaved) or UDP (client_port).
    let is_tcp = transport.contains("TCP") || transport.contains("interleaved=");
    let (rtp_ch, rtcp_ch) = if is_tcp {
        parse_transport_interleaved(transport)
    } else {
        (0, 0)
    };

    // For UDP: parse client_port and create UDP socket.
    let (udp_socket, udp_client_addr, transport_response) = if is_tcp {
        let resp = format!("RTP/AVP/TCP;unicast;interleaved={}-{}", rtp_ch, rtcp_ch);
        (None, None, resp)
    } else {
        // Parse client_port=X-Y.
        let client_rtp_port = parse_client_port(transport).unwrap_or(5000);
        let client_addr = std::net::SocketAddr::new(peer_addr.ip(), client_rtp_port);
        // Bind a UDP socket on a random server port.
        let sock = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
        let server_port = sock.local_addr()?.port();
        let resp = format!(
            "RTP/AVP;unicast;client_port={}-{};server_port={}-{}",
            client_rtp_port,
            client_rtp_port + 1,
            server_port,
            server_port + 1
        );
        (Some(Arc::new(sock)), Some(client_addr), resp)
    };

    // Generate session ID and SSRC.
    let sid = generate_session_id();
    let ssrc: u32;
    let initial_seq: u16;
    {
        let mut rng = rand::thread_rng();
        ssrc = rng.gen();
        initial_seq = rng.gen();
    }

    // Create session.
    let session = Session {
        id: sid.clone(),
        rtp_channel: rtp_ch,
        rtcp_channel: rtcp_ch,
        is_playing: false,
        ssrc,
        seq: Arc::new(AtomicU16::new(initial_seq)),
        base_time: Instant::now(),
        last_ts: Arc::new(AtomicU32::new(0)),
        is_udp: !is_tcp,
        udp_socket,
        udp_client_addr,
    };

    {
        let mut guard = inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.sessions.insert(sid.clone(), session);
    }

    *session_id = Some(sid.clone());

    send_response(
        writer,
        "200",
        "OK",
        &[
            ("CSeq", &cseq.to_string()),
            ("Session", &sid),
            ("Transport", &transport_response),
        ],
        "",
    )
    .await
    .ok();

    Ok(())
}

/// Parse the client RTP port from a UDP Transport header (e.g. `client_port=50000-50001`).
fn parse_client_port(transport: &str) -> Option<u16> {
    for part in transport.split(';') {
        let part = part.trim();
        if let Some(range) = part.strip_prefix("client_port=") {
            let ports: Vec<&str> = range.splitn(2, '-').collect();
            return ports.first().and_then(|s| s.parse().ok());
        }
    }
    None
}

/// Parse interleaved channel numbers from the Transport header.
fn parse_transport_interleaved(transport: &str) -> (u8, u8) {
    for part in transport.split(';') {
        let part = part.trim();
        if let Some(range) = part.strip_prefix("interleaved=") {
            let channels: Vec<&str> = range.splitn(2, '-').collect();
            if channels.len() == 2 {
                let rtp = channels[0].parse().unwrap_or(0);
                let rtcp = channels[1].parse().unwrap_or(1);
                return (rtp, rtcp);
            }
        }
    }
    (0, 1)
}

/// Handle PLAY request — subscribe to AuHub, start frame delivery.
#[allow(clippy::too_many_arguments)]
async fn handle_play(
    req: &RtspRequest,
    writer: &mut tokio::net::tcp::WriteHalf<'_>,
    session_id: &mut Option<String>,
    cseq: u32,
    has_auth: &mut bool,
    frame_bridge: &mut Option<tokio::sync::mpsc::Receiver<AccessUnit>>,
    inner: &Arc<Mutex<Inner>>,
    au_hub: &Arc<AuHub>,
    config: &RtspConfig,
) -> Result<(), RtspError> {
    // Check authentication.
    if is_auth_configured(config) && !*has_auth {
        if let Some(auth_header) = req.headers.get("Authorization") {
            if check_basic_auth(auth_header, config)
                || check_digest_auth(auth_header, "PLAY", &req.url, "", config)
            {
                *has_auth = true;
            }
        }
    }

    if is_auth_configured(config) && !*has_auth {
        let nonce = generate_nonce();
        let www_auth = format!(
            "Basic realm=\"{}\", Digest realm=\"{}\", nonce=\"{}\", algorithm=MD5",
            config.realm, config.realm, nonce
        );
        send_response(
            writer,
            "401",
            "Unauthorized",
            &[("CSeq", &cseq.to_string()), ("WWW-Authenticate", &www_auth)],
            "",
        )
        .await
        .ok();
        return Ok(());
    }

    // Get session ID from header or connection state.
    let sid = req
        .headers
        .get("Session")
        .cloned()
        .or_else(|| session_id.clone())
        .unwrap_or_default();

    if sid.is_empty() {
        send_response(
            writer,
            "454",
            "Session Not Found",
            &[("CSeq", &cseq.to_string())],
            "",
        )
        .await
        .ok();
        return Ok(());
    }

    // Mark session as playing.
    let session_exists = {
        let mut guard = inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(session) = guard.sessions.get_mut(&sid) {
            session.is_playing = true;
            true
        } else {
            false
        }
    };

    if !session_exists {
        send_response(
            writer,
            "454",
            "Session Not Found",
            &[("CSeq", &cseq.to_string())],
            "",
        )
        .await
        .ok();
        return Ok(());
    }

    *session_id = Some(sid.clone());

    // Subscribe to AuHub and bridge to async channel.
    let subscriber = au_hub.subscribe();
    let (tx, rx) = tokio::sync::mpsc::channel::<AccessUnit>(64);

    // Spawn blocking task to bridge sync receiver to async channel.
    tokio::task::spawn_blocking(move || {
        while let Ok(au) = subscriber.receiver.recv() {
            if tx.blocking_send(au).is_err() {
                break;
            }
        }
    });

    *frame_bridge = Some(rx);

    send_response(
        writer,
        "200",
        "OK",
        &[
            ("CSeq", &cseq.to_string()),
            ("Session", &sid),
            ("Range", "npt=0.000-"),
        ],
        "",
    )
    .await
    .ok();

    Ok(())
}

/// Handle TEARDOWN request — remove session, stop frame delivery.
async fn handle_teardown(
    writer: &mut tokio::net::tcp::WriteHalf<'_>,
    session_id: &mut Option<String>,
    cseq: u32,
    frame_bridge: &mut Option<tokio::sync::mpsc::Receiver<AccessUnit>>,
    inner: &Arc<Mutex<Inner>>,
    _config: &RtspConfig,
) {
    let sid = session_id.take();

    if let Some(ref sid) = sid {
        // Remove session from server state.
        {
            let mut guard = inner.lock().unwrap_or_else(|e| e.into_inner());
            guard.sessions.remove(sid);
        }

        // Drop frame bridge (stops the spawn_blocking task).
        *frame_bridge = None;

        send_response(
            writer,
            "200",
            "OK",
            &[("CSeq", &cseq.to_string()), ("Session", sid)],
            "",
        )
        .await
        .ok();
    } else {
        send_response(
            writer,
            "454",
            "Session Not Found",
            &[("CSeq", &cseq.to_string())],
            "",
        )
        .await
        .ok();
    }
}

// ============================================================================
// Response helpers
// ============================================================================

/// Send an RTSP response.
async fn send_response(
    writer: &mut tokio::net::tcp::WriteHalf<'_>,
    status_code: &str,
    reason: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Result<(), RtspError> {
    let mut response = format!("RTSP/1.0 {status_code} {reason}\r\n");

    for (name, value) in headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }

    if !body.is_empty() {
        response.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }

    response.push_str("\r\n");
    response.push_str(body);

    writer.write_all(response.as_bytes()).await?;
    writer.flush().await?;

    Ok(())
}

/// Generate a random session ID (16 hex characters).
fn generate_session_id() -> String {
    let mut rng = rand::thread_rng();
    let id: u64 = rng.gen();
    format!("{:016x}", id)
}

// ============================================================================
// Frame delivery
// ============================================================================

/// Convert an [`AccessUnit`] to interleaved RTP frames, sending to a writer.
///
/// Returns the number of interleaved RTP frames written.
pub async fn write_access_unit(
    au: &AccessUnit,
    writer: &mut tokio::net::tcp::WriteHalf<'_>,
    session: &Session,
    inner: &Arc<Mutex<Inner>>,
) -> Result<usize, RtspError> {
    let frames = access_unit_to_frames(au, session, inner);
    let count = frames.len();

    for frame in &frames {
        writer.write_all(frame).await?;
    }
    writer.flush().await?;

    Ok(count)
}

/// Convert an [`AccessUnit`] to interleaved RTP frame bytes.
///
/// This is the core frame→RTP conversion.
pub fn access_unit_to_frames(
    au: &AccessUnit,
    session: &Session,
    inner: &Arc<Mutex<Inner>>,
) -> Vec<Vec<u8>> {
    let base_time = session.base_time;
    let ssrc = session.ssrc;
    let rtp_channel = session.rtp_channel;

    // RTP timestamp — strictly monotonic to prevent DTS duplication.
    let computed = (au
        .timestamp
        .saturating_duration_since(base_time)
        .as_secs_f64()
        * 90000.0) as u32;
    let prev = session.last_ts.fetch_max(computed, Ordering::SeqCst);
    // Minimum increment: 3000 ticks (1 frame at 30fps / 90kHz).
    // Prevents DTS duplication when camera produces frames in bursts.
    let timestamp = if prev + 3000 > computed {
        prev + 3000
    } else {
        computed
    };
    session.last_ts.store(timestamp, Ordering::SeqCst);

    // Collect all NALUs to send.
    let mut all_nalus: Vec<&Nalu> = Vec::new();
    let mut injected: Vec<Nalu> = Vec::new();

    // For key frames, inject stored SPS/PPS if not present in the AU.
    if au.is_key_frame {
        let has_sps = au.nalus.iter().any(|n| n.is_sps);
        let has_pps = au.nalus.iter().any(|n| n.is_pps);

        if !has_sps || !has_pps {
            let guard = inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(sps_data) = &guard.sps {
                if !has_sps {
                    injected.push(Nalu {
                        nalu_type: 7,
                        data: sps_data.clone(),
                        is_idr: false,
                        is_sps: true,
                        is_pps: false,
                        is_aud: false,
                    });
                }
            }
            if let Some(pps_data) = &guard.pps {
                if !has_pps {
                    injected.push(Nalu {
                        nalu_type: 8,
                        data: pps_data.clone(),
                        is_idr: false,
                        is_sps: false,
                        is_pps: true,
                        is_aud: false,
                    });
                }
            }
        }
    }

    // Add injected SPS/PPS first, then the access unit NALUs.
    for nalu in &injected {
        all_nalus.push(nalu);
    }

    // Add all NALUs from the access unit, updating shared SPS/PPS state.
    for nalu in &au.nalus {
        if nalu.is_sps || nalu.is_pps {
            let mut guard = inner.lock().unwrap_or_else(|e| e.into_inner());
            if nalu.is_sps {
                guard.sps = Some(nalu.data.clone());
            }
            if nalu.is_pps {
                guard.pps = Some(nalu.data.clone());
            }
        }
        all_nalus.push(nalu);
    }

    // Packetize each NALU into RTP payloads.
    let mut frames: Vec<Vec<u8>> = Vec::new();
    let total_nalus = all_nalus.len();

    for (nalu_idx, nalu) in all_nalus.iter().enumerate() {
        let payloads = packetize_nalu(nalu, RTP_MTU);
        let total_payloads = payloads.len();

        for (pkt_idx, payload) in payloads.iter().enumerate() {
            let is_last_pkt = nalu_idx == total_nalus - 1 && pkt_idx == total_payloads - 1;
            let seq = session.seq.fetch_add(1, Ordering::SeqCst);

            let frame = build_rtp_packet(rtp_channel, is_last_pkt, seq, timestamp, ssrc, payload);
            frames.push(frame);
        }
    }

    frames
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // SDP generation
    // -----------------------------------------------------------------------

    #[test]
    fn test_sdp_generation_contains_required_fields() {
        let inner = Arc::new(Mutex::new(Inner {
            sessions: HashMap::new(),
            sps: None,
            pps: None,
        }));

        let sdp = generate_sdp(&inner);

        assert!(sdp.contains("v=0"), "SDP should contain v=0");
        assert!(sdp.contains("o=-"), "SDP should contain o=");
        assert!(
            sdp.contains("s=MiBee Eye Stream"),
            "SDP should contain session name"
        );
        assert!(sdp.contains("t=0 0"), "SDP should contain t=0 0");
        assert!(sdp.contains("m=video"), "SDP should contain m=video");
        assert!(sdp.contains("RTP/AVP"), "SDP should contain RTP/AVP");
        assert!(sdp.contains("H264"), "SDP should contain H264 codec");
        assert!(
            sdp.contains("profile-level-id"),
            "SDP should contain profile-level-id"
        );
        assert!(
            sdp.contains("packetization-mode=1"),
            "SDP should contain packetization-mode=1"
        );
        assert!(sdp.contains("90000"), "SDP should contain 90kHz clock rate");
    }

    #[test]
    fn test_sdp_with_sps_pps_includes_sprop() {
        let inner = Arc::new(Mutex::new(Inner {
            sessions: HashMap::new(),
            sps: Some(vec![0x67, 0x42, 0x00, 0x1e]),
            pps: Some(vec![0x68, 0xce, 0x38, 0x80]),
        }));

        let sdp = generate_sdp(&inner);

        assert!(
            sdp.contains("sprop-parameter-sets"),
            "SDP should contain sprop-parameter-sets"
        );
        assert!(
            sdp.contains("profile-level-id=42001e"),
            "SDP should have correct profile-level-id"
        );
    }

    #[test]
    fn test_sdp_default_profile_level_id() {
        let inner = Arc::new(Mutex::new(Inner {
            sessions: HashMap::new(),
            sps: None,
            pps: None,
        }));

        let sdp = generate_sdp(&inner);

        assert!(
            sdp.contains("profile-level-id=42e01f"),
            "SDP should use default profile-level-id"
        );
    }

    // -----------------------------------------------------------------------
    // RTP header
    // -----------------------------------------------------------------------

    #[test]
    fn test_rtp_header_format() {
        let header = build_rtp_header(false, 1234, 3600, 0xDEAD_BEEF);

        assert_eq!(header[0], 0x80, "V=2, P=0, X=0, CC=0");
        assert_eq!(header[1], 96, "M=0, PT=96");
        assert_eq!(header[2], 0x04, "seq=1234 → high byte 0x04");
        assert_eq!(header[3], 0xD2, "seq=1234 → low byte 0xD2");
        assert_eq!(header[7], 0x10, "timestamp=3600 → low byte 0x10");
        assert_eq!(header[8], 0xDE, "SSRC byte 3");
        assert_eq!(header[9], 0xAD, "SSRC byte 2");
        assert_eq!(header[10], 0xBE, "SSRC byte 1");
        assert_eq!(header[11], 0xEF, "SSRC byte 0");
    }

    #[test]
    fn test_rtp_header_marker() {
        let with_marker = build_rtp_header(true, 0, 0, 0);
        let without_marker = build_rtp_header(false, 0, 0, 0);

        assert_eq!(with_marker[1], 0x80 | 96, "Marker bit should be set");
        assert_eq!(without_marker[1], 96, "Marker bit should be clear");
    }

    // -----------------------------------------------------------------------
    // RTP packetization - single NALU
    // -----------------------------------------------------------------------

    #[test]
    fn test_rtp_packetization_single_nalu() {
        let nalu = Nalu {
            nalu_type: 5,
            data: vec![0x65, 0x88, 0x84, 0x00, 0x10],
            is_idr: true,
            is_sps: false,
            is_pps: false,
            is_aud: false,
        };

        let packets = packetize_nalu(&nalu, RTP_MTU);

        assert_eq!(packets.len(), 1, "Small NALU should produce 1 packet");
        assert_eq!(
            packets[0],
            vec![0x65, 0x88, 0x84, 0x00, 0x10],
            "Single NALU payload should be raw NALU data"
        );
    }

    #[test]
    fn test_rtp_packetization_empty_nalu() {
        let nalu = Nalu {
            nalu_type: 0,
            data: vec![],
            is_idr: false,
            is_sps: false,
            is_pps: false,
            is_aud: false,
        };

        let packets = packetize_nalu(&nalu, RTP_MTU);
        assert!(packets.is_empty(), "Empty NALU should produce no packets");
    }

    // -----------------------------------------------------------------------
    // RTP FU-A fragmentation
    // -----------------------------------------------------------------------

    #[test]
    fn test_rtp_fu_a_fragmentation() {
        // A large NALU that exceeds MTU.
        let mut data = vec![0xAB; RTP_MTU + 100];
        data.insert(0, 0x65); // IDR type 5

        let nalu = Nalu {
            nalu_type: 5,
            data,
            is_idr: true,
            is_sps: false,
            is_pps: false,
            is_aud: false,
        };

        let packets = packetize_nalu(&nalu, RTP_MTU);

        assert!(
            packets.len() > 1,
            "Large NALU should produce multiple FU-A fragments"
        );

        // First packet: FU-A start.
        assert_eq!(
            packets[0][0] & 0x1F,
            28,
            "FU-A indicator should have type=28"
        );
        assert_eq!(
            packets[0][1] & 0x80,
            0x80,
            "First FU-A header should have S=1"
        );
        assert_eq!(packets[0][1] & 0x40, 0, "First FU-A header should have E=0");

        // Last packet: FU-A end.
        let last = packets.last().unwrap();
        assert_eq!(last[1] & 0x40, 0x40, "Last FU-A header should have E=1");
        assert_eq!(last[1] & 0x80, 0, "Last FU-A header should have S=0");

        // Middle packets (if any): FU-A continuation.
        for p in &packets[1..packets.len() - 1] {
            assert_eq!(p[1] & 0xC0, 0, "Middle FU-A header should have S=0, E=0");
        }
    }

    #[test]
    fn test_rtp_fu_a_reconstructs_original_type() {
        let mut data = vec![0xCD; RTP_MTU + 50];
        data.insert(0, 0x41); // non-IDR slice, type=1

        let nalu = Nalu {
            nalu_type: 1,
            data,
            is_idr: false,
            is_sps: false,
            is_pps: false,
            is_aud: false,
        };

        let packets = packetize_nalu(&nalu, RTP_MTU);

        for pkt in &packets {
            // FU header type should be the original NAL type (1).
            assert_eq!(
                pkt[1] & 0x1F,
                1,
                "FU header should preserve original NAL type"
            );
            // FU indicator should preserve NRI bits.
            assert_eq!(pkt[0] & 0x60, 0x40, "FU indicator should preserve NRI bits");
        }
    }

    // -----------------------------------------------------------------------
    // Interleaved frame format
    // -----------------------------------------------------------------------

    #[test]
    fn test_interleaved_format() {
        let rtp_data = [
            0x80u8, 0x60, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
        ];
        let frame = build_interleaved_rtp(0, &rtp_data);

        assert_eq!(frame[0], 0x24, "Should start with $ magic byte");
        assert_eq!(frame[1], 0, "Channel should be 0");
        assert_eq!(
            (frame[2] as usize) << 8 | (frame[3] as usize),
            rtp_data.len(),
            "Length should match RTP packet size"
        );
        assert_eq!(&frame[4..], &rtp_data[..], "Payload should match RTP data");
    }

    // -----------------------------------------------------------------------
    // Basic auth
    // -----------------------------------------------------------------------

    #[test]
    fn test_basic_auth_valid() {
        let config = RtspConfig {
            username: "admin".to_string(),
            password: "secret".to_string(),
            ..Default::default()
        };

        // Base64 of "admin:secret" is "YWRtaW46c2VjcmV0".
        let header = "Basic YWRtaW46c2VjcmV0";
        assert!(
            check_basic_auth(header, &config),
            "Valid credentials should pass"
        );
    }

    #[test]
    fn test_basic_auth_invalid() {
        let config = RtspConfig {
            username: "admin".to_string(),
            password: "secret".to_string(),
            ..Default::default()
        };

        // Wrong password.
        let header = "Basic YWRtaW46d3Jvbmc=";
        assert!(
            !check_basic_auth(header, &config),
            "Wrong password should fail"
        );

        // Wrong username.
        let header = "Basic Z3Vlc3Q6c2VjcmV0";
        assert!(
            !check_basic_auth(header, &config),
            "Wrong username should fail"
        );

        // Missing prefix.
        let header = "YWRtaW46c2VjcmV0";
        assert!(
            !check_basic_auth(header, &config),
            "Missing Basic prefix should fail"
        );
    }

    #[test]
    fn test_basic_auth_no_auth_config() {
        let config = RtspConfig::default(); // empty username/password
        let header = "Basic YWRtaW46c2VjcmV0";
        assert!(
            !check_basic_auth(header, &config),
            "Auth should fail when no credentials configured"
        );
    }

    // -----------------------------------------------------------------------
    // Digest auth
    // -----------------------------------------------------------------------

    #[test]
    fn test_digest_auth_nonce_verification() {
        let config = RtspConfig {
            username: "admin".to_string(),
            password: "secret".to_string(),
            realm: "MiBee Eye RTSP".to_string(),
            ..Default::default()
        };

        let nonce = "abc123";
        let method = "DESCRIBE";
        let uri = "/streamid=0";

        use md5::{Digest, Md5};

        let ha1 = {
            let mut hasher = Md5::new();
            hasher.update(b"admin:MiBee Eye RTSP:secret");
            format!("{:x}", hasher.finalize())
        };

        let ha2 = {
            let mut hasher = Md5::new();
            hasher.update(b"DESCRIBE:/streamid=0");
            format!("{:x}", hasher.finalize())
        };

        let expected = {
            let mut hasher = Md5::new();
            hasher.update(format!("{ha1}:{nonce}:{ha2}"));
            format!("{:x}", hasher.finalize())
        };

        let auth_header = format!(
            "Digest username=\"admin\", realm=\"MiBee Eye RTSP\", \
             nonce=\"{nonce}\", uri=\"{uri}\", response=\"{expected}\""
        );

        assert!(
            check_digest_auth(&auth_header, method, uri, nonce, &config),
            "Correct digest response should pass"
        );
    }

    #[test]
    fn test_digest_auth_wrong_password() {
        let config = RtspConfig {
            username: "admin".to_string(),
            password: "secret".to_string(),
            realm: "MiBee Eye RTSP".to_string(),
            ..Default::default()
        };

        let nonce = "abc123";
        let method = "DESCRIBE";
        let uri = "/streamid=0";

        use md5::{Digest, Md5};

        let ha1 = {
            let mut hasher = Md5::new();
            hasher.update(b"admin:MiBee Eye RTSP:wrongpass");
            format!("{:x}", hasher.finalize())
        };

        let ha2 = {
            let mut hasher = Md5::new();
            hasher.update(b"DESCRIBE:/streamid=0");
            format!("{:x}", hasher.finalize())
        };

        let expected = {
            let mut hasher = Md5::new();
            hasher.update(format!("{ha1}:{nonce}:{ha2}"));
            format!("{:x}", hasher.finalize())
        };

        let auth_header = format!(
            "Digest username=\"admin\", realm=\"MiBee Eye RTSP\", \
             nonce=\"{nonce}\", uri=\"{uri}\", response=\"{expected}\""
        );

        assert!(
            !check_digest_auth(&auth_header, method, uri, nonce, &config),
            "Wrong password should fail digest auth"
        );
    }

    #[test]
    fn test_digest_auth_missing_params() {
        let config = RtspConfig {
            username: "admin".to_string(),
            password: "secret".to_string(),
            realm: "MiBee Eye RTSP".to_string(),
            ..Default::default()
        };

        // Missing response.
        let header = "Digest username=\"admin\", realm=\"Test\"";
        assert!(
            !check_digest_auth(header, "DESCRIBE", "/", "nonce", &config),
            "Missing response should fail"
        );

        // Missing username.
        let header = "Digest realm=\"Test\", response=\"abc\"";
        assert!(
            !check_digest_auth(header, "DESCRIBE", "/", "nonce", &config),
            "Missing username should fail"
        );
    }

    #[test]
    fn test_digest_auth_wrong_username() {
        let config = RtspConfig {
            username: "admin".to_string(),
            password: "secret".to_string(),
            realm: "MiBee Eye RTSP".to_string(),
            ..Default::default()
        };

        use md5::{Digest, Md5};

        let ha1 = {
            let mut hasher = Md5::new();
            hasher.update(b"hacker:MiBee Eye RTSP:secret");
            format!("{:x}", hasher.finalize())
        };

        let ha2 = {
            let mut hasher = Md5::new();
            hasher.update(b"DESCRIBE:/");
            format!("{:x}", hasher.finalize())
        };

        let expected = {
            let mut hasher = Md5::new();
            hasher.update(format!("{ha1}:nonce:{ha2}"));
            format!("{:x}", hasher.finalize())
        };

        let header = format!(
            "Digest username=\"hacker\", realm=\"MiBee Eye RTSP\", \
             nonce=\"nonce\", uri=\"/\", response=\"{expected}\""
        );

        assert!(
            !check_digest_auth(&header, "DESCRIBE", "/", "nonce", &config),
            "Wrong username should fail"
        );
    }

    // -----------------------------------------------------------------------
    // Nonce generation
    // -----------------------------------------------------------------------

    #[test]
    fn test_generate_nonce_not_empty() {
        let nonce = generate_nonce();
        assert!(!nonce.is_empty(), "Nonce should not be empty");
        assert_eq!(
            nonce.len(),
            32,
            "Nonce should be 32 hex characters (16 bytes)"
        );
    }

    #[test]
    fn test_generate_nonce_unique() {
        let n1 = generate_nonce();
        let n2 = generate_nonce();
        assert_ne!(n1, n2, "Nonces should be unique");
    }

    // -----------------------------------------------------------------------
    // Digest params parsing
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_digest_params() {
        let header = r#"Digest username="admin", realm="Test Realm", nonce="abc123", uri="/stream", response="deadbeef""#;
        let params = parse_digest_params(header);

        assert_eq!(params.get("username").map(|s| s.as_str()), Some("admin"));
        assert_eq!(params.get("realm").map(|s| s.as_str()), Some("Test Realm"));
        assert_eq!(params.get("nonce").map(|s| s.as_str()), Some("abc123"));
        assert_eq!(params.get("uri").map(|s| s.as_str()), Some("/stream"));
        assert_eq!(params.get("response").map(|s| s.as_str()), Some("deadbeef"));
    }

    // -----------------------------------------------------------------------
    // Transport header parsing
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_transport_interleaved() {
        let (rtp, rtcp) = parse_transport_interleaved("RTP/AVP/TCP;unicast;interleaved=0-1");
        assert_eq!(rtp, 0);
        assert_eq!(rtcp, 1);
    }

    #[test]
    fn test_parse_transport_interleaved_custom() {
        let (rtp, rtcp) = parse_transport_interleaved("RTP/AVP/TCP;unicast;interleaved=2-3");
        assert_eq!(rtp, 2);
        assert_eq!(rtcp, 3);
    }

    #[test]
    fn test_parse_transport_interleaved_default() {
        let (rtp, rtcp) = parse_transport_interleaved("RTP/AVP/TCP;unicast");
        assert_eq!(rtp, 0, "Default RTP channel should be 0");
        assert_eq!(rtcp, 1, "Default RTCP channel should be 1");
    }

    // -----------------------------------------------------------------------
    // Session ID generation
    // -----------------------------------------------------------------------

    #[test]
    fn test_generate_session_id_not_empty() {
        let sid = generate_session_id();
        assert!(!sid.is_empty(), "Session ID should not be empty");
        assert_eq!(sid.len(), 16, "Session ID should be 16 hex characters");
    }

    #[test]
    fn test_generate_session_id_unique() {
        let s1 = generate_session_id();
        let s2 = generate_session_id();
        assert_ne!(s1, s2, "Session IDs should be unique");
    }

    // -----------------------------------------------------------------------
    // Access unit to interleaved frames
    // -----------------------------------------------------------------------

    #[test]
    fn test_access_unit_to_frames_basic() {
        let inner = Arc::new(Mutex::new(Inner {
            sessions: HashMap::new(),
            sps: Some(vec![0x67, 0x42, 0x00, 0x1e]),
            pps: Some(vec![0x68, 0xce, 0x38, 0x80]),
        }));

        let session = Session {
            id: "test".to_string(),
            rtp_channel: 0,
            rtcp_channel: 1,
            is_playing: true,
            ssrc: 0xDEAD_BEEF,
            seq: Arc::new(AtomicU16::new(100)),
            base_time: Instant::now(),
            last_ts: Arc::new(AtomicU32::new(0)),
            is_udp: false,
            udp_socket: None,
            udp_client_addr: None,
        };

        let au = AccessUnit {
            nalus: vec![Nalu {
                nalu_type: 5,
                data: vec![0x65, 0x88, 0x84, 0x00, 0x10],
                is_idr: true,
                is_sps: false,
                is_pps: false,
                is_aud: false,
            }],
            timestamp: Instant::now(),
            is_key_frame: true,
        };

        let frames = access_unit_to_frames(&au, &session, &inner);

        assert!(!frames.is_empty(), "Should produce at least one frame");

        // Each frame should start with the interleaved magic byte.
        for frame in &frames {
            assert_eq!(frame[0], 0x24, "Frame should start with $ marker");
        }

        // Key frame without SPS/PPS should inject stored SPS/PPS.
        // So we expect SPS NALU → PPS NALU → IDR NALU.
        // Actually with the injected SPS/PPS, we have 3 NALUs.
        // Let's just verify we get the right structure.
        assert!(
            frames.len() >= 3,
            "Key frame should inject SPS+PPS+IDR, got {} frames",
            frames.len()
        );
    }

    // -----------------------------------------------------------------------
    // Base64 encode/decode
    // -----------------------------------------------------------------------

    #[test]
    fn test_base64_roundtrip() {
        let original = b"admin:secret";
        let encoded = base64_encode(original);
        let decoded = decode_base64(&encoded).unwrap();
        assert_eq!(decoded, original, "Base64 roundtrip should preserve data");
    }

    #[test]
    fn test_base64_encode_sprop() {
        let sps = vec![0x67, 0x42, 0x00, 0x1e];
        let encoded = base64_encode(&sps);
        assert!(!encoded.is_empty(), "Base64 output should not be empty");
        // Verify it decodes correctly.
        let decoded = decode_base64(&encoded).unwrap();
        assert_eq!(decoded, sps);
    }

    // -----------------------------------------------------------------------
    // RTSP request parsing
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_rtsp_request_describe() {
        let data = "DESCRIBE rtsp://localhost:8554/stream RTSP/1.0\r\nCSeq: 2\r\n\r\n";
        let req = parse_rtsp_request(data).unwrap();
        assert_eq!(req.method, "DESCRIBE");
        assert_eq!(req.url, "/stream");
        assert_eq!(req.cseq, 2);
    }

    #[test]
    fn test_parse_rtsp_request_options() {
        let data = "OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n";
        let req = parse_rtsp_request(data).unwrap();
        assert_eq!(req.method, "OPTIONS");
        assert_eq!(req.cseq, 1);
    }

    #[test]
    fn test_parse_rtsp_request_setup() {
        let data = "SETUP rtsp://localhost:8554/stream/streamid=0 RTSP/1.0\r\n\
                     CSeq: 3\r\n\
                     Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\
                     \r\n";
        let req = parse_rtsp_request(data).unwrap();
        assert_eq!(req.method, "SETUP");
        assert_eq!(req.url, "/stream/streamid=0");
        assert_eq!(req.cseq, 3);
        assert_eq!(
            req.headers.get("Transport").map(|s| s.as_str()),
            Some("RTP/AVP/TCP;unicast;interleaved=0-1")
        );
    }

    // -----------------------------------------------------------------------
    // Loopback integration tests (real TCP, real session state machine)
    // -----------------------------------------------------------------------

    fn test_nalu(nalu_type: u8, data: Vec<u8>) -> Nalu {
        Nalu {
            nalu_type,
            is_idr: nalu_type == 5,
            is_sps: nalu_type == 7,
            is_pps: nalu_type == 8,
            is_aud: nalu_type == 9,
            data,
        }
    }

    fn key_au() -> AccessUnit {
        AccessUnit {
            nalus: vec![
                test_nalu(7, vec![0x67, 0x42, 0x00, 0x1e]),
                test_nalu(8, vec![0x68, 0xce, 0x38, 0x80]),
                test_nalu(5, vec![0x65, 0x88, 0x84, 0x00, 0x10, 0x3f]),
            ],
            timestamp: Instant::now(),
            is_key_frame: true,
        }
    }

    fn p_au() -> AccessUnit {
        AccessUnit {
            nalus: vec![test_nalu(1, vec![0x41, 0x9a, 0x02, 0x05])],
            timestamp: Instant::now(),
            is_key_frame: false,
        }
    }

    /// Read one RTSP response (headers + optional Content-Length body).
    async fn read_response(stream: &mut TcpStream) -> (String, Option<String>) {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        // Read until end of headers.
        loop {
            stream.read_exact(&mut byte).await.unwrap();
            buf.push(byte[0]);
            if buf.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let headers = String::from_utf8_lossy(&buf).to_string();
        let content_len = headers
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.trim()
                    .eq_ignore_ascii_case("Content-Length")
                    .then(|| v.trim().parse::<usize>().ok())?
            })
            .unwrap_or(0);
        let mut body = vec![0u8; content_len];
        if content_len > 0 {
            stream.read_exact(&mut body).await.unwrap();
        }
        (
            headers,
            (!body.is_empty()).then(|| String::from_utf8_lossy(&body).to_string()),
        )
    }

    async fn send_request(stream: &mut TcpStream, req: &str) {
        use tokio::io::AsyncWriteExt;
        stream.write_all(req.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
    }

    /// Read one `$`-framed interleaved RTP packet.
    async fn read_interleaved(stream: &mut TcpStream) -> (u8, Vec<u8>) {
        use tokio::io::AsyncReadExt;
        let mut hdr = [0u8; 4];
        stream.read_exact(&mut hdr).await.unwrap();
        assert_eq!(hdr[0], 0x24, "expected interleaved frame marker");
        let len = ((hdr[2] as usize) << 8) | hdr[3] as usize;
        let mut payload = vec![0u8; len];
        stream.read_exact(&mut payload).await.unwrap();
        (hdr[1], payload)
    }

    /// Start a server on an ephemeral port; returns its AuHub and port.
    /// The accept-loop task owns the server and dies with the test runtime.
    async fn spawn_server(config: RtspConfig) -> (Arc<AuHub>, u16) {
        let server = RtspServer::new(config).await.unwrap();
        let port = server.listener.local_addr().unwrap().port();
        let hub = server.au_hub().clone();
        tokio::spawn(async move {
            let _ = server.start().await;
        });
        (hub, port)
    }

    #[tokio::test]
    async fn test_full_interleaved_session_flow() {
        let (hub, port) = spawn_server(RtspConfig {
            port: 0,
            ..RtspConfig::default()
        })
        .await;
        let addr = format!("127.0.0.1:{port}");
        let mut client = TcpStream::connect(&addr).await.unwrap();

        // OPTIONS
        send_request(
            &mut client,
            "OPTIONS rtsp://127.0.0.1/stream RTSP/1.0\r\nCSeq: 1\r\n\r\n",
        )
        .await;
        let (hdrs, _) = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            read_response(&mut client),
        )
        .await
        .expect("OPTIONS response");
        assert!(
            hdrs.starts_with("RTSP/1.0 200"),
            "OPTIONS must return 200: {hdrs}"
        );
        assert!(hdrs.contains("Public: OPTIONS, DESCRIBE"));

        // DESCRIBE
        send_request(
            &mut client,
            "DESCRIBE rtsp://127.0.0.1/stream RTSP/1.0\r\nCSeq: 2\r\n\r\n",
        )
        .await;
        let (hdrs, body) = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            read_response(&mut client),
        )
        .await
        .expect("DESCRIBE response");
        assert!(
            hdrs.starts_with("RTSP/1.0 200"),
            "DESCRIBE must return 200: {hdrs}"
        );
        assert!(hdrs.contains("Content-Type: application/sdp"));
        let sdp = body.expect("DESCRIBE must carry the SDP body");
        assert!(sdp.contains("m=video"));

        // SETUP (TCP interleaved)
        send_request(
            &mut client,
            "SETUP rtsp://127.0.0.1/stream/streamid=0 RTSP/1.0\r\nCSeq: 3\r\n\
             Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n",
        )
        .await;
        let (hdrs, _) = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            read_response(&mut client),
        )
        .await
        .expect("SETUP response");
        assert!(
            hdrs.starts_with("RTSP/1.0 200"),
            "SETUP must return 200: {hdrs}"
        );
        assert!(hdrs.contains("Session: "), "SETUP must allocate a session");
        assert!(hdrs.contains("interleaved=0-1"));
        let sid = hdrs
            .lines()
            .find_map(|l| l.strip_prefix("Session: ").map(|s| s.trim().to_string()))
            .expect("session id header");

        // PLAY
        send_request(
            &mut client,
            &format!("PLAY rtsp://127.0.0.1/stream RTSP/1.0\r\nCSeq: 4\r\nSession: {sid}\r\n\r\n"),
        )
        .await;
        let (hdrs, _) = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            read_response(&mut client),
        )
        .await
        .expect("PLAY response");
        assert!(
            hdrs.starts_with("RTSP/1.0 200"),
            "PLAY must return 200: {hdrs}"
        );
        assert!(hdrs.contains("Range: npt=0.000-"));

        // Push a key frame: SPS, PPS and IDR each ride their own RTP packet.
        hub.write(key_au());
        let (ch, pkt) = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            read_interleaved(&mut client),
        )
        .await
        .expect("first interleaved RTP packet");
        assert_eq!(ch, 0, "RTP rides channel 0");
        assert_eq!(pkt[0] >> 6, 0b10, "RTP version 2");
        assert_eq!(pkt[1] & 0x7f, 96, "dynamic payload type 96");
        // Drain PPS + IDR of the key frame.
        for _ in 0..2 {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                read_interleaved(&mut client),
            )
            .await
            .expect("key frame RTP packets");
        }

        // A P-frame after the key frame flows through.
        hub.write(p_au());
        let (_, pkt2) = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            read_interleaved(&mut client),
        )
        .await
        .expect("P-frame interleaved RTP packet");
        assert_eq!(&pkt2[12..13], &[0x41], "P-frame NALU header");

        // TEARDOWN
        send_request(
            &mut client,
            &format!(
                "TEARDOWN rtsp://127.0.0.1/stream RTSP/1.0\r\nCSeq: 5\r\nSession: {sid}\r\n\r\n"
            ),
        )
        .await;
        let (hdrs, _) = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            read_response(&mut client),
        )
        .await
        .expect("TEARDOWN response");
        assert!(
            hdrs.starts_with("RTSP/1.0 200"),
            "TEARDOWN must return 200: {hdrs}"
        );

        drop(client);
    }

    #[tokio::test]
    async fn test_auth_challenge_then_basic_success() {
        let config = RtspConfig {
            port: 0,
            username: "admin".into(),
            password: "secret".into(),
            ..RtspConfig::default()
        };
        let (_hub, port) = spawn_server(config).await;
        let mut client = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();

        // No credentials → 401 with a Digest challenge.
        send_request(
            &mut client,
            "DESCRIBE rtsp://127.0.0.1/stream RTSP/1.0\r\nCSeq: 1\r\n\r\n",
        )
        .await;
        let (hdrs, _) = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            read_response(&mut client),
        )
        .await
        .expect("401 response");
        assert!(hdrs.starts_with("RTSP/1.0 401"), "auth required: {hdrs}");
        assert!(hdrs.contains("WWW-Authenticate"));
        assert!(hdrs.contains("nonce="));

        // Wrong password → still 401.
        let bad = base64_encode(b"admin:wrong");
        send_request(
            &mut client,
            &format!(
                "DESCRIBE rtsp://127.0.0.1/stream RTSP/1.0\r\nCSeq: 2\r\n\
                 Authorization: Basic {bad}\r\n\r\n"
            ),
        )
        .await;
        let (hdrs, _) = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            read_response(&mut client),
        )
        .await
        .expect("401 for wrong password");
        assert!(
            hdrs.starts_with("RTSP/1.0 401"),
            "wrong password rejected: {hdrs}"
        );

        // Correct credentials → 200 and the connection stays authenticated.
        let good = base64_encode(b"admin:secret");
        send_request(
            &mut client,
            &format!(
                "DESCRIBE rtsp://127.0.0.1/stream RTSP/1.0\r\nCSeq: 3\r\n\
                 Authorization: Basic {good}\r\n\r\n"
            ),
        )
        .await;
        let (hdrs, body) = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            read_response(&mut client),
        )
        .await
        .expect("200 after Basic auth");
        assert!(
            hdrs.starts_with("RTSP/1.0 200"),
            "correct password accepted: {hdrs}"
        );
        assert!(body.expect("SDP body").contains("m=video"));

        // Follow-up request on the same connection needs no Authorization.
        send_request(
            &mut client,
            "SETUP rtsp://127.0.0.1/stream/streamid=0 RTSP/1.0\r\nCSeq: 4\r\n\
             Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n",
        )
        .await;
        let (hdrs, _) = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            read_response(&mut client),
        )
        .await
        .expect("SETUP after auth");
        assert!(
            hdrs.starts_with("RTSP/1.0 200"),
            "auth is remembered per connection: {hdrs}"
        );

        drop(client);
    }

    #[tokio::test]
    async fn test_play_without_session_is_454() {
        let (_hub, port) = spawn_server(RtspConfig {
            port: 0,
            ..RtspConfig::default()
        })
        .await;
        let mut client = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();

        send_request(
            &mut client,
            "PLAY rtsp://127.0.0.1/stream RTSP/1.0\r\nCSeq: 1\r\n\r\n",
        )
        .await;
        let (hdrs, _) = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            read_response(&mut client),
        )
        .await
        .expect("454 response");
        assert!(
            hdrs.starts_with("RTSP/1.0 454"),
            "PLAY without session: {hdrs}"
        );

        // PLAY for an unknown session id.
        send_request(
            &mut client,
            "PLAY rtsp://127.0.0.1/stream RTSP/1.0\r\nCSeq: 2\r\nSession: deadbeef\r\n\r\n",
        )
        .await;
        let (hdrs, _) = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            read_response(&mut client),
        )
        .await
        .expect("454 for unknown session");
        assert!(
            hdrs.starts_with("RTSP/1.0 454"),
            "unknown session rejected: {hdrs}"
        );

        // TEARDOWN without a session also answers 454.
        send_request(
            &mut client,
            "TEARDOWN rtsp://127.0.0.1/stream RTSP/1.0\r\nCSeq: 3\r\n\r\n",
        )
        .await;
        let (hdrs, _) = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            read_response(&mut client),
        )
        .await
        .expect("454 TEARDOWN");
        assert!(
            hdrs.starts_with("RTSP/1.0 454"),
            "TEARDOWN without session: {hdrs}"
        );

        drop(client);
    }

    #[tokio::test]
    async fn test_unknown_method_405_and_get_parameter_200() {
        let (_hub, port) = spawn_server(RtspConfig {
            port: 0,
            ..RtspConfig::default()
        })
        .await;
        let mut client = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();

        send_request(
            &mut client,
            "RECORD rtsp://127.0.0.1/stream RTSP/1.0\r\nCSeq: 1\r\n\r\n",
        )
        .await;
        let (hdrs, _) = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            read_response(&mut client),
        )
        .await
        .expect("405 response");
        assert!(hdrs.starts_with("RTSP/1.0 405"), "unknown method: {hdrs}");

        send_request(
            &mut client,
            "GET_PARAMETER rtsp://127.0.0.1/stream RTSP/1.0\r\nCSeq: 2\r\n\r\n",
        )
        .await;
        let (hdrs, _) = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            read_response(&mut client),
        )
        .await
        .expect("GET_PARAMETER 200");
        assert!(
            hdrs.starts_with("RTSP/1.0 200"),
            "GET_PARAMETER keep-alive: {hdrs}"
        );

        drop(client);
    }

    #[tokio::test]
    async fn test_malformed_requests_answer_400() {
        let (_hub, port) = spawn_server(RtspConfig {
            port: 0,
            ..RtspConfig::default()
        })
        .await;
        let mut client = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();

        // Valid UTF-8 but a broken request line.
        send_request(&mut client, "NOT-RTSP\r\n\r\n").await;
        let (hdrs, _) = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            read_response(&mut client),
        )
        .await
        .expect("400 response");
        assert!(
            hdrs.starts_with("RTSP/1.0 400"),
            "broken request line: {hdrs}"
        );

        // Non-UTF-8 payload with a complete header block.
        use tokio::io::AsyncWriteExt;
        client
            .write_all(&[
                0xff, 0xfe, b' ', b'x', b' ', b'R', b'T', b'S', b'P', b'/', b'1', b'.', b'0',
                b'\r', b'\n', b'\r', b'\n',
            ])
            .await
            .unwrap();
        let (hdrs, _) = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            read_response(&mut client),
        )
        .await
        .expect("400 for non-UTF8");
        assert!(
            hdrs.starts_with("RTSP/1.0 400"),
            "non-UTF-8 request: {hdrs}"
        );

        drop(client);
    }

    #[tokio::test]
    async fn test_udp_setup_and_play_delivers_rtp() {
        let (hub, port) = spawn_server(RtspConfig {
            port: 0,
            ..RtspConfig::default()
        })
        .await;

        // Client-side UDP socket whose port goes into the Transport header.
        let udp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_rtp_port = udp.local_addr().unwrap().port();

        let mut client = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();

        send_request(
            &mut client,
            &format!(
                "SETUP rtsp://127.0.0.1/stream RTSP/1.0\r\nCSeq: 1\r\n\
                 Transport: RTP/AVP;unicast;client_port={client_rtp_port}-{}\r\n\r\n",
                client_rtp_port + 1
            ),
        )
        .await;
        let (hdrs, _) = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            read_response(&mut client),
        )
        .await
        .expect("UDP SETUP response");
        assert!(hdrs.starts_with("RTSP/1.0 200"), "UDP SETUP: {hdrs}");
        assert!(
            hdrs.contains("server_port="),
            "server must advertise its UDP ports: {hdrs}"
        );
        let sid = hdrs
            .lines()
            .find_map(|l| l.strip_prefix("Session: ").map(|s| s.trim().to_string()))
            .expect("session id header");

        send_request(
            &mut client,
            &format!("PLAY rtsp://127.0.0.1/stream RTSP/1.0\r\nCSeq: 2\r\nSession: {sid}\r\n\r\n"),
        )
        .await;
        let (hdrs, _) = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            read_response(&mut client),
        )
        .await
        .expect("PLAY response");
        assert!(hdrs.starts_with("RTSP/1.0 200"), "UDP PLAY: {hdrs}");

        hub.write(key_au());
        let mut buf = [0u8; 2048];
        let (len, _) =
            tokio::time::timeout(std::time::Duration::from_secs(3), udp.recv_from(&mut buf))
                .await
                .expect("UDP RTP packet timed out")
                .expect("UDP RTP recv failed");
        assert!(len >= 12, "RTP header present");
        assert_eq!(buf[0] >> 6, 0b10, "RTP version 2");
        assert_eq!(buf[1] & 0x7f, 96, "payload type 96");
        assert_ne!(buf[0], 0x24, "no interleaving marker over UDP");

        drop(client);
    }

    #[tokio::test]
    async fn test_write_access_unit_streams_interleaved_frames() {
        // exercise the public write path with a real socket pair
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut probe = TcpStream::connect(addr).await.unwrap();
        let (mut server_side, _) = listener.accept().await.unwrap();
        // write_access_unit takes the borrowed half from TcpStream::split().
        let (_reader, mut writer) = server_side.split();

        let session = Session {
            id: "session".into(),
            rtp_channel: 0,
            rtcp_channel: 1,
            is_playing: true,
            ssrc: 0x1122_3344,
            seq: Arc::new(AtomicU16::new(7)),
            base_time: Instant::now(),
            last_ts: Arc::new(AtomicU32::new(0)),
            is_udp: false,
            udp_socket: None,
            udp_client_addr: None,
        };
        let inner = Arc::new(Mutex::new(Inner {
            sessions: HashMap::new(),
            sps: None,
            pps: None,
        }));

        let count = write_access_unit(&key_au(), &mut writer, &session, &inner)
            .await
            .unwrap();
        // SPS + PPS + IDR → three interleaved packets.
        assert_eq!(count, 3, "one packet per NALU");

        let (ch, pkt) = read_interleaved(&mut probe).await;
        assert_eq!(ch, 0);
        assert_eq!(pkt[1] & 0x7f, 96);
    }
}
