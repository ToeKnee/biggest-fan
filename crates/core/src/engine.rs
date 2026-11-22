//! The control loop: discover hardware, apply fan modes with hysteresis,
//! and report live status.

use crate::case::{Airflow, CaseConfig, FanSlot};
use crate::config::model::{AppConfig, FanConfig, HysteresisConfig};
use crate::error::CoreError;
use crate::fans::driver::FanDriver;
use crate::fans::model::{FanCurve, FanDescriptor, FanId, FanMode, FanStatus};
use crate::fans::nvidia::NvidiaFan;
use crate::fans::sysfs::SysfsFan;
use crate::sensors::model::{SensorDescriptor, SensorId, SensorSource, SensorStatus};
use crate::sensors::{HwmonSource, NvidiaSource, SensorCategory, sensor_category};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Everything found on the machine at startup.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Discovery {
    /// Temperature sensors from all sources.
    pub sensors: Vec<SensorDescriptor>,
    /// Controllable fan headers.
    pub fans: Vec<FanDescriptor>,
}

/// What [`Engine::resync_hardware`] found when it looked at the machine
/// again (after a resume from suspend, normally).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResyncReport {
    /// Configured headers found again and re-bound to their group.
    pub rebound: usize,
    /// Configured headers the machine is not exposing (yet); their group
    /// keeps the old handle, so their failed writes keep being reported.
    pub missing_headers: Vec<String>,
    /// Headers the machine now exposes that no group drives, not even one
    /// parked as `hidden` — a header the config has never heard of.
    pub unclaimed_headers: Vec<String>,
}

/// Discovers sensors and fan headers.
///
/// `hwmon_root` is normally `/sys/class/hwmon`; tests may point it at a
/// fixture tree. `use_nvidia` controls whether NVML is attempted — the
/// binaries always attempt it and need no NVIDIA-related configuration:
/// a machine without NVIDIA management just discovers no NVIDIA
/// sensors or coolers. Tests pass `false` to stay off the GPU entirely.
///
/// # Errors
///
/// [`CoreError::Io`] when the hwmon root is unreadable, or [`CoreError::Nvidia`]
/// when NVML starts but cannot enumerate devices (a broken driver state,
/// reported rather than quietly hiding an NVIDIA GPU).
pub fn discover(hwmon_root: &Path, use_nvidia: bool) -> Result<Discovery, CoreError> {
    let mut hwmon = HwmonSource::with_root(hwmon_root);
    let mut discovery = Discovery {
        sensors: hwmon.discover()?,
        fans: SysfsFan::discover_all(hwmon_root)?
            .iter()
            .map(SysfsFan::descriptor)
            .collect(),
    };
    if use_nvidia {
        if let Some(mut nvidia) = NvidiaSource::try_new()? {
            discovery.sensors.extend(nvidia.discover()?);
        }
        // GPU fans are driven through NVML, not sysfs headers.
        discovery
            .fans
            .extend(NvidiaFan::discover_all()?.iter().map(NvidiaFan::descriptor));
    }
    order_sensors(&mut discovery.sensors);
    Ok(discovery)
}

/// Orders sensors by how useful they are (CPU and GPU first, then
/// motherboard/drive temperatures, then case temperatures) and by display
/// name within a category.
fn order_sensors(sensors: &mut [SensorDescriptor]) {
    sensors.sort_by_key(|sensor| (category_of(sensor), sensor.default_name.clone()));
}

/// What kind of temperature a sensor reports: CPU, GPU, motherboard/drive,
/// case, or something else.
fn category_of(sensor: &SensorDescriptor) -> SensorCategory {
    let chip = sensor.raw_label.split_whitespace().next().unwrap_or("");
    sensor_category(chip, &sensor.default_name)
}

/// Builds a starting config for freshly discovered hardware.
///
/// Every fan runs in auto mode driven by the sensor that best matches the
/// header (`CPU_FAN` headers follow the CPU, GPU-related headers the
/// graphics card, and so on).
#[must_use]
pub fn default_config(discovery: &Discovery) -> AppConfig {
    AppConfig {
        fans: discovery
            .fans
            .iter()
            .map(|fan| FanConfig {
                id: fan.id.clone(),
                name: None,
                rpm_factor: 1.0,
                min_speed: 0.0,
                members: Vec::new(),
                hidden: false,
                mode: governing_sensor_for(&discovery.sensors, fan).map_or_else(
                    || FanMode::Fixed { speed: 0.5 },
                    |sensor| FanMode::Auto {
                        sensor,
                        curve: FanCurve::default(),
                    },
                ),
                slot: None,
            })
            .collect(),
        ..AppConfig::default()
    }
}

/// Picks the sensor most likely to govern one fan, from what the header
/// is called. Headers named after the CPU (or plain `pwm1`, the usual
/// `CPU_FAN` header) follow CPU temperatures; GPU/aux headers follow the
/// graphics card; chassis-named headers follow case temperatures where
/// the machine reports any.
fn governing_sensor_for(sensors: &[SensorDescriptor], fan: &FanDescriptor) -> Option<SensorId> {
    let hint = format!("{} {}", fan.default_name, fan.id).to_lowercase();
    let priorities: &[SensorCategory] = if hint.contains("cpu") || fan.id.as_str().ends_with("-p1")
    {
        &[
            SensorCategory::Cpu,
            SensorCategory::Motherboard,
            SensorCategory::Case,
        ]
    } else if hint.contains("gpu") || hint.contains("aux") || hint.contains("vga") {
        &[
            SensorCategory::Gpu,
            SensorCategory::Cpu,
            SensorCategory::Motherboard,
        ]
    } else if hint.contains("case")
        || hint.contains("chassis")
        || hint.contains("sys")
        || hint.contains("front")
        || hint.contains("rear")
        || hint.contains("top")
    {
        &[
            SensorCategory::Case,
            SensorCategory::Motherboard,
            SensorCategory::Cpu,
        ]
    } else {
        &[
            SensorCategory::Cpu,
            SensorCategory::Motherboard,
            SensorCategory::Case,
        ]
    };
    for category in priorities {
        let matches: Vec<&SensorDescriptor> = sensors
            .iter()
            .filter(|sensor| {
                let chip = sensor
                    .raw_label
                    .split_whitespace()
                    .next()
                    .unwrap_or_default();
                sensor_category(chip, &sensor.default_name) == *category
            })
            .collect();
        // A whole-package reading (Tctl/Tdie) governs better than one
        // CCD's sensor, so skip per-CCD names when a better one exists.
        if let Some(sensor) = matches
            .iter()
            .find(|sensor| !sensor.default_name.to_lowercase().contains("ccd"))
            .or_else(|| matches.first())
        {
            return Some(sensor.id.clone());
        }
    }
    sensors.first().map(|sensor| sensor.id.clone())
}

/// Per-run wiring of the engine: where to look and whether writes happen.
#[derive(Clone, Debug)]
pub struct EngineOptions {
    /// hwmon class directory (tests point this at a fixture).
    pub hwmon_root: PathBuf,
    /// Attempt to read NVIDIA GPUs through NVML. The binaries always keep
    /// this `true` (no NVIDIA management just discovers nothing NVIDIA);
    /// `false` is for tests and fixtures that must not touch NVML.
    pub use_nvidia: bool,
    /// When false the loop computes speeds but never touches fan headers
    /// (`--dry-run` and tests).
    pub apply_writes: bool,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            hwmon_root: PathBuf::from("/sys/class/hwmon"),
            use_nvidia: true,
            apply_writes: true,
        }
    }
}

/// What one control-loop tick observed and did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TickReport {
    /// Every known sensor with its latest reading.
    pub sensors: Vec<SensorStatus>,
    /// Every managed fan with its commanded speed and read-back RPM.
    pub fans: Vec<FanStatus>,
}

/// A fan as the case view should draw it.
#[derive(Clone, Debug, PartialEq)]
pub struct CaseFanView {
    /// Leader header id of this fan group (what slot edits address).
    pub id: FanId,
    /// Display name.
    pub name: String,
    /// Position on the case canvas.
    pub slot: FanSlot,
    /// Commanded speed fraction.
    pub speed: f64,
    /// Measured RPM when available.
    pub rpm: Option<f64>,
    /// Warning note from the last tick, if any.
    pub note: Option<String>,
}

/// Hysteresis bookkeeping for one auto fan.
#[derive(Clone, Copy, Debug, Default)]
struct FanRuntime {
    /// Speed fraction the engine last commanded (on the previous tick, or
    /// by an immediate mode change in [`Engine::set_fan_mode`]).
    last_commanded: f64,
    /// Temperature that last *raised* the speed; the speed may not fall
    /// until the sensor drops `temp_band_c` below this.
    hold_temp_c: Option<f64>,
}

#[derive(Debug)]
struct FanEntry {
    fan_config: FanConfig,
    /// Headers driven as one logical fan: the leader (`fan_config.id`) is
    /// first, followed by any extra members.
    headers: Vec<FanDriver>,
    runtime: FanRuntime,
    display_name: String,
    /// Set when a write proved these headers cannot be driven by software
    /// (the driver refuses even though the register opens for writing);
    /// the fan then carries a note and is parked as `hidden`.
    lost_control: Option<String>,
}

/// Manages the sensors and fans from the config, ticking the control loop.
pub struct Engine {
    config: AppConfig,
    config_path: PathBuf,
    apply_writes: bool,
    hwmon: HwmonSource,
    /// NVML source when the NVIDIA driver is present; also reports GPU
    /// thermal throttling for fan notes.
    nvidia: Option<NvidiaSource>,
    sensors: Vec<SensorDescriptor>,
    sensor_names: BTreeMap<SensorId, String>,
    latest: BTreeMap<SensorId, f64>,
    fans: Vec<FanEntry>,
    /// Every fan header discovered at startup, managed or not.
    all_headers: Vec<FanDriver>,
    /// Configs of headers found disconnected; skipped until re-enabled.
    hidden_fans: Vec<FanConfig>,
    last_report: TickReport,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("config", &self.config)
            .field("config_path", &self.config_path)
            .field("apply_writes", &self.apply_writes)
            .field("hwmon", &self.hwmon)
            .field("nvidia", &self.nvidia)
            .field("sensors", &self.sensors)
            .field("sensor_names", &self.sensor_names)
            .field("latest", &self.latest)
            .field("fans", &self.fans)
            .field("all_headers", &self.all_headers)
            .field("hidden_fans", &self.hidden_fans)
            .field("last_report", &self.last_report)
            .finish()
    }
}

impl Engine {
    /// Wires an engine to live hardware according to `options`, validating
    /// that every configured fan header (leader and members) and auto-mode
    /// sensor id exists. Auto fans start their ramp from the duty their
    /// header currently holds, so taking over never dips a spinning fan.
    ///
    /// # Errors
    ///
    /// [`CoreError::UnknownFan`] / [`CoreError::UnknownSensor`] when the
    /// config references hardware that is not present,
    /// [`CoreError::DuplicateFan`] for repeated fan ids,
    /// [`CoreError::DuplicateFanMember`] for headers driven by two groups,
    /// or [`CoreError::Io`] when discovery fails.
    pub fn startup(
        mut config: AppConfig,
        config_path: impl Into<PathBuf>,
        options: &EngineOptions,
    ) -> Result<Self, CoreError> {
        let mut hwmon = HwmonSource::with_root(&options.hwmon_root);
        let mut nvidia = if options.use_nvidia {
            NvidiaSource::try_new()?
        } else {
            None
        };
        let mut sensors = hwmon.discover()?;
        if let Some(nvidia) = &mut nvidia {
            sensors.extend(nvidia.discover()?);
        }
        order_sensors(&mut sensors);
        let sensor_names: BTreeMap<SensorId, String> = sensors
            .iter()
            .map(|sensor| {
                let name = config
                    .sensor_names
                    .get(sensor.id.as_str())
                    .cloned()
                    .unwrap_or_else(|| sensor.default_name.clone());
                (sensor.id.clone(), name)
            })
            .collect();

        let mut all_headers: Vec<FanDriver> = SysfsFan::discover_all(&options.hwmon_root)?
            .into_iter()
            .map(FanDriver::Sysfs)
            .collect();
        if options.use_nvidia {
            // GPU coolers join the claimable headers.
            all_headers.extend(
                NvidiaFan::discover_all()?
                    .into_iter()
                    .map(FanDriver::Nvidia),
            );
        }
        let (fans, hidden_fans) = Self::claim_fans(
            &all_headers,
            &sensor_names,
            std::mem::take(&mut config.fans),
            config.case,
        )?;

        Ok(Self {
            config,
            config_path: config_path.into(),
            apply_writes: options.apply_writes,
            hwmon,
            nvidia,
            sensors,
            sensor_names,
            latest: BTreeMap::new(),
            fans,
            all_headers,
            hidden_fans,
            last_report: TickReport::default(),
        })
    }

    /// Claims configured fan groups against the discovered headers: a
    /// group's leader header comes first, every header belongs to at most
    /// one group, groups flagged `hidden` go to the parked list, slots are
    /// clamped back inside the case canvas, and managed fans end up ordered
    /// by controller name then header number.
    ///
    /// # Errors
    ///
    /// [`CoreError::DuplicateFan`], [`CoreError::DuplicateFanMember`],
    /// [`CoreError::UnknownFan`] or [`CoreError::UnknownSensor`] as for
    /// [`Engine::startup`].
    fn claim_fans(
        all_headers: &[FanDriver],
        sensor_names: &BTreeMap<SensorId, String>,
        fan_configs: Vec<FanConfig>,
        case: CaseConfig,
    ) -> Result<(Vec<FanEntry>, Vec<FanConfig>), CoreError> {
        let mut fans = Vec::new();
        let mut hidden_fans = Vec::new();
        // Headers already driven by some logical fan; each header may
        // belong to only one group.
        let mut claimed: Vec<FanId> = Vec::new();
        for mut fan_config in fan_configs {
            // A slot saved off-canvas (or grown past the edge) would put
            // the circle where no drag can ever reach it again.
            if let Some(slot) = fan_config.slot {
                fan_config.slot = Some(fit_slot(slot, &case));
            }
            if fans
                .iter()
                .any(|entry: &FanEntry| entry.headers[0].id() == &fan_config.id)
            {
                return Err(CoreError::DuplicateFan(fan_config.id.to_string()));
            }
            if fan_config.hidden {
                hidden_fans.push(fan_config);
                continue;
            }
            let member_ids: Vec<FanId> = std::iter::once(fan_config.id.clone())
                .chain(fan_config.members.iter().cloned())
                .collect();
            let mut headers = Vec::new();
            for member_id in &member_ids {
                if claimed.iter().any(|claimed_id| claimed_id == member_id) {
                    return Err(CoreError::DuplicateFanMember(member_id.to_string()));
                }
                let matched = all_headers
                    .iter()
                    .find(|fan| fan.id() == member_id)
                    .ok_or_else(|| CoreError::UnknownFan(member_id.to_string()))?;
                headers.push(matched.clone());
                claimed.push(member_id.clone());
            }
            if let FanMode::Auto { sensor, .. } = &fan_config.mode
                && !sensor_names.contains_key(sensor)
            {
                return Err(CoreError::UnknownSensor(sensor.to_string()));
            }
            let display_name = display_name_for(&fan_config, &headers);
            let runtime = claimed_runtime(&fan_config, &headers);
            fans.push(FanEntry {
                fan_config,
                headers,
                runtime,
                display_name,
                lost_control: None,
            });
        }
        // Show and save fans in controller-name then header-number order,
        // whatever order the config file listed them in.
        fans.sort_by_key(|entry| entry.headers[0].sort_key());
        Ok((fans, hidden_fans))
    }

    /// Runs one control-loop tick: read sensors, compute speeds (with
    /// hysteresis for auto fans), apply them unless dry-run, and report.
    ///
    /// # Errors
    ///
    /// [`CoreError::Io`] when a fan header write fails; per-sensor read
    /// failures are reported as fail-safe notes instead.
    pub fn tick(&mut self) -> Result<TickReport, CoreError> {
        self.latest.clear();
        if let Ok(readings) = self.hwmon.read_all() {
            self.latest.extend(readings);
        }
        if let Some(nvidia) = &mut self.nvidia
            && let Ok(readings) = nvidia.read_all()
        {
            self.latest.extend(readings);
        }

        let sensors: Vec<SensorStatus> = self
            .sensors
            .iter()
            .map(|sensor| SensorStatus {
                id: sensor.id.clone(),
                name: self.sensor_names[&sensor.id].clone(),
                kind: sensor.kind,
                temp_c: self.latest.get(&sensor.id).copied(),
            })
            .collect();

        let hysteresis = self.config.hysteresis;
        let fail_safe = self.config.fail_safe_speed.clamp(0.0, 1.0);
        let mut fans = Vec::new();
        for index in 0..self.fans.len() {
            let status = self.control_fan(index, hysteresis, fail_safe)?;
            fans.push(status);
        }

        // Park every fan whose driver refused this tick's writes (the
        // same way unconnected headers are parked), so one header that
        // software can never drive cannot stall the control loop.
        let lost: Vec<usize> = self
            .fans
            .iter()
            .enumerate()
            .filter(|(_index, entry)| entry.lost_control.is_some())
            .map(|(index, _entry)| index)
            .collect();
        for index in lost.into_iter().rev() {
            let mut entry = self.fans.remove(index);
            entry.fan_config.hidden = true;
            self.hidden_fans.push(entry.fan_config);
        }

        let report = TickReport { sensors, fans };
        self.last_report = report.clone();
        Ok(report)
    }

    /// Computes and applies one fan's speed for the current tick.
    fn control_fan(
        &mut self,
        index: usize,
        hysteresis: HysteresisConfig,
        fail_safe: f64,
    ) -> Result<FanStatus, CoreError> {
        // Work on a copy so `entry` stays free for the writes below.
        let mode = self.fans[index].fan_config.mode.clone();
        // NVML throttle state must be read before `entry`'s mutable
        // borrow of the fans list below is taken.
        let throttle_note = self.gpu_throttle_note(&mode);
        let entry = &mut self.fans[index];
        let mut note = throttle_note;
        let mut governing_temp_c = None;
        let base = entry.runtime.last_commanded;
        let mut target = base;
        // A fan with a speed floor — an AIO pump that must never stop — can
        // be taken nowhere near the floor by a curve, a slider or a ramp.
        let floor = entry.fan_config.min_speed.clamp(0.0, 1.0);

        match &mode {
            FanMode::Off => {
                entry.runtime = FanRuntime::default();
                target = 0.0;
            }
            FanMode::Fixed { speed } => {
                entry.runtime = FanRuntime::default();
                target = speed.clamp(0.0, 1.0).max(floor);
            }
            FanMode::Auto { sensor, curve } => {
                match self.latest.get(sensor).copied() {
                    None => {
                        // Fail safe: a sensor we cannot read must not leave
                        // the fan parked at some old low speed. Fail-safes
                        // bypass rate limiting so protection is immediate.
                        let fail_safe_now = fail_safe.max(floor);
                        target = fail_safe_now;
                        entry.runtime.hold_temp_c = None;
                        note = Some(fail_safe_note(&self.sensor_names, sensor, fail_safe_now));
                    }
                    Some(temp_c) => {
                        governing_temp_c = Some(temp_c);
                        let desired = curve.speed_at(temp_c);
                        if desired > base {
                            target = desired;
                            entry.runtime.hold_temp_c = Some(temp_c);
                        } else if desired < base {
                            // Hold the speed until the sensor falls a full
                            // band below the temperature that raised it.
                            let allowed = entry
                                .runtime
                                .hold_temp_c
                                .is_none_or(|hold| temp_c <= hold - hysteresis.temp_band_c);
                            if allowed {
                                target = desired;
                                entry.runtime.hold_temp_c = Some(temp_c);
                            } else {
                                target = base;
                            }
                        }
                        // Rate-limit auto changes: up like a rocket, down
                        // like a feather. Raising speed is the safe
                        // direction, so it runs at the roomy rocket step;
                        // the way back down is the feather step, so a fan
                        // that no longer needs to shout goes quiet slowly
                        // instead of pumping up and down.
                        if target > base {
                            if hysteresis.max_step_up_per_tick > 0.0 {
                                target = approach(base, target, hysteresis.max_step_up_per_tick);
                            }
                        } else if hysteresis.max_step_per_tick > 0.0 {
                            target = approach(base, target, hysteresis.max_step_per_tick);
                        }
                    }
                }
            }
        }

        let mut commanded = target.clamp(0.0, 1.0);
        if !matches!(mode, FanMode::Off) {
            // The floor outranks rate limiting: raising a fan is the safe
            // direction, so a floored fan jumps to it at once instead of
            // ramping up through the speed it is not allowed to run at.
            // _Off_ is exempt — stopping a fan by hand is the user's choice.
            commanded = commanded.max(floor);
        }
        // What the hardware will actually run, asked *before* the writes:
        // the NVIDIA mapping depends on who holds the fans beforehand, what
        // the card reads and whether a take-over hold is running, which is
        // exactly what `apply` will use. An NVIDIA cooler parks below 25 %,
        // holds 30 % inside the band, and takes a cooler the card has spun
        // above its floor at the floor itself, keeping that floor while the
        // dwell lasts (see `fans/nvidia.rs`). _Off_ never takes over: it
        // parks through `stop`, so the report must not claim otherwise.
        let effective = if matches!(mode, FanMode::Off) {
            0.0
        } else {
            entry.headers[0].effective_speed(commanded)
        };
        if self.apply_writes {
            write_headers(
                &mut entry.headers,
                &mode,
                commanded,
                &mut entry.lost_control,
            )?;
        }
        entry.runtime.last_commanded = commanded;
        // A note when the read-back contradicts the speed just written: a
        // GPU cooler whose stop the driver refused, or one that never
        // started turning. A lost header outranks that note — it is about
        // to be parked anyway.
        if let Some(readback) = entry.headers[0].speed_note(effective) {
            note.get_or_insert(readback);
        }
        if let Some(message) = &entry.lost_control {
            note = Some(message.clone());
        }

        Ok(FanStatus {
            id: entry.fan_config.id.clone(),
            name: entry.display_name.clone(),
            mode,
            // Report the speed the leader header can actually run, not
            // just what the curve asked for (an NVIDIA cooler parks below
            // 25 %, holds 30 % inside the band, and takes a card-driven
            // cooler above its floor at the floor); the ramp and hold band
            // still use the requested value, so groups that mix driver
            // kinds are approximated.
            commanded_speed: effective,
            measured_percent: entry.headers.iter().find_map(FanDriver::read_percent),
            measured_rpm: entry
                .headers
                .iter()
                .find_map(FanDriver::read_rpm)
                .map(|rpm| rpm * entry.fan_config.rpm_factor),
            governing_temp_c,
            note,
        })
    }

    /// A warning note when the auto mode's governing sensor is a GPU
    /// whose NVML throttle report currently flags a thermal slowdown.
    fn gpu_throttle_note(&self, mode: &FanMode) -> Option<String> {
        let FanMode::Auto { sensor, .. } = mode else {
            return None;
        };
        let gpu_index = gpu_index_in_sensor_id(sensor.as_str())?;
        self.nvidia.as_ref()?.is_gpu_throttled(gpu_index).then(|| {
            "GPU is thermal throttling — its temperature is above the card's limit".to_string()
        })
    }

    /// Refreshes one fan group's mode and (unless dry-run) applies it at
    /// once. `fan_id` may be the group leader or one of its members.
    /// Switching to _auto_ continues the speed ramp from the speed the fan
    /// is already at instead of restarting it at 0, so changing curve
    /// profiles (the GUI's Quiet / Balanced / Performance dropdown) never
    /// dips the fan.
    ///
    /// # Errors
    ///
    /// [`CoreError::UnknownFan`] / [`CoreError::UnknownSensor`] for invalid
    /// ids, [`CoreError::Io`] when an immediate write fails.
    pub fn set_fan_mode(&mut self, fan_id: &FanId, mode: &FanMode) -> Result<(), CoreError> {
        if let FanMode::Auto { sensor, .. } = mode
            && !self.sensor_names.contains_key(sensor)
        {
            return Err(CoreError::UnknownSensor(sensor.to_string()));
        }
        let index = self
            .find_group(fan_id)
            .ok_or_else(|| CoreError::UnknownFan(fan_id.to_string()))?;
        let entry = &mut self.fans[index];
        // The floor is applied to both the write below and the ramp baseline,
        // so a pump never sees a dip even for a tick.
        let floor = entry.fan_config.min_speed.clamp(0.0, 1.0);
        entry.fan_config.mode = mode.clone();
        entry.runtime = ramp_baseline_for(mode, entry.runtime.last_commanded, floor);
        let commanded = entry.runtime.last_commanded;
        if self.apply_writes {
            for header in &mut entry.headers {
                match mode {
                    FanMode::Off => header.stop()?,
                    FanMode::Fixed { .. } => header.apply(commanded)?,
                    FanMode::Auto { .. } => {}
                }
            }
        }
        Ok(())
    }

    /// Current mode of a managed fan (used to keep curves when re-binding).
    #[must_use]
    pub fn fan_mode(&self, fan_id: &FanId) -> Option<FanMode> {
        self.find_group(fan_id)
            .map(|index| self.fans[index].fan_config.mode.clone())
    }

    /// Index of the fan group whose leader or members include `fan_id`.
    fn find_group(&self, fan_id: &FanId) -> Option<usize> {
        self.fans
            .iter()
            .position(|entry| entry.headers.iter().any(|header| header.id() == fan_id))
    }

    /// Every fan header this machine exposes, managed or not.
    #[must_use]
    pub fn fan_headers(&self) -> Vec<FanDescriptor> {
        self.all_headers.iter().map(FanDriver::descriptor).collect()
    }

    /// Fan headers no managed group drives yet (parked ones included) —
    /// the pool the GUI's "add fan" choice picks from.
    #[must_use]
    pub fn unclaimed_headers(&self) -> Vec<FanDescriptor> {
        self.all_headers
            .iter()
            .filter(|header| self.find_group(header.id()).is_none())
            .map(FanDriver::descriptor)
            .collect()
    }

    /// Takes one header over as a managed fan of its own: a new entry
    /// auto-bound to its most likely governing sensor (a half-speed
    /// constant when the machine offers no sensor), or the revival of a
    /// config detection had parked. Kept in memory and persisted by
    /// [`Engine::save_config`].
    ///
    /// # Errors
    ///
    /// [`CoreError::UnknownFan`] when the machine has no such header,
    /// [`CoreError::DuplicateFanMember`] when a managed group already
    /// drives it.
    pub fn add_fan(&mut self, fan_id: &FanId) -> Result<(), CoreError> {
        if self.find_group(fan_id).is_some() {
            return Err(CoreError::DuplicateFanMember(fan_id.to_string()));
        }
        let mut fan_config = if let Some(position) = self
            .hidden_fans
            .iter()
            .position(|config| &config.id == fan_id)
        {
            let mut config = self.hidden_fans.remove(position);
            config.hidden = false;
            config
        } else {
            let header = self
                .all_headers
                .iter()
                .find(|header| header.id() == fan_id)
                .ok_or_else(|| CoreError::UnknownFan(fan_id.to_string()))?;
            FanConfig {
                id: fan_id.clone(),
                name: None,
                rpm_factor: 1.0,
                min_speed: 0.0,
                members: Vec::new(),
                hidden: false,
                mode: governing_sensor_for(&self.sensors, &header.descriptor()).map_or_else(
                    || FanMode::Fixed { speed: 0.5 },
                    |sensor| FanMode::Auto {
                        sensor,
                        curve: FanCurve::default(),
                    },
                ),
                slot: None,
            }
        };
        // A revived config may name a header whose hardware has since
        // vanished; without its leader it cannot be managed.
        let headers: Vec<FanDriver> = std::iter::once(fan_id)
            .chain(fan_config.members.iter())
            .filter_map(|member_id| {
                self.all_headers
                    .iter()
                    .find(|header| header.id() == member_id)
                    .cloned()
            })
            .collect();
        if headers.is_empty() || headers[0].id() != fan_id {
            return Err(CoreError::UnknownFan(fan_id.to_string()));
        }
        if let Some(slot) = fan_config.slot {
            fan_config.slot = Some(fit_slot(slot, &self.config.case));
        }
        let display_name = display_name_for(&fan_config, &headers);
        let runtime = claimed_runtime(&fan_config, &headers);
        self.fans.push(FanEntry {
            fan_config,
            headers,
            runtime,
            display_name,
            lost_control: None,
        });
        self.fans.sort_by_key(|entry| entry.headers[0].sort_key());
        Ok(())
    }

    /// Extra headers driven together with `fan_id`'s group (empty when it
    /// drives a single header). `fan_id` may be the leader or a member.
    #[must_use]
    pub fn fan_group(&self, fan_id: &FanId) -> Option<Vec<FanId>> {
        let index = self.find_group(fan_id)?;
        Some(self.fans[index].fan_config.members.clone())
    }

    /// The leader of the group currently driving `header_id`, if that
    /// header is managed at all.
    #[must_use]
    pub fn fan_owner(&self, header_id: &FanId) -> Option<FanId> {
        self.fans
            .iter()
            .find(|entry| entry.headers.iter().any(|header| header.id() == header_id))
            .map(|entry| entry.fan_config.id.clone())
    }

    /// Replaces the extra headers driven together with `fan_id`'s group,
    /// so several headers (an AIO's fans, for example) behave as one.
    /// `member_ids` excludes the group's leader.
    ///
    /// # Errors
    ///
    /// [`CoreError::UnknownFan`] when `fan_id` is not managed or a member
    /// header does not exist, [`CoreError::DuplicateFanMember`] when a
    /// member is already driven by another group.
    pub fn set_fan_members(
        &mut self,
        fan_id: &FanId,
        member_ids: &[FanId],
    ) -> Result<(), CoreError> {
        let index = self
            .find_group(fan_id)
            .ok_or_else(|| CoreError::UnknownFan(fan_id.to_string()))?;
        let leader = self.fans[index].fan_config.id.clone();
        let member_ids: Vec<FanId> = member_ids
            .iter()
            .filter(|id| **id != leader)
            .cloned()
            .collect();
        for member_id in &member_ids {
            if !self
                .all_headers
                .iter()
                .any(|header| header.id() == member_id)
            {
                return Err(CoreError::UnknownFan(member_id.to_string()));
            }
            if self.fans.iter().enumerate().any(|(other, entry)| {
                other != index && entry.headers.iter().any(|header| header.id() == member_id)
            }) {
                return Err(CoreError::DuplicateFanMember(member_id.to_string()));
            }
        }
        let mut headers = Vec::new();
        for header_id in std::iter::once(&leader).chain(member_ids.iter()) {
            let header = self
                .all_headers
                .iter()
                .find(|header| header.id() == header_id)
                .ok_or_else(|| CoreError::UnknownFan(header_id.to_string()))?;
            headers.push(header.clone());
        }
        let entry = &mut self.fans[index];
        entry.headers = headers;
        entry.fan_config.members = member_ids;
        let name = display_name_for(&entry.fan_config, &entry.headers);
        entry.display_name = name;
        Ok(())
    }

    /// Where a managed fan is drawn on the case view: the configured slot,
    /// or the auto-placed fallback when none is set.
    #[must_use]
    pub fn fan_slot(&self, fan_id: &FanId) -> Option<FanSlot> {
        let index = self.find_group(fan_id)?;
        let entry = &self.fans[index];
        Some(entry.fan_config.slot.unwrap_or_else(|| {
            default_slot(
                index,
                self.fans.len(),
                &self.config.case,
                is_gpu_cooler(&entry.headers[0]),
            )
        }))
    }

    /// Moves or resizes a managed fan on the case view; kept in memory and
    /// persisted by [`Engine::save_config`].
    ///
    /// # Errors
    ///
    /// [`CoreError::UnknownFan`] when the fan is not managed.
    pub fn set_fan_slot(&mut self, fan_id: &FanId, slot: FanSlot) -> Result<(), CoreError> {
        let index = self
            .find_group(fan_id)
            .ok_or_else(|| CoreError::UnknownFan(fan_id.to_string()))?;
        self.fans[index].fan_config.slot = Some(fit_slot(slot, &self.config.case));
        Ok(())
    }

    /// Renames a managed fan group for display (and for `Save config`). An
    /// empty name falls back to the hardware-derived default again.
    ///
    /// # Errors
    ///
    /// [`CoreError::UnknownFan`] when the fan is not managed.
    pub fn set_fan_name(&mut self, fan_id: &FanId, name: &str) -> Result<(), CoreError> {
        let index = self
            .find_group(fan_id)
            .ok_or_else(|| CoreError::UnknownFan(fan_id.to_string()))?;
        let trimmed = name.trim();
        let entry = &mut self.fans[index];
        entry.fan_config.name = (!trimmed.is_empty()).then(|| trimmed.to_string());
        entry.display_name = display_name_for(&entry.fan_config, &entry.headers);
        Ok(())
    }

    /// Corrects a tachometer that reports too fast or too slow: measured
    /// RPM is multiplied by `factor` (0.5 for a reading that always shows
    /// double). Clamped to 0.01..=8.0 and persisted by `Save config`.
    ///
    /// # Errors
    ///
    /// [`CoreError::UnknownFan`] when the fan is not managed.
    pub fn set_fan_rpm_factor(&mut self, fan_id: &FanId, factor: f64) -> Result<(), CoreError> {
        let index = self
            .find_group(fan_id)
            .ok_or_else(|| CoreError::UnknownFan(fan_id.to_string()))?;
        self.fans[index].fan_config.rpm_factor = factor.clamp(0.01, 8.0);
        Ok(())
    }

    /// The tachometer correction currently configured for a managed fan.
    #[must_use]
    pub fn fan_rpm_factor(&self, fan_id: &FanId) -> Option<f64> {
        self.find_group(fan_id)
            .map(|index| self.fans[index].fan_config.rpm_factor)
    }

    /// The speed floor currently configured for a managed fan: the smallest
    /// fraction the engine may command in fixed or auto mode.
    #[must_use]
    pub fn fan_min_speed(&self, fan_id: &FanId) -> Option<f64> {
        self.find_group(fan_id)
            .map(|index| self.fans[index].fan_config.min_speed)
    }

    /// Sets a fan's speed floor (0.0..=1.0, 0.0 = no floor): an AIO pump can
    /// be kept turning however quiet its curve is, while _Off_ mode still
    /// stops it because that is a deliberate choice. Persisted by
    /// `Save config`.
    ///
    /// # Errors
    ///
    /// [`CoreError::UnknownFan`] when the fan is not managed.
    pub fn set_fan_min_speed(&mut self, fan_id: &FanId, speed: f64) -> Result<(), CoreError> {
        let index = self
            .find_group(fan_id)
            .ok_or_else(|| CoreError::UnknownFan(fan_id.to_string()))?;
        self.fans[index].fan_config.min_speed = speed.clamp(0.0, 1.0);
        Ok(())
    }

    /// Saves the live config (including mode, group and slot changes made
    /// through the engine) back to its file, keeping hidden headers
    /// present but disabled.
    ///
    /// # Errors
    ///
    /// As for [`crate::config::save`].
    pub fn save_config(&self) -> Result<(), CoreError> {
        crate::config::save(&self.current_config(), &self.config_path)
    }

    /// The complete config as this engine currently stands: live fan
    /// settings (including modes changed since startup and flags parked
    /// this session) merged with the rest of the loaded config. This is
    /// exactly what [`Engine::save_config`] would write.
    #[must_use]
    pub fn current_config(&self) -> AppConfig {
        let mut fans: Vec<FanConfig> = self
            .fans
            .iter()
            .map(|entry| entry.fan_config.clone())
            .collect();
        fans.extend(self.hidden_fans.iter().cloned());
        fans.sort_by(|a, b| a.id.cmp(&b.id));
        AppConfig {
            fans,
            ..self.config.clone()
        }
    }

    /// Swaps in a whole config (the D-Bus service calls this when the GUI
    /// saves its edits): fan groups are re-claimed against the discovered
    /// headers and sensor display names refreshed, just like at startup.
    /// Groups keep the ramp position of the group they replace, so saving
    /// a config never dips a running fan.
    ///
    /// # Errors
    ///
    /// As for [`Engine::startup`] when the config references hardware that
    /// is not present.
    pub fn apply_config(&mut self, mut config: AppConfig) -> Result<(), CoreError> {
        self.sensor_names = self
            .sensors
            .iter()
            .map(|sensor| {
                let name = config
                    .sensor_names
                    .get(sensor.id.as_str())
                    .cloned()
                    .unwrap_or_else(|| sensor.default_name.clone());
                (sensor.id.clone(), name)
            })
            .collect();
        let previous_runtime: BTreeMap<FanId, FanRuntime> = self
            .fans
            .iter()
            .map(|entry| (entry.fan_config.id.clone(), entry.runtime))
            .collect();
        let (mut fans, hidden_fans) = Self::claim_fans(
            &self.all_headers,
            &self.sensor_names,
            std::mem::take(&mut config.fans),
            config.case,
        )?;
        for entry in &mut fans {
            if let Some(previous) = previous_runtime.get(&entry.fan_config.id) {
                let floor = entry.fan_config.min_speed.clamp(0.0, 1.0);
                entry.runtime =
                    ramp_baseline_for(&entry.fan_config.mode, previous.last_commanded, floor);
            }
        }
        self.config = config;
        self.fans = fans;
        self.hidden_fans = hidden_fans;
        Ok(())
    }

    /// Looks at the hardware again and re-binds the config to it — what the
    /// control loop needs after the machine wakes from suspend.
    ///
    /// A suspend can re-number the `/sys/class/hwmon/hwmonN` directories (a
    /// USB fan controller re-binding, for example), leave the board's
    /// `pwmN_enable` files back in hardware-automatic mode, and let the
    /// graphics card run its own fan policy meanwhile. So:
    ///
    /// * sensors are discovered again — their ids come from the chip name,
    ///   so a re-numbered directory keeps its id and its config name;
    /// * `pwmN` headers are discovered again and every group is re-bound to
    ///   the header with its id, which re-asserts manual control on the next
    ///   write;
    /// * NVIDIA coolers keep their NVML handles (NVML is initialised once per
    ///   process) and forget who holds their fans, see [`FanDriver::reset_state`];
    /// * every group's ramp restarts from the duty the hardware holds now, so
    ///   a fan the firmware left at full speed eases down instead of the
    ///   engine assuming its pre-sleep position.
    ///
    /// A configured header that has not re-appeared keeps its old handle, so
    /// its writes fail loudly rather than the fan quietly leaving the config.
    ///
    /// # Errors
    ///
    /// [`CoreError::Io`] when the hwmon tree cannot be read at all, in which
    /// case the bindings are left as they were.
    pub fn resync_hardware(&mut self) -> Result<ResyncReport, CoreError> {
        // Read everything into locals first, so a tree that cannot be read
        // leaves the engine's bindings alone.
        let mut sensors = self.hwmon.discover()?;
        if let Some(nvidia) = &mut self.nvidia {
            sensors.extend(nvidia.discover()?);
        }
        order_sensors(&mut sensors);
        self.sensor_names = sensors
            .iter()
            .map(|sensor| {
                let name = self
                    .config
                    .sensor_names
                    .get(sensor.id.as_str())
                    .cloned()
                    .unwrap_or_else(|| sensor.default_name.clone());
                (sensor.id.clone(), name)
            })
            .collect();
        self.sensors = sensors;

        // `pwmN` headers come from a fresh scan; GPU coolers keep their
        // handles and forget the state the sleep invalidated.
        let mut headers: Vec<FanDriver> = SysfsFan::discover_all(self.hwmon.root())?
            .into_iter()
            .map(FanDriver::Sysfs)
            .collect();
        headers.extend(
            self.all_headers
                .iter()
                .filter(|header| matches!(header, FanDriver::Nvidia(_)))
                .cloned(),
        );
        for header in &mut headers {
            header.reset_state();
        }
        self.all_headers = headers;

        let mut report = ResyncReport::default();
        for entry in &mut self.fans {
            let previous = std::mem::take(&mut entry.headers);
            let mut headers = Vec::with_capacity(previous.len());
            for header in previous {
                let id = header.id().clone();
                if let Some(found) = self.all_headers.iter().find(|found| found.id() == &id) {
                    headers.push(found.clone());
                    report.rebound += 1;
                } else {
                    // Not back yet: keep the stale handle so the control
                    // loop reports the failed writes for this fan.
                    headers.push(header);
                    report.missing_headers.push(id.to_string());
                }
            }
            entry.headers = headers;
            entry.display_name = display_name_for(&entry.fan_config, &entry.headers);
            entry.runtime = claimed_runtime(&entry.fan_config, &entry.headers);
            // Whatever made a header unmanageable may have gone with the
            // sleep; the next write decides whether it is still true.
            entry.lost_control = None;
        }

        // Headers no group drives at all. A header belonging to a group the
        // config parks as `hidden` (Detect found nothing plugged in, or the
        // driver refuses software control) is deliberately left alone, so
        // naming it after every resume would just be noise.
        let driven: Vec<FanId> = self
            .fans
            .iter()
            .flat_map(|entry| entry.headers.iter().map(|header| header.id().clone()))
            .chain(self.hidden_fans.iter().flat_map(|hidden| {
                std::iter::once(hidden.id.clone()).chain(hidden.members.iter().cloned())
            }))
            .collect();
        report.unclaimed_headers = self
            .all_headers
            .iter()
            .filter(|header| !driven.iter().any(|id| id == header.id()))
            .map(|header| header.id().to_string())
            .collect();

        Ok(report)
    }

    /// Configured refresh period, floored at 100 ms.
    #[must_use]
    pub fn poll_interval_ms(&self) -> u64 {
        self.config.poll_interval_ms.max(100)
    }

    /// Changes the refresh period (also floored at 100 ms).
    pub fn set_poll_interval_ms(&mut self, interval_ms: u64) {
        self.config.poll_interval_ms = interval_ms.max(100);
    }

    /// Whether the daemon should keep its fan table to the startup tick
    /// plus warnings (config `quiet = true`; the daemon's `--quiet` flag
    /// forces this on as well). The engine itself is indifferent — it is
    /// the daemon's logging that consults this.
    #[must_use]
    pub const fn quiet(&self) -> bool {
        self.config.quiet
    }

    /// Case canvas size for the GUI.
    #[must_use]
    pub const fn case_config(&self) -> CaseConfig {
        self.config.case
    }

    /// Where each managed fan should be drawn, using the configured slot
    /// or an auto-placed fallback grid at the top left.
    #[must_use]
    pub fn case_fans(&self) -> Vec<CaseFanView> {
        self.fans
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let slot = entry.fan_config.slot.unwrap_or_else(|| {
                    default_slot(
                        index,
                        self.fans.len(),
                        &self.config.case,
                        is_gpu_cooler(&entry.headers[0]),
                    )
                });
                let status = self
                    .last_report
                    .fans
                    .iter()
                    .find(|status| &status.id == entry.headers[0].id());
                CaseFanView {
                    id: entry.headers[0].id().clone(),
                    name: entry.display_name.clone(),
                    slot,
                    speed: status.map_or(0.0, |status| status.commanded_speed),
                    rpm: status.and_then(|status| status.measured_rpm),
                    note: status.and_then(|status| status.note.clone()),
                }
            })
            .collect()
    }

    /// Every sensor this machine reported at startup.
    #[must_use]
    pub fn sensors(&self) -> &[SensorDescriptor] {
        &self.sensors
    }

    /// The managed fan groups as (leader id, display name), in listing
    /// order.
    #[must_use]
    pub fn managed_fans(&self) -> Vec<(FanId, String)> {
        self.fans
            .iter()
            .map(|entry| (entry.headers[0].id().clone(), entry.display_name.clone()))
            .collect()
    }

    /// Whether a sensor is hidden from the GUI's temperature list.
    /// Hiding is display-only: the sensor is still read every tick and
    /// can still drive the fans bound to it.
    #[must_use]
    pub fn sensor_hidden(&self, sensor_id: &SensorId) -> bool {
        self.config
            .hidden_sensors
            .iter()
            .any(|hidden| hidden == sensor_id)
    }

    /// Hide or show a sensor in the GUI's temperature list. Any sensor
    /// this machine has can be hidden, even one a fan follows: only the
    /// list changes. The flag lives in the config, so `save_config` (or
    /// the GUI's Save button) keeps it across restarts.
    ///
    /// # Errors
    ///
    /// [`CoreError::UnknownSensor`] when this machine has no such sensor.
    pub fn set_sensor_hidden(
        &mut self,
        sensor_id: &SensorId,
        hidden: bool,
    ) -> Result<(), CoreError> {
        if !self.sensors.iter().any(|sensor| &sensor.id == sensor_id) {
            return Err(CoreError::UnknownSensor(sensor_id.to_string()));
        }
        self.config
            .hidden_sensors
            .retain(|hidden_id| hidden_id != sensor_id);
        if hidden {
            self.config.hidden_sensors.push(sensor_id.clone());
        }
        Ok(())
    }

    /// Resolved display name for a sensor id.
    #[must_use]
    pub fn sensor_name(&self, sensor_id: &SensorId) -> String {
        self.sensor_names
            .get(sensor_id)
            .cloned()
            .unwrap_or_else(|| sensor_id.to_string())
    }

    /// Path of the config file backing this engine.
    #[must_use]
    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// Whether writes to fan headers are enabled (false for dry runs).
    #[must_use]
    pub const fn apply_writes(&self) -> bool {
        self.apply_writes
    }

    /// Spins every managed fan up to full, reads tachometers after
    /// `settle`, and disables any fan group whose headers either report no
    /// RPM (nothing is plugged in) or refuse the spin-up write because the
    /// driver exposes them as not software-controllable. Groups with a
    /// tachometer that spin up stay visible. Newly-hidden groups go back to
    /// the motherboard's control where possible and are recorded in the
    /// config (`hidden = true`) until re-enabled by editing the file.
    ///
    /// Returns the leader ids of the newly-hidden groups.
    ///
    /// # Errors
    ///
    /// [`CoreError::Config`] when writes are disabled (`--dry-run`),
    /// [`CoreError::Io`] when a header cannot be spun because this process
    /// lacks the privileges to write it.
    pub fn detect_unconnected_fans(&mut self, settle: Duration) -> Result<Vec<FanId>, CoreError> {
        if !self.apply_writes {
            return Err(CoreError::Config(
                "fan detection needs write access to the headers (drop --dry-run)".to_string(),
            ));
        }
        for entry in &mut self.fans {
            for header in &mut entry.headers {
                if let Err(error) = header.apply(1.0) {
                    if header.write_refused(&error) {
                        // A permission error on a register that opens for
                        // writing is the driver refusing software control,
                        // not a lack of privileges: disable this group.
                        entry.lost_control = Some(format!(
                            "{} refuses writes — not software-controllable",
                            header.id()
                        ));
                        break;
                    }
                    return Err(error);
                }
            }
        }
        std::thread::sleep(settle);

        let parked: Vec<usize> = self
            .fans
            .iter()
            .enumerate()
            .filter(|(_index, entry)| {
                entry.lost_control.is_some()
                    || (entry.headers.iter().any(FanDriver::tach_supported)
                        && !entry
                            .headers
                            .iter()
                            .any(|header| header.read_rpm().is_some_and(|rpm| rpm > 1.0)))
            })
            .map(|(index, _entry)| index)
            .collect();

        let mut newly_hidden = Vec::new();
        for index in parked.into_iter().rev() {
            let mut entry = self.fans.remove(index);
            for header in &mut entry.headers {
                // Zero the duty the spin-up left at full speed, then hand
                // the enable mode back. A driver-refused header refuses
                // these too; ignoring the failure is correct — there is
                // nothing left to fix.
                header.stop().ok();
                header.restore_control().ok();
            }
            newly_hidden.push(entry.fan_config.id.clone());
            entry.fan_config.hidden = true;
            self.hidden_fans.push(entry.fan_config);
        }

        // Put the connected fans back to the speeds they ran before the
        // detection spin-up.
        for entry in &mut self.fans {
            if matches!(entry.fan_config.mode, FanMode::Off) {
                for header in &mut entry.headers {
                    header.stop()?;
                }
            } else {
                let speed = entry.runtime.last_commanded;
                for header in &mut entry.headers {
                    header.apply(speed)?;
                }
            }
        }
        Ok(newly_hidden)
    }

    /// Parks fans that another process just found unusable: the GUI calls
    /// this to mirror a `DetectFans` run by the service, so its lists
    /// match without spinning headers again.
    pub fn park_fans(&mut self, ids: &[FanId]) {
        // Indices first: the removal loop below mutates `self.fans`.
        #[expect(
            clippy::needless_collect,
            reason = "the removal loop mutates self.fans, so these indices must be materialised first"
        )]
        let parked: Vec<usize> = self
            .fans
            .iter()
            .enumerate()
            .filter(|(_index, entry)| ids.contains(&entry.fan_config.id))
            .map(|(index, _entry)| index)
            .collect();
        for index in parked.into_iter().rev() {
            let mut entry = self.fans.remove(index);
            entry.fan_config.hidden = true;
            self.hidden_fans.push(entry.fan_config);
        }
    }

    /// Hands every managed fan header back to automatic control:
    /// motherboard headers get the `pwmN_enable` mode found at startup
    /// restored, GPU coolers go back to the NVIDIA driver. A dry run never
    /// took control and restores nothing.
    ///
    /// Called on shutdown so fans are never left in manual mode. The first
    /// write failure is returned after every other header is restored.
    ///
    /// # Errors
    ///
    /// [`CoreError::Io`] when an enable file cannot be written.
    pub fn release_control(&mut self) -> Result<(), CoreError> {
        if !self.apply_writes {
            return Ok(());
        }
        let mut first_error = None;
        for entry in &mut self.fans {
            for header in &mut entry.headers {
                if let Err(error) = header.restore_control() {
                    first_error.get_or_insert(error);
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

/// Commands one fan group's headers: a stop where _Off_ asks for one,
/// otherwise the commanded fraction. A header whose driver refuses software
/// control entirely is reported through `lost_control` (the tick parks that
/// group afterwards) instead of failing every tick from then on.
fn write_headers(
    headers: &mut [FanDriver],
    mode: &FanMode,
    commanded: f64,
    lost_control: &mut Option<String>,
) -> Result<(), CoreError> {
    for header in headers {
        let written = if matches!(mode, FanMode::Off) {
            header.stop()
        } else {
            header.apply(commanded)
        };
        if let Err(error) = written {
            if header.write_refused(&error) {
                // The register opens for writing yet the driver still
                // refuses: software can never drive this header.
                *lost_control = Some(format!(
                    "{} refuses writes — the driver exposes this header as not software-controllable; disabled",
                    header.id()
                ));
                break;
            }
            return Err(error);
        }
    }
    Ok(())
}

/// Warning note for an auto fan whose sensor stopped answering: the fan is
/// running at the fail-safe speed instead of the speed its curve wants.
fn fail_safe_note(
    sensor_names: &BTreeMap<SensorId, String>,
    sensor: &SensorId,
    speed: f64,
) -> String {
    let sensor_name = sensor_names
        .get(sensor)
        .cloned()
        .unwrap_or_else(|| sensor.to_string());
    format!(
        "{sensor_name} is not responding — running this fan at {:.0}% as a fail-safe",
        speed * 100.0
    )
}

/// Group display name: a user-chosen `name` always wins; a lone header
/// shows its hardware default, and a group notes its extra members.
fn display_name_for(fan_config: &FanConfig, headers: &[FanDriver]) -> String {
    if let Some(name) = &fan_config.name {
        return name.clone();
    }
    let primary = headers[0].default_name();
    match headers.len() {
        1 => primary.to_string(),
        extra => format!("{primary} (+{} more)", extra - 1),
    }
}

/// GPU ordinal encoded in an NVML sensor id, e.g. `nvidia-gpu0-t1` → 0.
fn gpu_index_in_sensor_id(id: &str) -> Option<u32> {
    let digits: String = id
        .strip_prefix("nvidia-gpu")?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// Moves `from` toward `to` by at most `step`.
fn approach(from: f64, to: f64, step: f64) -> f64 {
    if (to - from).abs() <= step {
        to
    } else if to > from {
        from + step
    } else {
        from - step
    }
}

/// The runtime a freshly claimed group starts with: the ramp baseline is
/// seeded from the duty the header actually holds at takeover — what the
/// BIOS, the card's own policy or a previous run left behind — so taking
/// over a spinning fan eases it toward the curve instead of dropping it to
/// the first ramp step and crawling back up. Headers whose duty cannot be
/// read (or headers already parked at 0) start from 0.
fn claimed_runtime(fan_config: &FanConfig, headers: &[FanDriver]) -> FanRuntime {
    let held = headers
        .iter()
        .find_map(FanDriver::read_percent)
        .unwrap_or(0.0);
    ramp_baseline_for(&fan_config.mode, held, fan_config.min_speed.clamp(0.0, 1.0))
}

/// The ramp baseline a mode change leaves behind: _auto_ keeps the speed
/// the engine was already commanding, so switching profile (a curve preset,
/// a sensor re-bind, a re-saved config) eases the fan from where it is
/// instead of dropping it and ramping back up; _off_ and _fixed_ reset the
/// baseline to what the change itself commands — a fan deliberately stopped
/// really does start its next ramp at 0. The old curve's hold band never
/// carries over to a new one.
const fn ramp_baseline_for(mode: &FanMode, previous_commanded: f64, floor: f64) -> FanRuntime {
    let last_commanded = match mode {
        FanMode::Auto { .. } => previous_commanded,
        FanMode::Off => 0.0,
        FanMode::Fixed { speed } => speed.clamp(0.0, 1.0).max(floor),
    };
    FanRuntime {
        last_commanded,
        hold_temp_c: None,
    }
}

/// Whether this header drives an NVIDIA GPU cooler (drawn as a card).
const fn is_gpu_cooler(header: &FanDriver) -> bool {
    matches!(header, FanDriver::Nvidia(_))
}

/// How far a slot's shape must stay from the case walls: a fan drawn
/// flush with the outline (or its rpm/⚠ label across it) reads as
/// touching the edge. Shapes are also sized so they still fit inside
/// the padded walls.
const SLOT_EDGE_PAD: f64 = 16.0;

/// The largest a shape's side along one canvas dimension may get: the
/// padded interior, floored at the 10-unit minimum shape size.
const fn max_shape_size(dimension: f64) -> f64 {
    (-2.0_f64).mul_add(SLOT_EDGE_PAD, dimension).max(10.0)
}

/// Clamps a slot inside the case canvas: the shape fits, and its centre
/// stays [`SLOT_EDGE_PAD`] away from the walls so nothing touches the
/// outline — a fan drawn off-canvas can never be grabbed to move it
/// back. (On a canvas too small for the shape plus padding, the shape
/// just centres itself.)
fn fit_slot(mut slot: FanSlot, case: &CaseConfig) -> FanSlot {
    if let Some((width, height)) = slot.rect {
        slot.rect = Some((
            width.clamp(10.0, max_shape_size(case.width)),
            height.clamp(10.0, max_shape_size(case.height)),
        ));
    } else {
        let max_diameter = max_shape_size(case.width.min(case.height));
        slot.diameter = slot.diameter.clamp(10.0, max_diameter);
    }
    let (half_width, half_height) = slot.half_extents();
    let (x_floor, y_floor) = (half_width + SLOT_EDGE_PAD, half_height + SLOT_EDGE_PAD);
    slot.x = slot.x.clamp(x_floor, (case.width - x_floor).max(x_floor));
    slot.y = slot.y.clamp(y_floor, (case.height - y_floor).max(y_floor));
    slot
}

/// Fallback layout for fans without a configured slot: GPU coolers get
/// a full-width card mid-case (where a graphics card sits in a tower),
/// everything else a grid of circles starting at the top-left, always
/// inside the canvas. (A single column used to run fans with no slot
/// past the bottom of the case, where they were invisible and
/// impossible to drag.)
fn default_slot(index: usize, total: usize, case: &CaseConfig, rectangular: bool) -> FanSlot {
    if rectangular {
        let (width, height) = (case.width * 0.72, case.height * 0.24);
        return fit_slot(
            FanSlot {
                x: case.width / 2.0,
                y: case.height * 0.42,
                diameter: 60.0,
                rect: Some((width, height)),
                airflow: Airflow::Internal,
            },
            case,
        );
    }
    let diameter = 60.0;
    // The grid spans the padded interior: inset is the smallest distance
    // a centre may sit from a wall, and the pitch shrinks (never below
    // what keeps circles apart) if the preferred spacing would overflow.
    let inset = diameter / 2.0 + SLOT_EDGE_PAD;
    let usable_width = (-2.0_f64).mul_add(inset, case.width).max(0.0);
    let usable_height = (-2.0_f64).mul_add(inset, case.height).max(0.0);
    // Smallest column count whose grid fits every fan (integer square
    // root of `total`, never below 1).
    let mut columns = 1usize;
    while columns * columns < total {
        columns += 1;
    }
    let rows = total.div_ceil(columns);
    #[expect(clippy::cast_precision_loss, reason = "grid counts are tiny")]
    let pitch = (diameter + 14.0)
        .min(usable_width / ((columns - 1).max(1)) as f64)
        .min(usable_height / ((rows - 1).max(1)) as f64);
    #[expect(clippy::cast_precision_loss, reason = "grid indices are tiny")]
    let (column, row) = ((index % columns) as f64, (index / columns) as f64);
    fit_slot(
        FanSlot {
            x: column.mul_add(pitch, inset),
            y: row.mul_add(pitch, inset),
            diameter,
            rect: None,
            airflow: Airflow::Exhaust,
        },
        case,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fans::{CurvePoint, CurvePreset};

    /// Fake hwmon tree: one CPU chip and one super-IO chip with a fan.
    fn fixture(root: &Path) {
        let cpu = root.join("hwmon0");
        std::fs::create_dir_all(&cpu).expect("cpu dir");
        std::fs::write(cpu.join("name"), "k10temp\n").expect("name");
        std::fs::write(cpu.join("temp1_input"), "55000\n").expect("temp");
        std::fs::write(cpu.join("temp1_label"), "Tctl\n").expect("label");

        let superio = root.join("hwmon1");
        std::fs::create_dir_all(&superio).expect("superio dir");
        std::fs::write(superio.join("name"), "nct6795\n").expect("name");
        std::fs::write(superio.join("pwm1"), "0\n").expect("pwm");
        std::fs::write(superio.join("pwm1_enable"), "1\n").expect("enable");
        std::fs::write(superio.join("fan1_input"), "1000\n").expect("fan");
    }

    /// The one-fan fixture engine, with the ramp settings pinned.
    fn engine_with(temp_dir: &Path, hysteresis: crate::config::model::HysteresisConfig) -> Engine {
        let config = AppConfig {
            fans: vec![FanConfig {
                id: FanId::new("nct6795-p1"),
                name: Some("Rear exhaust".to_string()),
                rpm_factor: 1.0,
                min_speed: 0.0,
                members: Vec::new(),
                hidden: false,
                mode: FanMode::Auto {
                    sensor: SensorId::new("k10temp-t1"),
                    curve: FanCurve::new(vec![
                        CurvePoint {
                            temp_c: 40.0,
                            speed: 0.2,
                        },
                        CurvePoint {
                            temp_c: 80.0,
                            speed: 1.0,
                        },
                    ])
                    .expect("curve"),
                },
                slot: None,
            }],
            hysteresis,
            ..AppConfig::default()
        };
        let options = EngineOptions {
            hwmon_root: temp_dir.to_path_buf(),
            use_nvidia: false,
            apply_writes: true,
        };
        Engine::startup(config, temp_dir.join("config.toml"), &options).expect("engine")
    }

    fn set_cpu_temp(root: &Path, millidegrees: i64) {
        let path = root.join("hwmon0").join("temp1_input");
        std::fs::write(&path, format!("{millidegrees}\n")).expect("set temp");
    }

    fn read_pwm(root: &Path) -> i64 {
        let path = root.join("hwmon1").join("pwm1");
        std::fs::read_to_string(path)
            .expect("pwm")
            .trim()
            .parse()
            .expect("pwm number")
    }

    #[test]
    fn auto_fan_follows_the_curve_and_holds_within_the_band() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let mut engine = engine_with(
            dir.path(),
            crate::config::model::HysteresisConfig {
                temp_band_c: 2.0,
                max_step_per_tick: 1.0, // no rate limiting in this test
                max_step_up_per_tick: 1.0,
            },
        );

        // 55 °C on a 40→0.2 / 80→1.0 curve (slope 0.02/°C) is 0.5 → pwm 128.
        let report = engine.tick().expect("tick 1");
        assert_eq!(report.fans[0].name, "Rear exhaust");
        assert!((report.fans[0].commanded_speed - 0.5).abs() < 1e-9);
        assert_eq!(read_pwm(dir.path()), 128);
        assert_eq!(report.fans[0].measured_rpm, Some(1000.0));

        // 54 °C wants a lower speed, but is inside the 2 °C hold band of the
        // 55 °C that raised the fan, so speed stays put.
        set_cpu_temp(dir.path(), 54_000);
        let report = engine.tick().expect("tick 2");
        assert!((report.fans[0].commanded_speed - 0.5).abs() < 1e-9);

        // 52 °C is at or below 55 - 2, so the fan may slow down (0.44 → pwm 112).
        set_cpu_temp(dir.path(), 52_000);
        let report = engine.tick().expect("tick 3");
        assert!((report.fans[0].commanded_speed - 0.44).abs() < 1e-9);
        assert_eq!(read_pwm(dir.path()), 112);
    }

    #[test]
    fn auto_fans_ramp_at_the_configured_step_rate() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let mut engine = engine_with(
            dir.path(),
            crate::config::model::HysteresisConfig {
                temp_band_c: 2.0,
                max_step_per_tick: 0.1,
                max_step_up_per_tick: 0.1,
            },
        );
        set_cpu_temp(dir.path(), 79_000);

        // Curve wants ~0.975 at 79 °C, but only +0.1 is allowed per tick.
        let report = engine.tick().expect("tick 1");
        assert!((report.fans[0].commanded_speed - 0.1).abs() < 1e-9);
        let report = engine.tick().expect("tick 2");
        assert!((report.fans[0].commanded_speed - 0.2).abs() < 1e-9);
    }

    #[test]
    fn speed_goes_up_like_a_rocket_and_down_like_a_feather() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let mut engine = engine_with(
            dir.path(),
            crate::config::model::HysteresisConfig {
                temp_band_c: 0.0,
                max_step_per_tick: 0.1,    // feather: the way down
                max_step_up_per_tick: 0.5, // rocket: the way up (the default)
            },
        );

        // The fixture header starts parked (pwm1 = 0); 55 °C on the
        // fixture curve wants 0.5. One rocket step covers the whole
        // climb — protection lands fast instead of creeping up at the
        // feather step, which would have needed five ticks here.
        let report = engine.tick().expect("tick 1");
        assert!((report.fans[0].commanded_speed - 0.5).abs() < 1e-9);

        // Cooling to 40 °C only wants 0.2, but the way back down crawls
        // at the feather step: 0.4, then 0.3, then 0.2.
        set_cpu_temp(dir.path(), 40_000);
        let report = engine.tick().expect("tick 2");
        assert!((report.fans[0].commanded_speed - 0.4).abs() < 1e-9);
        let report = engine.tick().expect("tick 3");
        assert!((report.fans[0].commanded_speed - 0.3).abs() < 1e-9);
        let report = engine.tick().expect("tick 4");
        assert!((report.fans[0].commanded_speed - 0.2).abs() < 1e-9);
    }

    #[test]
    fn dead_sensor_triggers_the_fail_safe() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let mut engine = engine_with(
            dir.path(),
            crate::config::model::HysteresisConfig {
                temp_band_c: 0.0,
                max_step_per_tick: 1.0,
                max_step_up_per_tick: 1.0,
            },
        );
        engine.tick().expect("first tick");

        // Remove the CPU sensor: an auto fan must go to fail_safe_speed.
        std::fs::write(dir.path().join("hwmon0/temp1_input"), "nope\n").expect("break temp");
        let report = engine.tick().expect("tick after failure");
        assert_eq!(report.sensors[0].temp_c, None);
        let fan = &report.fans[0];
        assert!((fan.commanded_speed - 1.0).abs() < 1e-9);
        assert_eq!(read_pwm(dir.path()), 255);
        assert!(
            fan.note
                .as_deref()
                .is_some_and(|note| note.contains("fail-safe"))
        );
    }

    #[test]
    fn off_mode_writes_zero_and_fixed_applies_at_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let mut engine = engine_with(
            dir.path(),
            crate::config::model::HysteresisConfig::default(),
        );
        let fan_id = FanId::new("nct6795-p1");

        engine.set_fan_mode(&fan_id, &FanMode::Off).expect("off");
        assert_eq!(read_pwm(dir.path()), 0);
        assert_eq!(
            read_trimmed(dir.path().join("hwmon1/pwm1_enable")).as_deref(),
            Some("0")
        );

        engine
            .set_fan_mode(&fan_id, &FanMode::Fixed { speed: 0.5 })
            .expect("fixed");
        assert_eq!(read_pwm(dir.path()), 128);
        assert_eq!(
            engine.fan_mode(&fan_id),
            Some(FanMode::Fixed { speed: 0.5 })
        );
    }

    /// The curve the `engine_with` fixture starts with (0.2 at 40 °C,
    /// 1.0 at 80 °C), for tests that rebuild an auto mode.
    fn fixture_curve() -> FanCurve {
        FanCurve::new(vec![
            CurvePoint {
                temp_c: 40.0,
                speed: 0.2,
            },
            CurvePoint {
                temp_c: 80.0,
                speed: 1.0,
            },
        ])
        .expect("curve")
    }

    #[test]
    fn changing_curve_profile_ramps_from_the_current_speed() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let mut engine = engine_with(
            dir.path(),
            crate::config::model::HysteresisConfig {
                temp_band_c: 2.0,
                max_step_per_tick: 0.1,
                max_step_up_per_tick: 0.1,
            },
        );
        let fan_id = FanId::new("nct6795-p1");
        set_cpu_temp(dir.path(), 79_000);

        // Ramp up: 0.1 then 0.2 (as in the ramp-rate test above).
        engine.tick().expect("tick 1");
        let report = engine.tick().expect("tick 2");
        assert!((report.fans[0].commanded_speed - 0.2).abs() < 1e-9);

        // A preset change goes through set_fan_mode with the new curve.
        // The fan must keep climbing from 0.2, not restart its ramp at 0.
        engine
            .set_fan_mode(
                &fan_id,
                &FanMode::Auto {
                    sensor: SensorId::new("k10temp-t1"),
                    curve: CurvePreset::Performance.curve(),
                },
            )
            .expect("switch profile");
        let report = engine.tick().expect("tick after the switch");
        assert!(
            (report.fans[0].commanded_speed - 0.3).abs() < 1e-9,
            "the ramp continues from 0.2 instead of resetting to 0"
        );
    }

    #[test]
    fn fixed_to_auto_continues_from_the_fixed_speed() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let mut engine = engine_with(
            dir.path(),
            crate::config::model::HysteresisConfig {
                temp_band_c: 0.0,
                max_step_per_tick: 0.1,
                max_step_up_per_tick: 0.1,
            },
        );
        let fan_id = FanId::new("nct6795-p1");

        // The slider parks the fan at 60 %; switching back to _auto_ with
        // no tick in between must ramp from 0.6, not from 0. The curve
        // wants 0.5 at 55 °C, so the fan eases down by one step.
        engine
            .set_fan_mode(&fan_id, &FanMode::Fixed { speed: 0.6 })
            .expect("fixed");
        engine
            .set_fan_mode(
                &fan_id,
                &FanMode::Auto {
                    sensor: SensorId::new("k10temp-t1"),
                    curve: fixture_curve(),
                },
            )
            .expect("auto");
        let report = engine.tick().expect("first auto tick");
        assert!(
            (report.fans[0].commanded_speed - 0.5).abs() < 1e-9,
            "one step down from 0.6, not a jump from 0 to 0.1"
        );
    }

    #[test]
    fn auto_after_off_still_ramps_up_from_zero() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let mut engine = engine_with(
            dir.path(),
            crate::config::model::HysteresisConfig {
                temp_band_c: 0.0,
                max_step_per_tick: 0.1,
                max_step_up_per_tick: 0.1,
            },
        );
        let fan_id = FanId::new("nct6795-p1");

        // A fan the user stopped really did stop, so its next ramp must
        // start at 0 — continuing from the pre-off speed would jump it.
        engine.set_fan_mode(&fan_id, &FanMode::Off).expect("off");
        engine
            .set_fan_mode(
                &fan_id,
                &FanMode::Auto {
                    sensor: SensorId::new("k10temp-t1"),
                    curve: fixture_curve(),
                },
            )
            .expect("auto");
        let report = engine.tick().expect("first tick after off");
        assert!(
            (report.fans[0].commanded_speed - 0.1).abs() < 1e-9,
            "a stopped fan ramps up from 0, one step per tick"
        );
    }

    #[test]
    fn taking_over_a_spinning_header_ramps_from_its_current_duty() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        std::fs::write(dir.path().join("hwmon1/pwm1"), "153\n").expect("seed duty");
        let mut engine = engine_with(
            dir.path(),
            crate::config::model::HysteresisConfig {
                temp_band_c: 2.0,
                max_step_per_tick: 0.1,
                max_step_up_per_tick: 0.1,
            },
        );

        // 55 °C on the fixture curve wants 0.5 — one ramp step below the
        // duty taken over, so the fan eases straight there instead of
        // first dropping toward 0 and crawling back up.
        let report = engine.tick().expect("first tick after takeover");
        assert!(
            (report.fans[0].commanded_speed - 0.5).abs() < 1e-9,
            "takeover continues from the 60 % the BIOS left"
        );
        assert_eq!(read_pwm(dir.path()), 128);
    }

    #[test]
    fn add_fan_continues_from_the_duty_the_bios_left() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        std::fs::write(dir.path().join("hwmon1/pwm2"), "204\n").expect("pwm2");
        std::fs::write(dir.path().join("hwmon1/fan2_input"), "800\n").expect("fan2");
        let mut engine = engine_over(dir.path(), true, Vec::new()).expect("engine");

        // The added header is auto-bound to the CPU sensor with the
        // default curve (0.5 at 55 °C); it was spinning at 80 %, so the
        // first tick eases one step down instead of commanding 10 %.
        engine.add_fan(&FanId::new("nct6795-p2")).expect("add");
        let report = engine.tick().expect("first tick");
        let added = report
            .fans
            .iter()
            .find(|fan| fan.id.as_str() == "nct6795-p2")
            .expect("p2");
        assert!(
            (added.commanded_speed - 0.7).abs() < 1e-9,
            "one step down from the 80 % taken over, not 0.1 up from a stop"
        );
    }

    #[test]
    fn resync_finds_a_header_whose_hwmon_directory_was_renumbered() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let mut engine = engine_with(
            dir.path(),
            crate::config::model::HysteresisConfig {
                temp_band_c: 0.0,
                max_step_per_tick: 1.0,
                max_step_up_per_tick: 1.0,
            },
        );
        engine.tick().expect("first tick");

        // A controller that re-binds while the machine sleeps comes back on a
        // different hwmon number; the fan id (chip name plus header) does not
        // change, so re-discovery finds the header again.
        std::fs::rename(dir.path().join("hwmon1"), dir.path().join("hwmon7")).expect("rename");
        let report = engine.resync_hardware().expect("resync");
        assert_eq!(report.rebound, 1, "the header is found again by id");
        assert!(report.missing_headers.is_empty(), "{report:?}");

        set_cpu_temp(dir.path(), 80_000); // the fixture curve wants full speed
        engine.tick().expect("tick after resume");
        let pwm = std::fs::read_to_string(dir.path().join("hwmon7/pwm1")).expect("pwm7");
        assert_eq!(pwm.trim(), "255", "writes follow the re-numbered path");
    }

    #[test]
    fn resync_restarts_the_ramp_from_the_duty_the_firmware_left() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let mut engine = engine_with(
            dir.path(),
            crate::config::model::HysteresisConfig {
                temp_band_c: 0.0,
                max_step_per_tick: 0.1,
                max_step_up_per_tick: 0.1,
            },
        );
        engine.tick().expect("first tick"); // 55 °C on the fixture curve wants 0.5

        // On the way out of suspend the firmware put this fan back to full
        // speed. The engine must not assume it is still where it left it.
        std::fs::write(dir.path().join("hwmon1/pwm1"), "255\n").expect("firmware duty");
        engine.resync_hardware().expect("resync");
        let report = engine.tick().expect("tick after resume");
        assert!(
            (report.fans[0].commanded_speed - 0.9).abs() < 1e-9,
            "one feather step down from the 100 % the fan was left at, not from \
             the 50 % the engine remembered"
        );
        assert_eq!(read_pwm(dir.path()), 230);
    }

    #[test]
    fn resync_keeps_a_header_the_machine_has_not_given_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let mut engine = engine_with(
            dir.path(),
            crate::config::model::HysteresisConfig::default(),
        );
        std::fs::remove_dir_all(dir.path().join("hwmon1")).expect("remove the chip");

        let report = engine.resync_hardware().expect("resync");
        assert_eq!(report.rebound, 0);
        assert_eq!(report.missing_headers, vec!["nct6795-p1".to_string()]);
        assert!(report.unclaimed_headers.is_empty(), "{report:?}");
        // The group stays managed — its writes fail loudly until the
        // controller returns — rather than the fan leaving the config.
        assert!(
            engine
                .current_config()
                .fans
                .iter()
                .any(|fan| fan.id.as_str() == "nct6795-p1" && !fan.hidden),
            "still managed, waiting for its header back"
        );
    }

    #[test]
    fn resync_leaves_a_parked_header_out_of_the_unclaimed_list() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let mut engine = engine_with(
            dir.path(),
            crate::config::model::HysteresisConfig::default(),
        );

        // What Detect does to a header with nothing plugged in: park the
        // group as `hidden`. Nothing drives it on purpose, so a resume has
        // nothing to tell the user about it.
        engine.park_fans(&[FanId::new("nct6795-p1")]);
        let report = engine.resync_hardware().expect("resync");
        assert_eq!(report.rebound, 0, "no managed fan is left to re-bind");
        assert!(
            report.unclaimed_headers.is_empty(),
            "a parked header is deliberate, not news: {report:?}"
        );

        // A header the config really has never seen is still reported.
        let superio = dir.path().join("hwmon1");
        std::fs::write(superio.join("pwm2"), "0\n").expect("new header");
        std::fs::write(superio.join("pwm2_enable"), "1\n").expect("new enable");
        let report = engine.resync_hardware().expect("second resync");
        assert_eq!(
            report.unclaimed_headers,
            vec!["nct6795-p2".to_string()],
            "{report:?}"
        );
    }

    #[test]
    fn a_speed_floor_keeps_a_pump_turning_below_its_curve() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let mut engine = engine_with(
            dir.path(),
            crate::config::model::HysteresisConfig::default(),
        );
        let fan_id = FanId::new("nct6795-p1");
        // The configured curve asks for 20 % at 30 °C; this fan is a pump
        // that must never fall below 40 %.
        engine
            .set_fan_min_speed(&fan_id, 0.4)
            .expect("set the floor");
        set_cpu_temp(dir.path(), 30_000);

        // The floor outranks the ramp: the fan jumps to 40 % instead of
        // crawling up through speeds it is not allowed to run at.
        let report = engine.tick().expect("tick");
        assert!(
            (report.fans[0].commanded_speed - 0.4).abs() < 1e-9,
            "the floor wins at once, not after a ramp"
        );
        assert_eq!(read_pwm(dir.path()), 102);

        // Hold: a cold sensor cannot pull a floored fan under the floor.
        let report = engine.tick().expect("tick");
        assert!(
            (report.fans[0].commanded_speed - 0.4).abs() < 1e-9,
            "the floor holds through a cool sensor"
        );

        // Above the floor the curve is still in charge, ramping as configured.
        set_cpu_temp(dir.path(), 65_000);
        let mut speeds = Vec::new();
        for _ in 0..4 {
            speeds.push(engine.tick().expect("tick").fans[0].commanded_speed);
        }
        assert!(
            (speeds[3] - 0.7).abs() < 1e-9,
            "the curve drives the fan once it clears the floor: {speeds:?}"
        );
        assert!(
            speeds.iter().all(|speed| *speed >= 0.4 - 1e-9),
            "never below the floor: {speeds:?}"
        );
    }

    #[test]
    fn a_floor_never_resurrects_a_fan_the_user_turned_off() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let mut engine = engine_with(
            dir.path(),
            crate::config::model::HysteresisConfig::default(),
        );
        let fan_id = FanId::new("nct6795-p1");
        engine
            .set_fan_min_speed(&fan_id, 0.4)
            .expect("set the floor");
        set_cpu_temp(dir.path(), 30_000);
        engine.tick().expect("tick");
        assert_eq!(
            read_pwm(dir.path()),
            102,
            "the curve's 20 % at 30 °C is floored up to 40 %"
        );

        // Stopping the fan is a deliberate act, so _Off_ beats the floor.
        engine.set_fan_mode(&fan_id, &FanMode::Off).expect("off");
        let report = engine.tick().expect("tick");
        assert_eq!(read_pwm(dir.path()), 0, "off stays off");
        assert!(report.fans[0].commanded_speed < 1e-9);
    }

    #[test]
    fn setting_the_floor_clamps_and_persists() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let mut engine = engine_with(
            dir.path(),
            crate::config::model::HysteresisConfig::default(),
        );
        let fan_id = FanId::new("nct6795-p1");
        assert_eq!(
            engine.fan_min_speed(&fan_id),
            Some(0.0),
            "no floor by default"
        );

        engine
            .set_fan_min_speed(&fan_id, 1.5)
            .expect("set above full speed");
        assert_eq!(
            engine.fan_min_speed(&fan_id),
            Some(1.0),
            "clamped to full speed"
        );
        engine
            .set_fan_min_speed(&fan_id, -0.2)
            .expect("set below stopped");
        assert_eq!(
            engine.fan_min_speed(&fan_id),
            Some(0.0),
            "clamped to stopped"
        );

        engine
            .set_fan_min_speed(&fan_id, 0.35)
            .expect("set the floor");
        assert!(
            (engine.current_config().fans[0].min_speed - 0.35).abs() < 1e-9,
            "the floor is part of the config the engine saves"
        );

        // Switching to a fixed speed writes at once, so that write carries
        // the floor too — a pump never sees the dip.
        engine
            .set_fan_mode(&fan_id, &FanMode::Fixed { speed: 0.1 })
            .expect("fixed below the floor");
        assert_eq!(
            read_pwm(dir.path()),
            89,
            "the requested 10 % is floored to 35 % on the way in"
        );

        let unknown = engine
            .set_fan_min_speed(&FanId::new("nope-p9"), 0.5)
            .expect_err("not a managed fan");
        assert!(matches!(unknown, CoreError::UnknownFan(_)));
    }

    #[test]
    fn the_report_carries_the_speed_the_hardware_reports() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let mut engine = engine_with(
            dir.path(),
            crate::config::model::HysteresisConfig::default(),
        );
        let fan_id = FanId::new("nct6795-p1");
        engine
            .set_fan_mode(&fan_id, &FanMode::Fixed { speed: 0.5 })
            .expect("fixed");

        let fan = &engine.tick().expect("tick").fans[0];
        assert!((fan.commanded_speed - 0.5).abs() < 1e-9);
        let percent = fan.measured_percent.expect("the chip reports its duty");
        assert!((percent - 0.5).abs() < 0.01, "128 of 255 reads {percent}");
        assert!(
            fan.measured_rpm
                .is_some_and(|rpm| (rpm - 1000.0).abs() < 1e-9),
            "the tachometer is still reported alongside"
        );
    }

    #[test]
    fn dry_run_computes_but_never_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let config = AppConfig {
            fans: vec![FanConfig {
                id: FanId::new("nct6795-p1"),
                name: None,
                rpm_factor: 1.0,
                min_speed: 0.0,
                members: Vec::new(),
                hidden: false,
                mode: FanMode::Fixed { speed: 1.0 },
                slot: None,
            }],
            ..AppConfig::default()
        };
        let options = EngineOptions {
            hwmon_root: dir.path().to_path_buf(),
            use_nvidia: false,
            apply_writes: false,
        };
        let mut engine =
            Engine::startup(config, dir.path().join("config.toml"), &options).expect("engine");
        let report = engine.tick().expect("tick");
        assert!((report.fans[0].commanded_speed - 1.0).abs() < 1e-9);
        assert_eq!(read_pwm(dir.path()), 0); // pwm1 untouched
    }

    #[test]
    fn unknown_fan_and_sensor_are_rejected_at_startup() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let options = EngineOptions {
            hwmon_root: dir.path().to_path_buf(),
            use_nvidia: false,
            apply_writes: false,
        };

        let bad_fan = AppConfig {
            fans: vec![FanConfig {
                id: FanId::new("does-not-exist-p9"),
                name: None,
                rpm_factor: 1.0,
                min_speed: 0.0,
                members: Vec::new(),
                hidden: false,
                mode: FanMode::Off,
                slot: None,
            }],
            ..AppConfig::default()
        };
        let error = Engine::startup(bad_fan, "cfg.toml", &options).expect_err("unknown fan");
        assert!(matches!(error, CoreError::UnknownFan(_)));

        let bad_sensor = AppConfig {
            fans: vec![FanConfig {
                id: FanId::new("nct6795-p1"),
                name: None,
                rpm_factor: 1.0,
                min_speed: 0.0,
                members: Vec::new(),
                hidden: false,
                mode: FanMode::Auto {
                    sensor: SensorId::new("k10temp-t9"),
                    curve: FanCurve::default(),
                },
                slot: None,
            }],
            ..AppConfig::default()
        };
        let error = Engine::startup(bad_sensor, "cfg.toml", &options).expect_err("unknown sensor");
        assert!(matches!(error, CoreError::UnknownSensor(_)));
    }

    #[test]
    fn default_config_drives_every_fan_from_the_cpu_sensor() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let discovery = discover(dir.path(), false).expect("discover");
        let config = default_config(&discovery);
        assert_eq!(config.fans.len(), 1);
        assert_eq!(config.fans[0].id.as_str(), "nct6795-p1");
        let FanMode::Auto { sensor, .. } = &config.fans[0].mode else {
            panic!("auto expected");
        };
        assert_eq!(sensor.as_str(), "k10temp-t1");
    }

    #[test]
    fn governing_sensor_prefers_the_package_reading_over_a_ccd() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let cpu = dir.path().join("hwmon0");
        std::fs::write(cpu.join("temp2_input"), "57000\n").expect("temp2");
        std::fs::write(cpu.join("temp2_label"), "Tccd1\n").expect("label2");
        let config = default_config(&discover(dir.path(), false).expect("discover"));
        let FanMode::Auto { sensor, .. } = &config.fans[0].mode else {
            panic!("auto expected");
        };
        // "CPU temperature" (Tctl), not alphabetically-first "CPU CCD 1".
        assert_eq!(sensor.as_str(), "k10temp-t1");
    }

    #[test]
    fn default_config_matches_fan_labels_to_sensible_sensors() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        // A GPU-style chip and labelled headers on the super-IO chip.
        let gpu = dir.path().join("hwmon2");
        std::fs::create_dir_all(&gpu).expect("gpu dir");
        std::fs::write(gpu.join("name"), "nvidia-gpu0\n").expect("name");
        std::fs::write(gpu.join("temp1_input"), "61000\n").expect("temp");
        std::fs::write(gpu.join("temp1_label"), "GPU\n").expect("label");

        let superio = dir.path().join("hwmon1");
        std::fs::write(superio.join("temp1_input"), "38000\n").expect("temp");
        std::fs::write(superio.join("temp1_label"), "Chassis\n").expect("label");
        std::fs::write(superio.join("pwm2"), "0\n").expect("pwm2");
        std::fs::write(superio.join("pwm2_label"), "CHASSIS2\n").expect("pwm2 label");
        std::fs::write(superio.join("pwm3"), "0\n").expect("pwm3");
        std::fs::write(superio.join("pwm3_label"), "GPU_AUX\n").expect("pwm3 label");

        let discovery = discover(dir.path(), false).expect("discover");
        let config = default_config(&discovery);
        let governed = |id: &str| {
            config
                .fans
                .iter()
                .find(|fan| fan.id.as_str() == id)
                .and_then(|fan| match &fan.mode {
                    FanMode::Auto { sensor, .. } => Some(sensor.as_str().to_string()),
                    _ => None,
                })
        };
        // CPU_FAN-style p1 header follows the CPU, chassis header the case
        // sensor, GPU aux header the graphics card.
        assert_eq!(governed("nct6795-p1").as_deref(), Some("k10temp-t1"));
        assert_eq!(governed("nct6795-p2").as_deref(), Some("nct6795-t1"));
        assert_eq!(governed("nct6795-p3").as_deref(), Some("nvidia-gpu0-t1"));
    }

    #[test]
    fn discovered_sensors_are_ordered_by_usefulness() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let superio = dir.path().join("hwmon1");
        // A motherboard-ish reading and a case reading on the super-IO.
        std::fs::write(superio.join("temp1_input"), "41000\n").expect("temp");
        std::fs::write(superio.join("temp1_label"), "SYSTIN\n").expect("label");
        std::fs::write(superio.join("temp2_input"), "30000\n").expect("temp");
        std::fs::write(superio.join("temp2_label"), "Chassis\n").expect("label");
        let nvme = dir.path().join("hwmon2");
        std::fs::create_dir_all(&nvme).expect("nvme dir");
        std::fs::write(nvme.join("name"), "nvme\n").expect("name");
        std::fs::write(nvme.join("temp1_input"), "44000\n").expect("temp");

        let discovery = discover(dir.path(), false).expect("discover");
        let names: Vec<&str> = discovery
            .sensors
            .iter()
            .map(|sensor| sensor.default_name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "CPU temperature",
                "NCT 6795 SYSTIN",
                "NVMe drive temperature",
                "NCT 6795 Chassis",
            ]
        );
    }

    #[test]
    fn config_round_trip_through_engine_save() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let config_path = dir.path().join("sub").join("config.toml");
        let mut config = default_config(&discover(dir.path(), false).expect("discover"));
        config.poll_interval_ms = 900;
        crate::config::save(&config, &config_path).expect("initial save");

        let options = EngineOptions {
            hwmon_root: dir.path().to_path_buf(),
            use_nvidia: false,
            apply_writes: false,
        };
        let mut engine = Engine::startup(config, config_path.clone(), &options).expect("engine");
        engine.set_poll_interval_ms(20);
        assert_eq!(engine.poll_interval_ms(), 100);
        engine
            .set_fan_mode(&FanId::new("nct6795-p1"), &FanMode::Off)
            .expect("off");
        engine.save_config().expect("save");

        let reloaded = crate::config::load(&config_path).expect("reload");
        assert_eq!(reloaded.poll_interval_ms, 100);
        assert_eq!(reloaded.fans[0].mode, FanMode::Off);
    }

    fn read_trimmed(path: std::path::PathBuf) -> Option<String> {
        std::fs::read_to_string(path)
            .ok()
            .map(|content| content.trim().to_string())
    }

    #[test]
    fn case_fans_get_auto_slots_when_not_configured() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let options = EngineOptions {
            hwmon_root: dir.path().to_path_buf(),
            use_nvidia: false,
            apply_writes: false,
        };
        let config = default_config(&discover(dir.path(), false).expect("discover"));
        let engine = Engine::startup(config, "cfg.toml", &options).expect("engine");
        let views = engine.case_fans();
        assert_eq!(views.len(), 1);
        assert!((views[0].slot.diameter - 60.0).abs() < 1e-9);
        assert_eq!(views[0].slot.airflow, Airflow::Exhaust);
    }

    fn engine_over(
        dir: &Path,
        apply_writes: bool,
        fans: Vec<FanConfig>,
    ) -> Result<Engine, CoreError> {
        let config = AppConfig {
            fans,
            ..AppConfig::default()
        };
        let options = EngineOptions {
            hwmon_root: dir.to_path_buf(),
            use_nvidia: false,
            apply_writes,
        };
        Engine::startup(config, dir.join("config.toml"), &options)
    }

    fn read_sysfs(root: &Path, file: &str) -> String {
        std::fs::read_to_string(root.join("hwmon1").join(file))
            .expect(file)
            .trim()
            .to_string()
    }

    fn fixed_fan(id: &str, members: &[&str], hidden: bool) -> FanConfig {
        FanConfig {
            id: FanId::new(id),
            name: None,
            rpm_factor: 1.0,
            min_speed: 0.0,
            members: members.iter().map(|id| FanId::new(*id)).collect(),
            hidden,
            mode: FanMode::Fixed { speed: 0.5 },
            slot: None,
        }
    }

    #[test]
    fn fan_groups_drive_every_member_header() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        std::fs::write(dir.path().join("hwmon1/pwm2"), "0\n").expect("pwm2");
        std::fs::write(dir.path().join("hwmon1/fan2_input"), "1234\n").expect("fan2");
        let mut engine = engine_over(
            dir.path(),
            true,
            vec![fixed_fan("nct6795-p1", &["nct6795-p2"], false)],
        )
        .expect("engine");
        assert_eq!(
            engine.fan_group(&FanId::new("nct6795-p1")).expect("group"),
            vec![FanId::new("nct6795-p2")]
        );
        // A group of two gets a display name that says so.
        assert_eq!(engine.managed_fans()[0].1, "Fan 1 on NCT 6795 (+1 more)");
        let report = engine.tick().expect("tick");
        assert_eq!(read_pwm(dir.path()), 128);
        assert_eq!(read_sysfs(dir.path(), "pwm2"), "128");
        assert_eq!(report.fans[0].measured_rpm, Some(1000.0));
    }

    #[test]
    fn group_member_conflicts_are_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        std::fs::write(dir.path().join("hwmon1/pwm2"), "0\n").expect("pwm2");

        // The same header claimed by two groups.
        let doubled = engine_over(
            dir.path(),
            false,
            vec![
                fixed_fan("nct6795-p1", &["nct6795-p2"], false),
                fixed_fan("nct6795-p2", &[], false),
            ],
        )
        .expect_err("header driven twice");
        assert!(matches!(doubled, CoreError::DuplicateFanMember(_)));

        // A header that does not exist.
        let unknown = engine_over(
            dir.path(),
            false,
            vec![fixed_fan("nct6795-p1", &["nct6795-p7"], false)],
        )
        .expect_err("unknown member");
        assert!(matches!(unknown, CoreError::UnknownFan(_)));
    }

    #[test]
    fn set_fan_members_reattaches_headers_live() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        std::fs::write(dir.path().join("hwmon1/pwm2"), "0\n").expect("pwm2");
        let mut engine = engine_over(dir.path(), true, vec![fixed_fan("nct6795-p1", &[], false)])
            .expect("engine");
        engine
            .set_fan_members(&FanId::new("nct6795-p1"), &[FanId::new("nct6795-p2")])
            .expect("grouped");
        engine
            .set_fan_mode(&FanId::new("nct6795-p1"), &FanMode::Off)
            .expect("off via leader");
        assert_eq!(read_sysfs(dir.path(), "pwm2"), "0");
        // Off writes enable=0 on both headers.
        assert_eq!(read_sysfs(dir.path(), "pwm1_enable"), "0");
        // A member id also names its group.
        assert_eq!(
            engine.fan_mode(&FanId::new("nct6795-p2")),
            Some(FanMode::Off)
        );
    }

    #[test]
    fn hidden_fans_are_skipped_and_survive_saving() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        std::fs::write(dir.path().join("hwmon1/pwm2"), "0\n").expect("pwm2");
        let config_path = dir.path().join("config.toml");
        let engine = engine_over(
            dir.path(),
            false,
            vec![
                fixed_fan("nct6795-p1", &[], false),
                fixed_fan("nct6795-p2", &[], true),
            ],
        )
        .expect("engine");
        assert_eq!(engine.managed_fans().len(), 1);
        engine.save_config().expect("save");
        let reloaded = crate::config::load(&config_path).expect("reload");
        let hidden = reloaded
            .fans
            .iter()
            .find(|fan| fan.id.as_str() == "nct6795-p2")
            .expect("hidden fan kept");
        assert!(hidden.hidden);
    }

    #[test]
    fn detect_unconnected_fans_hides_silent_headers() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        // p1 has a spinning tach; p2 reports nothing while kicked to full.
        std::fs::write(dir.path().join("hwmon1/pwm2"), "0\n").expect("pwm2");
        std::fs::write(dir.path().join("hwmon1/pwm2_enable"), "2\n").expect("pwm2_enable");
        std::fs::write(dir.path().join("hwmon1/fan2_input"), "0\n").expect("fan2");
        let mut engine = engine_over(
            dir.path(),
            true,
            vec![
                fixed_fan("nct6795-p1", &[], false),
                fixed_fan("nct6795-p2", &[], false),
            ],
        )
        .expect("engine");
        engine.tick().expect("tick"); // runs both at 50%
        assert_eq!(read_pwm(dir.path()), 128);

        let hidden = engine
            .detect_unconnected_fans(std::time::Duration::from_millis(1))
            .expect("detect");
        assert_eq!(hidden, vec![FanId::new("nct6795-p2")]);
        assert_eq!(engine.managed_fans().len(), 1);
        // The connected fan goes back to its previous speed after the spin-up.
        assert_eq!(read_pwm(dir.path()), 128);
        // The dead header was parked: zero duty (not left screaming at the
        // spin-up's full speed) and its enable mode back to the "2"
        // (hardware-automatic) found at startup.
        assert_eq!(read_sysfs(dir.path(), "pwm2"), "0");
        assert_eq!(read_sysfs(dir.path(), "pwm2_enable"), "2");

        engine.save_config().expect("save");
        let reloaded = crate::config::load(&dir.path().join("config.toml")).expect("reload");
        assert!(
            reloaded
                .fans
                .iter()
                .any(|fan| fan.id.as_str() == "nct6795-p2" && fan.hidden)
        );
    }

    #[test]
    fn detect_requires_write_access() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let mut engine = engine_over(dir.path(), false, vec![fixed_fan("nct6795-p1", &[], false)])
            .expect("engine");
        let error = engine
            .detect_unconnected_fans(std::time::Duration::from_millis(1))
            .expect_err("dry run must refuse");
        assert!(matches!(error, CoreError::Config(_)));
    }

    #[test]
    fn write_refused_needs_a_permission_error_on_an_openable_register() {
        // A register that opens for writing yet still refuses the write
        // proves the driver denies software control, not a lack of
        // privileges; the same error on a register that will not even open
        // is just this process lacking permission.
        let dir = tempfile::tempdir().expect("tempdir");
        let chip = dir.path().join("hwmon0");
        std::fs::create_dir_all(&chip).expect("chip dir");
        std::fs::write(chip.join("name"), "nct6795\n").expect("name");
        std::fs::write(chip.join("pwm1"), "0\n").expect("pwm1");
        let headers = SysfsFan::discover_all(dir.path()).expect("discovery");
        let driver = FanDriver::Sysfs(headers[0].clone());

        let permission_error = CoreError::Io {
            path: std::path::PathBuf::from("/sys/class/hwmon/hwmon9/pwm1"),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "refused"),
        };
        assert!(driver.write_refused(&permission_error));
        // Other failures are never "driver refused".
        let not_found = CoreError::Io {
            path: std::path::PathBuf::from("/sys/class/hwmon/hwmon9/pwm1"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "gone"),
        };
        assert!(!driver.write_refused(&not_found));
        assert!(!driver.write_refused(&CoreError::Config("nope".to_string())));
    }

    #[test]
    fn apply_config_swaps_in_edited_fan_settings() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        std::fs::write(dir.path().join("hwmon1/pwm2"), "0\n").expect("pwm2");
        let mut engine = engine_over(dir.path(), true, vec![fixed_fan("nct6795-p1", &[], false)])
            .expect("engine");
        engine.tick().expect("tick at 50%");
        assert_eq!(read_pwm(dir.path()), 128);

        let edited = AppConfig {
            fans: vec![
                FanConfig {
                    mode: FanMode::Fixed { speed: 0.25 },
                    ..fixed_fan("nct6795-p1", &[], false)
                },
                fixed_fan("nct6795-p2", &[], true),
            ],
            ..AppConfig::default()
        };
        engine.apply_config(edited).expect("apply");
        engine.tick().expect("tick at 25%");
        assert_eq!(read_pwm(dir.path()), 64);
        assert_eq!(engine.managed_fans().len(), 1);

        // current_config round-trips through TOML with the live changes.
        let text = crate::config::to_text(&engine.current_config()).expect("text");
        let reparsed = crate::config::from_text(&text).expect("reparse");
        let p1 = reparsed
            .fans
            .iter()
            .find(|fan| fan.id.as_str() == "nct6795-p1")
            .expect("p1");
        assert_eq!(p1.mode, FanMode::Fixed { speed: 0.25 });
        assert!(!p1.hidden);
        assert!(
            reparsed
                .fans
                .iter()
                .any(|fan| fan.id.as_str() == "nct6795-p2" && fan.hidden)
        );

        // A config naming hardware that is not present is rejected,
        // leaving the running engine untouched.
        let bogus = AppConfig {
            fans: vec![fixed_fan("nope-p9", &[], false)],
            ..AppConfig::default()
        };
        assert!(matches!(
            engine.apply_config(bogus),
            Err(CoreError::UnknownFan(_))
        ));
        assert_eq!(engine.managed_fans().len(), 1);
    }

    #[test]
    fn re_applying_the_config_keeps_the_ramp_position() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let mut engine = engine_with(
            dir.path(),
            crate::config::model::HysteresisConfig {
                temp_band_c: 2.0,
                max_step_per_tick: 0.1,
                max_step_up_per_tick: 0.1,
            },
        );
        set_cpu_temp(dir.path(), 79_000);
        engine.tick().expect("tick 1");
        let report = engine.tick().expect("tick 2");
        assert!((report.fans[0].commanded_speed - 0.2).abs() < 1e-9);

        // Saving the config round-trips it through apply_config; the fan
        // must keep climbing from 0.2 instead of restarting its ramp at 0.
        let saved = engine.current_config();
        engine.apply_config(saved).expect("re-apply");
        let report = engine.tick().expect("tick after the save");
        assert!(
            (report.fans[0].commanded_speed - 0.3).abs() < 1e-9,
            "saving must not reset the ramp"
        );
    }

    #[test]
    fn park_fans_hides_ids_reported_by_another_process() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        std::fs::write(dir.path().join("hwmon1/pwm2"), "0\n").expect("pwm2");
        let mut engine = engine_over(
            dir.path(),
            false,
            vec![
                fixed_fan("nct6795-p1", &[], false),
                fixed_fan("nct6795-p2", &[], false),
            ],
        )
        .expect("engine");
        engine.park_fans(&[FanId::new("nct6795-p2")]);
        assert_eq!(engine.managed_fans().len(), 1);
        engine.save_config().expect("save");
        let reloaded = crate::config::load(&dir.path().join("config.toml")).expect("reload");
        assert!(
            reloaded
                .fans
                .iter()
                .any(|fan| fan.id.as_str() == "nct6795-p2" && fan.hidden)
        );
    }

    #[test]
    fn release_control_returns_headers_to_hardware_control() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        // The chip originally drives this header automatically.
        std::fs::write(dir.path().join("hwmon1/pwm1_enable"), "2\n").expect("enable 2");
        let mut engine = engine_over(dir.path(), true, vec![fixed_fan("nct6795-p1", &[], false)])
            .expect("engine");
        engine.tick().expect("tick");
        assert_eq!(read_sysfs(dir.path(), "pwm1_enable"), "1");
        engine.release_control().expect("release");
        assert_eq!(read_sysfs(dir.path(), "pwm1_enable"), "2");
    }

    #[test]
    fn fan_slots_round_trip_and_reject_unknown_fans() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let mut engine = engine_over(dir.path(), false, vec![fixed_fan("nct6795-p1", &[], false)])
            .expect("engine");
        // Auto-placed fallback slot before anything is configured.
        let auto = engine.fan_slot(&FanId::new("nct6795-p1")).expect("slot");
        assert!((auto.diameter - 60.0).abs() < 1e-9);
        let slot = FanSlot {
            x: 90.0,
            y: 90.0,
            diameter: 120.0,
            rect: None,
            airflow: Airflow::Intake,
        };
        engine
            .set_fan_slot(&FanId::new("nct6795-p1"), slot)
            .expect("set");
        assert_eq!(engine.case_fans()[0].slot, slot);
        let error = engine
            .set_fan_slot(&FanId::new("nope-p9"), slot)
            .expect_err("unknown fan");
        assert!(matches!(error, CoreError::UnknownFan(_)));
    }

    #[test]
    fn auto_slots_grid_every_fan_onto_the_canvas() {
        // The user's complaint: with a dozen unplaced fans the old single
        // column ran off the bottom of the case, where circles were both
        // invisible and impossible to grab. Every auto slot must be whole,
        // on the page, and at a distinct position.
        let case = CaseConfig::default();
        let total = 12;
        let mut seen = Vec::new();
        for index in 0..total {
            let slot = default_slot(index, total, &case, false);
            let radius = slot.diameter / 2.0;
            assert!(
                slot.x >= radius + SLOT_EDGE_PAD && slot.x <= case.width - radius - SLOT_EDGE_PAD,
                "slot {index} must sit clear of the side walls"
            );
            assert!(
                slot.y >= radius + SLOT_EDGE_PAD && slot.y <= case.height - radius - SLOT_EDGE_PAD,
                "slot {index} must sit clear of the top and bottom walls"
            );
            assert!(
                !seen.contains(&(slot.x, slot.y)),
                "slot {index} lands where another already sits"
            );
            seen.push((slot.x, slot.y));
        }
    }

    #[test]
    fn far_off_canvas_slots_are_clamped_back_into_the_case() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let mut engine = engine_over(dir.path(), false, vec![fixed_fan("nct6795-p1", &[], false)])
            .expect("engine");
        let far = FanSlot {
            x: 4000.0,
            y: -100.0,
            diameter: 500.0,
            rect: None,
            airflow: Airflow::Internal,
        };
        engine
            .set_fan_slot(&FanId::new("nct6795-p1"), far)
            .expect("set");
        let fitted = engine.fan_slot(&FanId::new("nct6795-p1")).expect("slot");
        let case = engine.case_config();
        let radius = fitted.diameter / 2.0;
        assert!(
            2.0_f64.mul_add(SLOT_EDGE_PAD, fitted.diameter) <= case.width.min(case.height),
            "even a huge fan is sized to fit inside the padded walls"
        );
        assert!(
            fitted.x >= radius + SLOT_EDGE_PAD && fitted.x <= case.width - radius - SLOT_EDGE_PAD
        );
        assert!(
            fitted.y >= radius + SLOT_EDGE_PAD && fitted.y <= case.height - radius - SLOT_EDGE_PAD
        );
    }

    #[test]
    fn slots_saved_off_canvas_are_clamped_at_startup() {
        // An old config (or a hand-edited one) can save a slot past the
        // canvas bottom; loading it must pull the fan back where the GUI
        // can draw and drag it.
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let mut fan = fixed_fan("nct6795-p1", &[], false);
        fan.slot = Some(FanSlot {
            x: 282.0,
            y: 900.0,
            diameter: 60.0,
            rect: None,
            airflow: Airflow::Exhaust,
        });
        let engine = engine_over(dir.path(), false, vec![fan]).expect("engine");
        let slot = engine.fan_slot(&FanId::new("nct6795-p1")).expect("slot");
        let case = engine.case_config();
        assert!(slot.y + 30.0 <= case.height - SLOT_EDGE_PAD);
    }

    #[test]
    fn rectangular_slots_are_clamped_and_survive_saving() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let mut engine = engine_over(dir.path(), false, vec![fixed_fan("nct6795-p1", &[], false)])
            .expect("engine");
        // A card wider than the 300-wide canvas is trimmed to fit inside
        // the padded walls.
        engine
            .set_fan_slot(
                &FanId::new("nct6795-p1"),
                FanSlot {
                    x: 150.0,
                    y: 120.0,
                    diameter: 60.0,
                    rect: Some((500.0, 100.0)),
                    airflow: Airflow::Internal,
                },
            )
            .expect("set");
        let slot = engine.fan_slot(&FanId::new("nct6795-p1")).expect("slot");
        assert_eq!(slot.rect, Some((268.0, 100.0)));
        assert!(slot.is_rectangular());
        let case = engine.case_config();
        let (half_width, half_height) = slot.half_extents();
        assert!(
            slot.x >= half_width + SLOT_EDGE_PAD
                && slot.x <= case.width - half_width - SLOT_EDGE_PAD
        );
        assert!(
            slot.y >= half_height + SLOT_EDGE_PAD
                && slot.y <= case.height - half_height - SLOT_EDGE_PAD
        );
        engine.save_config().expect("save");
        let reloaded = crate::config::load(&dir.path().join("config.toml")).expect("reload");
        assert_eq!(
            reloaded.fans[0].slot.and_then(|slot| slot.rect),
            Some((268.0, 100.0))
        );
    }

    #[test]
    fn add_fan_takes_over_an_unclaimed_header_and_errors_cleanly() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        std::fs::write(dir.path().join("hwmon1/pwm2"), "0\n").expect("pwm2");
        std::fs::write(dir.path().join("hwmon1/fan2_input"), "800\n").expect("fan2");
        let mut engine = engine_over(dir.path(), false, vec![fixed_fan("nct6795-p1", &[], false)])
            .expect("engine");
        let unclaimed: Vec<String> = engine
            .unclaimed_headers()
            .into_iter()
            .map(|header| header.id.to_string())
            .collect();
        assert_eq!(unclaimed, vec!["nct6795-p2"]);

        engine.add_fan(&FanId::new("nct6795-p2")).expect("add");
        assert_eq!(engine.unclaimed_headers(), Vec::<FanDescriptor>::new());
        // A fresh auto fan binds to the most likely sensor on offer.
        assert!(
            matches!(
                engine.fan_mode(&FanId::new("nct6795-p2")),
                Some(FanMode::Auto { .. })
            ),
            "an added fan starts auto-bound to a sensor"
        );
        // Managed order follows the claim order rule (controller, header).
        assert_eq!(engine.managed_fans().len(), 2);
        assert_eq!(engine.managed_fans()[1].0, FanId::new("nct6795-p2"));

        let again = engine
            .add_fan(&FanId::new("nct6795-p2"))
            .expect_err("already managed");
        assert!(matches!(again, CoreError::DuplicateFanMember(_)));
        let missing = engine
            .add_fan(&FanId::new("nct6795-p9"))
            .expect_err("no such header");
        assert!(matches!(missing, CoreError::UnknownFan(_)));

        // The new fan is part of the config that `Save config` writes.
        engine.save_config().expect("save");
        let reloaded = crate::config::load(&dir.path().join("config.toml")).expect("reload");
        assert!(
            reloaded
                .fans
                .iter()
                .any(|fan| fan.id.as_str() == "nct6795-p2")
        );
    }

    #[test]
    fn add_fan_revives_a_parked_header_keeping_its_settings() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let mut engine = engine_over(dir.path(), false, vec![fixed_fan("nct6795-p1", &[], true)])
            .expect("engine");
        assert!(engine.fans.is_empty());
        // Parked headers still show up as addable.
        assert_eq!(engine.unclaimed_headers().len(), 1);

        engine.add_fan(&FanId::new("nct6795-p1")).expect("revive");
        assert_eq!(engine.hidden_fans, Vec::<FanConfig>::new());
        assert_eq!(engine.fans.len(), 1);
        assert_eq!(
            engine.fan_mode(&FanId::new("nct6795-p1")),
            Some(FanMode::Fixed { speed: 0.5 }),
            "the parked fan comes back with the mode it had"
        );
    }

    #[test]
    fn renaming_a_fan_shows_and_saves_and_empty_restores_the_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let mut engine = engine_over(dir.path(), false, vec![fixed_fan("nct6795-p1", &[], false)])
            .expect("engine");
        engine
            .set_fan_name(&FanId::new("nct6795-p1"), " Front intake ")
            .expect("rename");
        assert_eq!(
            engine.managed_fans()[0].1,
            "Front intake",
            "the display name follows the rename"
        );
        engine.save_config().expect("save");
        let reloaded = crate::config::load(&dir.path().join("config.toml")).expect("reload");
        assert_eq!(
            reloaded.fans[0].name.as_deref(),
            Some("Front intake"),
            "the rename survives a save round trip"
        );
        // An empty name falls back to the hardware-derived default.
        engine
            .set_fan_name(&FanId::new("nct6795-p1"), "   ")
            .expect("clear name");
        assert_eq!(engine.managed_fans()[0].1, "Fan 1 on NCT 6795");
        let error = engine
            .set_fan_name(&FanId::new("nope-p9"), "x")
            .expect_err("unknown fan");
        assert!(matches!(error, CoreError::UnknownFan(_)));
    }

    #[test]
    fn engine_reports_logging_quiet_from_its_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let options = EngineOptions {
            hwmon_root: dir.path().to_path_buf(),
            use_nvidia: false,
            apply_writes: true,
        };
        let chatty = Engine::startup(
            AppConfig::default(),
            dir.path().join("config.toml"),
            &options,
        )
        .expect("default engine");
        assert!(!chatty.quiet(), "no config means every tick logs");
        let quiet_engine = Engine::startup(
            AppConfig {
                quiet: true,
                ..AppConfig::default()
            },
            dir.path().join("config.toml"),
            &options,
        )
        .expect("quiet engine");
        assert!(quiet_engine.quiet());
    }

    #[test]
    fn hiding_a_sensor_keeps_it_reading_and_driving() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let mut engine = engine_with(
            dir.path(),
            crate::config::model::HysteresisConfig {
                temp_band_c: 2.0,
                max_step_per_tick: 1.0,
                max_step_up_per_tick: 1.0,
            },
        );
        let cpu = SensorId::new("k10temp-t1");
        engine.set_sensor_hidden(&cpu, true).expect("hide");
        assert!(engine.sensor_hidden(&cpu));
        // The fan bound to the hidden sensor keeps following its curve:
        // 70 °C on the fixture's 40→0.2 / 80→1.0 curve is 0.8.
        set_cpu_temp(dir.path(), 70_000);
        let report = engine.tick().expect("tick");
        assert!(
            (report.fans[0].commanded_speed - 0.8).abs() < 1e-9,
            "a hidden sensor still drives its fan"
        );
        // The tick report keeps reporting the hidden sensor too — charts
        // and fail-safes need its reading; only the GUI list consults the
        // flag.
        assert!(
            report
                .sensors
                .iter()
                .any(|sensor| sensor.id == cpu && sensor.temp_c.is_some())
        );
        // The flag lands in the config the engine would save.
        assert_eq!(engine.current_config().hidden_sensors, vec![cpu.clone()]);
        // ...and hiding again is a no-op, so a double hide cannot
        // duplicate the id.
        engine.set_sensor_hidden(&cpu, true).expect("re-hide");
        assert_eq!(engine.current_config().hidden_sensors.len(), 1);
        // Showing it again empties the list.
        engine.set_sensor_hidden(&cpu, false).expect("show");
        assert!(!engine.sensor_hidden(&cpu));
        assert!(
            engine.current_config().hidden_sensors.is_empty(),
            "showing it again empties the list"
        );
    }

    #[test]
    fn set_sensor_hidden_rejects_a_sensor_the_machine_lacks() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let mut engine = engine_with(
            dir.path(),
            crate::config::model::HysteresisConfig::default(),
        );
        let error = engine
            .set_sensor_hidden(&SensorId::new("nope-t9"), true)
            .expect_err("unknown sensor");
        assert!(matches!(error, CoreError::UnknownSensor(_)));
    }

    #[test]
    fn startup_accepts_a_config_that_hides_a_bound_sensor() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let config = AppConfig {
            fans: vec![FanConfig {
                id: FanId::new("nct6795-p1"),
                name: None,
                rpm_factor: 1.0,
                min_speed: 0.0,
                members: Vec::new(),
                hidden: false,
                mode: FanMode::Auto {
                    sensor: SensorId::new("k10temp-t1"),
                    curve: FanCurve::default(),
                },
                slot: None,
            }],
            // A hidden sensor a fan is bound to — legal, and still used:
            // hiding only changes what the GUI lists.
            hidden_sensors: vec![SensorId::new("k10temp-t1")],
            ..AppConfig::default()
        };
        let mut engine = Engine::startup(
            config,
            dir.path().join("config.toml"),
            &EngineOptions {
                hwmon_root: dir.path().to_path_buf(),
                use_nvidia: false,
                apply_writes: true,
            },
        )
        .expect("a hidden binding is still a valid binding");
        assert!(engine.sensor_hidden(&SensorId::new("k10temp-t1")));
        let report = engine.tick().expect("tick");
        assert_eq!(
            report.fans[0].governing_temp_c,
            Some(55.0),
            "the hidden sensor still governs its fan"
        );
    }

    #[test]
    fn rpm_factor_corrects_a_tach_that_reports_double() {
        // The fixture tach reads 1000; a fan whose reading always shows
        // double needs a factor of 0.5.
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let mut engine = engine_over(dir.path(), false, vec![fixed_fan("nct6795-p1", &[], false)])
            .expect("engine");
        let report = engine.tick().expect("tick");
        assert_eq!(report.fans[0].measured_rpm, Some(1000.0));
        engine
            .set_fan_rpm_factor(&FanId::new("nct6795-p1"), 0.5)
            .expect("factor");
        let report = engine.tick().expect("tick");
        assert_eq!(report.fans[0].measured_rpm, Some(500.0));
        engine.save_config().expect("save");
        let reloaded = crate::config::load(&dir.path().join("config.toml")).expect("reload");
        assert!(
            (reloaded.fans[0].rpm_factor - 0.5).abs() < 1e-12,
            "the correction is persisted"
        );
        // A factor far off in the wilds of the slider is clamped, not
        // trusted.
        engine
            .set_fan_rpm_factor(&FanId::new("nct6795-p1"), -3.0)
            .expect("clamp");
        let report = engine.tick().expect("tick");
        assert_eq!(report.fans[0].measured_rpm, Some(10.0));
    }

    #[test]
    fn gpu_index_in_sensor_id_parses_nvml_sensor_ids() {
        assert_eq!(gpu_index_in_sensor_id("nvidia-gpu0-t1"), Some(0));
        assert_eq!(gpu_index_in_sensor_id("nvidia-gpu12-t1"), Some(12));
        assert_eq!(gpu_index_in_sensor_id("k10temp-t1"), None);
        assert_eq!(gpu_index_in_sensor_id("nvidia-gpu-t1"), None);
        // The cooler id shares the ordinal, but it is not a sensor id.
        assert_eq!(gpu_index_in_sensor_id("nvidia-gpu0-cooler"), Some(0));
    }
}
