//! The auto-tune behind the Stress lab's "Run full test" button: warm
//! the machine under full stress, then search each fan down to the
//! slowest speed that keeps every temperature down.
//!
//! The suggestion each fan earns is that speed as a curve.
//!
//! The search per fan gallops down from full speed in doubling steps and
//! then bisects its last safe/unsafe pair to a 5 % grid, and every wait
//! ends as soon as the temperatures stop moving (with floors on both the
//! soak and the waits) — a full test is measured in tens of minutes, not
//! hours.
//!
//! All changes go through the running service as transient `SetFanMode`
//! requests, so a tune is reversible: pressing *Save config* keeps the
//! suggested curves, aborting the tune (or closing the window) restores
//! the modes the fans had when the tune started, and so does a
//! thermal-guard trip on the stressors. NVML's thermal-throttle flag is
//! watched throughout — a throttle reverts the current probe at once and
//! the final report says whether any throttle happened, because the
//! suggested curves are only trustworthy if none did.
//!
//! The state machine is time-stamp driven (the GUI never sleeps): the
//! Stress lab panel calls [`Tune::step`] on its half-second tick.

use crate::dbus::FanRequest;
use biggest_fan_core as core;
use biggest_fan_stress_lab::thermal::GpuMonitor;
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

/// The first step down from full speed; each accepted probe doubles it,
/// until a probe fails and the search bisects instead.
const NOTCH: f64 = 0.1;
/// The search stops once its safe and unsafe speeds are this close, so a
/// suggestion is accurate to 5 % without trying every 5 % step.
const GRID: f64 = 0.05;
/// How far any sensor may climb above the last accepted speed before the
/// probe is reverted.
const RISE_LIMIT_C: f64 = 2.0;
/// Never suggest a fan run slower than this under full load.
const FLOOR_SPEED: f64 = 0.1;
/// A probe is never judged before this much time has passed, however
/// steady the readings look.
const MIN_SETTLE: Duration = Duration::from_secs(45);
/// How much temperature history the steady check looks at.
const STEADY_WINDOW: Duration = Duration::from_secs(20);
/// The most any sensor may move inside [`STEADY_WINDOW`] for the machine
/// to count as having stopped moving.
const STEADY_SPREAD_C: f64 = 0.5;

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
    /// The slowest speed so far that kept everything cool: the search's
    /// safe hand, and the speed the fan sits at between probes.
    safe_speed: f64,
    /// The fastest speed known to be too slow: the search's other hand.
    /// `None` while no probe has failed yet.
    unsafe_speed: Option<f64>,
    /// The gallop's current step down, doubled for each accepted probe
    /// while nothing has failed.
    step: f64,
    /// The speed currently being tried.
    trying_speed: f64,
    /// The last probe failed, so the fan is back at `safe_speed`: one
    /// more settle re-anchors the baseline there before the next probe.
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
    /// Which fan is being searched.
    index: usize,
    /// When the current soak or probe started.
    phase_started: Instant,
    warmup: Duration,
    /// The most a probe may be waited for; a steady machine ends the
    /// wait sooner (never before [`MIN_SETTLE`]).
    settle: Duration,
    /// Readings since the current soak or probe started, oldest first,
    /// for the steady check that ends waits early.
    samples: VecDeque<(Duration, BTreeMap<core::SensorId, f64>)>,
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
                    unsafe_speed: None,
                    step: NOTCH,
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
            // half the warm-up, but never under a minute. A steady
            // machine ends a wait sooner; this is only the deadline.
            settle: (warmup / 2).max(Duration::from_secs(60)),
            samples: VecDeque::new(),
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
        let elapsed = now.duration_since(self.phase_started);
        self.record(elapsed, temps);
        match self.phase {
            Phase::Warming => {
                // The soak may end early once the machine has genuinely
                // stopped heating — but never before half the requested
                // warm-up, because water and radiators take their time.
                let soaked = elapsed >= self.warmup
                    || (elapsed >= self.warmup / 2 && self.steady(elapsed, temps));
                if soaked {
                    self.baseline = temps.clone();
                    if self.fans.is_empty() {
                        self.log.push("no auto-driven fans to tune".to_string());
                        self.phase = Phase::Done;
                    } else {
                        self.phase = Phase::Settling;
                        self.begin_probe(now, send);
                    }
                }
            }
            Phase::Settling => self.step_settling(now, elapsed, temps, send),
            Phase::Done | Phase::Aborted => {}
        }
    }

    fn step_settling(
        &mut self,
        now: Instant,
        elapsed: Duration,
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
        // Judge a probe once the machine has sat at its speed for
        // `MIN_SETTLE` and stopped moving — or after `settle` anyway.
        let settled =
            elapsed >= self.settle || (elapsed >= MIN_SETTLE && self.steady(elapsed, temps));
        if !settled && !tripped {
            return;
        }
        if self.fans[self.index].reverted {
            // The revert has had its settle: re-anchor the baseline at
            // the restored state (so the next probe is not blamed for
            // heat still leaving this one) and carry on searching.
            self.fans[self.index].reverted = false;
            self.baseline = temps.clone();
            self.begin_probe(now, send);
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
            let tried = self.fans[self.index].trying_speed;
            self.fans[self.index].unsafe_speed = Some(tried);
            self.fans[self.index].reverted = true;
            send(fixed_request(&self.fans[self.index], safe));
            self.start_wait(now);
        } else {
            // This speed held; accept it and look lower.
            let accepted = self.fans[self.index].trying_speed;
            self.fans[self.index].safe_speed = accepted;
            if self.fans[self.index].unsafe_speed.is_none() {
                self.fans[self.index].step *= 2.0;
            }
            self.baseline = temps.clone();
            self.begin_probe(now, send);
        }
    }

    /// Sends the current fan's next probe speed, or moves to the next
    /// fan once this fan's search has narrowed to [`GRID`].
    fn begin_probe(&mut self, now: Instant, send: &mut dyn FnMut(FanRequest)) {
        let Some(speed) = next_probe(&self.fans[self.index]) else {
            self.begin_next_fan(now, send);
            return;
        };
        self.fans[self.index].trying_speed = speed;
        self.start_wait(now);
        send(fixed_request(&self.fans[self.index], speed));
    }

    fn begin_next_fan(&mut self, now: Instant, send: &mut dyn FnMut(FanRequest)) {
        self.index += 1;
        if self.index >= self.fans.len() {
            self.finish(send);
            return;
        }
        self.begin_probe(now, send);
    }

    /// Starts a new soak/probe wait: the steady window only ever covers
    /// the state the fans are in right now.
    fn start_wait(&mut self, now: Instant) {
        self.phase_started = now;
        self.samples.clear();
    }

    /// Adds this step's readings to the steady window, dropping the ones
    /// that have fallen out of it.
    fn record(&mut self, elapsed: Duration, temps: &BTreeMap<core::SensorId, f64>) {
        while self
            .samples
            .front()
            .is_some_and(|(oldest, _)| elapsed.saturating_sub(*oldest) > STEADY_WINDOW)
        {
            self.samples.pop_front();
        }
        self.samples.push_back((elapsed, temps.clone()));
    }

    /// Whether every sensor has sat inside [`STEADY_SPREAD_C`] for a full
    /// trailing [`STEADY_WINDOW`] of readings.
    #[must_use]
    fn steady(&self, elapsed: Duration, temps: &BTreeMap<core::SensorId, f64>) -> bool {
        let Some((oldest, _)) = self.samples.front() else {
            return false;
        };
        if elapsed.saturating_sub(*oldest) < STEADY_WINDOW {
            return false;
        }
        // The window and the current readings must agree for every
        // sensor; a sensor not in the window has not moved by
        // definition.
        for (sensor, current) in temps {
            let mut lowest = *current;
            let mut highest = *current;
            for (_, sample) in &self.samples {
                if let Some(seen) = sample.get(sensor) {
                    lowest = lowest.min(*seen);
                    highest = highest.max(*seen);
                }
            }
            if highest - lowest > STEADY_SPREAD_C {
                return false;
            }
        }
        true
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

/// The next speed to try for a fan, or `None` when its search is done.
///
/// While no probe has failed the search gallops down in doubling steps
/// (10 %, 20 %, 40 %, … of what is left), which finds a fan that may run
/// slowly in a handful of probes. Once a speed has failed, the search
/// bisects the safe/unsafe pair until the two are [`GRID`] apart, which
/// is as close a call as is worth waiting for — except for a fan that
/// never accepted anything below full speed, which stops at its first
/// failure: splitting the last 10 % is settles spent for nothing.
#[must_use]
fn next_probe(fan: &TuneFan) -> Option<f64> {
    if let Some(unsafe_speed) = fan.unsafe_speed {
        let gap = fan.safe_speed - unsafe_speed;
        let at_the_top = fan.safe_speed >= 1.0 - f64::EPSILON;
        return if gap <= GRID + f64::EPSILON || at_the_top {
            None
        } else {
            Some(unsafe_speed + gap / 2.0)
        };
    }
    if fan.safe_speed <= FLOOR_SPEED {
        // Even the floor held: there is nowhere left to look.
        return None;
    }
    Some((fan.safe_speed - fan.step).max(FLOOR_SPEED))
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
        // Warm-up, then the gallop down to the floor (0.9, 0.7, 0.3, 0.1)
        // for each fan, then suggestions for both. The clock must advance
        // past each settle, like it would in the running app.
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
        assert_eq!(
            tune.fans
                .iter()
                .filter(|fan| fan.unsafe_speed.is_none())
                .count(),
            2,
            "a cool machine never fails a probe"
        );
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
    fn the_search_bisects_to_the_slowest_speed_a_fan_holds() {
        let config = core::config::from_text(
            r#"
[[fans]]
id = "chip-p1"
mode = { type = "auto", sensor = "chip-t1" }
"#,
        )
        .expect("one-fan config parses");
        let warmup = Duration::from_secs(10);
        let t0 = Instant::now();
        let mut tune = Tune::new(&config, warmup, t0);
        // This fan holds 60 % but no slower: any probe below that reads
        // 5 °C hotter than the accepted state.
        let mut last_sent = 1.0;
        let mut requests = Vec::new();
        let mut now = t0 + warmup + Duration::from_secs(1);
        for _ in 0..20 {
            let temp = if last_sent < 0.6 { 75.0 } else { 70.0 };
            tune.step(now, &temps(temp), &mut |r| {
                if let FanRequest::SetFanMode { mode_json, .. } = &r
                    && let Ok(core::FanMode::Fixed { speed }) = serde_json::from_str(mode_json)
                {
                    last_sent = speed;
                }
                requests.push(r);
            });
            if tune.finished() {
                break;
            }
            now += tune.settle + Duration::from_secs(1);
        }
        let fan = &tune.fans[0];
        assert!(tune.finished());
        assert!(
            (0.6..0.65).contains(&fan.safe_speed),
            "the search lands just above the fan's real limit: {}",
            fan.safe_speed
        );
        let unsafe_speed = fan.unsafe_speed.expect("a probe must have failed");
        assert!(
            fan.safe_speed - unsafe_speed <= GRID + 1e-9,
            "the bracket closed to the grid: {} vs {unsafe_speed}",
            fan.safe_speed
        );
        // Galloping and bisecting cost fewer probes than walking every
        // 10 % step from 100 % down to 60 % would have.
        let probes = requests
            .iter()
            .filter(|r| matches!(r, FanRequest::SetFanMode { mode_json, .. } if mode_json.contains("fixed")))
            .count();
        assert!(
            probes <= 10,
            "the search keeps its probe count down: {probes}"
        );
    }

    #[test]
    fn a_soak_that_stops_heating_ends_early() {
        let config = sample_config();
        let t0 = Instant::now();
        let mut tune = Tune::new(&config, Duration::from_secs(600), t0);
        let mut now = t0;
        for _ in 0..200 {
            now += Duration::from_secs(5);
            tune.step(now, &temps(70.0), &mut |_| {});
            if tune.phase != Phase::Warming {
                break;
            }
        }
        let soaked_in = now.duration_since(t0);
        assert_eq!(tune.phase, Phase::Settling);
        assert!(
            soaked_in >= tune.warmup / 2,
            "a soak never ends before half the requested warm-up: {soaked_in:?}"
        );
        assert!(
            soaked_in < tune.warmup,
            "a machine that stopped heating is not soaked for the full request: {soaked_in:?}"
        );
    }

    #[test]
    fn a_probe_is_judged_as_soon_as_the_machine_is_steady() {
        let config = core::config::from_text(
            r#"
[[fans]]
id = "chip-p1"
mode = { type = "auto", sensor = "chip-t1" }
"#,
        )
        .expect("one-fan config parses");
        let warmup = Duration::from_secs(600);
        let t0 = Instant::now();
        let mut tune = Tune::new(&config, warmup, t0);
        let mut requests = Vec::new();
        // The soak's deadline ends the soak here; then the first probe
        // goes out at `probe_start`.
        let probe_start = t0 + warmup + Duration::from_secs(1);
        tune.step(probe_start, &temps(70.0), &mut |r| {
            requests.push(r);
        });
        assert_eq!(requests.len(), 1, "the soak ended and the probe started");
        // Nothing moves, so the verdict must arrive long before the
        // `settle` deadline (half the warm-up here).
        let mut now = probe_start;
        let mut judged = None;
        for _ in 0..80 {
            now += Duration::from_secs(5);
            let before = requests.len();
            tune.step(now, &temps(70.0), &mut |r| requests.push(r));
            if requests.len() > before {
                judged = Some(now.duration_since(probe_start));
                break;
            }
        }
        let judged = judged.expect("the probe is eventually judged");
        assert!(
            judged >= MIN_SETTLE,
            "a probe is never judged before MIN_SETTLE: {judged:?}"
        );
        assert!(
            judged < tune.settle / 2,
            "a steady machine is judged early, not at the deadline: {judged:?} vs {:?}",
            tune.settle
        );
    }

    #[test]
    fn a_rising_temperature_reverts_the_probe_and_keeps_the_fan_high() {
        let config = sample_config();
        let warmup = Duration::from_secs(10);
        let t0 = Instant::now();
        let mut tune = Tune::new(&config, warmup, t0);
        // This fan must keep full speed: any slower probe reads 5 °C
        // hotter than the accepted state.
        let mut last_sent = 1.0;
        let mut requests = Vec::new();
        let mut now = t0 + warmup + Duration::from_secs(1);
        for _ in 0..40 {
            let temp = if last_sent < 1.0 { 75.0 } else { 70.0 };
            tune.step(now, &temps(temp), &mut |r| {
                if let FanRequest::SetFanMode { mode_json, .. } = &r
                    && let Ok(core::FanMode::Fixed { speed }) = serde_json::from_str(mode_json)
                {
                    last_sent = speed;
                }
                requests.push(r);
            });
            if tune.finished() {
                break;
            }
            now += tune.settle + Duration::from_secs(1);
        }
        assert_eq!(tune.fans[0].safe_speed, 1.0, "the fan keeps full speed");
        assert!(
            requests.iter().any(|r| matches!(
                r,
                FanRequest::SetFanMode { mode_json, .. } if mode_json.contains("1.0")
            )),
            "a failed probe is reverted at once"
        );
        assert!(tune.report().contains("cannot go below 100%"));
        // Every failed probe is reverted and re-anchored, so the bracket
        // closes just under full speed instead of wandering down the fan.
        let unsafe_speed = tune.fans[0].unsafe_speed.expect("probes failed");
        assert!(
            (0.89..1.0).contains(&unsafe_speed),
            "the search stopped at its first probe instead of bisecting the top: {unsafe_speed}"
        );
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
