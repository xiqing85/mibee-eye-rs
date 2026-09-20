//! AI detections → ONVIF Pull-Point MotionAlarm events.
//!
//! The same accepted rising edge that fans out to the GB/T 28181 alarm
//! NOTIFY and the SPEC v1 §6 `alarm` SSE event also feeds the ONVIF
//! events service (onvif-device-rs 0.7): NVRs that manage this camera
//! over ONVIF can `CreatePullPointSubscription` on
//! `tns1:VideoSource/MotionAlarm` instead of (or besides) the GB alarm
//! channel. Nothing is delivered until a client actually subscribes —
//! `EventsService::publish_event` is a no-op without live pull-points.

use crate::onvif::events::{Event, SimpleItem};

/// The MotionAlarm property event for one accepted AI rising edge.
///
/// `Source` is the SPEC single-camera id (`"0"`); `State=true` marks the
/// alarm rise (the bridge is rising-edge-only, mirroring the GB NOTIFY);
/// `Targets` carries the detection count that crossed the edge.
#[must_use]
pub fn motion_alarm_event(targets: usize) -> Event {
    Event {
        topic: "tns1:VideoSource/MotionAlarm".to_string(),
        source: vec![SimpleItem::new("Source", "0")],
        data: vec![
            SimpleItem::new("State", "true"),
            SimpleItem::new("Targets", &targets.to_string()),
        ],
        ..Event::new("tns1:VideoSource/MotionAlarm")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn motion_alarm_event_shape() {
        let ev = motion_alarm_event(3);
        assert_eq!(ev.topic, "tns1:VideoSource/MotionAlarm");
        assert_eq!(ev.property_operation, "", "defaults to Changed at publish");
        let source = &ev.source;
        assert_eq!(source.len(), 1);
        assert_eq!(
            (source[0].name.as_str(), source[0].value.as_str()),
            ("Source", "0")
        );
        let data = &ev.data;
        assert_eq!(data.len(), 2);
        assert_eq!(
            (data[0].name.as_str(), data[0].value.as_str()),
            ("State", "true")
        );
        assert_eq!(
            (data[1].name.as_str(), data[1].value.as_str()),
            ("Targets", "3")
        );
        assert!(ev.key.is_empty());
    }
}
