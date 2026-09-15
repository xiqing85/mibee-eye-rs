pub mod ai;
pub mod camera;
pub mod config;
pub mod features;
pub mod gb28181_alarm;
pub mod gb28181_snapshot;
pub mod gb35114_glue;
// GB/T 28181 device stack lives in the `gb28181-rs` crate; re-exported under
// the historical module path so `crate::gb28181::…` references keep working.
pub use gb28181_rs as gb28181;
pub mod h264;
pub mod hardware;
pub mod motion;
// ONVIF Device stack lives in the `onvif-rs` crate; re-exported under the
// historical module path so `crate::onvif::…` references keep working.
pub use onvif_device_rs as onvif;
pub mod pipeline;
// Virtual-PTZ state machine lives in the `onvif-rs` crate.
pub mod ptz {
    pub use onvif_device_rs::ptz_state as state;
}
pub mod recording;
pub mod storage;
pub mod streaming;
pub mod watermark;
pub mod web;
