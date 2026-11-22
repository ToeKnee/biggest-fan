//! Plain data types for fans: ids, modes and piecewise-linear fan curves.

use crate::error::CoreError;
use crate::sensors::model::SensorId;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Stable identifier of a controllable fan header, e.g. `nct6795-p1`.
///
/// Follows the same scheme as sensor ids: chip name (plus a `-K` counter
/// when several chips share one name) and the index of the `pwmN` sysfs
/// file that controls the header.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FanId(String);

impl FanId {
    /// Creates an id from its raw string form.
    #[must_use]
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// The raw id string, e.g. `nct6795-p1`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<FanId> for String {
    fn from(id: FanId) -> Self {
        id.0
    }
}

/// One point of a fan curve: at `temp_c` the fan runs at `speed`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CurvePoint {
    /// Sensor temperature in degrees Celsius.
    pub temp_c: f64,
    /// Fan speed as a fraction of maximum (0.0 to 1.0).
    pub speed: f64,
}

/// Points used when an auto fan does not specify its own curve.
pub const DEFAULT_CURVE_POINTS: [CurvePoint; 4] = [
    CurvePoint {
        temp_c: 35.0,
        speed: 0.20,
    },
    CurvePoint {
        temp_c: 50.0,
        speed: 0.40,
    },
    CurvePoint {
        temp_c: 65.0,
        speed: 0.70,
    },
    CurvePoint {
        temp_c: 80.0,
        speed: 1.00,
    },
];

/// Starting points for an auto fan's curve, offered in the GUI.
///
/// Modern parts run comfortably well below their limit (Ryzen holds
/// 95 °C, Intel mobile parts 100 °C+, `GeForce` cards boost against a power
/// limit), so `Quiet` stops the fan altogether while the machine is idle and
/// only ramps hard near the temperature limit.
///
/// A fan that must never stop — an AIO pump — keeps running whatever the
/// preset says, by giving it a `min_speed` floor in the configuration
/// ([`crate::config::FanConfig::min_speed`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CurvePreset {
    /// Stopped while the machine is idle, barely audible through ordinary
    /// loads, and full speed only near the temperature limit.
    Quiet,
    /// The default curve: a steady climb from 35 °C to 80 °C.
    Balanced,
    /// Already cooling hard early; full speed by 80 °C.
    Performance,
}

impl CurvePreset {
    /// Name shown in the GUI.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Quiet => "Quiet",
            Self::Balanced => "Balanced",
            Self::Performance => "Performance",
        }
    }

    /// The curve this preset stands for.
    ///
    /// # Panics
    ///
    /// Never: every preset's points are statically valid, and tests
    /// verify each one builds and sorts.
    #[must_use]
    pub fn curve(self) -> FanCurve {
        match self {
            Self::Quiet => FanCurve::new(vec![
                CurvePoint {
                    temp_c: 25.0,
                    speed: 0.0,
                },
                CurvePoint {
                    temp_c: 40.0,
                    speed: 0.10,
                },
                CurvePoint {
                    temp_c: 55.0,
                    speed: 0.20,
                },
                CurvePoint {
                    temp_c: 70.0,
                    speed: 0.35,
                },
                CurvePoint {
                    temp_c: 85.0,
                    speed: 0.60,
                },
                CurvePoint {
                    temp_c: 95.0,
                    speed: 1.00,
                },
            ]),
            Self::Balanced => Ok(FanCurve::default()),
            Self::Performance => FanCurve::new(vec![
                CurvePoint {
                    temp_c: 30.0,
                    speed: 0.35,
                },
                CurvePoint {
                    temp_c: 55.0,
                    speed: 0.60,
                },
                CurvePoint {
                    temp_c: 70.0,
                    speed: 0.80,
                },
                CurvePoint {
                    temp_c: 80.0,
                    speed: 1.00,
                },
            ]),
        }
        .expect("preset points are valid by construction")
    }

    /// The preset whose points match `curve` exactly, if any.
    #[must_use]
    pub fn matching(curve: &FanCurve) -> Option<Self> {
        [Self::Quiet, Self::Balanced, Self::Performance]
            .into_iter()
            .find(|preset| &preset.curve() == curve)
    }
}

/// Piecewise-linear fan speed curve, sorted by temperature.
///
/// Speeds are interpolated between points and clamped outside the curve's
/// temperature range:
///
/// ```text
/// speed
/// 1.0 |              *------------   (>= 80 °C)
/// 0.7 |            *
/// 0.4 |        *
/// 0.2 |     *
///     |  --*                         (< 35 °C)
///     +----|----|----|----|---- temp
///       35   50   65   80   °C
/// ```
///
/// Serialized as a plain array of points (see [`CurvePoint`]) so config
/// files can write `curve = [{ temp_c = 40.0, speed = 0.2 }, ...]`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(try_from = "Vec<CurvePoint>")]
pub struct FanCurve {
    points: Vec<CurvePoint>,
}

impl Serialize for FanCurve {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.points.serialize(serializer)
    }
}

impl FanCurve {
    /// Builds a curve from points; they must be non-empty, strictly sorted
    /// by temperature, and speeds must lie within 0.0..=1.0.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::InvalidCurve`] when the points are not usable.
    pub fn new(points: Vec<CurvePoint>) -> Result<Self, CoreError> {
        if points.is_empty() {
            return Err(CoreError::InvalidCurve(
                "curve needs at least one point".into(),
            ));
        }
        for point in &points {
            if !point.speed.is_finite() || !(0.0..=1.0).contains(&point.speed) {
                return Err(CoreError::InvalidCurve(format!(
                    "speed {} is outside 0.0..=1.0",
                    point.speed
                )));
            }
            if !point.temp_c.is_finite() {
                return Err(CoreError::InvalidCurve(
                    "temperature is not a number".into(),
                ));
            }
        }
        for pair in points.windows(2) {
            if pair[0].temp_c >= pair[1].temp_c {
                return Err(CoreError::InvalidCurve(format!(
                    "temperatures must increase, but {} >= {}",
                    pair[0].temp_c, pair[1].temp_c
                )));
            }
        }
        Ok(Self { points })
    }

    /// The curve's points in temperature order.
    #[must_use]
    pub fn points(&self) -> &[CurvePoint] {
        &self.points
    }

    /// Interpolated speed fraction for a temperature.
    ///
    /// Speeds outside the point range are clamped to the first/last point.
    /// A non-finite temperature returns the maximum curve speed, so a broken
    /// sensor reading drives the fan to full speed rather than crashing or
    /// silently stopping.
    #[must_use]
    pub fn speed_at(&self, temp_c: f64) -> f64 {
        let first = self.points[0];
        let last = self.points[self.points.len() - 1];
        if !temp_c.is_finite() {
            return first.speed.max(last.speed);
        }
        if temp_c <= first.temp_c {
            return first.speed;
        }
        if temp_c >= last.temp_c {
            return last.speed;
        }
        for pair in self.points.windows(2) {
            let (low, high) = (pair[0], pair[1]);
            if temp_c <= high.temp_c {
                let span = high.temp_c - low.temp_c;
                let weight = (temp_c - low.temp_c) / span;
                return weight.mul_add(high.speed - low.speed, low.speed);
            }
        }
        last.speed
    }
}

impl Default for FanCurve {
    fn default() -> Self {
        Self {
            points: DEFAULT_CURVE_POINTS.to_vec(),
        }
    }
}

impl TryFrom<Vec<CurvePoint>> for FanCurve {
    type Error = CoreError;

    fn try_from(points: Vec<CurvePoint>) -> Result<Self, Self::Error> {
        Self::new(points)
    }
}

/// How a fan's speed is decided.
///
/// In TOML configuration this is an inline table tagged by `type`:
///
/// ```toml
/// mode = { type = "off" }
/// mode = { type = "fixed", speed = 0.4 }
/// mode = { type = "auto", sensor = "k10temp-t1", curve = [
///     { temp_c = 40.0, speed = 0.2 },
///     { temp_c = 80.0, speed = 1.0 },
/// ] }
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FanMode {
    /// Fan is stopped.
    Off,
    /// Fan runs at a constant fraction of maximum speed (0.0 to 1.0).
    Fixed {
        /// Constant speed fraction.
        speed: f64,
    },
    /// Speed follows the sensor through a fan curve, with hysteresis
    /// applied by the engine.
    Auto {
        /// Sensor whose temperature drives the fan.
        sensor: SensorId,
        /// Speed curve; defaults to [`DEFAULT_CURVE_POINTS`] when omitted.
        #[serde(default)]
        curve: FanCurve,
    },
}

/// A fan header found during discovery, before any user-renaming.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FanDescriptor {
    /// Stable id usable from configuration.
    pub id: FanId,
    /// Best guess at a human readable name, e.g. `Fan 1 on nct6795`.
    /// The fan's `name` in configuration takes precedence.
    pub default_name: String,
    /// Whether a tachometer (RPM) input is readable for this header.
    pub rpm_supported: bool,
}

/// A fan's live state after a control-loop tick.
#[derive(Clone, Debug, PartialEq)]
pub struct FanStatus {
    /// Stable id.
    pub id: FanId,
    /// Resolved display name.
    pub name: String,
    /// Mode the fan is being run in.
    pub mode: FanMode,
    /// Speed fraction actually commanded this tick (0.0 to 1.0), as
    /// the fan's hardware can run it: an NVIDIA cooler parks rather than
    /// crawl, and only runs at 30 % or above.
    pub commanded_speed: f64,
    /// Speed fraction the hardware reports for this fan's leader header:
    /// NVML's fan percent for a GPU cooler, the `pwmN` duty for a sysfs
    /// header. `None` when nothing can be read. A parked GPU cooler shows up
    /// here as 0 %, and on a driver with NVML's RPM call as 0 rpm too.
    pub measured_percent: Option<f64>,
    /// Measured speed in RPM, when the hardware reports one: a sysfs
    /// tachometer, or NVML's RPM call for a GPU cooler on a recent driver.
    pub measured_rpm: Option<f64>,
    /// Temperature of the governing sensor in Auto mode.
    pub governing_temp_c: Option<f64>,
    /// Warning for the user, e.g. a fail-safe note after a failed sensor read.
    pub note: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn points(pairs: &[(f64, f64)]) -> Vec<CurvePoint> {
        pairs
            .iter()
            .map(|&(temp_c, speed)| CurvePoint { temp_c, speed })
            .collect()
    }

    #[test]
    fn interpolates_between_points() {
        let curve = FanCurve::new(points(&[(40.0, 0.2), (80.0, 1.0)])).expect("valid");
        assert!((curve.speed_at(60.0) - 0.6).abs() < 1e-12);
        assert!((curve.speed_at(50.0) - 0.4).abs() < 1e-12);
    }

    #[test]
    fn clamps_outside_the_curve_range() {
        let curve = FanCurve::new(points(&[(40.0, 0.2), (80.0, 1.0)])).expect("valid");
        assert!((curve.speed_at(20.0) - 0.2).abs() < 1e-12);
        assert!((curve.speed_at(120.0) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn non_finite_temperature_drives_full_speed() {
        let curve = FanCurve::new(points(&[(40.0, 0.2), (80.0, 1.0)])).expect("valid");
        assert!((curve.speed_at(f64::NAN) - 1.0).abs() < 1e-12);
        assert!((curve.speed_at(f64::INFINITY) - 1.0).abs() < 1e-12);
        assert!((curve.speed_at(f64::NEG_INFINITY) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn rejects_invalid_curves() {
        assert!(FanCurve::new(Vec::new()).is_err());
        assert!(FanCurve::new(points(&[(50.0, 0.5), (40.0, 0.2)])).is_err());
        assert!(FanCurve::new(points(&[(40.0, 0.5), (40.0, 0.2)])).is_err());
        assert!(FanCurve::new(points(&[(40.0, 1.5)])).is_err());
        assert!(FanCurve::new(points(&[(40.0, -0.1)])).is_err());
    }

    #[test]
    fn default_curve_is_valid_and_sorted() {
        let curve = FanCurve::default();
        assert!(FanCurve::new(curve.points().to_vec()).is_ok());
    }

    #[test]
    fn every_curve_preset_is_valid_sorted_and_matchable() {
        for preset in [
            CurvePreset::Quiet,
            CurvePreset::Balanced,
            CurvePreset::Performance,
        ] {
            let curve = preset.curve();
            assert!(
                curve
                    .points()
                    .windows(2)
                    .all(|pair| pair[0].temp_c < pair[1].temp_c),
                "{} points must increase",
                preset.label()
            );
            assert_eq!(
                CurvePreset::matching(&curve),
                Some(preset),
                "{} must be recognisable",
                preset.label()
            );
        }
        assert_eq!(
            CurvePreset::matching(
                &FanCurve::new(points(&[(40.0, 0.2), (80.0, 1.0)])).expect("valid")
            ),
            None,
            "a hand-made curve matches no preset"
        );
        // Quiet stops while the machine is idle, stays calmer than Balanced
        // in normal load, and both go full only at the very top of the range.
        let (quiet, balanced) = (CurvePreset::Quiet.curve(), CurvePreset::Balanced.curve());
        assert_eq!(
            quiet.speed_at(25.0),
            0.0,
            "Quiet stops the fan while the machine is idle"
        );
        assert!(
            quiet.speed_at(60.0) < balanced.speed_at(60.0),
            "Quiet should be gentler at ordinary temperatures"
        );
    }

    #[test]
    fn toml_round_trip_keeps_modes_and_curves() {
        let modes = [
            FanMode::Off,
            FanMode::Fixed { speed: 0.4 },
            FanMode::Auto {
                sensor: SensorId::new("k10temp-t1"),
                curve: FanCurve::new(points(&[(40.0, 0.2), (80.0, 1.0)])).expect("valid"),
            },
        ];
        for mode in modes {
            let text = toml::to_string(&mode).expect("serialize");
            let back: FanMode = toml::from_str(&text).expect("deserialize");
            assert_eq!(mode, back);
        }
    }

    #[test]
    fn auto_mode_without_curve_uses_the_default() {
        let mode: FanMode =
            toml::from_str("type = \"auto\"\nsensor = \"k10temp-t1\"\n").expect("deserialize");
        let FanMode::Auto { curve, .. } = mode else {
            panic!("expected auto mode");
        };
        assert_eq!(curve, FanCurve::default());
    }
}
