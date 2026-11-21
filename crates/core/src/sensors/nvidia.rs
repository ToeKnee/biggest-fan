//! NVIDIA GPU temperatures — and thermal-throttle alerts — through NVML.
//!
//! NVML is loaded lazily, so on machines without the NVIDIA driver this
//! source reports itself as unavailable instead of failing.

use crate::error::CoreError;
use crate::sensors::model::{SensorDescriptor, SensorId, SensorSource, SensorSourceKind};
use nvml_wrapper::Nvml;
use nvml_wrapper::bitmasks::device::ThrottleReasons;
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
    /// Per-GPU thermal throttle flag from the last `read_all` run.
    throttled: BTreeMap<u32, bool>,
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
            throttled: BTreeMap::new(),
        }))
    }

    /// Whether the given GPU's last throttle report indicated a
    /// temperature above the card's thermal limit. Unknown GPUs count as
    /// not throttling.
    #[must_use]
    pub fn is_gpu_throttled(&self, gpu_index: u32) -> bool {
        self.throttled.get(&gpu_index).copied().unwrap_or(false)
    }

    /// Whether any GPU's last throttle report indicated a temperature
    /// above that card's thermal limit.
    #[must_use]
    pub fn any_throttled(&self) -> bool {
        self.throttled.values().copied().any(|throttled| throttled)
    }
}

impl std::fmt::Debug for NvidiaSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NvidiaSource")
            .field("cache", &self.cache)
            .field("throttled", &self.throttled)
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
        self.throttled.clear();
        for gpu in &self.cache {
            let Ok(device) = self.nvml.device_by_index(gpu.nvml_index) else {
                continue;
            };
            if let Ok(temp_c) =
                device.temperature(nvml_wrapper::enum_wrappers::device::TemperatureSensor::Gpu)
            {
                readings.insert(gpu.id.clone(), f64::from(temp_c));
            }
            // Throttle reasons are reported per GPU next to the
            // temperature; drivers without the call report an error and
            // the GPU is treated as "not throttling".
            if let Ok(reasons) = device.current_throttle_reasons() {
                self.throttled
                    .insert(gpu.nvml_index, gpu_thermal_throttling(reasons));
            }
        }
        Ok(readings)
    }
}

/// Whether NVML's throttle reasons point to the GPU running hot: the
/// software or hardware thermal slowdown flag. Power caps, idle and the
/// power-brake flag are deliberately not treated as thermal problems.
fn gpu_thermal_throttling(reasons: ThrottleReasons) -> bool {
    reasons.intersects(ThrottleReasons::HW_THERMAL_SLOWDOWN | ThrottleReasons::SW_THERMAL_SLOWDOWN)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_thermal_flags_count_as_thermal_throttling() {
        assert!(gpu_thermal_throttling(ThrottleReasons::HW_THERMAL_SLOWDOWN));
        assert!(gpu_thermal_throttling(ThrottleReasons::SW_THERMAL_SLOWDOWN));
        assert!(gpu_thermal_throttling(
            ThrottleReasons::GPU_IDLE | ThrottleReasons::HW_THERMAL_SLOWDOWN
        ));
        // Power and idle limits are normal operation, not a hot GPU.
        assert!(!gpu_thermal_throttling(ThrottleReasons::SW_POWER_CAP));
        assert!(!gpu_thermal_throttling(
            ThrottleReasons::HW_SLOWDOWN | ThrottleReasons::HW_POWER_BRAKE_SLOWDOWN
        ));
        assert!(!gpu_thermal_throttling(ThrottleReasons::NONE));
    }
}
