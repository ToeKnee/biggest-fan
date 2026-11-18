//! NVIDIA GPU temperatures through NVML.
//!
//! NVML is loaded lazily, so on machines without the NVIDIA driver this
//! source reports itself as unavailable instead of failing.

use crate::error::CoreError;
use crate::sensors::model::{SensorDescriptor, SensorId, SensorSource, SensorSourceKind};
use nvml_wrapper::Nvml;
use std::collections::BTreeMap;

/// A GPU temperature input remembered from the last discovery run.
#[derive(Clone, Debug)]
struct CachedGpu {
    id: SensorId,
    nvml_index: u32,
}

/// Reads GPU core temperatures through the NVML library.
pub struct NvidiaSource {
    nvml: Nvml,
    cache: Vec<CachedGpu>,
}

impl NvidiaSource {
    /// Initialises NVML, returning `Ok(None)` when the NVIDIA driver is
    /// absent or NVML cannot start. That is normal on non-NVIDIA machines;
    /// any init failure is treated as "no NVIDIA GPU management here".
    ///
    /// # Errors
    ///
    /// Currently never: init failures are treated as "unavailable". The
    /// `Result` keeps this future-proof as NVML errors surface.
    pub fn try_new() -> Result<Option<Self>, CoreError> {
        Ok(Nvml::init().ok().map(|nvml| Self {
            nvml,
            cache: Vec::new(),
        }))
    }
}

impl std::fmt::Debug for NvidiaSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NvidiaSource")
            .field("cache", &self.cache)
            .finish_non_exhaustive()
    }
}

impl SensorSource for NvidiaSource {
    fn kind(&self) -> SensorSourceKind {
        SensorSourceKind::Nvidia
    }

    fn discover(&mut self) -> Result<Vec<SensorDescriptor>, CoreError> {
        self.cache.clear();
        let mut descriptors = Vec::new();
        let count = self.nvml.device_count()?;
        for nvml_index in 0..count {
            let device = self.nvml.device_by_index(nvml_index)?;
            let model = device
                .name()
                .unwrap_or_else(|_| format!("GPU {nvml_index}"));
            let id = SensorId::new(format!("nvidia-gpu{nvml_index}-t1"));
            self.cache.push(CachedGpu {
                id: id.clone(),
                nvml_index,
            });
            descriptors.push(SensorDescriptor {
                id,
                kind: SensorSourceKind::Nvidia,
                raw_label: format!("NVML GPU {nvml_index}"),
                default_name: format!("{model} temperature"),
            });
        }
        Ok(descriptors)
    }

    fn read_all(&mut self) -> Result<BTreeMap<SensorId, f64>, CoreError> {
        let mut readings = BTreeMap::new();
        for gpu in &self.cache {
            if let Ok(device) = self.nvml.device_by_index(gpu.nvml_index)
                && let Ok(temp_c) =
                    device.temperature(nvml_wrapper::enum_wrappers::device::TemperatureSensor::Gpu)
            {
                readings.insert(gpu.id.clone(), f64::from(temp_c));
            }
        }
        Ok(readings)
    }
}
