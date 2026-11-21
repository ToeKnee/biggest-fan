//! The thermal guard every stressor shares: what to watch, and what it
//! means for the run when something runs hot.

use nvml_wrapper::Nvml;
use nvml_wrapper::enum_wrappers::device::TemperatureSensor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How a lab command finished.
#[derive(Debug)]
pub enum LabError {
    /// The thermal guard tripped: stress stopped early on purpose.
    Thermal(String),
    /// Anything else that kept the command from doing its job.
    Failed(String),
}

/// What a finished stress run did. [`StressReport`] comes back from the
/// library entry points (`cpu::stress`, `gpu::stress`); the CLI prints it
/// and the GUI's Stress lab panel shows it in the status line.
#[derive(Debug)]
pub struct StressReport {
    /// Seconds the load actually ran — less than asked when it was
    /// stopped early or the thermal guard tripped.
    pub elapsed_secs: f64,
    /// One human-readable line: throughput, checksum, state hash, etc.
    pub summary: String,
    /// The thermal guard aborted the run.
    pub tripped: bool,
}

/// k10temp sysfs files, found by chip `name` so hwmon numbering need not
/// be guessed. Prefers the `Tctl` label (package-wide maximum), falling
/// back to `temp1_input`.
#[must_use]
pub fn k10temp_path(hwmon_root: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(hwmon_root).ok()?;
    for entry in entries.flatten() {
        let chip = entry.path();
        if read_sysfs_string(&chip.join("name")).as_deref() != Some("k10temp") {
            continue;
        }
        for index in 1..=8 {
            let label = read_sysfs_string(&chip.join(format!("temp{index}_label")));
            if label.as_deref() == Some("Tctl") {
                return Some(chip.join(format!("temp{index}_input")));
            }
        }
        return Some(chip.join("temp1_input"));
    }
    None
}

/// Reads a millidegree sysfs input (e.g. `55000`) as °C.
#[must_use]
pub fn read_millidegrees(path: &Path) -> Option<f64> {
    let text = read_sysfs_string(path)?;
    text.parse::<i64>().ok().map(|milli| {
        // Temperatures stay far below 2^53, so the rounding here is
        // irrelevant; `f64::from` does not accept i64.
        #[expect(
            clippy::cast_precision_loss,
            reason = "millidegree readings never approach 2^53"
        )]
        let degrees = milli as f64;
        degrees / 1000.0
    })
}

/// `fs::read_to_string` works on sysfs pseudo-files (it reads without
/// trusting the 0-byte file size); trailing newline trimmed.
fn read_sysfs_string(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_string())
}

/// Milliseconds since the Unix epoch, for CSV timestamps.
#[must_use]
pub fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// The GPU side of telemetry: NVML opened once, or `None` when the driver
/// does not answer (a sandbox, or no NVIDIA card).
pub struct GpuMonitor {
    device: nvml_wrapper::Device<'static>,
}

impl GpuMonitor {
    /// Best-effort: `None` rather than an error when NVML is unavailable.
    #[must_use]
    pub fn try_open() -> Option<Self> {
        // Leak the Nvml handle so the Device can borrow it for `'static`.
        // One deliberate leak in a short-lived lab process beats lifetime
        // gymnastics; NVML teardown is unnecessary at exit. The handle is
        // leaked *before* `device_by_index`, or the device would borrow a
        // local that dies at the end of this function.
        let nvml: &'static Nvml = Box::leak(Box::new(Nvml::init().ok()?));
        let device = nvml.device_by_index(0).ok()?;
        Some(Self { device })
    }

    /// The GPU core temperature, when NVML answers.
    #[must_use]
    pub fn temperature_c(&self) -> Option<f64> {
        self.device
            .temperature(TemperatureSensor::Gpu)
            .ok()
            .map(f64::from)
    }

    /// Fan percentage (0–100) of fan 0; `None` when the card reports no
    /// controllable fan.
    #[must_use]
    pub fn fan_percent(&self) -> Option<f64> {
        self.device.fan_speed(0).ok().map(f64::from)
    }

    /// Graphics-engine utilisation percent.
    #[must_use]
    pub fn gpu_util_percent(&self) -> Option<f64> {
        self.device
            .utilization_rates()
            .ok()
            .map(|rates| f64::from(rates.gpu))
    }

    /// Board power draw in watts.
    #[must_use]
    pub fn power_watts(&self) -> Option<f64> {
        self.device
            .power_usage()
            .ok()
            .map(|milliwatts| f64::from(milliwatts) / 1000.0)
    }

    /// Whether the driver currently flags a *thermal* slowdown.
    #[must_use]
    pub fn thermal_throttling(&self) -> bool {
        use nvml_wrapper::bitmasks::device::ThrottleReasons;
        self.device.current_throttle_reasons().is_ok_and(|reasons| {
            reasons.intersects(
                ThrottleReasons::HW_THERMAL_SLOWDOWN | ThrottleReasons::SW_THERMAL_SLOWDOWN,
            )
        })
    }
}

/// Spawns the guard thread: it watches k10temp (when `cpu_path` is known)
/// and its own freshly-opened NVML handle once a second. At `max_temp_c`
/// the shared `stop` flag goes up and a message names the offender; each
/// stressor's loop polls `stop` and unwinds.
///
/// The guard opens NVML itself because the caller's handle cannot cross
/// into the thread; both are best-effort, so in a sandbox (no sysfs k10temp
/// chip, no NVML) the guard simply never fires.
///
/// `max_temp_c <= 0.0` disables the guard: no thread, and the shared flag
/// is one nothing ever sets.
#[must_use]
pub fn spawn_guard(
    cpu_path: Option<PathBuf>,
    max_temp_c: f64,
    stop: Arc<AtomicBool>,
    tripped: Arc<AtomicBool>,
) -> Option<std::thread::JoinHandle<()>> {
    if max_temp_c <= 0.0 {
        return None;
    }
    Some(
        std::thread::Builder::new()
            .name("thermal-guard".to_string())
            .spawn(move || {
                let guard_gpu = GpuMonitor::try_open();
                while !stop.load(Ordering::Relaxed) {
                    let mut hot: Vec<String> = Vec::new();
                    if let Some(path) = &cpu_path
                        && let Some(temp) = read_millidegrees(path)
                        && temp >= max_temp_c
                    {
                        hot.push(format!("k10temp {temp:.0} °C"));
                    }
                    if let Some(temp) = guard_gpu.as_ref().and_then(GpuMonitor::temperature_c)
                        && temp >= max_temp_c
                    {
                        hot.push(format!("GPU {temp:.0} °C"));
                    }
                    if !hot.is_empty() {
                        eprintln!(
                            "thermal guard: {} reached {max_temp_c:.0} °C — stopping the stress",
                            hot.join(", ")
                        );
                        tripped.store(true, Ordering::Relaxed);
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
            })
            .expect("spawn the thermal guard thread"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "stress-lab-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn reads_millidegrees_and_tolerates_junk() {
        let dir = temp_dir("milli");
        let file = dir.join("temp1_input");
        std::fs::write(&file, "55000\n").expect("write");
        assert_eq!(read_millidegrees(&file), Some(55.0));
        std::fs::write(&file, "not a number\n").expect("write junk");
        assert_eq!(read_millidegrees(&file), None);
        assert_eq!(read_millidegrees(&dir.join("missing")), None);
        std::fs::remove_dir_all(&dir).expect("clean up");
    }

    #[test]
    fn finds_the_k10temp_chip_and_prefers_tctl() {
        let dir = temp_dir("k10");
        let chip = dir.join("hwmon7");
        std::fs::create_dir_all(&chip).expect("chip dir");
        std::fs::write(chip.join("name"), "k10temp\n").expect("name");
        std::fs::write(chip.join("temp1_input"), "40000\n").expect("t1");
        std::fs::write(chip.join("temp1_label"), "Tdie\n").expect("label");
        std::fs::write(chip.join("temp2_input"), "52000\n").expect("t2");
        std::fs::write(chip.join("temp2_label"), "Tctl\n").expect("label");
        let found = k10temp_path(&dir).expect("k10temp found");
        assert_eq!(found, chip.join("temp2_input"), "Tctl wins");
        std::fs::remove_dir_all(&dir).expect("clean up");
    }

    #[test]
    fn k10temp_lookup_is_silent_when_the_chip_is_absent() {
        let dir = temp_dir("nok10");
        let chip = dir.join("hwmon0");
        std::fs::create_dir_all(&chip).expect("chip dir");
        std::fs::write(chip.join("name"), "nvme\n").expect("name");
        assert_eq!(k10temp_path(&dir), None);
        std::fs::remove_dir_all(&dir).expect("clean up");
    }
}
