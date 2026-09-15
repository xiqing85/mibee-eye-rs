//! AI detections → GB/T 28181 alarm NOTIFY (§9.5 / A.2.5).
//!
//! The camera's only alarm source is the AI moving-target detector
//! (NanoDet). Per the 2022 standard's value tables an analytics alarm is
//! `AlarmMethod=5` (视频报警) with `AlarmType=2` (运动目标检测报警);
//! priority is 4 (四级警情 — informational analytics, not a physical
//! sensor). NOTIFYs fire on the RISING edge of "targets present" with a
//! cooldown, so a busy scene cannot storm the platform, and the
//! platform's `DeviceConfig(AlarmReport)` switch (A.2.3.2.10) gates
//! motion reporting at runtime. Everything is a no-op until a platform
//! actually SUBSCRIBEs — the library's notifier handles that.

use std::sync::{
    atomic::{AtomicBool, AtomicI64, Ordering},
    Arc, RwLock,
};
use std::time::Duration;

use crate::gb28181::DeviceNotifier;

/// Anti-storm default: minimum spacing between alarm NOTIFYs.
pub const DEFAULT_ALARM_COOLDOWN_SECS: u64 = 30;

/// Bridges AI detection batches into alarm NOTIFYs on the live GB28181
/// notifier. Shared between the AI event-bus subscriber (feeds it), the
/// server lifecycle (updates the notifier slot per restart) and the
/// DeviceConfig AlarmReport switch (runtime gate).
pub struct AlarmBridge {
    notifier: RwLock<Option<Arc<DeviceNotifier>>>,
    /// Runtime gate from `DeviceConfig(AlarmReport)` MotionDetection
    /// (0 off, 1 on). Boot default comes from config.
    motion_reporting: AtomicBool,
    prev_target: AtomicBool,
    /// Epoch-ms of the last accepted alarm; negative = never.
    last_sent_ms: AtomicI64,
    cooldown_ms: u64,
}

impl AlarmBridge {
    /// `enabled` is the boot default of the AlarmReport gate.
    #[must_use]
    pub fn new(enabled: bool, cooldown: Duration) -> Self {
        Self {
            notifier: RwLock::new(None),
            motion_reporting: AtomicBool::new(enabled),
            prev_target: AtomicBool::new(false),
            last_sent_ms: AtomicI64::new(-1),
            cooldown_ms: cooldown.as_millis() as u64,
        }
    }

    /// The server task owns the notifier; the retry loop hands each new
    /// instance in (and `None` would detach — kept for symmetry).
    pub fn update_notifier(&self, notifier: Option<Arc<DeviceNotifier>>) {
        *self.notifier.write().expect("alarm notifier lock") = notifier;
    }

    /// `DeviceConfig(AlarmReport)` MotionDetection switch (0 off, 1 on).
    pub fn set_motion_reporting(&self, on: bool) {
        self.motion_reporting.store(on, Ordering::Relaxed);
    }

    /// Feed one detection batch (epoch-ms clock for testability).
    /// Returns whether an alarm NOTIFY went out. A decided-but-unsent
    /// alarm (server down / nobody subscribed) is not retried — the next
    /// one rides the next rising edge.
    pub fn on_detections(&self, now_ms: u64, target_count: usize) -> bool {
        if !self.take_edge(now_ms, target_count > 0) {
            return false;
        }
        let Some(notifier) = self.notifier.read().expect("alarm notifier lock").clone() else {
            return false;
        };
        let (priority, method, alarm_type, time, desc) = build_alarm(now_ms, target_count);
        notifier.send_alarm(priority, method, &time, alarm_type, &desc)
    }

    /// Rising-edge + gate + cooldown state machine; books `last_sent_ms`
    /// when it accepts an edge.
    fn take_edge(&self, now_ms: u64, has_target: bool) -> bool {
        let prev = self.prev_target.swap(has_target, Ordering::Relaxed);
        if !(has_target && !prev) {
            return false;
        }
        if !self.motion_reporting.load(Ordering::Relaxed) {
            return false;
        }
        let last = self.last_sent_ms.load(Ordering::Relaxed);
        if last >= 0 && now_ms.saturating_sub(last as u64) < self.cooldown_ms {
            return false;
        }
        self.last_sent_ms.store(now_ms as i64, Ordering::Relaxed);
        true
    }
}

/// Standard-pinned alarm field values (2022 value tables: method 5 视频
/// 报警 → type 2 运动目标检测报警; priority 4 四级警情).
fn build_alarm(
    now_ms: u64,
    target_count: usize,
) -> (&'static str, &'static str, &'static str, String, String) {
    let time = crate::gb28181::client::format_gb_time_ms(now_ms);
    let desc = format!("AI moving-target detection: {target_count} target(s)");
    ("4", "5", "2", time, desc)
}

/// `DeviceConfig(AlarmReport)` host seam: the MotionDetection switch
/// gates AI alarm NOTIFYs at runtime (FieldDetection has no source on
/// this camera — no AI region events).
pub struct AlarmReportGate(pub Arc<AlarmBridge>);

impl crate::gb28181::server::DeviceConfigHandler for AlarmReportGate {
    fn on_alarm_report(&self, motion_detection: u32, _field_detection: u32) {
        self.0.set_motion_reporting(motion_detection == 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rising_edge_only_is_accepted() {
        let b = AlarmBridge::new(true, Duration::from_secs(30));
        assert!(b.take_edge(1_000, true), "first appearance");
        assert!(!b.take_edge(2_000, true), "sustained presence");
        assert!(!b.take_edge(3_000, false), "falling edge never alarms");
        // Re-appearance inside the 30s cooldown window is suppressed.
        assert!(!b.take_edge(4_000, true), "re-appearance within cooldown");
        assert!(!b.take_edge(35_000, false));
        assert!(b.take_edge(36_000, true), "re-appearance after cooldown");
    }

    #[test]
    fn cooldown_suppresses_rapid_re_rise() {
        let b = AlarmBridge::new(true, Duration::from_secs(30));
        assert!(b.take_edge(10_000, true));
        assert!(!b.take_edge(11_000, false), "target lost (never alarms)");
        // Rising again 1s after the last alarm: inside the window.
        assert!(!b.take_edge(12_000, true));
        // Still inside at +29.999s.
        assert!(!b.take_edge(30_000, false));
        assert!(!b.take_edge(39_999, true));
        // Outside at +30s (falling first so the next true is a rise).
        assert!(!b.take_edge(40_000, false));
        assert!(b.take_edge(40_000, true));
    }

    #[test]
    fn motion_gate_blocks_and_reenables() {
        let b = AlarmBridge::new(false, Duration::from_secs(30));
        assert!(!b.take_edge(1_000, true), "gate off at boot");
        b.set_motion_reporting(true);
        assert!(!b.take_edge(2_000, true), "prev already true — no edge");
        assert!(!b.take_edge(3_000, false));
        assert!(b.take_edge(4_000, true), "gate on: edge accepted");
        b.set_motion_reporting(false);
        assert!(!b.take_edge(5_000, false));
        assert!(!b.take_edge(6_000, true), "gate off again");
    }

    #[test]
    fn first_alarm_has_no_cooldown_floor() {
        // last_sent starts negative — the very first edge must pass.
        let b = AlarmBridge::new(true, Duration::from_secs(30));
        assert!(b.take_edge(0, true));
    }

    #[test]
    fn on_detections_without_notifier_is_a_safe_skip() {
        let b = AlarmBridge::new(true, Duration::from_secs(30));
        assert!(!b.on_detections(1_000, 3));
        assert!(!b.on_detections(2_000, 0));
    }

    #[test]
    fn alarm_values_are_standard_pinned() {
        let (priority, method, alarm_type, time, desc) = build_alarm(1_000, 2);
        assert_eq!(priority, "4"); // 四级警情
        assert_eq!(method, "5"); // 视频报警
        assert_eq!(alarm_type, "2"); // 运动目标检测报警
        assert!(time.contains('T'), "GB time format: {time}");
        assert!(desc.contains("2 target"), "{desc}");
    }
}
