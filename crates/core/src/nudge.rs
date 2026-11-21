//! Background nudging: learning how much slower each automatic fan can
//! run, using the machine's own real-world load instead of a stress test.
//!
//! [`Nudge::step`] runs once per control-loop tick. Every automatic fan
//! arrives as a [`NudgeFan`] view and every readable temperature as a
//! [`NudgeSensor`] view. The nudger keeps a short temperature history and,
//! when conditions are trustworthy — the fan's sensor is warm and has been
//! steady for a window — trims *one* fan a little below the speed its curve
//! asks for and watches what happens:
//!
//! - nothing important gets hotter for `hold_secs` → the trim is kept (the
//!   engine writes it to [`NudgeConfig::learned`] and saves the config);
//! - a watched temperature climbs more than `rise_limit_c`, the GPU starts
//!   thermal throttling, or the load falls away → the trim is reverted.
//!
//! Slowing one fan can cost a *different* component's temperature, so a
//! trial is judged by every watched temperature (CPU, GPU, motherboard and
//! drive sensors, plus the trialled fan's own sensor), not only by the
//! sensor that fan follows.
//!
//! A trim is a multiplier on the curve, never an absolute speed: a hotter
//! day still drives the fan back up its curve, and the fan's own
//! `min_speed` floor is never crossed. Only one fan is tried at a time, so
//! every verdict belongs to the fan that was actually changed.
//!
//! ```
//! use biggest_fan_core::config::NudgeConfig;
//! use biggest_fan_core::fans::FanId;
//! use biggest_fan_core::nudge::{Nudge, NudgeFan, NudgeSensor};
//! use biggest_fan_core::sensors::SensorId;
//!
//! let config = NudgeConfig {
//!     enabled: true,
//!     hold_secs: 2,
//!     ..NudgeConfig::default()
//! };
//! let mut nudge = Nudge::default();
//! let sensor = SensorId::new("k10temp-t1");
//! let fan_id = FanId::new("nct6795-p1");
//! let fan = NudgeFan {
//!     id: fan_id.clone(),
//!     sensor: sensor.clone(),
//!     name: "Rear exhaust".to_string(),
//!     temp_c: 70.0,
//!     curve_speed: 0.8,
//!     floor: 0.0,
//!     trim: 0.0,
//!     locked: false,
//! };
//! let sensors = [NudgeSensor {
//!     id: sensor.clone(),
//!     name: "CPU temperature".to_string(),
//!     temp_c: 70.0,
//!     watched: true,
//! }];
//!
//! // Warm, steady ticks first, then the nudger asks for a trim.
//! let mut asked_for_trim = false;
//! for _tick in 0..20 {
//!     let effects = nudge.step(&config, 1000, false, &[fan.clone()], &sensors);
//!     asked_for_trim |= !effects.changes.is_empty();
//! }
//! assert!(asked_for_trim, "a warm, steady fan is worth a trial");
//! ```

use crate::config::model::NudgeConfig;
use crate::fans::FanId;
use crate::sensors::SensorId;
use std::collections::{BTreeMap, VecDeque};

/// Below this curve speed there is nothing to gain by slowing a fan down.
const MIN_CURVE_SPEED: f64 = 0.05;

/// Assumed control-loop period before the first tick hands the real one over.
const DEFAULT_POLL_MS: u64 = 1000;

/// One automatic fan as the nudger sees it on this tick.
#[derive(Clone, Debug)]
pub struct NudgeFan {
    /// Fan group id, as the config knows it.
    pub id: FanId,
    /// The sensor this fan's auto mode follows.
    pub sensor: SensorId,
    /// Display name, for the log.
    pub name: String,
    /// Latest reading of [`Self::sensor`].
    pub temp_c: f64,
    /// Speed the curve asks for at [`Self::temp_c`], before any trim.
    pub curve_speed: f64,
    /// The fan's own speed floor (`min_speed`).
    pub floor: f64,
    /// Trim currently applied to this fan (0.0 = nothing learned yet).
    pub trim: f64,
    /// True when this fan must not be nudged: a cooler the hardware drives
    /// itself (an NVIDIA card), where fighting the curve would fight the
    /// driver.
    pub locked: bool,
}

/// One temperature as the nudger sees it on this tick.
#[derive(Clone, Debug)]
pub struct NudgeSensor {
    /// Sensor id, as the config knows it.
    pub id: SensorId,
    /// Display name, for the log.
    pub name: String,
    /// Latest reading.
    pub temp_c: f64,
    /// Whether a trial must be judged against this sensor: CPU, GPU,
    /// motherboard and drive temperatures are, so a slowdown that costs one
    /// of them is rejected even when the fan's own sensor says nothing.
    pub watched: bool,
}

/// One fan whose trim the engine must change this tick.
#[derive(Debug)]
pub struct NudgeChange {
    /// Fan group to change.
    pub id: FanId,
    /// New trim: the fan's speed becomes `curve_speed * (1.0 - trim)`.
    pub trim: f64,
    /// True when a trial has just started: the fan's temperature hold must
    /// be released, or the hysteresis band would keep the old speed and the
    /// trial would measure nothing.
    pub lift_hold: bool,
}

/// What one [`Nudge::step`] decided.
#[derive(Debug, Default)]
pub struct NudgeEffects {
    /// Trims for the engine to apply.
    pub changes: Vec<NudgeChange>,
    /// A learned trim changed, so the config is worth saving.
    pub save_needed: bool,
}

/// The temperature a sensor was sitting on when a trial started, and
/// whether crossing it counts against the trial.
#[derive(Debug)]
struct Plateau {
    id: SensorId,
    name: String,
    temp_c: f64,
    watched: bool,
}

/// The trial in progress.
#[derive(Debug)]
struct Trial {
    id: FanId,
    name: String,
    /// The sensor this fan follows, so a dropped load can be spotted on it.
    sensor: SensorId,
    /// Trim the fan had before the trial; restored when a trial fails.
    accepted_trim: f64,
    /// The trim being tried.
    wanted_trim: f64,
    /// What each sensor was reading before the trim, hottest reading of its
    /// recent window.
    plateaus: Vec<Plateau>,
    /// Ticks left to watch before the trim counts as held.
    ticks_left: u64,
}

/// Cross-tick memory for the nudger, owned by [`crate::engine::Engine`]
/// and reset whenever a new config is loaded.
#[derive(Debug)]
pub struct Nudge {
    /// Control-loop ticks seen so far (the nudger's clock).
    tick: u64,
    /// Control-loop period, refreshed from the engine on every tick, so
    /// `hold_secs` and `cooldown_secs` can be counted in ticks.
    poll_ms: u64,
    /// Recent readings of each sensor, newest last.
    history: BTreeMap<SensorId, VecDeque<f64>>,
    /// Tick of each fan's most recent trial, so fans take turns: without
    /// this the first fan in the config would be tried again for ever
    /// until it hit `max_trim`.
    last_trial: BTreeMap<FanId, u64>,
    trial: Option<Trial>,
    /// Tick at which the next trial may start.
    next_trial_tick: u64,
    /// Log lines the daemon prints.
    notes: Vec<String>,
}

impl Default for Nudge {
    fn default() -> Self {
        Self {
            tick: 0,
            poll_ms: DEFAULT_POLL_MS,
            history: BTreeMap::new(),
            last_trial: BTreeMap::new(),
            trial: None,
            next_trial_tick: 0,
            notes: Vec::new(),
        }
    }
}

impl Nudge {
    /// Advances the nudger one tick and reports the fan changes it wants.
    ///
    /// `gpu_throttling` is NVML's thermal-throttle flag for any GPU: a
    /// throttling card ends a trial immediately, because a learned trim is
    /// only trustworthy while the card never slows itself down.
    #[must_use = "the nudger's fan changes must be applied"]
    pub fn step(
        &mut self,
        config: &NudgeConfig,
        poll_ms: u64,
        gpu_throttling: bool,
        fans: &[NudgeFan],
        sensors: &[NudgeSensor],
    ) -> NudgeEffects {
        self.tick = self.tick.saturating_add(1);
        self.poll_ms = poll_ms.max(1);
        let window = window_ticks(config, self.poll_ms);
        for sensor in sensors {
            let readings = self.history.entry(sensor.id.clone()).or_default();
            readings.push_back(sensor.temp_c);
            while readings.len() > window {
                readings.pop_front();
            }
        }

        let mut effects = NudgeEffects::default();
        if let Some(trial) = self.trial.take() {
            self.settle_trial(config, gpu_throttling, fans, sensors, trial, &mut effects);
        }
        if self.trial.is_none() {
            self.start_trial(config, fans, sensors, &mut effects);
        }
        effects
    }

    /// The trim currently being trialled on this fan, if it is on trial.
    /// A trial's trim is never written to the config until it is kept.
    #[must_use]
    pub fn trial_trim(&self, fan_id: &FanId) -> Option<f64> {
        self.trial
            .as_ref()
            .filter(|trial| trial.id == *fan_id)
            .map(|trial| trial.wanted_trim)
    }

    /// Cancels a trial on one fan (the user just took that fan out of auto
    /// mode, for example). Nothing else needs undoing: a trial's trim was
    /// never in [`NudgeConfig::learned`].
    pub fn cancel_fan(&mut self, config: &NudgeConfig, fan_id: &FanId) {
        let Some(trial) = self.trial.take() else {
            return;
        };
        if trial.id != *fan_id {
            self.trial = Some(trial);
            return;
        }
        self.notes.push(format!(
            "{}: nudge trial cancelled (its mode was changed)",
            trial.name
        ));
        self.next_trial_tick = self
            .tick
            .saturating_add(cooldown_ticks(config, self.poll_ms));
    }

    /// Takes the log lines accumulated since the last call.
    pub fn drain_notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notes)
    }

    /// Adds a line for the daemon log (a learned trim that could not be
    /// saved, for example).
    pub fn push_note(&mut self, note: String) {
        self.notes.push(note);
    }

    /// Decides what to do with the trial in progress: keep it, revert it,
    /// or keep waiting.
    fn settle_trial(
        &mut self,
        config: &NudgeConfig,
        gpu_throttling: bool,
        fans: &[NudgeFan],
        sensors: &[NudgeSensor],
        mut trial: Trial,
        effects: &mut NudgeEffects,
    ) {
        if fans.iter().all(|fan| fan.id != trial.id) {
            self.revert(config, &trial, "it stopped being an automatic fan", effects);
            return;
        }
        if gpu_throttling {
            self.revert(config, &trial, "the GPU began thermal throttling", effects);
            return;
        }
        // Any watched sensor climbing over its plateau says the fan is
        // needed where it is, whoever caused the climb.
        for plateau in &trial.plateaus {
            if !plateau.watched {
                continue;
            }
            let Some(current) = sensors
                .iter()
                .find(|sensor| sensor.id == plateau.id)
                .map(|sensor| sensor.temp_c)
            else {
                continue;
            };
            let rise = current - plateau.temp_c;
            if rise > config.rise_limit_c {
                self.revert(
                    config,
                    &trial,
                    &format!("{} climbed {rise:.1} °C above its plateau", plateau.name),
                    effects,
                );
                return;
            }
        }

        trial.ticks_left = trial.ticks_left.saturating_sub(1);
        if trial.ticks_left > 0 {
            self.trial = Some(trial);
            return;
        }
        // Held long enough — unless the load went away and the quiet hold
        // proved nothing.
        let own_plateau = trial
            .plateaus
            .iter()
            .find(|plateau| plateau.id == trial.sensor)
            .map(|plateau| plateau.temp_c);
        let own_now = sensors
            .iter()
            .find(|sensor| sensor.id == trial.sensor)
            .map(|sensor| sensor.temp_c);
        let load_fell_away = match (own_plateau, own_now) {
            (Some(plateau), Some(current)) => current < plateau - config.drop_limit_c,
            _ => true,
        };
        if load_fell_away {
            self.revert(
                config,
                &trial,
                "the load fell away, so the trial proved nothing",
                effects,
            );
        } else {
            // Keeping it: the engine writes the trim into the config.
            effects.changes.push(NudgeChange {
                id: trial.id.clone(),
                trim: trial.wanted_trim,
                lift_hold: false,
            });
            effects.save_needed = true;
            self.notes.push(format!(
                "{}: now runs {:.0}% below its curve",
                trial.name,
                trial.wanted_trim * 100.0
            ));
            self.next_trial_tick = self
                .tick
                .saturating_add(cooldown_ticks(config, self.poll_ms));
        }
    }

    /// Puts the fan back at the trim it had before the trial.
    fn revert(
        &mut self,
        config: &NudgeConfig,
        trial: &Trial,
        why: &str,
        effects: &mut NudgeEffects,
    ) {
        effects.changes.push(NudgeChange {
            id: trial.id.clone(),
            trim: trial.accepted_trim,
            lift_hold: false,
        });
        self.notes.push(format!(
            "{}: back to {:.0}% below its curve — {why}",
            trial.name,
            trial.accepted_trim * 100.0
        ));
        self.next_trial_tick = self
            .tick
            .saturating_add(cooldown_ticks(config, self.poll_ms));
    }

    /// Starts a trial on the fan that has waited longest for one.
    fn start_trial(
        &mut self,
        config: &NudgeConfig,
        fans: &[NudgeFan],
        sensors: &[NudgeSensor],
        effects: &mut NudgeEffects,
    ) {
        if !config.enabled || self.tick < self.next_trial_tick {
            return;
        }
        let Some(fan) = self.candidate(config, fans) else {
            return;
        };
        let next_trim = (fan.trim + config.step).min(config.max_trim);
        self.last_trial.insert(fan.id.clone(), self.tick);
        self.trial = Some(Trial {
            id: fan.id.clone(),
            name: fan.name.clone(),
            sensor: fan.sensor.clone(),
            accepted_trim: fan.trim,
            wanted_trim: next_trim,
            plateaus: plateaus(&self.history, sensors),
            ticks_left: hold_ticks(config, self.poll_ms),
        });
        effects.changes.push(NudgeChange {
            id: fan.id.clone(),
            trim: next_trim,
            lift_hold: true,
        });
        self.notes.push(format!(
            "{}: trying {:.0}% below its curve",
            fan.name,
            next_trim * 100.0
        ));
    }

    /// The fan to trial next: of those that currently deserve a trial, the
    /// one that has gone longest without one (a fan never tried wins, and
    /// ties keep config order).
    fn candidate<'a>(&self, config: &NudgeConfig, fans: &'a [NudgeFan]) -> Option<&'a NudgeFan> {
        let window = window_ticks(config, self.poll_ms);
        fans.iter()
            .filter(|fan| Self::eligible(config, fan, &self.history, window))
            .min_by_key(|fan| self.last_trial.get(&fan.id).copied())
    }

    /// Whether this fan can be trialled right now: automatic, unlocked,
    /// warm, sitting on a steady plateau, and with somewhere left to go
    /// above its own floor.
    fn eligible(
        config: &NudgeConfig,
        fan: &NudgeFan,
        history: &BTreeMap<SensorId, VecDeque<f64>>,
        window: usize,
    ) -> bool {
        if fan.locked || fan.temp_c < config.min_temp_c || fan.curve_speed <= MIN_CURVE_SPEED {
            return false;
        }
        // Only trial from a plateau: a sensor that has been climbing or
        // falling is moving for reasons this trial cannot see.
        let Some(readings) = history.get(&fan.sensor) else {
            return false;
        };
        if readings.len() < window {
            return false;
        }
        let (lowest, highest) = range(readings);
        if highest - lowest > config.steady_band_c {
            return false;
        }
        let next_trim = (fan.trim + config.step).min(config.max_trim);
        if next_trim <= fan.trim {
            return false;
        }
        // The fan's own floor must still be clearable; trialling a fan
        // that the floor would hold anyway teaches nothing.
        fan.curve_speed * (1.0 - next_trim) > fan.floor
    }
}

/// What every sensor was last sitting on, hottest reading of its window.
fn plateaus(history: &BTreeMap<SensorId, VecDeque<f64>>, sensors: &[NudgeSensor]) -> Vec<Plateau> {
    sensors
        .iter()
        .filter_map(|sensor| {
            history
                .get(&sensor.id)
                .filter(|readings| !readings.is_empty())
                .map(|readings| Plateau {
                    id: sensor.id.clone(),
                    name: sensor.name.clone(),
                    temp_c: range(readings).1,
                    watched: sensor.watched,
                })
        })
        .collect()
}

/// Ticks a trial must hold: `hold_secs` of control-loop ticks, at least one.
fn hold_ticks(config: &NudgeConfig, poll_ms: u64) -> u64 {
    let poll = poll_ms.max(1);
    (config.hold_secs.saturating_mul(1000) / poll).max(1)
}

/// Ticks to wait after a trial before starting the next one: at least one,
/// so a verdict and the trial after it never share a tick.
fn cooldown_ticks(config: &NudgeConfig, poll_ms: u64) -> u64 {
    let poll = poll_ms.max(1);
    (config.cooldown_secs.saturating_mul(1000) / poll).max(1)
}

/// Readings of one sensor kept, and required before a trial starts: a
/// quarter of the hold window, never fewer than four.
fn window_ticks(config: &NudgeConfig, poll_ms: u64) -> usize {
    usize::try_from(hold_ticks(config, poll_ms) / 4)
        .unwrap_or(usize::MAX)
        .max(4)
}

/// Coldest and hottest of a window of readings.
fn range(readings: &VecDeque<f64>) -> (f64, f64) {
    let mut lowest = f64::MAX;
    let mut highest = f64::MIN;
    for reading in readings {
        lowest = lowest.min(*reading);
        highest = highest.max(*reading);
    }
    (lowest, highest)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A nudger config with second-scale timings, so tests stay quick.
    fn config() -> NudgeConfig {
        NudgeConfig {
            enabled: true,
            step: 0.1,
            max_trim: 0.3,
            hold_secs: 5,
            rise_limit_c: 1.5,
            drop_limit_c: 2.0,
            min_temp_c: 55.0,
            steady_band_c: 1.0,
            cooldown_secs: 0,
            learned: BTreeMap::new(),
        }
    }

    const POLL_MS: u64 = 1000;

    /// One auto fan at `temp_c`, everything else at the defaults.
    fn fan(id: &FanId, sensor: &SensorId, temp_c: f64, trim: f64) -> NudgeFan {
        NudgeFan {
            id: id.clone(),
            sensor: sensor.clone(),
            name: "Rear exhaust".to_string(),
            temp_c,
            curve_speed: 0.8,
            floor: 0.0,
            trim,
            locked: false,
        }
    }

    /// One watched sensor at `temp_c`.
    fn sensor_reading(sensor: &SensorId, temp_c: f64) -> NudgeSensor {
        NudgeSensor {
            id: sensor.clone(),
            name: "CPU temperature".to_string(),
            temp_c,
            watched: true,
        }
    }

    /// An unwatched sensor (a case temp the trial is allowed to move).
    fn side_sensor(id: &str, temp_c: f64) -> NudgeSensor {
        NudgeSensor {
            id: SensorId::new(id),
            name: "Case temperature".to_string(),
            temp_c,
            watched: false,
        }
    }

    /// A watched sensor with a name the log lines can be checked against.
    fn watched_named(id: &SensorId, name: &str, temp_c: f64) -> NudgeSensor {
        NudgeSensor {
            id: id.clone(),
            name: name.to_string(),
            temp_c,
            watched: true,
        }
    }

    /// Fills the sample history up to the tick before a trial becomes
    /// possible, asserting nothing changed along the way.
    fn fill_history(nudge: &mut Nudge, settings: &NudgeConfig, fan: &NudgeFan, sensor: &SensorId) {
        let ticks = window_ticks(settings, POLL_MS) - 1;
        for _ in 0..ticks {
            let effects = nudge.step(
                settings,
                POLL_MS,
                false,
                std::slice::from_ref(fan),
                std::slice::from_ref(&sensor_reading(sensor, fan.temp_c)),
            );
            assert!(
                effects.changes.is_empty(),
                "warming up the history should not change anything"
            );
        }
    }

    /// Steps one tick that is expected to start a trial, and checks it did.
    fn start_a_trial(nudge: &mut Nudge, settings: &NudgeConfig, fan: &NudgeFan, sensor: &SensorId) {
        let effects = nudge.step(
            settings,
            POLL_MS,
            false,
            std::slice::from_ref(fan),
            std::slice::from_ref(&sensor_reading(sensor, fan.temp_c)),
        );
        assert_eq!(effects.changes.len(), 1, "this step starts the trial");
    }

    #[test]
    fn disabled_never_trials_a_fan() {
        let mut nudge = Nudge::default();
        let id = FanId::new("nct6795-p1");
        let sensor = SensorId::new("k10temp-t1");
        let fan = fan(&id, &sensor, 70.0, 0.0);
        let quiet = NudgeConfig {
            enabled: false,
            ..config()
        };
        for _ in 0..30 {
            let effects = nudge.step(
                &quiet,
                POLL_MS,
                false,
                std::slice::from_ref(&fan),
                std::slice::from_ref(&sensor_reading(&sensor, 70.0)),
            );
            assert!(effects.changes.is_empty());
            assert!(!effects.save_needed);
        }
    }

    #[test]
    fn a_warm_steady_fan_earns_a_trial() {
        let mut nudge = Nudge::default();
        let id = FanId::new("nct6795-p1");
        let sensor = SensorId::new("k10temp-t1");
        let fan = fan(&id, &sensor, 70.0, 0.0);
        // The history only needs `window_ticks` samples (a quarter of the
        // hold, floored at four), so the very next step can start a trial.
        fill_history(&mut nudge, &config(), &fan, &sensor);
        let effects = nudge.step(
            &config(),
            POLL_MS,
            false,
            std::slice::from_ref(&fan),
            std::slice::from_ref(&sensor_reading(&sensor, 70.0)),
        );
        assert_eq!(effects.changes.len(), 1, "one fan at a time");
        assert_eq!(effects.changes[0].id, id);
        assert!(
            (effects.changes[0].trim - 0.1).abs() < 1e-9,
            "one step of trim"
        );
        assert!(
            effects.changes[0].lift_hold,
            "the trial must be allowed to reach the slower speed"
        );
        assert!(!effects.save_needed, "a trial in progress saves nothing");
    }

    #[test]
    fn a_quiet_hold_keeps_the_trim_and_asks_to_save() {
        let mut nudge = Nudge::default();
        let id = FanId::new("nct6795-p1");
        let sensor = SensorId::new("k10temp-t1");
        let fan = fan(&id, &sensor, 70.0, 0.0);
        fill_history(&mut nudge, &config(), &fan, &sensor);
        start_a_trial(&mut nudge, &config(), &fan, &sensor);
        assert!(
            nudge.trial_trim(&id).is_some(),
            "the trial's trim is held by the nudger, not the config"
        );
        // hold_secs = 5 ticks of watching, nothing gets hotter.
        for tick in 0..5 {
            let effects = nudge.step(
                &config(),
                POLL_MS,
                false,
                std::slice::from_ref(&fan),
                std::slice::from_ref(&sensor_reading(&sensor, 70.0)),
            );
            if tick == 4 {
                assert_eq!(effects.changes.len(), 1, "the held trim is kept");
                assert!((effects.changes[0].trim - 0.1).abs() < 1e-9);
                assert!(effects.save_needed, "the hold is over — keep it");
            } else {
                assert!(effects.changes.is_empty(), "still holding on tick {tick}");
                assert!(!effects.save_needed);
            }
        }
        assert!(nudge.trial.is_none(), "the trial is finished");
        assert!(nudge.trial_trim(&id).is_none());
    }

    #[test]
    fn a_temperature_rise_reverts_the_trim() {
        let mut nudge = Nudge::default();
        let id = FanId::new("nct6795-p1");
        let sensor = SensorId::new("k10temp-t1");
        let steady = fan(&id, &sensor, 70.0, 0.0);
        fill_history(&mut nudge, &config(), &steady, &sensor);
        start_a_trial(&mut nudge, &config(), &steady, &sensor);

        // The trim did hurt: 72 °C is 2 °C above the 70 °C plateau.
        let hot = fan(&id, &sensor, 72.0, 0.1);
        let effects = nudge.step(
            &config(),
            POLL_MS,
            false,
            std::slice::from_ref(&hot),
            std::slice::from_ref(&sensor_reading(&sensor, 72.0)),
        );
        assert_eq!(effects.changes.len(), 1, "the trim is reverted");
        assert!(effects.changes[0].trim.abs() < 1e-9);
        assert!(
            !effects.save_needed,
            "nothing was learned, so the config is unchanged"
        );
        assert!(nudge.trial.is_none());
    }

    #[test]
    fn a_hotter_component_elsewhere_reverts_the_trim_too() {
        let mut nudge = Nudge::default();
        let id = FanId::new("arctic_fan-p1");
        // A case fan following a case sensor: its own sensor may be calm
        // while the GPU pays for the slower air.
        let own = SensorId::new("nct6799-t1");
        let gpu = SensorId::new("nvidia-gpu0-t1");
        let steady = NudgeFan {
            sensor: own.clone(),
            temp_c: 45.0,
            ..fan(&id, &own, 45.0, 0.0)
        };
        let settings = NudgeConfig {
            min_temp_c: 40.0,
            ..config()
        };
        for _ in 0..3 {
            let effects = nudge.step(
                &settings,
                POLL_MS,
                false,
                std::slice::from_ref(&steady),
                &[
                    watched_named(&gpu, "GPU temperature", 50.0),
                    side_sensor(own.as_str(), 45.0),
                ],
            );
            assert!(effects.changes.is_empty(), "warming up the history");
        }
        let started = nudge.step(
            &settings,
            POLL_MS,
            false,
            std::slice::from_ref(&steady),
            &[
                watched_named(&gpu, "GPU temperature", 50.0),
                side_sensor(own.as_str(), 45.0),
            ],
        );
        assert_eq!(started.changes.len(), 1, "this step starts the trial");
        assert!(nudge.trial.is_some(), "the trial is running");

        // The GPU climbs 4 °C while the case sensor stays put.
        let effects = nudge.step(
            &settings,
            POLL_MS,
            false,
            std::slice::from_ref(&steady),
            &[
                watched_named(&gpu, "GPU temperature", 54.0),
                side_sensor(own.as_str(), 45.0),
            ],
        );
        assert_eq!(effects.changes.len(), 1, "the trim is reverted");
        assert!(effects.changes[0].trim.abs() < 1e-9);
        assert!(
            nudge
                .notes
                .iter()
                .any(|note| note.contains("GPU temperature climbed")),
            "the log should name the component: {:?}",
            nudge.notes
        );
    }

    #[test]
    fn an_unwatched_sensor_may_move_freely() {
        let mut nudge = Nudge::default();
        let id = FanId::new("arctic_fan-p1");
        let own = SensorId::new("nct6799-t1");
        let steady = NudgeFan {
            sensor: own.clone(),
            temp_c: 45.0,
            ..fan(&id, &own, 45.0, 0.0)
        };
        let settings = NudgeConfig {
            min_temp_c: 40.0,
            ..config()
        };
        fill_history(&mut nudge, &settings, &steady, &own);
        let started = nudge.step(
            &settings,
            POLL_MS,
            false,
            std::slice::from_ref(&steady),
            &[side_sensor(own.as_str(), 45.0)],
        );
        assert_eq!(started.changes.len(), 1, "this step starts the trial");
        // A case temperature drifting up is the point of the trial, not a
        // reason to abandon it.
        for _ in 0..4 {
            let effects = nudge.step(
                &settings,
                POLL_MS,
                false,
                std::slice::from_ref(&steady),
                &[side_sensor(own.as_str(), 45.0)],
            );
            assert!(effects.changes.is_empty(), "still holding");
        }
        assert!(nudge.trial.is_some(), "the trial is still running");
    }

    #[test]
    fn gpu_throttling_ends_a_trial_at_once() {
        let mut nudge = Nudge::default();
        let id = FanId::new("nct6795-p1");
        let sensor = SensorId::new("k10temp-t1");
        let steady = fan(&id, &sensor, 70.0, 0.0);
        fill_history(&mut nudge, &config(), &steady, &sensor);
        start_a_trial(&mut nudge, &config(), &steady, &sensor);

        let effects = nudge.step(
            &config(),
            POLL_MS,
            true,
            std::slice::from_ref(&steady),
            std::slice::from_ref(&sensor_reading(&sensor, 70.0)),
        );
        assert_eq!(effects.changes.len(), 1, "the trim is reverted");
        assert!(effects.changes[0].trim.abs() < 1e-9);
        assert!(
            nudge
                .notes
                .iter()
                .any(|note| note.contains("thermal throttling")),
            "the log should say why: {:?}",
            nudge.notes
        );
    }

    #[test]
    fn a_dropped_load_proves_nothing_and_is_reverted() {
        let mut nudge = Nudge::default();
        let id = FanId::new("nct6795-p1");
        let sensor = SensorId::new("k10temp-t1");
        let steady = fan(&id, &sensor, 70.0, 0.0);
        fill_history(&mut nudge, &config(), &steady, &sensor);
        start_a_trial(&mut nudge, &config(), &steady, &sensor);

        // Temps fell 5 °C: the machine went idle, so a quiet hold here
        // would be a false pass.
        let settings = config();
        for _ in 0..=settings.hold_secs {
            let cool = fan(&id, &sensor, 65.0, 0.1);
            let effects = nudge.step(
                &settings,
                POLL_MS,
                false,
                std::slice::from_ref(&cool),
                std::slice::from_ref(&sensor_reading(&sensor, 65.0)),
            );
            if !effects.changes.is_empty() {
                assert!(effects.changes[0].trim.abs() < 1e-9);
                return;
            }
        }
        panic!("the trial should have been reverted as inconclusive");
    }

    #[test]
    fn a_climbing_sensor_is_never_a_starting_point() {
        let mut nudge = Nudge::default();
        let id = FanId::new("nct6795-p1");
        let sensor = SensorId::new("k10temp-t1");
        let settings = config();
        for step in 0..20 {
            let temp_c = 60.0 + f64::from(step);
            let rising = fan(&id, &sensor, temp_c, 0.0);
            let effects = nudge.step(
                &settings,
                POLL_MS,
                false,
                std::slice::from_ref(&rising),
                std::slice::from_ref(&sensor_reading(&sensor, temp_c)),
            );
            assert!(
                effects.changes.is_empty(),
                "a sensor climbing {step} °C is not a plateau"
            );
        }
    }

    #[test]
    fn cool_locked_and_floored_fans_are_left_alone() {
        let id = FanId::new("nct6795-p4");
        let sensor = SensorId::new("k10temp-t1");
        let settings = config();

        let cool = fan(&id, &sensor, 40.0, 0.0);
        let locked = NudgeFan {
            locked: true,
            ..fan(&id, &sensor, 70.0, 0.0)
        };
        // The curve wants 0.8 and the floor is 0.75: no trim can be seen.
        let floored = NudgeFan {
            floor: 0.75,
            ..fan(&id, &sensor, 70.0, 0.0)
        };
        for other in [&cool, &locked, &floored] {
            let mut nudge = Nudge::default();
            for _ in 0..30 {
                let effects = nudge.step(
                    &settings,
                    POLL_MS,
                    false,
                    std::slice::from_ref(other),
                    std::slice::from_ref(&sensor_reading(&sensor, other.temp_c)),
                );
                assert!(effects.changes.is_empty());
            }
        }
    }

    #[test]
    fn trim_stops_at_max_trim() {
        let mut nudge = Nudge::default();
        let id = FanId::new("nct6795-p1");
        let sensor = SensorId::new("k10temp-t1");
        // Already learned 0.3, which is max_trim: nothing left to try.
        let trimmed = fan(&id, &sensor, 70.0, 0.3);
        for _ in 0..30 {
            let effects = nudge.step(
                &config(),
                POLL_MS,
                false,
                std::slice::from_ref(&trimmed),
                std::slice::from_ref(&sensor_reading(&sensor, 70.0)),
            );
            assert!(effects.changes.is_empty(), "0.3 is the ceiling");
        }
    }

    #[test]
    fn accepted_trials_walk_down_to_the_ceiling() {
        let mut nudge = Nudge::default();
        let id = FanId::new("nct6795-p1");
        let sensor = SensorId::new("k10temp-t1");
        let settings = config();
        let mut learned = 0.0;
        let mut kept = 0;
        // Far more ticks than three trials need: the trim should walk to
        // 0.1, 0.2, 0.3 and then stop at max_trim.
        for _tick in 0..200 {
            let effects = nudge.step(
                &settings,
                POLL_MS,
                false,
                std::slice::from_ref(&fan(&id, &sensor, 70.0, learned)),
                std::slice::from_ref(&sensor_reading(&sensor, 70.0)),
            );
            for change in effects.changes {
                learned = change.trim;
            }
            kept += usize::from(effects.save_needed);
        }
        assert!(
            (learned - settings.max_trim).abs() < 1e-9,
            "the nudger should walk down to the ceiling, stopped at {learned}"
        );
        assert_eq!(kept, 3, "one save per accepted trim");
    }

    #[test]
    fn a_fan_that_stops_being_automatic_ends_its_trial() {
        let mut nudge = Nudge::default();
        let id = FanId::new("nct6795-p1");
        let sensor = SensorId::new("k10temp-t1");
        let steady = fan(&id, &sensor, 70.0, 0.0);
        let settings = config();
        fill_history(&mut nudge, &settings, &steady, &sensor);
        start_a_trial(&mut nudge, &settings, &steady, &sensor);

        // The fan left the lineup (hidden, or parked as disconnected).
        let effects = nudge.step(
            &settings,
            POLL_MS,
            false,
            &[],
            std::slice::from_ref(&sensor_reading(&sensor, 70.0)),
        );
        assert_eq!(effects.changes.len(), 1, "the trial trim is dropped");
        assert!(effects.changes[0].trim.abs() < 1e-9);
        assert!(nudge.trial.is_none());
    }

    #[test]
    fn cancelling_a_trial_forgets_it() {
        let mut nudge = Nudge::default();
        let id = FanId::new("nct6795-p1");
        let other = FanId::new("arctic_fan-p1");
        let sensor = SensorId::new("k10temp-t1");
        let settings = config();
        let steady = fan(&id, &sensor, 70.0, 0.0);
        fill_history(&mut nudge, &settings, &steady, &sensor);
        start_a_trial(&mut nudge, &settings, &steady, &sensor);

        nudge.cancel_fan(&settings, &other);
        assert!(
            nudge.trial_trim(&id).is_some(),
            "a different fan's trial is untouched"
        );
        nudge.cancel_fan(&settings, &id);
        assert!(nudge.trial.is_none(), "the trial is gone");
        assert!(
            nudge.trial_trim(&id).is_none(),
            "an unproven trim leaves with the trial"
        );
    }

    #[test]
    fn two_eligible_fans_take_turns() {
        let mut nudge = Nudge::default();
        let settings = config();
        let first = FanId::new("nct6795-p1");
        let second = FanId::new("nct6795-p2");
        let first_sensor = SensorId::new("k10temp-t1");
        let second_sensor = SensorId::new("k10temp-t2");
        let fans = [
            fan(&first, &first_sensor, 70.0, 0.0),
            fan(&second, &second_sensor, 70.0, 0.0),
        ];
        let readings = [
            sensor_reading(&first_sensor, 70.0),
            sensor_reading(&second_sensor, 70.0),
        ];

        // A trial needs a full window of steady readings first, then the
        // first fan in config order goes first (both have never been tried).
        for _ in 0..window_ticks(&settings, POLL_MS) - 1 {
            let effects = nudge.step(&settings, POLL_MS, false, &fans, &readings);
            assert!(
                effects.changes.is_empty(),
                "warming up the history should not change anything"
            );
        }
        let effects = nudge.step(&settings, POLL_MS, false, &fans, &readings);
        assert_eq!(effects.changes.len(), 1);
        assert_eq!(effects.changes[0].id, first, "the first fan waits longest");

        // Its hold completes and is kept, then the next trial must go to
        // the other fan, not the one that was just trialled.
        for _ in 1..hold_ticks(&settings, POLL_MS) {
            let effects = nudge.step(&settings, POLL_MS, false, &fans, &readings);
            assert!(effects.changes.is_empty(), "the trial is still holding");
        }
        let effects = nudge.step(&settings, POLL_MS, false, &fans, &readings);
        assert_eq!(effects.changes.len(), 1, "the verdict is kept");
        assert!(effects.save_needed);
        let effects = nudge.step(&settings, POLL_MS, false, &fans, &readings);
        assert_eq!(effects.changes.len(), 1);
        assert_eq!(effects.changes[0].id, second, "fans take turns");

        // The second fan's hold completes too; then the first fan, which
        // has again waited longest, is up again.
        for _ in 1..hold_ticks(&settings, POLL_MS) {
            let effects = nudge.step(&settings, POLL_MS, false, &fans, &readings);
            assert!(effects.changes.is_empty(), "the trial is still holding");
        }
        let effects = nudge.step(&settings, POLL_MS, false, &fans, &readings);
        assert_eq!(effects.changes.len(), 1, "the second verdict is kept");
        let effects = nudge.step(&settings, POLL_MS, false, &fans, &readings);
        assert_eq!(effects.changes.len(), 1);
        assert_eq!(
            effects.changes[0].id, first,
            "back to the fan that waited longest"
        );
    }

    #[test]
    fn notes_are_drained_once() {
        let mut nudge = Nudge::default();
        nudge.push_note("hello".to_string());
        assert_eq!(nudge.drain_notes(), vec!["hello".to_string()]);
        assert_eq!(nudge.drain_notes(), Vec::<String>::new());
    }
}
