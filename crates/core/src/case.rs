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
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FanSlot {
    /// Centre x on the canvas.
    pub x: f64,
    /// Centre y on the canvas.
    pub y: f64,
    /// Circle diameter in canvas units.
    pub diameter: f64,
    /// Airflow direction.
    #[serde(default)]
    pub airflow: Airflow,
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
