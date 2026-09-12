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
        let mut prev_frame_time: Option<Instant> = None;
        let mut media_clock: u64 = 0;

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
                        let init = fmp4::build_init_segment(&s, &p, 1280, 720);
                        initialized = true;
                        yield Ok::<_, std::convert::Infallible>(init);
                    }
                    _ => continue,
                }
            }

            let nalus: Vec<Vec<u8>> = au.nalus.iter().map(|n| n.data.clone()).collect();
            // Wall-clock interval → 90 kHz ticks keeps the MSE timeline
            // gapless and true-speed regardless of sensor fps.
            let now = Instant::now();
            let duration = match prev_frame_time {
                Some(prev) => {
                    let us = now.duration_since(prev).as_micros() as u64;
                    (us * 90 / 1_000).clamp(90, 18_000) as u32
                }
                None => 6000,
            };
            prev_frame_time = Some(now);
            let timestamp = media_clock;
            media_clock += duration as u64;
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
