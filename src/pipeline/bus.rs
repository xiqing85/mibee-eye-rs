use crate::features::ai::Detection;
use crate::pipeline::error::PipelineError;
use crate::pipeline::traits::Frame;
/// Events that flow through the pipeline.
#[derive(Debug, Clone)]
pub enum PipelineEvent {
    /// A new video frame is ready.
    FrameReady(Frame),
    /// A pipeline parameter was changed at runtime.
    ParameterChanged { name: String, value: String },
    /// The pipeline status changed.
    StatusChanged { status: String },
    /// Motion was detected in a frame.
    MotionDetected {
        bbox: (u32, u32, u32, u32),
        score: f64,
    },
    /// Recording state changed.
    RecordingStatus { active: bool },
    /// AI detection results on a frame.
    AiDetection {
        detections: Vec<Detection>,
        frame_number: u64,
    },
}

/// Event bus for broadcasting pipeline events to multiple listeners.
///
/// Wraps [`tokio::sync::broadcast`] for fan-out event distribution.
/// All subscribers receive every published event (subject to channel capacity).
pub struct EventBus {
    sender: tokio::sync::broadcast::Sender<PipelineEvent>,
    /// Kept alive to prevent the broadcast channel from closing
    /// when all explicit subscribers are dropped.
    _zombie: tokio::sync::broadcast::Receiver<PipelineEvent>,
}

impl EventBus {
    /// Create a new event bus with the given channel capacity.
    ///
    /// `capacity` is the max number of events buffered per receiver before
    /// messages are lagged (dropped) for slow consumers.
    pub fn new(capacity: usize) -> Self {
        let (sender, zombie) = tokio::sync::broadcast::channel(capacity);
        Self {
            sender,
            _zombie: zombie,
        }
    }

    /// Publish an event to all subscribers.
    ///
    /// Returns the number of active receivers that received the event,
    /// or a [`PipelineError::Channel`] if the send failed.
    pub fn publish(&self, event: PipelineEvent) -> Result<usize, PipelineError> {
        self.sender
            .send(event)
            .map_err(|e| PipelineError::Channel(e.to_string()))
    }

    /// Subscribe to receive events.
    ///
    /// Returns a receiver that will receive all events published after
    /// subscription creation (no replay of past events).
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<PipelineEvent> {
        self.sender.subscribe()
    }

    /// Returns the number of active subscribers.
    pub fn receiver_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

impl Clone for EventBus {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            _zombie: self.sender.subscribe(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_frame() -> Frame {
        Frame {
            data: vec![0u8; 100],
            timestamp: std::time::Instant::now(),
            is_key_frame: true,
            width: 1920,
            height: 1080,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_publish_subscribe() {
        let bus = EventBus::new(16);

        let event = PipelineEvent::FrameReady(make_frame());
        let mut rx = bus.subscribe();

        let count = bus.publish(event).unwrap();
        // +1 for internal zombie receiver kept to keep channel alive
        assert_eq!(count, 2);

        let received = rx.recv().await.unwrap();
        match received {
            PipelineEvent::FrameReady(f) => {
                assert_eq!(f.width, 1920);
                assert_eq!(f.height, 1080);
            }
            _ => panic!("Expected FrameReady event"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_multiple_subscribers() {
        let bus = EventBus::new(16);

        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();

        let count = bus
            .publish(PipelineEvent::StatusChanged {
                status: "running".to_string(),
            })
            .unwrap();
        // +1 for internal zombie receiver
        assert_eq!(count, 3);

        // Both subscribers receive the event
        assert!(rx1.recv().await.is_ok());
        assert!(rx2.recv().await.is_ok());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_receiver_count() {
        let bus = EventBus::new(16);
        // +1 for internal zombie receiver
        assert_eq!(bus.receiver_count(), 1);

        let _rx1 = bus.subscribe();
        assert_eq!(bus.receiver_count(), 2);

        let _rx2 = bus.subscribe();
        assert_eq!(bus.receiver_count(), 3);

        drop(_rx1);
        // After drop, count may lag until next send
        let _ = bus.publish(PipelineEvent::StatusChanged {
            status: "tick".to_string(),
        });
        assert_eq!(bus.receiver_count(), 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_different_event_types() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();

        bus.publish(PipelineEvent::ParameterChanged {
            name: "bitrate".to_string(),
            value: "2000".to_string(),
        })
        .unwrap();
        match rx.recv().await.unwrap() {
            PipelineEvent::ParameterChanged { name, value } => {
                assert_eq!(name, "bitrate");
                assert_eq!(value, "2000");
            }
            _ => panic!("Expected ParameterChanged"),
        }

        bus.publish(PipelineEvent::MotionDetected {
            bbox: (10, 20, 100, 200),
            score: 0.95,
        })
        .unwrap();
        match rx.recv().await.unwrap() {
            PipelineEvent::MotionDetected { bbox, score } => {
                assert_eq!(bbox, (10, 20, 100, 200));
                assert!((score - 0.95).abs() < 1e-9);
            }
            _ => panic!("Expected MotionDetected"),
        }

        bus.publish(PipelineEvent::RecordingStatus { active: true })
            .unwrap();
        match rx.recv().await.unwrap() {
            PipelineEvent::RecordingStatus { active } => {
                assert!(active);
            }
            _ => panic!("Expected RecordingStatus"),
        }

        bus.publish(PipelineEvent::AiDetection {
            detections: vec![],
            frame_number: 42,
        })
        .unwrap();
        match rx.recv().await.unwrap() {
            PipelineEvent::AiDetection {
                detections,
                frame_number,
            } => {
                assert_eq!(frame_number, 42);
                assert!(detections.is_empty());
            }
            _ => panic!("Expected AiDetection"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_no_subscribers() {
        let bus = EventBus::new(16);
        let count = bus
            .publish(PipelineEvent::StatusChanged {
                status: "idle".to_string(),
            })
            .unwrap();
        // Only the internal zombie receiver
        assert_eq!(count, 1);
    }
}
