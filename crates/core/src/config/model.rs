//! The application configuration model, stored as TOML.
//!
//! Example config:
//!
//! ```toml
//! poll_interval_ms = 1000
//! fail_safe_speed = 1.0
//!
//! [hysteresis]
//! temp_band_c = 2.0
//! max_step_per_tick = 0.1
//!
//! [sensor_names]
//! "k10temp-t3" = "CPU CCD 1 temperature"
//!
//! [[fans]]
//! id = "nct6795-p1"
//! name = "Front 120mm intake"
//! rpm_factor = 0.5                   # halve a tach that reports double
//! members = ["nct6795-p2", "nct6795-p3"]  # driven together as one fan
//! mode = { type = "auto", sensor = "k10temp-t1" }
//!
//! [fans.slot]
//! x = 40.0
//! y = 380.0
//! diameter = 70.0
//! airflow = "intake"
//! ```

use crate::case::{CaseConfig, FanSlot};
use crate::fans::FanId;
use crate::fans::FanMode;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Tuning values that keep auto fans from pumping up and down.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HysteresisConfig {
    /// How far below the temperature that last raised the speed the sensor
    /// must fall before the speed is allowed to drop.
    pub temp_band_c: f64,
    /// Largest speed-fraction change per update tick (1.0 disables
    /// rate-limiting).
    pub max_step_per_tick: f64,
}

impl Default for HysteresisConfig {
    fn default() -> Self {
        Self {
            temp_band_c: 2.0,
            max_step_per_tick: 0.1,
        }
    }
}

/// One logical fan's configuration: a leader header plus any extra headers
/// driven together with it (an AIO radiator's three fans, for example).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FanConfig {
    /// Leader header, e.g. `nct6795-p1` (see `biggest-fan scan`).
    pub id: FanId,
    /// Display name; overrides the hardware-derived default when set.
    #[serde(default)]
    pub name: Option<String>,
    /// Measured RPM is multiplied by this factor (default 1.0). Some
    /// tachometers report exactly double (pulses-per-revolution wired or
    /// reported wrongly); a factor of 0.5 corrects the reading without
    /// touching hardware.
    #[serde(default = "default_rpm_factor")]
    pub rpm_factor: f64,
    /// Extra headers driven as one with the leader; every member must be a
    /// header discovered on this machine and belongs to at most one group.
    #[serde(default)]
    pub members: Vec<FanId>,
    /// Headers found to have nothing plugged in, or whose driver refuses
    /// software control, are disabled and hidden from the lists. Detection
    /// (`biggest-fan detect-fans`) sets this, and so does a control-loop
    /// write the driver rejects even for root.
    #[serde(default)]
    pub hidden: bool,
    /// Off / fixed / auto.
    pub mode: FanMode,
    /// Where to draw this fan on the case view; auto-placed when omitted.
    #[serde(default)]
    pub slot: Option<FanSlot>,
}

/// Everything Biggest Fan reads from its config file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Control-loop refresh period in milliseconds (floors at 100 ms).
    pub poll_interval_ms: u64,
    /// Speed fraction used for auto fans whose sensor stops responding.
    pub fail_safe_speed: f64,
    /// Anti-pumping settings for auto fans.
    pub hysteresis: HysteresisConfig,
    /// Human readable display names keyed by sensor id.
    pub sensor_names: BTreeMap<String, String>,
    /// Fan headers under management.
    pub fans: Vec<FanConfig>,
    /// Case canvas size for the GUI.
    pub case: CaseConfig,
}

/// The neutral tachometer correction for [`FanConfig::rpm_factor`].
const fn default_rpm_factor() -> f64 {
    1.0
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            poll_interval_ms: 1000,
            fail_safe_speed: 1.0,
            hysteresis: HysteresisConfig::default(),
            sensor_names: BTreeMap::new(),
            fans: Vec::new(),
            case: CaseConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fans::{CurvePoint, FanCurve};
    use crate::sensors::SensorId;

    #[test]
    fn example_config_parses_with_every_feature() {
        let text = r#"
            poll_interval_ms = 500
            fail_safe_speed = 0.8

            [hysteresis]
            temp_band_c = 3.0
            max_step_per_tick = 0.05

            [sensor_names]
            "k10temp-t3" = "CPU CCD 1 temperature"

            [[fans]]
            id = "nct6795-p1"
            name = "Front 120mm intake"
            members = ["nct6795-p2", "it87-p4"]
            mode = { type = "auto", sensor = "k10temp-t1", curve = [
                { temp_c = 40.0, speed = 0.2 },
                { temp_c = 80.0, speed = 1.0 },
            ] }

            [fans.slot]
            x = 40.0
            y = 380.0
            diameter = 70.0
            airflow = "intake"
        "#;
        let config: AppConfig = toml::from_str(text).expect("parses");
        assert_eq!(config.poll_interval_ms, 500);
        assert!((config.fail_safe_speed - 0.8).abs() < 1e-12);
        assert!((config.hysteresis.temp_band_c - 3.0).abs() < 1e-12);
        assert_eq!(config.sensor_names["k10temp-t3"], "CPU CCD 1 temperature");
        assert_eq!(config.fans.len(), 1);
        assert_eq!(config.fans[0].name.as_deref(), Some("Front 120mm intake"));
        assert!(
            (config.fans[0].rpm_factor - 1.0).abs() < 1e-12,
            "a config without rpm_factor keeps raw tach readings"
        );
        let members: Vec<String> = config.fans[0]
            .members
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(members, ["nct6795-p2", "it87-p4"]);
        assert!(!config.fans[0].hidden);
        let slot = config.fans[0].slot.expect("slot");
        assert_eq!(slot.airflow, crate::case::Airflow::Intake);
        let crate::fans::FanMode::Auto { curve, sensor, .. } = &config.fans[0].mode else {
            panic!("expected auto mode");
        };
        assert_eq!(sensor.as_str(), "k10temp-t1");
        assert_eq!(curve.points().len(), 2);
    }

    #[test]
    fn empty_config_uses_sane_defaults() {
        let config: AppConfig = toml::from_str("").expect("parses");
        assert_eq!(config.poll_interval_ms, 1000);
        assert!((config.fail_safe_speed - 1.0).abs() < 1e-12);
        assert!((config.hysteresis.temp_band_c - 2.0).abs() < 1e-12);
        assert_eq!(config.fans, Vec::new());
    }

    #[test]
    fn fan_groups_and_hidden_flags_round_trip() {
        let config = AppConfig {
            fans: vec![
                FanConfig {
                    id: FanId::new("nct6795-p1"),
                    name: Some("AIO radiator".to_string()),
                    rpm_factor: 1.0,
                    members: vec![FanId::new("nct6795-p2"), FanId::new("nct6795-p3")],
                    hidden: false,
                    mode: crate::fans::FanMode::Fixed { speed: 0.5 },
                    slot: None,
                },
                FanConfig {
                    id: FanId::new("it87-p4"),
                    name: None,
                    rpm_factor: 0.5,
                    members: Vec::new(),
                    hidden: true,
                    mode: crate::fans::FanMode::Off,
                    slot: None,
                },
            ],
            ..AppConfig::default()
        };
        let text = toml::to_string_pretty(&config).expect("serialize");
        let back: AppConfig = toml::from_str(&text).expect("deserialize");
        assert_eq!(config, back);
    }

    #[test]
    fn round_trips_through_toml() {
        let config = AppConfig {
            poll_interval_ms: 750,
            fail_safe_speed: 1.0,
            hysteresis: HysteresisConfig::default(),
            sensor_names: BTreeMap::from([(
                "k10temp-t1".to_string(),
                "CPU temperature".to_string(),
            )]),
            fans: vec![
                FanConfig {
                    id: FanId::new("it87-p2"),
                    name: None,
                    rpm_factor: 1.0,
                    members: Vec::new(),
                    hidden: false,
                    mode: crate::fans::FanMode::Fixed { speed: 0.35 },
                    slot: None,
                },
                FanConfig {
                    id: FanId::new("nct6795-p1"),
                    name: Some("Top exhaust".to_string()),
                    rpm_factor: 1.0,
                    members: Vec::new(),
                    hidden: false,
                    mode: crate::fans::FanMode::Auto {
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
                    slot: Some(crate::case::FanSlot {
                        x: 150.0,
                        y: 60.0,
                        diameter: 80.0,
                        airflow: crate::case::Airflow::Exhaust,
                    }),
                },
            ],
            case: CaseConfig::default(),
        };
        let text = toml::to_string_pretty(&config).expect("serialize");
        let back: AppConfig = toml::from_str(&text).expect("deserialize");
        assert_eq!(config, back);
    }

    #[test]
    fn invalid_mode_reports_config_error() {
        let text = r#"
            [[fans]]
            id = "it87-p1"
            mode = { type = "wobbly" }
        "#;
        let error = toml::from_str::<AppConfig>(text).expect_err("bad tag");
        assert!(error.to_string().contains("wobbly"));
    }
}
