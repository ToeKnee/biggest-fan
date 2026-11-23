//! Plain data types shared by sensor sources.

use crate::error::CoreError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// Stable identifier of a temperature sensor, e.g. `k10temp-t1`.
///
/// For hwmon sensors the id is the chip name, an optional `-K` counter when
/// several chips share one name (two `NVMe` drives become `nvme-1-t1` and
/// `nvme-2-t1`), and the index of the `tempN_input` file. NVML GPU sensors
/// use `nvidia-gpuK-t1`. Ids are what configuration files reference.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SensorId(String);

impl SensorId {
    /// Creates an id from its raw string form.
    #[must_use]
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// The raw id string, e.g. `k10temp-t3`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SensorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<SensorId> for String {
    fn from(id: SensorId) -> Self {
        id.0
    }
}

/// Which interface a sensor came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SensorSourceKind {
    /// Linux hwmon sysfs (used by `lm-sensors`).
    Hwmon,
    /// NVIDIA GPU temperature read through NVML.
    Nvidia,
}

/// A sensor found during discovery, before any user-renaming.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SensorDescriptor {
    /// Stable id usable from configuration.
    pub id: SensorId,
    /// Which interface found it.
    pub kind: SensorSourceKind,
    /// What the hardware calls it, e.g. `k10temp Tccd1`.
    pub raw_label: String,
    /// Best guess at a human readable name, e.g. `CPU CCD 1`.
    /// Configuration can override this via the `[sensor_names]` table.
    pub default_name: String,
}

/// A live sensor reading plus its resolved display name.
#[derive(Clone, Debug, PartialEq)]
pub struct SensorStatus {
    /// Stable id.
    pub id: SensorId,
    /// Resolved display name (config override or default name).
    pub name: String,
    /// Which interface the reading came from.
    pub kind: SensorSourceKind,
    /// Temperature in degrees Celsius, or `None` if the read failed this tick.
    pub temp_c: Option<f64>,
}

/// Something that can list and read temperature sensors.
pub trait SensorSource: std::fmt::Debug {
    /// Which interface this source uses.
    fn kind(&self) -> SensorSourceKind;

    /// List every sensor this source can see, caching handles for
    /// [`SensorSource::read_all`].
    ///
    /// # Errors
    ///
    /// [`CoreError::Io`] when the underlying interface is unreachable.
    fn discover(&mut self) -> Result<Vec<SensorDescriptor>, CoreError>;

    /// Read every previously discovered sensor. Sensors that fail to read
    /// are simply absent from the returned map; the control loop treats a
    /// missing reading as a failed read and applies fail-safes.
    ///
    /// # Errors
    ///
    /// The whole interface failed; partial per-sensor failures never
    /// produce an error.
    fn read_all(&mut self) -> Result<BTreeMap<SensorId, f64>, CoreError>;
}
