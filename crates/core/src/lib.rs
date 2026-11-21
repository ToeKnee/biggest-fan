//! Core of Biggest Fan: hardware discovery, fan control and the control loop.
//!
//! The engine discovers temperature sensors (Linux hwmon — the interface used
//! by `lm-sensors` — and NVIDIA GPUs via NVML) and controllable fan headers,
//! then repeatedly maps sensor temperatures to fan speeds using fan curves
//! with hysteresis so fans do not pump up and down.
//!
//! ```no_run
//! use std::path::Path;
//!
//! let discovery = biggest_fan_core::discover(Path::new("/sys/class/hwmon"), true)
//!     .expect("hwmon discovery should work on Linux");
//! for sensor in &discovery.sensors {
//!     println!("{}: {}", sensor.id, sensor.default_name);
//! }
//! for fan in &discovery.fans {
//!     println!("{}: {}", fan.id, fan.default_name);
//! }
//! ```

#![warn(missing_docs)]
#![warn(clippy::all)]
#![warn(clippy::pedantic)]
#![warn(clippy::complexity)]
#![warn(clippy::nursery)]
#![warn(clippy::perf)]
#![warn(clippy::style)]

pub mod case;
pub mod config;
pub mod engine;
pub mod error;
pub mod fans;
pub mod nudge;
pub mod sensors;

pub use case::{Airflow, CaseConfig, FanSlot};
pub use config::AppConfig;
pub use engine::{CaseFanView, Discovery, Engine, EngineOptions, TickReport};
pub use engine::{default_config, discover};
pub use error::CoreError;
pub use fans::{
    CurvePoint, CurvePreset, FanCurve, FanDescriptor, FanDriver, FanId, FanMode, FanStatus,
    NvidiaFan, SysfsFan,
};
pub use sensors::{
    HwmonSource, NvidiaSource, SensorCategory, SensorDescriptor, SensorId, SensorSource,
    SensorSourceKind, SensorStatus, default_sensor_name, friendly_chip_name, sensor_category,
};
