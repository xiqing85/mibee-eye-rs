//! Chunked-HTTP fMP4 streaming for MSE (SPEC v1 §4.1: `GET
//! /api/cameras/{id}/stream.mse`).
//!
//! Replaces the WebSocket video path: the same fMP4 muxer (`web::fmp4`),
//! served as a `video/mp4` chunked response — init segment first, then one
//! `moof`+`mdat` per access unit. Low-latency design carried over from the
//! WS implementation: AuHub subscriber capacity 2, spawn_blocking bridge
//! drains to the latest frame, no intermediate buffering.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::h264::hub::AccessUnit;
use crate::web::api::AppState;
use crate::web::fmp4;

/// Shared floor of the fMP4 media clock (90 kHz ticks). Hands the MSE
/// timeline from one HTTP connection to the next: a new connection seeds
/// strictly past every timestamp any earlier connection emitted, so a
/// client that transparently reconnects (Wi-Fi blip, proxy idle cut) can
/// keep appending to its existing SourceBuffer instead of tearing the
/// decoder down — the seamless-reconnect contract (SPEC §4.1).
static MSE_FLOOR_TICKS: std::sync::Mutex<u64> = std::sync::Mutex::new(0);

fn seed_mse_clock() -> u64 {
    let mut floor = MSE_FLOOR_TICKS.lock().expect("mse floor lock");
    *floor += 90;
    *floor
}

fn publish_mse_clock(ticks: u64) {
    let mut floor = MSE_FLOOR_TICKS.lock().expect("mse floor lock");
    if ticks > *floor {
        *floor = ticks;
    }
}

/// Stamps one connection's frames on the shared media clock.
struct MseTimeline {
    prev: Option<Instant>,
    clock: u64,
}

impl MseTimeline {
    fn new() -> Self {
        Self {
            prev: None,
            clock: seed_mse_clock(),
        }
    }

    /// Returns this frame's timestamp (90 kHz ticks) and its duration.
    /// Wall-clock interval → ticks keeps the timeline gapless and
    /// true-speed regardless of sensor fps; long stalls clamp to 200 ms so
    /// server-side realignment skips stay seamless for the decoder.
    fn next(&mut self, now: Instant) -> (u64, u32) {
        let ticks = match self.prev {
            Some(prev) => {
                let us = now.duration_since(prev).as_micros() as u64 * 90 / 1_000;
                us.clamp(90, 18_000)
            }
            None => 6_000,
        };
        self.prev = Some(now);
        self.clock += ticks;
        publish_mse_clock(self.clock);
        (self.clock, ticks as u32)
    }
}

/// Decides whether a drained frame may still be serialized after loss.
///
/// `skipped` counts frames dropped by the drain-to-latest bridge (their
/// references are gone) and `dropped` is the hub's per-subscriber drop
/// counter. After either, only a key frame passes until one realigns.
struct RealignGate {
    need_key: bool,
    seen_drops: u64,
}

impl RealignGate {
    fn new(seen_drops: u64) -> Self {
        Self {
            need_key: false,
            seen_drops,
        }
    }

    fn allow(&mut self, latest_is_key: bool, skipped: usize, dropped: u64) -> bool {
        if dropped != self.seen_drops {
            self.seen_drops = dropped;
            self.need_key = true;
        }
        if skipped > 0 && !latest_is_key {
            self.need_key = true;
        }
        if self.need_key && !latest_is_key {
            return false;
        }
        self.need_key = false;
        true
    }
}

/// Axum handler for `GET /api/cameras/{id}/stream.mse`.
pub async fn stream_mse_handler(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Response {
    if id != "0" {
        return (StatusCode::NOT_FOUND, "no such camera").into_response();
    }
    let Some(au_hub) = state.au_hub.as_ref().map(Arc::clone) else {
        return (StatusCode::SERVICE_UNAVAILABLE, "streaming not available").into_response();
    };
    // The fMP4 init segment's track dims drive the browser's
    // videoWidth/videoHeight — use the post-rotation effective
    // resolution (SPEC appendix A #19), not a hardcoded 720p box.
    let (init_w, init_h) = state.config.read().await.camera.effective_dims();
    mse_response(au_hub, init_w, init_h).await
}

/// Axum handler for `GET /api/cameras/{id}/stream.sub.mse` — the
/// bandwidth-saving low-resolution substream (SPEC appendix A #20).
/// 404 unless the substream pipeline is actually running
/// (`capabilities.substream`).
pub async fn stream_sub_mse_handler(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Response {
    if id != "0" {
        return (StatusCode::NOT_FOUND, "no such camera").into_response();
    }
    let Some(sub_hub) = state.sub_au_hub.as_ref().map(Arc::clone) else {
        return (StatusCode::NOT_FOUND, "substream not enabled").into_response();
    };
    let cam = state.config.read().await.camera.clone();
    let (init_w, init_h) = (cam.substream.width, cam.substream.height);
    mse_response(sub_hub, init_w, init_h).await
}

/// Shared chunked-fMP4 body for both MSE endpoints.
async fn mse_response(au_hub: Arc<crate::h264::hub::AuHub>, init_w: u32, init_h: u32) -> Response {
    // Subscribe with tiny buffer (2) — old frames get dropped by AuHub.
    let subscriber = au_hub.subscribe_with_capacity(2);
    let sub_id = subscriber.id;
    let dropped = subscriber.dropped_counter();
    let rx = subscriber.receiver;

    // Bridge sync mpsc → tokio mpsc with drain-to-latest (capacity 1: the
    // bridge only ever holds the most recent frame).
    let (atx, mut arx) = tokio::sync::mpsc::channel::<AccessUnit>(1);
    tokio::task::spawn_blocking(move || {
        // A skipped or dropped unit leaves a reference-frame hole; feeding
        // the decoder the following non-key frames freezes the picture. The
        // gate blocks everything but key frames until one realigns.
        let mut gate = RealignGate::new(dropped.load(Ordering::Relaxed));
        while let Ok(au) = rx.recv() {
            let dropped_now = dropped.load(Ordering::Relaxed);
            let mut latest = au;
            let mut skipped = 0usize;
            while let Ok(extra) = rx.try_recv() {
                skipped += 1;
                latest = extra;
            }
            if !gate.allow(latest.is_key_frame, skipped, dropped_now) {
                continue;
            }
            if atx.blocking_send(latest).is_err() {
                break;
            }
        }
    });

    // Unsubscribe when the response stream is dropped (client disconnect).
    struct Unsub {
        hub: Arc<crate::h264::hub::AuHub>,
        id: usize,
    }
    impl Drop for Unsub {
        fn drop(&mut self) {
            self.hub.unsubscribe(self.id);
        }
    }
    let stream = async_stream::stream! {
        // Unsubscribes when the response stream is dropped (client
        // disconnect) — must live INSIDE the generator: a handler-local
        // would unsubscribe the moment the handler returns.
        let _unsub = Unsub { hub: au_hub, id: sub_id };
        let mut sequence: u32 = 0;
        let mut initialized = false;
        let mut timeline = MseTimeline::new();

        while let Some(au) = arx.recv().await {
            if !initialized {
                // Start every subscriber on a key frame so the init segment's
                // SPS/PPS describe the stream actually about to be sent.
                if !au.is_key_frame {
                    continue;
                }
                let (sps, pps) = extract_sps_pps(&au);
                match (sps, pps) {
                    (Some(s), Some(p)) => {
                        let init = fmp4::build_init_segment(&s, &p, init_w, init_h);
                        initialized = true;
                        yield Ok::<_, std::convert::Infallible>(init);
                    }
                    _ => continue,
                }
            }

            let nalus: Vec<Vec<u8>> = au.nalus.iter().map(|n| n.data.clone()).collect();
            // Wall-clock interval → 90 kHz ticks keeps the MSE timeline
            // gapless and true-speed regardless of sensor fps; the shared
            // clock hands the timeline to the client's next (re)connection.
            let (timestamp, duration) = timeline.next(Instant::now());
            let seg = fmp4::build_media_segment(&nalus, sequence, timestamp, duration, au.is_key_frame);
            sequence = sequence.wrapping_add(1);
            yield Ok(seg);
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "video/mp4")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn extract_sps_pps(au: &AccessUnit) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let mut sps = None;
    let mut pps = None;
    for nalu in &au.nalus {
        if nalu.is_sps && sps.is_none() {
            sps = Some(nalu.data.clone());
        }
        if nalu.is_pps && pps.is_none() {
            pps = Some(nalu.data.clone());
        }
    }
    (sps, pps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::hub::AuHub;
    use crate::h264::parser::Nalu;
    use axum::Router;
    use futures_util::StreamExt;
    use std::time::Duration;
    use tower::ServiceExt;

    fn nalu(nalu_type: u8, data: Vec<u8>) -> Nalu {
        Nalu {
            nalu_type,
            is_idr: nalu_type == 5,
            is_sps: nalu_type == 7,
            is_pps: nalu_type == 8,
            is_aud: nalu_type == 9,
            data,
        }
    }

    fn au(key: bool) -> AccessUnit {
        let nalus = if key {
            vec![
                nalu(7, vec![0x67, 0x42, 0x00, 0x1e]),
                nalu(8, vec![0x68, 0xce, 0x38, 0x80]),
                nalu(5, vec![0x65, 0x88, 0x84, 0x00]),
            ]
        } else {
            vec![nalu(1, vec![0x41, 0x9a, 0x02])]
        };
        AccessUnit {
            nalus,
            timestamp: Instant::now(),
            is_key_frame: key,
        }
    }

    #[test]
    fn test_realign_gate_no_loss_passes_all() {
        let mut g = RealignGate::new(0);
        assert!(g.allow(true, 0, 0));
        assert!(g.allow(false, 0, 0));
        assert!(g.allow(false, 0, 0));
    }

    // Seamless-reconnect contract (SPEC §4.1): the fMP4 timeline is handed
    // from one HTTP connection to the next, so a client that transparently
    // refetches keeps appending to its existing SourceBuffer.
    #[test]
    fn test_mse_timeline_handoff_across_connections() {
        let base = Instant::now();
        let mut c1 = MseTimeline::new();
        let mut last = 0;
        for i in 0..5 {
            let (ts, _) = c1.next(base + Duration::from_millis(i * 66));
            if i > 0 {
                assert!(ts > last, "connection 1 timestamps must increase");
            }
            last = ts;
        }
        let mut c2 = MseTimeline::new();
        let (first2, _) = c2.next(base + Duration::from_secs(10));
        assert!(
            first2 > last,
            "second connection must seed past first's last timestamp: {first2} <= {last}"
        );
    }

    #[test]
    fn test_mse_timeline_clamps_stall() {
        let base = Instant::now();
        let mut tl = MseTimeline::new();
        tl.next(base);
        let (_, d) = tl.next(base + Duration::from_secs(5));
        assert_eq!(d, 18_000, "long stall must clamp to 200ms of media time");
        let (_, d) = tl.next(base + Duration::from_millis(5_066));
        assert!(d >= 90, "normal frame duration must be ≥90 ticks");
    }

    /// Extract the 64-bit baseMediaDecodeTime from the first tfdt (v1) box.
    fn parse_tfdt(segment: &[u8]) -> Option<u64> {
        let pos = segment.windows(4).position(|w| w == b"tfdt")?;
        let v = segment.get(pos + 8..pos + 16)?;
        let mut b = [0u8; 8];
        b.copy_from_slice(v);
        Some(u64::from_be_bytes(b))
    }

    #[tokio::test]
    async fn test_stream_mse_timeline_continues_across_connections() {
        let hub = Arc::new(AuHub::new());
        let st = Arc::new(AppState {
            au_hub: Some(hub.clone()),
            ..AppState::default()
        });
        let app = Router::new()
            .route(
                "/api/cameras/:id/stream.mse",
                axum::routing::get(stream_mse_handler),
            )
            .with_state(st);

        let open = || {
            app.clone().oneshot(
                axum::http::Request::builder()
                    .uri("/api/cameras/0/stream.mse")
                    .body(Body::empty())
                    .unwrap(),
            )
        };
        let mut round_tfdts: Vec<(u64, u64)> = Vec::new(); // (first, last) per connection
        for _round in 0..2 {
            let resp = open().await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let mut body = resp.into_body().into_data_stream();
            // Prime: key frame initializes, then normal frames flow. The
            // drain-to-latest bridge collapses back-to-back writes, so pace
            // each AU apart.
            for key in [true, false, false] {
                tokio::time::sleep(Duration::from_millis(80)).await;
                hub.write(au(key));
            }
            let mut first: Option<u64> = None;
            let mut last = 0u64;
            for _ in 0..4 {
                let chunk = tokio::time::timeout(Duration::from_secs(3), body.next())
                    .await
                    .expect("segment timeout")
                    .expect("stream ended")
                    .expect("stream error");
                if !chunk.windows(4).any(|w| w == b"moof") {
                    continue; // init segment
                }
                let ts = parse_tfdt(&chunk).expect("moof must carry a tfdt v1 box");
                first.get_or_insert(ts);
                last = last.max(ts);
            }
            let first = first.expect("at least one media segment per connection");
            assert!(last >= first);
            round_tfdts.push((first, last));
            // Body drops here = client disconnect before the next connection.
        }
        let (first0, last0) = round_tfdts[0];
        let (first1, last1) = round_tfdts[1];
        assert!(last1 >= first1);
        assert!(
            first1 > last0,
            "second connection must continue past first's timeline: {first1} <= {last0} (first0={first0})"
        );
    }

    #[test]
    fn test_realign_gate_hub_drop_blocks_until_key() {
        let mut g = RealignGate::new(2);
        assert!(g.allow(true, 0, 2), "no loss yet");
        assert!(!g.allow(false, 0, 3), "drop detected: non-key blocked");
        assert!(!g.allow(false, 0, 3), "stays blocked");
        assert!(g.allow(true, 0, 3), "key realigns");
        assert!(g.allow(false, 0, 3), "normal flow resumes");
    }

    #[test]
    fn test_realign_gate_skipped_drain() {
        let mut g = RealignGate::new(0);
        // Drain landing on a key frame is a clean jump.
        assert!(g.allow(true, 3, 0));
        // Drain landing on a non-key frame leaves a hole.
        assert!(!g.allow(false, 2, 0), "skipped backlog on non-key blocked");
        assert!(!g.allow(false, 0, 0), "stays blocked until key");
        assert!(g.allow(true, 0, 0), "key realigns");
        assert!(g.allow(false, 0, 0));
    }

    #[test]
    fn test_realign_gate_second_drop_while_waiting() {
        let mut g = RealignGate::new(0);
        assert!(!g.allow(false, 0, 1));
        assert!(!g.allow(false, 0, 2), "second drop keeps the gate shut");
        assert!(g.allow(true, 0, 2));
    }

    #[tokio::test]
    async fn test_stream_mse_yields_init_then_media_segments() {
        let hub = Arc::new(AuHub::new());
        let st = Arc::new(AppState {
            au_hub: Some(hub.clone()),
            ..AppState::default()
        });
        let app = Router::new()
            .route(
                "/api/cameras/:id/stream.mse",
                axum::routing::get(stream_mse_handler),
            )
            .with_state(st);

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/cameras/0/stream.mse")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("content-type").unwrap(), "video/mp4");

        let mut body = resp.into_body().into_data_stream();
        // Non-key frame first: swallowed until a key frame initializes.
        tokio::time::sleep(Duration::from_millis(100)).await;
        hub.write(au(false));
        hub.write(au(true));

        let init = tokio::time::timeout(Duration::from_secs(3), body.next())
            .await
            .expect("init segment timeout")
            .expect("stream ended")
            .expect("stream error");
        assert!(
            init.windows(4).any(|w| w == b"ftyp"),
            "init segment must start with an ftyp box"
        );

        let media = tokio::time::timeout(Duration::from_secs(3), body.next())
            .await
            .expect("media segment timeout")
            .expect("stream ended")
            .expect("stream error");
        assert!(
            media.windows(4).any(|w| w == b"moof"),
            "media segment must be a moof-based fragment"
        );
    }

    #[tokio::test]
    async fn test_stream_mse_unknown_camera_is_404() {
        let st = Arc::new(AppState::default());
        let app = Router::new()
            .route(
                "/api/cameras/:id/stream.mse",
                axum::routing::get(stream_mse_handler),
            )
            .with_state(st);
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/cameras/9/stream.mse")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_stream_sub_mse_404_when_not_enabled() {
        // Even with a main hub present, the sub endpoint must 404 unless
        // the substream pipeline wired its own hub (SPEC appendix A #20).
        let st = Arc::new(AppState {
            au_hub: Some(Arc::new(AuHub::new())),
            ..AppState::default()
        });
        let app = Router::new()
            .route(
                "/api/cameras/:id/stream.sub.mse",
                axum::routing::get(stream_sub_mse_handler),
            )
            .with_state(st);
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/cameras/0/stream.sub.mse")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_stream_sub_mse_serves_sub_hub_frames() {
        let hub = Arc::new(AuHub::new());
        let st = Arc::new(AppState {
            sub_au_hub: Some(hub.clone()),
            ..AppState::default()
        });
        let app = Router::new()
            .route(
                "/api/cameras/:id/stream.sub.mse",
                axum::routing::get(stream_sub_mse_handler),
            )
            .with_state(st);

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/cameras/0/stream.sub.mse")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("content-type").unwrap(), "video/mp4");

        let mut body = resp.into_body().into_data_stream();
        tokio::time::sleep(Duration::from_millis(100)).await;
        hub.write(au(true));
        let init = tokio::time::timeout(Duration::from_secs(3), body.next())
            .await
            .expect("init segment timeout")
            .expect("stream ended")
            .expect("stream error");
        assert!(init.windows(4).any(|w| w == b"ftyp"));
    }

    #[tokio::test]
    async fn test_stream_mse_without_hub_is_503() {
        let st = Arc::new(AppState::default());
        let app = Router::new()
            .route(
                "/api/cameras/:id/stream.mse",
                axum::routing::get(stream_mse_handler),
            )
            .with_state(st);
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/cameras/0/stream.mse")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
