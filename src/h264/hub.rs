//! H.264 Access Unit hub with multi-subscriber fan-out and backpressure.
//!
//! Distributes incoming access units to all registered subscribers using
//! bounded channels. Drops units for slow subscribers and tracks drops.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::parser::Nalu;

/// A complete H.264 access unit (one or more NALUs forming a frame).
#[derive(Debug, Clone)]
pub struct AccessUnit {
    /// NAL units belonging to this access unit.
    pub nalus: Vec<Nalu>,
    /// Capture or presentation timestamp.
    pub timestamp: Instant,
    /// True if this access unit contains an IDR slice (key frame).
    pub is_key_frame: bool,
}

/// A registered subscriber that receives access units via a channel.
#[derive(Debug)]
pub struct Subscriber {
    /// Unique subscriber identifier.
    pub id: usize,
    /// AUs dropped for this subscriber because its channel buffer was full.
    /// Stream serializers (fMP4/MSE) watch this: a dropped unit leaves a
    /// reference-frame hole and only the next key frame realigns decoders.
    dropped: Arc<AtomicU64>,
    /// Receiver end of the bounded channel for access units.
    pub receiver: mpsc::Receiver<AccessUnit>,
}

/// Per-subscriber fan-out slot: bounded-channel sender plus that
/// subscriber's drop counter.
type SubscriberSlot = (mpsc::SyncSender<AccessUnit>, Arc<AtomicU64>);

/// Fans out access units to multiple subscribers with backpressure.
///
/// Thread-safe: `write`, `subscribe`, `unsubscribe`, and query methods
/// can be called concurrently from multiple threads.
pub struct AuHub {
    /// Map of subscriber ID → fan-out slot (bounded channel + drop counter).
    subscribers: Mutex<HashMap<usize, SubscriberSlot>>,
    /// Monotonically increasing ID counter for subscriber registration.
    next_id: AtomicUsize,
    /// Total number of access units dropped across all subscribers
    /// due to full channel buffers.
    dropped_aus: AtomicU64,
}

impl Subscriber {
    /// AUs dropped for this subscriber because its channel buffer was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Shared handle to the per-subscriber drop counter, for consumers that
    /// move the receiver into another thread (e.g. the MSE bridge task).
    pub fn dropped_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.dropped)
    }
}

impl AuHub {
    /// Creates a new access-unit fan-out hub.
    pub fn new() -> Self {
        AuHub {
            subscribers: Mutex::new(HashMap::new()),
            next_id: AtomicUsize::new(0),
            dropped_aus: AtomicU64::new(0),
        }
    }

    /// Writes an access unit to all subscribers.
    ///
    /// Non-blocking: drops the unit for any subscriber whose channel buffer
    /// is full and increments the (global and per-subscriber) drop counters.
    pub fn write(&self, au: AccessUnit) {
        let guard = self.subscribers.lock().unwrap();
        let mut dropped = 0u64;
        for (sender, counter) in guard.values() {
            if sender.try_send(au.clone()).is_err() {
                counter.fetch_add(1, Ordering::Relaxed);
                dropped += 1;
            }
        }
        if dropped > 0 {
            self.dropped_aus.fetch_add(dropped, Ordering::Relaxed);
        }
    }

    /// Registers a new subscriber with a bounded channel (buffer = 64).
    ///
    /// Returns a `Subscriber` whose `id` can be used to unsubscribe later.
    pub fn subscribe(&self) -> Subscriber {
        self.subscribe_with_capacity(64)
    }

    /// Registers a subscriber with a custom channel capacity.
    ///
    /// Use a small capacity (2-4) for low-latency consumers.
    pub fn subscribe_with_capacity(&self, capacity: usize) -> Subscriber {
        let (tx, rx) = mpsc::sync_channel(capacity);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let dropped = Arc::new(AtomicU64::new(0));

        let mut guard = self.subscribers.lock().unwrap();
        guard.insert(id, (tx, Arc::clone(&dropped)));

        Subscriber {
            id,
            dropped,
            receiver: rx,
        }
    }

    /// Removes a subscriber by ID and closes its channel.
    pub fn unsubscribe(&self, id: usize) {
        let mut guard = self.subscribers.lock().unwrap();
        guard.remove(&id);
    }

    /// Returns the number of currently registered subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.subscribers.lock().unwrap().len()
    }

    /// Returns the total number of access units dropped due to slow subscribers.
    pub fn dropped_aus(&self) -> u64 {
        self.dropped_aus.load(Ordering::Relaxed)
    }
}

impl Default for AuHub {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a simple (empty) access unit.
    fn make_au(is_key_frame: bool) -> AccessUnit {
        AccessUnit {
            nalus: Vec::new(),
            timestamp: Instant::now(),
            is_key_frame,
        }
    }

    // -----------------------------------------------------------------------
    // subscribe / unsubscribe
    // -----------------------------------------------------------------------

    #[test]
    fn test_subscribe_increases_count() {
        let hub = AuHub::new();
        assert_eq!(hub.subscriber_count(), 0);

        let _sub = hub.subscribe();
        assert_eq!(hub.subscriber_count(), 1);

        let _sub2 = hub.subscribe();
        assert_eq!(hub.subscriber_count(), 2);
    }

    #[test]
    fn test_unsubscribe_decreases_count() {
        let hub = AuHub::new();
        let sub = hub.subscribe();
        assert_eq!(hub.subscriber_count(), 1);

        hub.unsubscribe(sub.id);
        assert_eq!(hub.subscriber_count(), 0);
    }

    #[test]
    fn test_unsubscribe_nonexistent_is_noop() {
        let hub = AuHub::new();
        hub.unsubscribe(999);
        assert_eq!(hub.subscriber_count(), 0);
    }

    #[test]
    fn test_subscribe_ids_are_unique() {
        let hub = AuHub::new();
        let sub1 = hub.subscribe();
        let sub2 = hub.subscribe();
        assert_ne!(sub1.id, sub2.id);
    }

    #[test]
    fn test_subscribe_then_unsubscribe_all() {
        let hub = AuHub::new();
        let subs: Vec<_> = (0..5).map(|_| hub.subscribe()).collect();
        assert_eq!(hub.subscriber_count(), 5);

        for sub in subs {
            hub.unsubscribe(sub.id);
        }
        assert_eq!(hub.subscriber_count(), 0);
    }

    // -----------------------------------------------------------------------
    // fan-out
    // -----------------------------------------------------------------------

    #[test]
    fn test_write_single_subscriber_receives() {
        let hub = AuHub::new();
        let sub = hub.subscribe();

        let au = make_au(true);
        hub.write(au);

        let received = sub.receiver.try_recv();
        assert!(received.is_ok());
        assert!(received.unwrap().is_key_frame);
    }

    #[test]
    fn test_fan_out_to_two_subscribers() {
        let hub = AuHub::new();
        let sub1 = hub.subscribe();
        let sub2 = hub.subscribe();

        let au = make_au(true);
        hub.write(au);

        assert!(sub1.receiver.try_recv().is_ok());
        assert!(sub2.receiver.try_recv().is_ok());
    }

    #[test]
    fn test_fan_out_multiple_aus() {
        let hub = AuHub::new();
        let sub = hub.subscribe();

        for i in 0..10 {
            hub.write(AccessUnit {
                nalus: vec![Nalu {
                    nalu_type: 1,
                    data: vec![i],
                    is_idr: false,
                    is_sps: false,
                    is_pps: false,
                    is_aud: false,
                }],
                timestamp: Instant::now(),
                is_key_frame: i == 0,
            });
        }

        let mut count = 0;
        while let Ok(au) = sub.receiver.try_recv() {
            // First AU should be key frame if i==0
            if count == 0 {
                assert!(au.is_key_frame);
            }
            count += 1;
        }
        assert_eq!(count, 10);
    }

    #[test]
    fn test_write_with_no_subscribers_does_not_panic() {
        let hub = AuHub::new();
        hub.write(make_au(true));
        // Should not panic.
        assert_eq!(hub.dropped_aus(), 0);
    }

    // -----------------------------------------------------------------------
    // backpressure / dropped counter
    // -----------------------------------------------------------------------

    #[test]
    fn test_backpressure_drops_when_channel_full() {
        let hub = AuHub::new();
        let _sub = hub.subscribe();

        // Fill the channel buffer (capacity = 64).
        for _ in 0..64 {
            hub.write(make_au(false));
        }
        assert_eq!(hub.dropped_aus(), 0);

        // The 65th write should be dropped.
        hub.write(make_au(false));
        assert_eq!(hub.dropped_aus(), 1);

        // Subsequent writes continue to drop when buffer stays full.
        hub.write(make_au(false));
        assert_eq!(hub.dropped_aus(), 2);
    }

    #[test]
    fn test_backpressure_consuming_frees_buffer() {
        let hub = AuHub::new();
        let sub = hub.subscribe();

        // Fill buffer.
        for _ in 0..64 {
            hub.write(make_au(false));
        }
        assert_eq!(hub.dropped_aus(), 0);

        // Consume one message from the buffer.
        assert!(sub.receiver.try_recv().is_ok());

        // Now one more can fit without being dropped.
        hub.write(make_au(false));
        assert_eq!(hub.dropped_aus(), 0);
    }

    #[test]
    fn test_dropped_counter_multiple_subscribers() {
        let hub = AuHub::new();
        let sub1 = hub.subscribe();
        let _sub2 = hub.subscribe();

        // Fill both buffers.
        for _ in 0..64 {
            hub.write(make_au(false));
        }
        assert_eq!(hub.dropped_aus(), 0);

        // One more write: drops for both subscribers.
        hub.write(make_au(false));
        assert_eq!(hub.dropped_aus(), 2);

        // Consume from one subscriber only.
        assert!(sub1.receiver.try_recv().is_ok());

        // Now sub1 has capacity but sub2 is still full.
        // Next write should only drop for sub2.
        hub.write(make_au(false));
        assert_eq!(hub.dropped_aus(), 3);
    }

    // -----------------------------------------------------------------------
    // subscriber_count
    // -----------------------------------------------------------------------

    #[test]
    fn test_subscriber_count_starts_at_zero() {
        let hub = AuHub::new();
        assert_eq!(hub.subscriber_count(), 0);
    }

    #[test]
    fn test_subscriber_count_reflects_subscribe_unsubscribe() {
        let hub = AuHub::new();
        let s1 = hub.subscribe();
        let s2 = hub.subscribe();
        let s3 = hub.subscribe();
        assert_eq!(hub.subscriber_count(), 3);

        hub.unsubscribe(s2.id);
        assert_eq!(hub.subscriber_count(), 2);

        hub.unsubscribe(s1.id);
        assert_eq!(hub.subscriber_count(), 1);

        hub.unsubscribe(s3.id);
        assert_eq!(hub.subscriber_count(), 0);
    }

    // -----------------------------------------------------------------------
    // dropped_aus
    // -----------------------------------------------------------------------

    #[test]
    fn test_dropped_aus_starts_at_zero() {
        let hub = AuHub::new();
        assert_eq!(hub.dropped_aus(), 0);
    }

    #[test]
    fn test_dropped_aus_no_subscribers() {
        let hub = AuHub::new();
        hub.write(make_au(true));
        assert_eq!(hub.dropped_aus(), 0);
    }

    #[test]
    fn test_dropped_aus_after_unsubscribe() {
        let hub = AuHub::new();
        let sub = hub.subscribe();
        hub.unsubscribe(sub.id);

        // After unsubscribe, the sender is removed.
        // Writes should go to no subscribers.
        hub.write(make_au(false));
        assert_eq!(hub.dropped_aus(), 0);
    }

    // -----------------------------------------------------------------------
    // concurrent access (basic sanity)
    // -----------------------------------------------------------------------

    #[test]
    fn test_concurrent_write_and_subscribe() {
        use std::thread;

        let hub = std::sync::Arc::new(AuHub::new());
        let mut handles = Vec::new();

        // Spawn readers that subscribe, receive a few AUs, then unsubscribe.
        for _ in 0..4 {
            let hub = hub.clone();
            handles.push(thread::spawn(move || {
                let sub = hub.subscribe();
                // The subscriber reads until timeout or gets enough.
                for _ in 0..10 {
                    match sub.receiver.try_recv() {
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                hub.unsubscribe(sub.id);
            }));
        }

        // Writer that sends AUs.
        let hub_w = hub.clone();
        let writer = thread::spawn(move || {
            for _ in 0..50 {
                hub_w.write(make_au(false));
            }
        });
        handles.push(writer);

        for h in handles {
            h.join().unwrap();
        }

        // After all threads finish, subscriber_count should be 0
        // (all unsubscribed), and dropped_aus may be > 0.
        assert_eq!(hub.subscriber_count(), 0);
    }
}

#[cfg(test)]
mod drop_tests {
    use super::*;

    fn key_au() -> AccessUnit {
        AccessUnit {
            nalus: vec![],
            timestamp: Instant::now(),
            is_key_frame: true,
        }
    }

    #[test]
    fn per_subscriber_drop_counter_tracks_full_channel() {
        let hub = AuHub::new();
        let sub = hub.subscribe_with_capacity(2);
        for _ in 0..5 {
            hub.write(key_au());
        }
        assert_eq!(sub.dropped(), 3, "5 written, capacity 2, none read");
        assert_eq!(hub.dropped_aus(), 3);
        // Draining buffered units must not change the drop count.
        assert!(sub.receiver.try_recv().is_ok());
        assert_eq!(sub.dropped(), 3);
        hub.unsubscribe(sub.id);
    }

    #[test]
    fn drop_counter_is_per_subscriber() {
        let hub = AuHub::new();
        let slow = hub.subscribe_with_capacity(4);
        let fast = hub.subscribe_with_capacity(4);
        for _ in 0..6 {
            hub.write(key_au());
            let _ = fast.receiver.try_recv();
        }
        assert_eq!(slow.dropped(), 2);
        assert_eq!(fast.dropped(), 0);
        hub.unsubscribe(slow.id);
        hub.unsubscribe(fast.id);
    }
}
