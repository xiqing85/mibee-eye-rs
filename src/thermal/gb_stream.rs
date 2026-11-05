//! Thermal H.264 GB channel (dual-spectrum "plan A" from rs#3): renders
//! the latest MLX90640 grid (ironbow + temperature OSD) and encodes it
//! with the in-process openh264 encoder into a dedicated [`AuHub`], which
//! a second GB28181 device instance (independent device ID + SIP port,
//! catalog `PhotoelectricImagingType = 2`) streams to the platform.
//!
//! The encoder thread runs at a fixed cadence off the latest shared grid
//! — a lost sensor freezes the picture on the last known grid (the state
//! keeps the previous `to` when a pass fails) instead of dropping the
//! stream, matching the untrusted-payload contract of SPEC §3.7.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, RateControlMode,
};

use super::mlx90640::PIXELS;
use super::render;
use super::ThermalService;
use crate::h264::hub::{AccessUnit, AuHub};
use crate::h264::parser::Parser;

/// Stream geometry + rate control for the thermal GB channel. NVR agreed
/// on 320×240 @ 2–8 fps single-stream (rs#3 answer 4); the sensor itself
/// lands ~3.2–3.7 fps on a Pi, so 4 fps repeats a frame occasionally —
/// harmless and keeps the RTP clock steady.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GbStreamParams {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    /// IDR interval in frames (12 @ 4 fps ≈ one key frame every 3 s).
    pub i_period: u32,
}

impl Default for GbStreamParams {
    fn default() -> Self {
        Self {
            width: 320,
            height: 240,
            fps: 4,
            bitrate_bps: 300_000,
            i_period: 12,
        }
    }
}

/// Derive the second GB device instance's library config from the main
/// `[gb28181]` section plus the `[gb28181_thermal]` overrides: platform
/// address, SIP domain and register/heartbeat cadence carry over; device
/// identity, SIP port and the thermal catalog marking differ. The name
/// suffix keeps the two entries distinguishable in the platform's tree.
pub fn thermal_lib_config(
    base: &crate::config::Gb28181Config,
    t: &crate::config::Gb28181ThermalConfig,
    device_name: &str,
) -> crate::gb28181::config::Gb28181Config {
    let mut lib = base.lib.clone();
    lib.enabled = true;
    lib.device_id = t.device_id.clone();
    lib.channel_id = t.channel_id.clone();
    lib.local_sip_port = t.local_sip_port;
    if !t.password.is_empty() {
        lib.password = t.password.clone();
    }
    lib.device_name = Some(format!("{device_name} [thermal]"));
    // 光电成像类型 2 = 热成像 (GB/T 28181-2022): the whole point of the
    // second entry — platforms auto-classify it as the thermal lens.
    lib.photoelectric_imaging_type = Some(2);
    // The thermal instance has no playback and no sibling channels.
    lib.extra_channels.clear();
    lib
}

/// Spawn the render→encode→hub thread. The shared IDR flag wires
/// `DeviceControl IFrameCmd Send` from the thermal GB instance (the
/// platform forces a key frame on pull start / loss recovery).
pub fn spawn_encoder(
    svc: Arc<ThermalService>,
    params: GbStreamParams,
    hub: Arc<AuHub>,
    idr_flag: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("thermal-gb-encoder".into())
        .spawn(move || encode_loop(svc, params, hub, idr_flag))
        .expect("spawn thermal-gb-encoder")
}

fn encode_loop(
    svc: Arc<ThermalService>,
    params: GbStreamParams,
    hub: Arc<AuHub>,
    idr_flag: Arc<AtomicBool>,
) {
    let mut encoder = match Encoder::with_api_config(
        openh264::OpenH264API::from_source(),
        encoder_config(&params),
    ) {
        Ok(enc) => enc,
        Err(e) => {
            eprintln!("thermal-gb: openh264 init failed — channel disabled ({e})");
            return;
        }
    };
    let tick = Duration::from_secs_f64(1.0 / f64::from(params.fps.max(1)));
    let mut n = 0u64;
    loop {
        let started = Instant::now();
        if svc.is_stopped() {
            break;
        }
        // On-demand IDR (IFrameCmd): raise before this frame's encode.
        if idr_flag.swap(false, Ordering::Relaxed) {
            encoder.force_intra_frame();
        }
        let grid = svc.grid();
        match encode_frame(&mut encoder, &grid, params.width, params.height) {
            Some((data, key)) => {
                let nalus = Parser::parse(&data);
                if !nalus.is_empty() {
                    n = n.wrapping_add(1);
                    if n == 1 {
                        eprintln!(
                            "thermal-gb: first AU landed ({} bytes, key={key}, nalus={})",
                            data.len(),
                            nalus.len()
                        );
                    }
                    hub.write(AccessUnit {
                        nalus,
                        timestamp: Instant::now(),
                        is_key_frame: key,
                    });
                }
            }
            None => {
                // Recreate once per failure — the encoder object is not
                // reusable after an error; a fresh one restores the stream
                // on the next tick.
                encoder = match Encoder::with_api_config(
                    openh264::OpenH264API::from_source(),
                    encoder_config(&params),
                ) {
                    Ok(enc) => enc,
                    Err(e) => {
                        eprintln!("thermal-gb: openh264 re-init failed ({e})");
                        std::thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                };
            }
        }
        let spent = started.elapsed();
        if spent < tick {
            std::thread::sleep(tick - spent);
        }
    }
}

fn encoder_config(params: &GbStreamParams) -> EncoderConfig {
    EncoderConfig::new()
        .bitrate(BitRate::from_bps(params.bitrate_bps))
        .max_frame_rate(FrameRate::from_hz(params.fps.max(1) as f32))
        .rate_control_mode(RateControlMode::Bitrate)
        .intra_frame_period(IntraFramePeriod::from_num_frames(params.i_period.max(1)))
}

/// Render one grid → I420 → encode → Annex-B. `None` on encoder failure
/// (the caller re-creates the encoder).
fn encode_frame(
    encoder: &mut Encoder,
    grid: &[f32; PIXELS],
    width: u32,
    height: u32,
) -> Option<(Vec<u8>, bool)> {
    let (w, h) = (width as usize, height as usize);
    let rgb = render::render_rgb_annotated(grid, w, h, None, None);
    let i420 = render::rgb_to_i420(&rgb, w, h);
    let view = I420View::new(&i420, w, h);
    let bitstream = encoder.encode(&view).ok()?;
    let mut data = Vec::with_capacity(4096);
    bitstream.write_vec(&mut data);
    let key = matches!(
        bitstream.frame_type(),
        openh264::encoder::FrameType::IDR | openh264::encoder::FrameType::I
    );
    Some((data, key))
}

/// Borrows a raw I420 buffer as an openh264 `YUVSource` (same layout
/// contract as the camera software encoder).
struct I420View<'a> {
    data: &'a [u8],
    width: usize,
    height: usize,
}

impl<'a> I420View<'a> {
    fn new(data: &'a [u8], width: usize, height: usize) -> Self {
        Self {
            data,
            width,
            height,
        }
    }

    fn y_len(&self) -> usize {
        self.width * self.height
    }

    fn uv_len(&self) -> usize {
        self.y_len() / 4
    }
}

impl openh264::formats::YUVSource for I420View<'_> {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (self.width, self.width / 2, self.width / 2)
    }

    fn y(&self) -> &[u8] {
        &self.data[..self.y_len()]
    }

    fn u(&self) -> &[u8] {
        &self.data[self.y_len()..self.y_len() + self.uv_len()]
    }

    fn v(&self) -> &[u8] {
        &self.data[self.y_len() + self.uv_len()..self.y_len() + 2 * self.uv_len()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openh264::formats::YUVSource;

    #[test]
    fn default_params_match_nvr_agreed_profile() {
        let p = GbStreamParams::default();
        assert_eq!((p.width, p.height), (320, 240));
        assert!(
            (2..=8).contains(&p.fps),
            "fps must stay in NVR's 2-8 window"
        );
        assert!(p.bitrate_bps >= 100_000 && p.bitrate_bps <= 1_000_000);
    }

    #[test]
    fn encodes_uniform_grid_with_sps_pps_idr() {
        // Uniform grid (warmup state) must still produce a decodable
        // first AU — SPS+PPS+IDR, Annex-B framed.
        let params = GbStreamParams::default();
        let mut encoder = Encoder::with_api_config(
            openh264::OpenH264API::from_source(),
            encoder_config(&params),
        )
        .expect("encoder init");
        let grid = [20.0f32; PIXELS];
        let (data, key) = encode_frame(&mut encoder, &grid, params.width, params.height)
            .expect("uniform frame encodes");
        assert!(key, "first frame must be a key frame");
        assert_eq!(&data[..4], &[0, 0, 0, 1], "Annex-B start code");
        let types: Vec<u8> = Parser::parse(&data).iter().map(|n| n.nalu_type).collect();
        assert!(types.contains(&7), "SPS missing: {types:?}");
        assert!(types.contains(&8), "PPS missing: {types:?}");
        assert!(types.contains(&5), "IDR missing: {types:?}");
    }

    #[test]
    fn encode_survives_stretch_edge_case_all_equal() {
        // All-equal grid makes the percentile stretch span 0 — the
        // renderer clamps it; encoding must not see a NaN/panic.
        let params = GbStreamParams {
            width: 64,
            height: 48,
            ..GbStreamParams::default()
        };
        let mut encoder = Encoder::with_api_config(
            openh264::OpenH264API::from_source(),
            encoder_config(&params),
        )
        .expect("encoder init");
        let grid = [0.0f32; PIXELS];
        assert!(encode_frame(&mut encoder, &grid, 64, 48).is_some());
    }

    #[test]
    fn i420_view_layout() {
        let data: Vec<u8> = (0..12).collect();
        let view = I420View::new(&data, 4, 2);
        assert_eq!(view.dimensions(), (4, 2));
        assert_eq!(view.y(), &[0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(view.u(), &[8, 9]);
        assert_eq!(view.v(), &[10, 11]);
    }

    #[test]
    fn thermal_lib_config_overrides_and_inherits() {
        use crate::config::{Gb28181Config, Gb28181ThermalConfig};
        let mut base = Gb28181Config::default();
        base.lib.platform_sip_address = "192.168.63.30".into();
        base.lib.platform_sip_port = 5060;
        base.lib.device_id = "34020000001320000011".into();
        base.lib.channel_id = "34020000001310000011".into();
        base.lib.password = "inherited".into();
        base.lib.local_sip_port = 5060;
        base.lib.extra_channels = vec![crate::gb28181::config::CatalogChannel {
            device_id: "34020000001310000099".into(),
            name: "sub".into(),
            manufacturer: String::new(),
            model: String::new(),
            status: "ON".into(),
            ptz_type: None,
            photoelectric_imaging_type: None,
        }];
        let t = Gb28181ThermalConfig::default();
        let lib = thermal_lib_config(&base, &t, "MiBee");
        // Overridden identity/port/marking.
        assert_eq!(lib.device_id, "34020000001320000003");
        assert_eq!(lib.channel_id, "34020000001310000003");
        assert_eq!(lib.local_sip_port, 5061);
        assert_eq!(lib.photoelectric_imaging_type, Some(2));
        assert_eq!(lib.device_name.as_deref(), Some("MiBee [thermal]"));
        assert!(lib.extra_channels.is_empty());
        // Inherited platform + password.
        assert_eq!(lib.platform_sip_address, "192.168.63.30");
        assert_eq!(lib.password, "inherited");
        // Explicit password wins.
        let mut t2 = t.clone();
        t2.password = "override".into();
        assert_eq!(thermal_lib_config(&base, &t2, "x").password, "override");
    }

    #[test]
    fn thermal_config_section_parses_with_defaults() {
        // Bare `[gb28181_thermal] enabled = true` keeps the agreed stream
        // profile and second-device defaults.
        let cfg: crate::config::Config =
            toml::from_str("[gb28181_thermal]\nenabled = true\n[thermal]\nenabled = true\n")
                .unwrap();
        let t = &cfg.gb28181_thermal;
        assert!(t.enabled);
        assert_eq!(t.device_id, "34020000001320000003");
        assert_eq!(t.local_sip_port, 5061);
        assert_eq!((t.width, t.height, t.fps), (320, 240, 4));
        assert_eq!(t.bitrate_bps, 300_000);
        assert_eq!(t.i_period, 12);
        // And a config without the section stays fully default-off.
        let cfg: crate::config::Config = toml::from_str("").unwrap();
        assert!(!cfg.gb28181_thermal.enabled);
    }
}
