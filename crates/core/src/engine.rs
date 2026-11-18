//! The control loop: discover hardware, apply fan modes with hysteresis,
//! and report live status.

use crate::case::{Airflow, CaseConfig, FanSlot};
use crate::config::model::{AppConfig, FanConfig, HysteresisConfig};
use crate::error::CoreError;
use crate::fans::model::{FanCurve, FanDescriptor, FanId, FanMode, FanStatus};
use crate::fans::sysfs::SysfsFan;
use crate::sensors::model::{SensorDescriptor, SensorId, SensorSource, SensorStatus};
use crate::sensors::{HwmonSource, NvidiaSource, SensorCategory, sensor_category};
use std::collections::BTreeMap;
use std::io::ErrorKind;
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

/// Discovers sensors and fan headers.
///
/// `hwmon_root` is normally `/sys/class/hwmon`; tests may point it at a
/// fixture tree. `use_nvidia` controls whether NVML is attempted (a missing
/// NVIDIA driver is not an error).
///
/// # Errors
///
/// [`CoreError::Io`] when the hwmon root is unreadable, or NVML errors
/// beyond "driver not present".
pub fn discover(hwmon_root: &Path, use_nvidia: bool) -> Result<Discovery, CoreError> {
    let mut hwmon = HwmonSource::with_root(hwmon_root);
    let mut discovery = Discovery {
        sensors: hwmon.discover()?,
        fans: SysfsFan::discover_all(hwmon_root)?
            .iter()
            .map(SysfsFan::descriptor)
            .collect(),
    };
    if use_nvidia && let Some(mut nvidia) = NvidiaSource::try_new()? {
        discovery.sensors.extend(nvidia.discover()?);
    }
    order_sensors(&mut discovery.sensors);
    Ok(discovery)
}

/// Orders sensors by how useful they are (CPU and GPU first, then
/// motherboard/drive temperatures, then case temperatures) and by display
/// name within a category.
fn order_sensors(sensors: &mut [SensorDescriptor]) {
    sensors.sort_by_key(|sensor| {
        let chip = sensor.raw_label.split_whitespace().next().unwrap_or("");
        (
            sensor_category(chip, &sensor.default_name),
            sensor.default_name.clone(),
        )
    });
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
                members: Vec::new(),
                hidden: false,
                mode: governing_sensor_for(discovery, fan).map_or_else(
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
fn governing_sensor_for(discovery: &Discovery, fan: &FanDescriptor) -> Option<SensorId> {
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
        let matches: Vec<&SensorDescriptor> = discovery
            .sensors
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
    discovery.sensors.first().map(|sensor| sensor.id.clone())
}

/// Per-run wiring of the engine: where to look and whether writes happen.
#[derive(Clone, Debug)]
pub struct EngineOptions {
    /// hwmon class directory (tests point this at a fixture).
    pub hwmon_root: PathBuf,
    /// Attempt to read NVIDIA GPUs through NVML.
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
    /// Speed fraction commanded on the previous tick.
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
    sysfs: Vec<SysfsFan>,
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
    sources: Vec<Box<dyn SensorSource>>,
    sensors: Vec<SensorDescriptor>,
    sensor_names: BTreeMap<SensorId, String>,
    latest: BTreeMap<SensorId, f64>,
    fans: Vec<FanEntry>,
    /// Every fan header discovered at startup, managed or not.
    all_headers: Vec<SysfsFan>,
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
            .field("sources", &self.sources)
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
    /// sensor id exists.
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
        let mut sources: Vec<Box<dyn SensorSource>> =
            vec![Box::new(HwmonSource::with_root(&options.hwmon_root))];
        if options.use_nvidia
            && let Some(nvidia) = NvidiaSource::try_new()?
        {
            sources.push(Box::new(nvidia));
        }
        let mut sensors = Vec::new();
        for source in &mut sources {
            sensors.extend(source.discover()?);
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

        let all_headers = SysfsFan::discover_all(&options.hwmon_root)?;
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
            sources,
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
        all_headers: &[SysfsFan],
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
                .any(|entry: &FanEntry| entry.sysfs[0].id() == &fan_config.id)
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
            let mut sysfs = Vec::new();
            for member_id in &member_ids {
                if claimed.iter().any(|claimed_id| claimed_id == member_id) {
                    return Err(CoreError::DuplicateFanMember(member_id.to_string()));
                }
                let matched = all_headers
                    .iter()
                    .find(|fan| fan.id() == member_id)
                    .ok_or_else(|| CoreError::UnknownFan(member_id.to_string()))?;
                sysfs.push(matched.clone());
                claimed.push(member_id.clone());
            }
            if let FanMode::Auto { sensor, .. } = &fan_config.mode
                && !sensor_names.contains_key(sensor)
            {
                return Err(CoreError::UnknownSensor(sensor.to_string()));
            }
            let display_name = display_name_for(&fan_config, &sysfs);
            fans.push(FanEntry {
                fan_config,
                sysfs,
                runtime: FanRuntime::default(),
                display_name,
                lost_control: None,
            });
        }
        // Show and save fans in controller-name then header-number order,
        // whatever order the config file listed them in.
        fans.sort_by_key(|entry| {
            (
                entry.sysfs[0].chip().to_string(),
                entry.sysfs[0].pwm_index(),
            )
        });
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
        for source in &mut self.sources {
            if let Ok(readings) = source.read_all() {
                self.latest.extend(readings);
            }
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
        let entry = &mut self.fans[index];
        let mut note = None;
        let mut governing_temp_c = None;
        let base = entry.runtime.last_commanded;
        let mut target = base;
        // Work on a copy so `entry` stays free for the writes below.
        let mode = entry.fan_config.mode.clone();

        match &mode {
            FanMode::Off => {
                entry.runtime = FanRuntime::default();
                target = 0.0;
            }
            FanMode::Fixed { speed } => {
                entry.runtime = FanRuntime::default();
                target = speed.clamp(0.0, 1.0);
            }
            FanMode::Auto { sensor, curve } => {
                match self.latest.get(sensor).copied() {
                    None => {
                        // Fail safe: a sensor we cannot read must not leave
                        // the fan parked at some old low speed. Fail-safes
                        // bypass rate limiting so protection is immediate.
                        target = fail_safe;
                        entry.runtime.hold_temp_c = None;
                        let sensor_name = self
                            .sensor_names
                            .get(sensor)
                            .cloned()
                            .unwrap_or_else(|| sensor.to_string());
                        note = Some(format!(
                            "{sensor_name} is not responding — running this fan at {:.0}% as a fail-safe",
                            fail_safe * 100.0
                        ));
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
                        // Rate-limit auto changes so fans ramp instead of pumping.
                        if hysteresis.max_step_per_tick > 0.0 {
                            target = approach(base, target, hysteresis.max_step_per_tick);
                        }
                    }
                }
            }
        }

        let commanded = target.clamp(0.0, 1.0);
        if self.apply_writes {
            for header in &mut entry.sysfs {
                let written = if matches!(mode, FanMode::Off) {
                    header.stop()
                } else {
                    header.apply(commanded)
                };
                if let Err(error) = written {
                    if driver_refused(&error, header.open_writable()) {
                        // The register opens for writing yet the driver
                        // still refuses: software can never drive this
                        // header. Park it instead of failing every tick.
                        entry.lost_control = Some(format!(
                            "{} refuses writes — the driver exposes this header as not software-controllable; disabled",
                            header.id()
                        ));
                        break;
                    }
                    return Err(error);
                }
            }
        }
        entry.runtime.last_commanded = commanded;
        if let Some(message) = &entry.lost_control {
            note = Some(message.clone());
        }

        Ok(FanStatus {
            id: entry.fan_config.id.clone(),
            name: entry.display_name.clone(),
            mode,
            commanded_speed: commanded,
            measured_rpm: entry
                .sysfs
                .iter()
                .find_map(SysfsFan::read_rpm)
                .map(|rpm| rpm * entry.fan_config.rpm_factor),
            governing_temp_c,
            note,
        })
    }

    /// Refreshes one fan group's mode and (unless dry-run) applies it at
    /// once. `fan_id` may be the group leader or one of its members.
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
        entry.fan_config.mode = mode.clone();
        entry.runtime = FanRuntime::default();
        if self.apply_writes {
            for header in &mut entry.sysfs {
                match mode {
                    FanMode::Off => header.stop()?,
                    FanMode::Fixed { speed } => header.apply(speed.clamp(0.0, 1.0))?,
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
            .position(|entry| entry.sysfs.iter().any(|header| header.id() == fan_id))
    }

    /// Every fan header this machine exposes, managed or not.
    #[must_use]
    pub fn fan_headers(&self) -> Vec<FanDescriptor> {
        self.all_headers.iter().map(SysfsFan::descriptor).collect()
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
            .find(|entry| entry.sysfs.iter().any(|header| header.id() == header_id))
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
                other != index && entry.sysfs.iter().any(|header| header.id() == member_id)
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
        entry.sysfs = headers;
        entry.fan_config.members = member_ids;
        let name = display_name_for(&entry.fan_config, &entry.sysfs);
        entry.display_name = name;
        Ok(())
    }

    /// Where a managed fan is drawn on the case view: the configured slot,
    /// or the auto-placed fallback when none is set.
    #[must_use]
    pub fn fan_slot(&self, fan_id: &FanId) -> Option<FanSlot> {
        let index = self.find_group(fan_id)?;
        Some(
            self.fans[index]
                .fan_config
                .slot
                .unwrap_or_else(|| default_slot(index, self.fans.len(), &self.config.case)),
        )
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
        entry.display_name = display_name_for(&entry.fan_config, &entry.sysfs);
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
        let (fans, hidden_fans) = Self::claim_fans(
            &self.all_headers,
            &self.sensor_names,
            std::mem::take(&mut config.fans),
            config.case,
        )?;
        self.config = config;
        self.fans = fans;
        self.hidden_fans = hidden_fans;
        Ok(())
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
                let slot = entry
                    .fan_config
                    .slot
                    .unwrap_or_else(|| default_slot(index, self.fans.len(), &self.config.case));
                let status = self
                    .last_report
                    .fans
                    .iter()
                    .find(|status| &status.id == entry.sysfs[0].id());
                CaseFanView {
                    id: entry.sysfs[0].id().clone(),
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
            .map(|entry| (entry.sysfs[0].id().clone(), entry.display_name.clone()))
            .collect()
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
            for header in &mut entry.sysfs {
                if let Err(error) = header.apply(1.0) {
                    if driver_refused(&error, header.open_writable()) {
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
                    || (entry.sysfs.iter().any(SysfsFan::tach_supported)
                        && !entry
                            .sysfs
                            .iter()
                            .any(|header| header.read_rpm().is_some_and(|rpm| rpm > 1.0)))
            })
            .map(|(index, _entry)| index)
            .collect();

        let mut newly_hidden = Vec::new();
        for index in parked.into_iter().rev() {
            let mut entry = self.fans.remove(index);
            for header in &mut entry.sysfs {
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
                for header in &mut entry.sysfs {
                    header.stop()?;
                }
            } else {
                let speed = entry.runtime.last_commanded;
                for header in &mut entry.sysfs {
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

    /// Hands every managed fan header back to the motherboard's automatic
    /// control, restoring the `pwmN_enable` mode found at startup. A dry
    /// run never took control and restores nothing.
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
            for header in &mut entry.sysfs {
                if let Err(error) = header.restore_control() {
                    first_error.get_or_insert(error);
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

/// Group display name: a user-chosen `name` always wins; a lone header
/// shows its hardware default, and a group notes its extra members.
fn display_name_for(fan_config: &FanConfig, headers: &[SysfsFan]) -> String {
    if let Some(name) = &fan_config.name {
        return name.clone();
    }
    let primary = headers[0].default_name();
    match headers.len() {
        1 => primary.to_string(),
        extra => format!("{primary} (+{} more)", extra - 1),
    }
}

/// Whether a failed header write proves the *driver* refuses software
/// control (a permission error although the register opens for writing),
/// as opposed to this process merely lacking the privileges to write it.
fn driver_refused(error: &CoreError, register_openable: bool) -> bool {
    matches!(
        error,
        CoreError::Io { source, .. }
            if source.kind() == ErrorKind::PermissionDenied
    ) && register_openable
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

/// Clamps a slot inside the case canvas: the diameter can never exceed
/// the canvas, and the centre stays far enough from the edges that the
/// whole circle is on the page — a fan drawn off-canvas can never be
/// grabbed to move it back.
fn fit_slot(mut slot: FanSlot, case: &CaseConfig) -> FanSlot {
    slot.diameter = slot.diameter.clamp(10.0, case.width.min(case.height));
    let radius = slot.diameter / 2.0;
    slot.x = slot.x.clamp(radius, case.width - radius);
    slot.y = slot.y.clamp(radius, case.height - radius);
    slot
}

/// Fallback layout for fans without a configured slot: a grid of circles
/// starting at the top-left, always inside the canvas. (A single column
/// used to run fans with no slot past the bottom of the case, where they
/// were invisible and impossible to drag.)
fn default_slot(index: usize, total: usize, case: &CaseConfig) -> FanSlot {
    let diameter = 60.0;
    let pitch = diameter + 14.0;
    // Smallest column count whose grid fits every fan (integer square
    // root of `total`, never below 1).
    let mut columns = 1usize;
    while columns * columns < total {
        columns += 1;
    }
    #[expect(clippy::cast_precision_loss, reason = "grid indices are tiny")]
    let (column, row) = ((index % columns) as f64, (index / columns) as f64);
    fit_slot(
        FanSlot {
            x: column.mul_add(pitch, 18.0 + diameter / 2.0),
            y: row.mul_add(pitch, 18.0 + diameter / 2.0),
            diameter,
            airflow: Airflow::Exhaust,
        },
        case,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fans::CurvePoint;

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

    fn engine_with(temp_dir: &Path, hysteresis: crate::config::model::HysteresisConfig) -> Engine {
        let config = AppConfig {
            fans: vec![FanConfig {
                id: FanId::new("nct6795-p1"),
                name: Some("Rear exhaust".to_string()),
                rpm_factor: 1.0,
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
    fn dead_sensor_triggers_the_fail_safe() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let mut engine = engine_with(
            dir.path(),
            crate::config::model::HysteresisConfig {
                temp_band_c: 0.0,
                max_step_per_tick: 1.0,
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

    #[test]
    fn dry_run_computes_but_never_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let config = AppConfig {
            fans: vec![FanConfig {
                id: FanId::new("nct6795-p1"),
                name: None,
                rpm_factor: 1.0,
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
    fn driver_refused_needs_a_permission_error_on_an_openable_register() {
        let permission_error = CoreError::Io {
            path: std::path::PathBuf::from("/sys/class/hwmon/hwmon9/pwm1"),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "refused"),
        };
        // A register that opens for writing yet still refuses the write
        // proves the driver denies software control, not a lack of
        // privileges; the same error on a register that will not even open
        // is just this process lacking permission.
        assert!(driver_refused(&permission_error, true));
        assert!(!driver_refused(&permission_error, false));
        // Other failures are never "driver refused".
        let not_found = CoreError::Io {
            path: std::path::PathBuf::from("/sys/class/hwmon/hwmon9/pwm1"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "gone"),
        };
        assert!(!driver_refused(&not_found, true));
        assert!(!driver_refused(
            &CoreError::Config("nope".to_string()),
            true
        ));
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
            x: 60.0,
            y: 60.0,
            diameter: 120.0,
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
            let slot = default_slot(index, total, &case);
            let radius = slot.diameter / 2.0;
            assert!(slot.x >= radius && slot.x <= case.width - radius);
            assert!(slot.y >= radius && slot.y <= case.height - radius);
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
            airflow: Airflow::Internal,
        };
        engine
            .set_fan_slot(&FanId::new("nct6795-p1"), far)
            .expect("set");
        let fitted = engine.fan_slot(&FanId::new("nct6795-p1")).expect("slot");
        let case = engine.case_config();
        let radius = fitted.diameter / 2.0;
        assert!(fitted.diameter <= case.width.min(case.height));
        assert!(fitted.x >= radius && fitted.x <= case.width - radius);
        assert!(fitted.y >= radius && fitted.y <= case.height - radius);
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
            airflow: Airflow::Exhaust,
        });
        let engine = engine_over(dir.path(), false, vec![fan]).expect("engine");
        let slot = engine.fan_slot(&FanId::new("nct6795-p1")).expect("slot");
        let case = engine.case_config();
        assert!(slot.y + 30.0 <= case.height);
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
}
