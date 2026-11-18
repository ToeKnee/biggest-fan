//! Fan headers, fan modes and fan curves.

pub mod driver;
pub mod model;
pub mod nvidia;
pub mod sysfs;

pub use driver::FanDriver;
pub use model::{CurvePoint, CurvePreset, FanCurve, FanDescriptor, FanId, FanMode, FanStatus};
pub use nvidia::NvidiaFan;
pub use sysfs::SysfsFan;
