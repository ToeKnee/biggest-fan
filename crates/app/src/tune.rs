//! The auto-tune behind the Stress lab's "Run full test" button: warm
//! the machine under full stress, then try each fan one notch slower
//! until a temperature starts to rise.
//!
//! The suggestion each fan earns is the slowest curve it can hold.
//!
//! All changes go through the running service as transient `SetFanMode`
//! requests, so a tune is reversible: pressing *Save config* keeps the
//! suggested curves, aborting the tune (or closing the window) restores
//! the modes the fans had when the tune started, and so does a
//! thermal-guard trip on the stressors. NVML's thermal-throttle flag is
//! watched throughout — a throttle reverts the current notch at once and
//! the final report says whether any throttle happened, because the
//! suggested curves are only trustworthy if none did.
//!
//! The state machine is time-stamp driven (the GUI never sleeps): the
//! Stress lab panel calls [`Tune::step`] on its half-second tick.

use crate::dbus::FanRequest;
use biggest_fan_core as core;
use biggest_fan_stress_lab::thermal::GpuMonitor;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// The sweep tries 100%, 90%, 80%, ...
const NOTCH: f64 = 0.1;
/// How far any sensor may climb above the last accepted speed before
/// the notch is reverted.
const RISE_LIMIT_C: f64 = 2.0;
/// Never suggest a fan run slower than this under full load.
const FLOOR_SPEED: f64 = 0.1;

/// One fan being tuned.
struct TuneFan {
    /// Fan group id, as the service knows it.
    id: String,
    /// Display name for the report.
    name: String,
    /// The sensor its auto mode tracks.
    sensor: core::SensorId,
    /// The mode the fan had when the tune started, to restore on abort.
    original_mode: core::FanMode,
    /// The slowest speed so far where everything stayed cool.
    safe_speed: f64,
    /// The speed currently being tried.
    trying_speed: f64,
    /// The current notch was just reverted; one more settle period
    /// proves the revert has settled before the next fan starts.
    reverted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Stressors running, every candidate fan pinned to full speed.
    Warming,
    /// One fan at a time: hold a speed, watch, step down.
    Settling,
    /// Suggestions applied; the tune is over.
    Done,
    /// Cancelled; the original modes were restored.
    Aborted,
}

/// The tune state machine, driven by [`Tune::step`] on the panel tick.
pub struct Tune {
    phase: Phase,
    fans: Vec<TuneFan>,
    /// Which fan is being swept.
    index: usize,
    /// When the current phase or notch started.
    phase_started: Instant,
    warmup: Duration,
    settle: Duration,
    /// Sensor temperatures recorded at the last accepted state.
    baseline: BTreeMap<core::SensorId, f64>,
    gpu: Option<GpuMonitor>,
    throttle_hits: usize,
    log: Vec<String>,
}

impl Tune {
    /// Tunes every visible fan the config drives itself (`auto` mode
    /// names the sensor each tracks); pumps and hand-set fans (fixed or
    /// off modes) are left alone.
    #[must_use]
    pub fn new(config: &core::AppConfig, warmup: Duration, now: Instant) -> Self {
        let fans = config
            .fans
            .iter()
            .filter(|fan| !fan.hidden)
            .filter_map(|fan| {
                let core::FanMode::Auto { sensor, .. } = &fan.mode else {
                    return None;
                };
                Some(TuneFan {
                    id: fan.id.as_str().to_string(),
                    name: fan
                        .name
                        .clone()
                        .unwrap_or_else(|| fan.id.as_str().to_string()),
                    sensor: sensor.clone(),
                    original_mode: fan.mode.clone(),
                    safe_speed: 1.0,
                    trying_speed: 1.0 - NOTCH,
                    reverted: false,
                })
            })
            .collect();
        Self {
            phase: Phase::Warming,
            fans,
            index: 0,
            phase_started: now,
            warmup,
            // A new equilibrium is reached faster than the first one:
            // half the warm-up, but never under a minute.
            settle: (warmup / 2).max(Duration::from_secs(60)),
            baseline: BTreeMap::new(),
            gpu: GpuMonitor::try_open(),
            throttle_hits: 0,
            log: Vec::new(),
        }
    }

    /// Commands pinning every candidate fan to full speed for the
    /// warm-up.
    #[must_use]
    pub fn warmup_requests(&self) -> Vec<FanRequest> {
        self.fans
            .iter()
            .map(|fan| fixed_request(fan, 1.0))
            .collect()
    }

    /// Advances the machine. `temps` are the live sensor temperatures
    /// and `send` forwards requests to the running service.
    pub fn step(
        &mut self,
        now: Instant,
        temps: &BTreeMap<core::SensorId, f64>,
        send: &mut dyn FnMut(FanRequest),
    ) {
        match self.phase {
            Phase::Warming => {
                if now.duration_since(self.phase_started) >= self.warmup {
                    self.baseline = temps.clone();
                    if self.fans.is_empty() {
                        self.log.push("no auto-driven fans to tune".to_string());
                        self.phase = Phase::Done;
                    } else {
                        self.phase = Phase::Settling;
                        self.phase_started = now;
                        send(fixed_request(&self.fans[0], self.fans[0].trying_speed));
                    }
                }
            }
            Phase::Settling => self.step_settling(now, temps, send),
            Phase::Done | Phase::Aborted => {}
        }
    }

    fn step_settling(
        &mut self,
        now: Instant,
        temps: &BTreeMap<core::SensorId, f64>,
        send: &mut dyn FnMut(FanRequest),
    ) {
        let tripped = self
            .gpu
            .as_ref()
            .is_some_and(GpuMonitor::thermal_throttling);
        if tripped {
            self.throttle_hits += 1;
        }
        let settled = now.duration_since(self.phase_started) >= self.settle;
        if !settled && !tripped {
            return;
        }
        if self.fans[self.index].reverted {
            // The revert has had its settle: re-anchor the baseline at
            // the restored state (so the next fan is not blamed for this
            // one's heat) and move on.
            self.fans[self.index].reverted = false;
            self.baseline = temps.clone();
            self.begin_next_fan(now, send);
            return;
        }
        let rise = temps
            .iter()
            .filter_map(|(sensor, temp)| self.baseline.get(sensor).map(|was| temp - was))
            .fold(f64::NEG_INFINITY, f64::max);
        if tripped || rise > RISE_LIMIT_C {
            let name = self.fans[self.index].name.clone();
            let safe = self.fans[self.index].safe_speed;
            let reason = if tripped {
                "GPU thermal throttling".to_string()
            } else {
                format!("{rise:.1} °C rise")
            };
            self.log.push(format!(
                "{name}: cannot go below {:.0}% ({reason})",
                safe * 100.0
            ));
            self.fans[self.index].reverted = true;
            send(fixed_request(&self.fans[self.index], safe));
            self.phase_started = now;
        } else {
            // This speed held; accept it and try one notch lower.
            let accepted = self.fans[self.index].trying_speed;
            self.fans[self.index].safe_speed = accepted;
            self.baseline = temps.clone();
            let next = accepted - NOTCH;
            if next < FLOOR_SPEED {
                self.begin_next_fan(now, send);
            } else {
                self.fans[self.index].trying_speed = next;
                send(fixed_request(&self.fans[self.index], next));
                self.phase_started = now;
            }
        }
    }

    fn begin_next_fan(&mut self, now: Instant, send: &mut dyn FnMut(FanRequest)) {
        self.index += 1;
        if self.index >= self.fans.len() {
            self.finish(send);
            return;
        }
        self.phase = Phase::Settling;
        self.phase_started = now;
        let fan = &self.fans[self.index];
        send(fixed_request(fan, fan.trying_speed));
    }

    /// Writes each fan's suggested curve and closes the tune. The curve
    /// is the Balanced shape scaled so its full-load speed equals the
    /// slowest speed the fan proved it could hold.
    fn finish(&mut self, send: &mut dyn FnMut(FanRequest)) {
        for fan in &self.fans {
            let mode = suggested_mode(fan);
            match serde_json::to_string(&mode) {
                Ok(mode_json) => {
                    send(FanRequest::SetFanMode {
                        fan: fan.id.clone(),
                        mode_json,
                    });
                    self.log.push(format!(
                        "{}: holds {:.0}% under full load (curve suggested)",
                        fan.name,
                        fan.safe_speed * 100.0
                    ));
                }
                Err(error) => self.log.push(format!(
                    "{}: curve could not be serialised ({error}) — left at {:.0}%",
                    fan.name,
                    fan.safe_speed * 100.0
                )),
            }
        }
        if self.throttle_hits > 0 {
            self.log.push(format!(
                "GPU thermal throttling was seen {} time(s) during the tune — review the curves before saving",
                self.throttle_hits
            ));
        } else {
            self.log
                .push("no GPU thermal throttling seen during the tune".to_string());
        }
        self.phase = Phase::Done;
    }

    /// Puts the fans back in the modes they had when the tune started.
    pub fn abort(&mut self, send: &mut dyn FnMut(FanRequest)) {
        if self.finished() {
            return;
        }
        for fan in &self.fans {
            if let Ok(mode_json) = serde_json::to_string(&fan.original_mode) {
                send(FanRequest::SetFanMode {
                    fan: fan.id.clone(),
                    mode_json,
                });
            }
        }
        self.log
            .push("tune stopped: original modes restored".to_string());
        self.phase = Phase::Aborted;
    }

    /// Whether the tune is over (finished or cancelled).
    #[must_use]
    pub const fn finished(&self) -> bool {
        matches!(self.phase, Phase::Done | Phase::Aborted)
    }

    /// Short progress text for the status line.
    #[must_use]
    pub fn status(&self, now: Instant) -> String {
        match self.phase {
            Phase::Warming => {
                let left = self
                    .warmup
                    .saturating_sub(now.duration_since(self.phase_started));
                format!(
                    "tune: warming up, {:.0} min left",
                    left.as_secs_f64() / 60.0
                )
            }
            Phase::Settling => {
                let fan = &self.fans[self.index];
                format!(
                    "tune: fan {} of {} — {} at {:.0}%",
                    self.index + 1,
                    self.fans.len(),
                    fan.name,
                    fan.trying_speed * 100.0
                )
            }
            Phase::Done => "tune done: press Save config to keep the new curves".to_string(),
            Phase::Aborted => "tune stopped".to_string(),
        }
    }

    /// The full per-step report, for the status line after the tune.
    #[must_use]
    pub fn report(&self) -> String {
        self.log.join(" · ")
    }
}

fn fixed_request(fan: &TuneFan, speed: f64) -> FanRequest {
    FanRequest::SetFanMode {
        fan: fan.id.clone(),
        mode_json: serde_json::to_string(&core::FanMode::Fixed { speed })
            .unwrap_or_else(|_| r#"{"type":"fixed","speed":1.0}"#.to_string()),
    }
}

/// The Balanced curve's points scaled so the top speed equals what the
/// fan held under full load. Should the scaling ever be rejected, the
/// fan simply stays at its proven fixed speed.
fn suggested_mode(fan: &TuneFan) -> core::FanMode {
    let points = [
        (25.0, 0.00),
        (40.0, 0.10),
        (55.0, 0.20),
        (70.0, 0.35),
        (85.0, 0.60),
        (95.0, 1.00),
    ]
    .into_iter()
    .map(|(temp_c, speed)| core::CurvePoint {
        temp_c,
        speed: speed * fan.safe_speed,
    })
    .collect();
    core::FanCurve::new(points).map_or_else(
        |_| core::FanMode::Fixed {
            speed: fan.safe_speed,
        },
        |curve| core::FanMode::Auto {
            sensor: fan.sensor.clone(),
            curve,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two auto fans and one fixed pump to prove it is left alone.
    fn sample_config() -> core::AppConfig {
        core::config::from_text(
            r#"
[[fans]]
id = "chip-p1"
mode = { type = "auto", sensor = "chip-t1" }

[[fans]]
id = "chip-p2"
mode = { type = "auto", sensor = "gpu-t1" }

[[fans]]
id = "chip-p4"
mode = { type = "fixed", speed = 0.5 }
"#,
        )
        .expect("sample config parses")
    }

    fn temps(with_cpu: f64) -> BTreeMap<core::SensorId, f64> {
        BTreeMap::from([
            (core::SensorId::new("chip-t1"), with_cpu),
            (core::SensorId::new("gpu-t1"), 50.0),
        ])
    }

    #[test]
    fn only_visible_auto_fans_are_tuned() {
        let tune = Tune::new(&sample_config(), Duration::from_secs(600), Instant::now());
        assert_eq!(tune.fans.len(), 2, "the fixed pump is left alone");
        assert_eq!(tune.warmup_requests().len(), 2);
    }

    #[test]
    fn a_fan_that_stays_cool_walks_down_and_gets_a_scaled_curve() {
        let config = sample_config();
        let warmup = Duration::from_secs(10);
        let t0 = Instant::now();
        let mut tune = Tune::new(&config, warmup, t0);
        let mut requests = Vec::new();
        // Warm-up, then every notch accepted: 0.9 down to 0.1, then the
        // second fan the same, then suggestions for both. The clock must
        // advance past each settle, like it would in the running app.
        let mut now = t0 + warmup + Duration::from_secs(1);
        for _ in 0..40 {
            tune.step(now, &temps(70.0), &mut |r| {
                requests.push(r);
            });
            if tune.finished() {
                break;
            }
            now += tune.settle + Duration::from_secs(1);
        }
        assert_eq!(tune.phase, Phase::Done);
        let last_two = &requests[requests.len() - 2..];
        for request in last_two {
            let FanRequest::SetFanMode { mode_json, .. } = request else {
                panic!("the tune must end by suggesting curves");
            };
            assert!(mode_json.contains("\"auto\""), "a curve is suggested");
            assert!(
                mode_json.contains("0.1") || mode_json.contains("0.01"),
                "the curve tops out at the proven safe speed: {mode_json}"
            );
        }
        assert!(tune.report().contains("no GPU thermal throttling"));
    }

    #[test]
    fn a_rising_temperature_reverts_the_notch_and_keeps_the_fan_high() {
        let config = sample_config();
        let warmup = Duration::from_secs(10);
        let t0 = Instant::now();
        let mut tune = Tune::new(&config, warmup, t0);
        let mut requests = Vec::new();
        // Warm-up completes at a cool 70 °C; the first lower notch makes
        // the sensor jump, so the revert must restore 100% and the fan
        // must finish with safe_speed 1.0.
        tune.step(t0 + warmup, &temps(70.0), &mut |r| {
            requests.push(r);
        });
        tune.step(t0 + warmup + tune.settle, &temps(75.0), &mut |r| {
            requests.push(r);
        });
        let FanRequest::SetFanMode { mode_json, .. } = requests.last().expect("a revert request")
        else {
            panic!("the revert sends a fixed speed");
        };
        assert!(
            mode_json.contains('1'),
            "the revert goes back to full speed"
        );
        // One more settle proves the revert, then the sweep moves on.
        let mut now = t0 + warmup + tune.settle * 2;
        for _ in 0..40 {
            tune.step(now, &temps(75.0), &mut |r| {
                requests.push(r);
            });
            if tune.finished() {
                break;
            }
            now += tune.settle + Duration::from_secs(1);
        }
        assert_eq!(tune.fans[0].safe_speed, 1.0, "the fan keeps full speed");
        assert!(tune.report().contains("cannot go below 100%"));
    }

    #[test]
    fn aborting_restores_the_modes_from_before_the_tune() {
        let config = sample_config();
        let t0 = Instant::now();
        let mut tune = Tune::new(&config, Duration::from_secs(600), t0);
        let mut requests = Vec::new();
        tune.abort(&mut |r| {
            requests.push(r);
        });
        assert_eq!(
            requests.len(),
            2,
            "both auto fans go back to their old mode"
        );
        for request in &requests {
            let FanRequest::SetFanMode { mode_json, .. } = request else {
                panic!("restore uses SetFanMode");
            };
            assert!(
                mode_json.contains("\"auto\""),
                "the old auto mode comes back"
            );
        }
        assert!(tune.finished());
    }
}
