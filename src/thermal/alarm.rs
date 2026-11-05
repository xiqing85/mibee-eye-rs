//! Temperature over-limit alarm (rs#3 M4): evaluates the thermal
//! readings at 1 Hz and, on a sustained over-limit edge, sends a GB/T
//! 28181 alarm NOTIFY from the thermal device instance —
//! `AlarmMethod=2`（设备报警）+ `AlarmType=4`（高温报警, 2022 A.2.5.5
//! code table）+ configurable `AlarmPriority` (1 = 一级警情，火警级).
//!
//! The engine is a pure state machine (unit-testable); the task wiring
//! feeds it the reading `max` and holds a notifier slot the thermal GB
//! supervisor refreshes on every server rebuild.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use super::ThermalService;

/// `[thermal.alarm]` settings.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct TempAlarmConfig {
    pub enabled: bool,
    /// Over-limit threshold on the reading max (°C).
    pub threshold_c: f32,
    /// Re-arm hysteresis: the active state clears only below
    /// `threshold - hysteresis` (prevents flapping at the boundary).
    pub hysteresis_c: f32,
    /// Sustained-above duration before firing (anti-blip; a waving hand
    /// or a passing reflection stays under it).
    pub sustain_secs: u32,
    /// Minimum spacing between two fired alarms (anti-storm).
    pub cooldown_secs: u32,
    /// GB AlarmPriority 1..=4 (1 = highest, 火警级 default).
    pub priority: u8,
}

impl Default for TempAlarmConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold_c: 60.0,
            hysteresis_c: 3.0,
            sustain_secs: 5,
            cooldown_secs: 60,
            priority: 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Below threshold (or inside hysteresis band) — nothing pending.
    Idle,
    /// First crossed the threshold; waiting out the sustain window.
    Sustaining { since_ms: u64 },
    /// Fired and still above the clear line; re-arms after clearing.
    Active,
}

/// Pure over-limit state machine.
pub struct TempAlarmEngine {
    state: State,
    last_fired_ms: Option<u64>,
}

impl TempAlarmEngine {
    pub fn new() -> Self {
        Self {
            state: State::Idle,
            last_fired_ms: None,
        }
    }

    /// Feed one reading. Returns `Some(max_c)` on an accepted fire edge
    /// (sustained + cooldown passed).
    fn on_reading(&mut self, now_ms: u64, max_c: f32, cfg: &TempAlarmConfig) -> Option<f32> {
        let clear_line = cfg.threshold_c - cfg.hysteresis_c;
        match self.state {
            State::Idle => {
                if max_c >= cfg.threshold_c {
                    self.state = State::Sustaining { since_ms: now_ms };
                }
                None
            }
            State::Sustaining { since_ms } => {
                if max_c < clear_line {
                    // Dipped back inside the band before sustaining.
                    self.state = State::Idle;
                    return None;
                }
                if max_c >= cfg.threshold_c
                    && now_ms.saturating_sub(since_ms) >= u64::from(cfg.sustain_secs) * 1000
                {
                    if let Some(last) = self.last_fired_ms {
                        if now_ms.saturating_sub(last) < u64::from(cfg.cooldown_secs) * 1000 {
                            // Cooldown not elapsed — stay sustaining (no
                            // state churn) and retry on a later reading.
                            return None;
                        }
                    }
                    self.state = State::Active;
                    self.last_fired_ms = Some(now_ms);
                    return Some(max_c);
                }
                None
            }
            State::Active => {
                if max_c < clear_line {
                    self.state = State::Idle;
                }
                None
            }
        }
    }
}

impl Default for TempAlarmEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Notifier slot the thermal GB supervisor writes on every server
/// rebuild (the notifier is only valid for the live server instance).
pub type NotifierSlot = Arc<RwLock<Option<Arc<crate::gb28181::subscribe::DeviceNotifier>>>>;

/// Spawn the 1 Hz evaluation task. Untrusted readings (sensor lost /
/// warmup) hold the state machine as-is — a lost sensor must neither
/// fire nor silently clear a live alarm.
pub fn spawn_alarm_task(svc: Arc<ThermalService>, cfg: TempAlarmConfig, notifier: NotifierSlot) {
    tokio::spawn(async move {
        let mut engine = TempAlarmEngine::new();
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await; // first tick is immediate — skip
        loop {
            ticker.tick().await;
            if svc.is_stopped() {
                break;
            }
            let payload = svc.payload(false);
            if payload["trusted"].as_bool() != Some(true) {
                continue;
            }
            let Some(max_c) = payload["max"]["value_c"].as_f64() else {
                continue;
            };
            if let Some(fired_at) = engine.on_reading(now_ms(), max_c as f32, &cfg) {
                eprintln!(
                    "thermal-alarm: over-limit fired (max {fired_at:.1}°C ≥ {}°C) — NOTIFY priority {}",
                    cfg.threshold_c, cfg.priority
                );
                let Some(n) = notifier.read().expect("thermal notifier lock").clone() else {
                    eprintln!(
                        "thermal-alarm: no notifier (thermal GB instance not up) — NOTIFY not sent"
                    );
                    continue;
                };
                let desc = format!(
                    "Temperature over-limit: max {fired_at:.1}°C ≥ {:.1}°C (MLX90640)",
                    cfg.threshold_c
                );
                // AlarmMethod=2 设备报警 / AlarmType=4 高温报警 (2022
                // A.2.5.5 code table); time = fire instant. The send
                // result distinguishes "went out on the wire" (platform
                // holds an Alarm subscription) from a silent no-op.
                let sent = n.send_alarm(
                    &cfg.priority.to_string(),
                    "2",
                    &crate::gb28181::client::format_gb_time_ms(now_ms()),
                    "4",
                    &desc,
                );
                if sent {
                    println!(
                        "thermal-alarm: NOTIFY sent (Method=2 Type=4 priority {})",
                        cfg.priority
                    );
                } else {
                    eprintln!(
                        "thermal-alarm: NOTIFY NOT sent — no live Alarm subscription on the thermal instance"
                    );
                }
            }
        }
    });
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> TempAlarmConfig {
        TempAlarmConfig {
            enabled: true,
            threshold_c: 50.0,
            hysteresis_c: 3.0,
            sustain_secs: 5,
            cooldown_secs: 60,
            priority: 1,
        }
    }

    #[test]
    fn below_threshold_never_fires() {
        let mut e = TempAlarmEngine::new();
        let c = cfg();
        for t in 0..30 {
            assert!(e.on_reading(t * 1000, 40.0, &c).is_none());
        }
    }

    #[test]
    fn sustained_overlimit_fires() {
        let mut e = TempAlarmEngine::new();
        let c = cfg();
        // t=0 crosses; t=1..4 still sustaining; t=5 hits the window.
        assert!(e.on_reading(0, 55.0, &c).is_none());
        assert!(e.on_reading(4000, 55.0, &c).is_none());
        let fired = e.on_reading(5000, 55.0, &c);
        assert_eq!(fired, Some(55.0), "sustained 5s must fire");
    }

    #[test]
    fn short_blip_suppressed() {
        let mut e = TempAlarmEngine::new();
        let c = cfg();
        assert!(e.on_reading(0, 60.0, &c).is_none());
        // Drops below the clear line inside the sustain window.
        assert!(e.on_reading(2000, 45.0, &c).is_none());
        assert!(e.on_reading(4000, 60.0, &c).is_none(), "timer restarted");
        assert!(e.on_reading(8000, 60.0, &c).is_none(), "4s < 5s window");
        assert!(e.on_reading(9001, 60.0, &c).is_some(), "re-sustained fires");
    }

    #[test]
    fn hysteresis_band_holds_active_no_refire() {
        let mut e = TempAlarmEngine::new();
        let c = cfg();
        assert!(e.on_reading(0, 55.0, &c).is_none());
        assert!(e.on_reading(5000, 55.0, &c).is_some());
        // Inside the band (47..50): stays active, never re-fires.
        assert!(e.on_reading(6000, 48.5, &c).is_none());
        assert!(
            e.on_reading(200_000, 49.9, &c).is_none(),
            "no re-NOTIFY while active"
        );
        // Below the clear line: clears; a re-rise sustains from scratch
        // and fires again once cooldown (fired 5s + 60s = 65s) is past.
        assert!(e.on_reading(210_000, 40.0, &c).is_none());
        assert!(e.on_reading(211_000, 60.0, &c).is_none());
        assert!(e.on_reading(214_000, 60.0, &c).is_none(), "3s < 5s sustain");
        assert!(
            e.on_reading(216_000, 60.0, &c).is_some(),
            "sustained + cooldown elapsed"
        );
    }

    #[test]
    fn cooldown_blocks_then_allows_refire() {
        let mut e = TempAlarmEngine::new();
        let c = cfg();
        assert!(e.on_reading(0, 55.0, &c).is_none());
        assert!(e.on_reading(5000, 55.0, &c).is_some());
        // Clear + re-rise quickly: sustain met but cooldown still on.
        assert!(e.on_reading(10_000, 40.0, &c).is_none());
        assert!(e.on_reading(11_000, 55.0, &c).is_none());
        assert!(
            e.on_reading(16_000, 55.0, &c).is_none(),
            "cooldown until 65s"
        );
        // Cooldown passes while still hot: next reading fires.
        assert!(e.on_reading(70_000, 55.0, &c).is_some());
    }

    #[test]
    fn thermal_alarm_section_parses() {
        let cfg: crate::config::Config = toml::from_str(
            "[thermal]\nenabled = true\n[thermal.alarm]\nenabled = true\nthreshold_c = 45.5\nsustain_secs = 10\n",
        )
        .unwrap();
        let a = &cfg.thermal.alarm;
        assert!(a.enabled);
        assert_eq!(a.threshold_c, 45.5);
        assert_eq!(a.sustain_secs, 10);
        assert_eq!(a.hysteresis_c, 3.0, "hysteresis default");
        assert_eq!(a.priority, 1, "priority default");
        // Defaults with no section.
        let cfg: crate::config::Config = toml::from_str("").unwrap();
        assert!(!cfg.thermal.alarm.enabled);
        assert_eq!(cfg.thermal.alarm.threshold_c, 60.0);
    }
}
