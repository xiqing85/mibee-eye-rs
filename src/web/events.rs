//! SSE event channel (SPEC v1 §6): `GET /api/events`.
//!
//! Hub-based server push: connected clients receive named events
//! (`event: <type>\ndata: <json>\n\n`) announced in `capabilities.events`.
//! Each subscriber gets an unbounded channel; slow clients are dropped from
//! the hub when their channel closes (the HTTP layer applies backpressure).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use super::api::AppState;

/// SSE keep-alive comment interval (SPEC §6: 15 s).
const KEEPALIVE: Duration = Duration::from_secs(15);

type ClientId = u64;
type ClientSender = tokio::sync::mpsc::UnboundedSender<Vec<u8>>;

/// Broadcast hub for connected SSE clients.
#[derive(Clone, Debug, Default)]
pub struct EventHub {
    inner: Arc<Mutex<HubInner>>,
}

#[derive(Debug, Default)]
struct HubInner {
    next_id: ClientId,
    clients: HashMap<ClientId, ClientSender>,
}

impl EventHub {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Broadcast one event to every connected client (SPEC §6 frame format).
    pub fn broadcast(&self, event: &str, payload: &serde_json::Value) {
        let frame = format!("event: {event}\ndata: {payload}\n\n").into_bytes();
        let mut inner = self.inner.lock().expect("EventHub lock");
        let mut dead = Vec::new();
        for (id, tx) in &inner.clients {
            if tx.send(frame.clone()).is_err() {
                dead.push(*id);
            }
        }
        for id in dead {
            inner.clients.remove(&id);
        }
    }

    fn add(&self, tx: ClientSender) -> ClientId {
        let mut inner = self.inner.lock().expect("EventHub lock");
        let id = inner.next_id;
        inner.next_id += 1;
        inner.clients.insert(id, tx);
        id
    }

    fn remove(&self, id: ClientId) {
        self.inner
            .lock()
            .expect("EventHub lock")
            .clients
            .remove(&id);
    }

    #[must_use]
    pub fn client_count(&self) -> usize {
        self.inner.lock().expect("EventHub lock").clients.len()
    }
}

static GLOBAL_HUB: OnceLock<EventHub> = OnceLock::new();

/// The application-wide SSE hub.
pub fn global_hub() -> &'static EventHub {
    GLOBAL_HUB.get_or_init(EventHub::new)
}

/// `GET /api/events` — the SSE stream. The session cookie (sent
/// automatically by `EventSource`) authenticates the subscription via the
/// auth gate.
pub async fn events_handler(State(_state): State<Arc<AppState>>) -> Response {
    let hub = global_hub().clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let client_id = hub.add(tx);

    // Unsubscribe when the response stream is dropped (client disconnect).
    // The guard must live INSIDE the generator: a handler-local would remove
    // the sender the moment the handler returns, ending the stream instantly.
    struct Unsub {
        hub: EventHub,
        id: ClientId,
    }
    impl Drop for Unsub {
        fn drop(&mut self) {
            self.hub.remove(self.id);
        }
    }

    let stream = async_stream::stream! {
        let _unsub = Unsub {
            hub: hub.clone(),
            id: client_id,
        };
        // Per the SSE spec a leading retry hint lets browsers reconnect fast.
        yield Ok::<_, std::convert::Infallible>(b"retry: 3000\n\n".to_vec());
        loop {
            tokio::select! {
                frame = rx.recv() => {
                    match frame {
                        Some(bytes) => yield Ok::<_, std::convert::Infallible>(bytes),
                        None => break,
                    }
                }
                _ = tokio::time::sleep(KEEPALIVE) => {
                    yield Ok::<_, std::convert::Infallible>(b": keepalive\n\n".to_vec());
                }
            }
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn broadcast_reaches_subscribed_clients() {
        let hub = EventHub::new();
        let (tx1, mut rx1) = tokio::sync::mpsc::unbounded_channel();
        let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
        hub.add(tx1);
        hub.add(tx2);
        assert_eq!(hub.client_count(), 2);

        hub.broadcast(
            "ai_detection",
            &serde_json::json!({"detections": [], "frame_number": 7}),
        );

        for rx in [&mut rx1, &mut rx2] {
            let frame = rx.recv().await.expect("client must receive the event");
            let text = String::from_utf8(frame).unwrap();
            assert_eq!(
                text,
                "event: ai_detection\ndata: {\"detections\":[],\"frame_number\":7}\n\n"
            );
        }
    }

    #[tokio::test]
    async fn broadcast_prunes_disconnected_clients() {
        let hub = EventHub::new();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        hub.add(tx);
        assert_eq!(hub.client_count(), 1);
        drop(rx);

        hub.broadcast("recording", &serde_json::json!({"active": true}));
        assert_eq!(hub.client_count(), 0, "dead client must be pruned");
    }

    #[test]
    fn global_hub_is_shared() {
        assert_eq!(
            global_hub() as *const EventHub,
            global_hub() as *const EventHub
        );
    }
}
