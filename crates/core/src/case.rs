//! Graphical PC-case model: where each fan sits on the case drawing.

use serde::{Deserialize, Serialize};

/// Direction a fan moves air, used to pick its colour in the case view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Airflow {
    /// Draws cool air into the case.
    Intake,
    /// Pushes hot air out of the case.
    Exhaust,
    /// Moves air inside the case only (e.g. a CPU cooler fan).
    #[default]
    Internal,
}

/// A fan's position and size on the case canvas (logical pixels).
///
/// A plain circle (just `diameter`) draws a fan header; a `rect` (width
/// and height) draws a whole rectangular object instead — a graphics
/// card cooler, for example. Both forms round-trip through TOML:
///
/// ```toml
/// slot = { x = 40.0, y = 380.0, diameter = 70.0, airflow = "intake" }
/// slot = { x = 150.0, y = 120.0, diameter = 60.0, rect = [220.0, 80.0], airflow = "internal" }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FanSlot {
    /// Centre x on the canvas.
    pub x: f64,
    /// Centre y on the canvas.
    pub y: f64,
    /// Circle diameter in canvas units; ignored when `rect` is set.
    #[serde(default = "default_diameter")]
    pub diameter: f64,
    /// Width and height for rectangular slots (graphics-card coolers).
    /// Absent means the slot is drawn as a `diameter` circle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rect: Option<(f64, f64)>,
    /// Airflow direction.
    #[serde(default)]
    pub airflow: Airflow,
}

const fn default_diameter() -> f64 {
    60.0
}

impl FanSlot {
    /// Half-width and half-height of the drawn shape, for both circles
    /// and rectangles — hit-testing and canvas clamping use these.
    #[must_use]
    pub fn half_extents(&self) -> (f64, f64) {
        match self.rect {
            Some((width, height)) => (width / 2.0, height / 2.0),
            None => (self.diameter / 2.0, self.diameter / 2.0),
        }
    }

    /// Whether this slot draws a whole rectangle instead of a circle.
    #[must_use]
    pub const fn is_rectangular(&self) -> bool {
        self.rect.is_some()
    }
}

/// Size of the case canvas.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaseConfig {
    /// Canvas width.
    pub width: f64,
    /// Canvas height.
    pub height: f64,
}

impl Default for CaseConfig {
    fn default() -> Self {
        Self {
            width: 300.0,
            height: 460.0,
        }
    }
}
