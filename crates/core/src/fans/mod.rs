//! Fan headers, fan modes and fan curves.

pub mod model;
pub mod sysfs;

pub use model::{CurvePoint, CurvePreset, FanCurve, FanDescriptor, FanId, FanMode, FanStatus};
pub use sysfs::SysfsFan;
