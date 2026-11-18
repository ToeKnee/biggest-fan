//! Temperature sensor sources and models.

pub mod hwmon;
pub mod model;
pub mod naming;
pub mod nvidia;

pub use hwmon::HwmonSource;
pub use model::{SensorDescriptor, SensorId, SensorSource, SensorSourceKind, SensorStatus};
pub use naming::{SensorCategory, default_sensor_name, friendly_chip_name, sensor_category};
pub use nvidia::NvidiaSource;
