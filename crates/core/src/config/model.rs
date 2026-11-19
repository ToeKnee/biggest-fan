//! The application configuration model, stored as TOML.
//!
//! Example config:
//!
//! ```toml
//! poll_interval_ms = 1000
//! fail_safe_speed = 1.0
//! hidden_sensors = ["nct6795-t2"]  # hide useless temps from the GUI list
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
//! min_speed = 0.4                    # never slower than this (an AIO pump)
//! members = ["nct6795-p2", "nct6795-p3"]  # driven together as one fan
//! mode = { type = "auto", sensor = "k10temp-t1" }
//!
//! [fans.slot]
//! x = 40.0
//! y = 380.0
//! diameter = 70.0
//! airflow = "intake"
//! # A whole rectangular object (e.g. a graphics card cooler) instead of
//! # a circle:
//! # rect = [220.0, 80.0]
//! ```

use crate::case::{CaseConfig, FanSlot};
use crate::fans::FanId;
use crate::fans::FanMode;
use crate::sensors::SensorId;
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
    /// Speed floor: never command a speed below this (0.0..=1.0; the
    /// default 0.0 means no floor). Set it for something that must always
    /// turn — an AIO pump — so a Quiet curve or a cold sensor cannot park
    /// it. _Off_ mode is the exception: stopping a fan by hand is a choice
    /// the user makes, not something this floor overrides. On an NVIDIA
    /// cooler a floor under the park point
    /// ([`crate::fans::nvidia::SPIN_DOWN_FRACTION`]) cannot start the fans,
    /// so it effectively parks the cooler.
    #[serde(default)]
    pub min_speed: f64,
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
    /// Sensor ids hidden from the GUI's temperature list. Hiding is
    /// display-only: the engine keeps reading them every tick, so a fan
    /// bound to a hidden sensor keeps working (the GUI's hide buttons set
    /// this; hidden sensors still appear in the binding dropdowns).
    pub hidden_sensors: Vec<SensorId>,
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
            hidden_sensors: Vec::new(),
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
        assert!(
            config.hidden_sensors.is_empty(),
            "the example hides no sensors"
        );
        assert_eq!(config.fans.len(), 1);
        assert_eq!(config.fans[0].name.as_deref(), Some("Front 120mm intake"));
        assert!(
            (config.fans[0].rpm_factor - 1.0).abs() < 1e-12,
            "a config without rpm_factor keeps raw tach readings"
        );
        assert!(
            config.fans[0].min_speed == 0.0,
            "a config without min_speed has no floor"
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
    fn a_pump_floor_round_trips_and_defaults_to_no_floor() {
        let text = r#"
            [[fans]]
            id = "nct6795-p4"
            min_speed = 0.4
            mode = { type = "auto", sensor = "nct6795-t2" }

            [[fans]]
            id = "nct6795-p5"
            mode = { type = "off" }
        "#;
        let config: AppConfig = toml::from_str(text).expect("parses");
        assert!(
            (config.fans[0].min_speed - 0.4).abs() < 1e-12,
            "min_speed parses"
        );
        assert_eq!(config.fans[1].min_speed, 0.0, "omitted means no floor");
        let text = toml::to_string_pretty(&config).expect("serialize");
        let back: AppConfig = toml::from_str(&text).expect("deserialize");
        assert_eq!(config, back);
    }

    #[test]
    fn hidden_sensors_parse_default_and_round_trip() {
        let text = r#"
            hidden_sensors = ["nct6795-t2", "nvme-2-t1"]

            [[fans]]
            id = "nct6795-p1"
            mode = { type = "auto", sensor = "k10temp-t1" }
        "#;
        let config: AppConfig = toml::from_str(text).expect("parses");
        let hidden: Vec<String> = config
            .hidden_sensors
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(hidden, ["nct6795-t2", "nvme-2-t1"]);
        let text = toml::to_string_pretty(&config).expect("serialize");
        let back: AppConfig = toml::from_str(&text).expect("deserialize");
        assert_eq!(config, back);
        let bare: AppConfig = toml::from_str("").expect("parses");
        assert!(
            bare.hidden_sensors.is_empty(),
            "a config without hidden_sensors hides nothing"
        );
    }

    #[test]
    fn fan_groups_and_hidden_flags_round_trip() {
        let config = AppConfig {
            fans: vec![
                FanConfig {
                    id: FanId::new("nct6795-p1"),
                    name: Some("AIO radiator".to_string()),
                    rpm_factor: 1.0,
                    min_speed: 0.4,
                    members: vec![FanId::new("nct6795-p2"), FanId::new("nct6795-p3")],
                    hidden: false,
                    mode: crate::fans::FanMode::Fixed { speed: 0.5 },
                    slot: None,
                },
                FanConfig {
                    id: FanId::new("it87-p4"),
                    name: None,
                    rpm_factor: 0.5,
                    min_speed: 0.0,
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
            hidden_sensors: Vec::new(),
            fans: vec![
                FanConfig {
                    id: FanId::new("it87-p2"),
                    name: None,
                    rpm_factor: 1.0,
                    min_speed: 0.0,
                    members: Vec::new(),
                    hidden: false,
                    mode: crate::fans::FanMode::Fixed { speed: 0.35 },
                    slot: None,
                },
                FanConfig {
                    id: FanId::new("nct6795-p1"),
                    name: Some("Top exhaust".to_string()),
                    rpm_factor: 1.0,
                    min_speed: 0.0,
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
                        rect: Some((240.0, 90.0)),
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
    fn slot_shapes_parse_in_both_forms_and_default_the_diameter() {
        let text = r#"
            [[fans]]
            id = "nct6795-p1"
            mode = { type = "off" }
            [fans.slot]
            x = 40.0
            y = 380.0
            diameter = 70.0
            airflow = "intake"

            [[fans]]
            id = "nvidia-gpu0-cooler"
            mode = { type = "off" }
            [fans.slot]
            x = 150.0
            y = 120.0
            rect = [220.0, 80.0]
            airflow = "internal"
        "#;
        let config: AppConfig = toml::from_str(text).expect("both slot forms parse");
        let circle = config.fans[0].slot.expect("circle slot");
        assert_eq!(circle.diameter, 70.0);
        assert_eq!(circle.rect, None);
        assert!(!circle.is_rectangular());
        let card = config.fans[1].slot.expect("rect slot");
        assert_eq!(card.rect, Some((220.0, 80.0)));
        // Omitted `diameter` falls back to the standard circle size.
        assert_eq!(card.diameter, 60.0);
        assert!(card.is_rectangular());
        assert_eq!(card.half_extents(), (110.0, 40.0));
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
