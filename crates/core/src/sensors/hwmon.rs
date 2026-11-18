//! Linux hwmon sysfs sensor source — the same interface `lm-sensors` reads.
//!
//! Temperature inputs live at `<hwmon root>/hwmonN/tempM_input` and hold
//! millidegrees Celsius, e.g. `45000` for 45 °C. The `name` file of each
//! hwmon directory identifies the chip driver (`k10temp`, `nct6795`, ...).

use crate::error::CoreError;
use crate::sensors::model::{SensorDescriptor, SensorId, SensorSource, SensorSourceKind};
use crate::sensors::naming::default_sensor_name;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One hwmon chip directory plus its reported chip name.
#[derive(Clone, Debug)]
pub(crate) struct HwmonChip {
    /// Chip driver name from the `name` file, e.g. `k10temp`.
    pub(crate) name: String,
    /// The hwmon directory itself, e.g. `/sys/class/hwmon/hwmon3`.
    pub(crate) dir: PathBuf,
    /// Index of this chip among chips with the same name (0-based).
    pub(crate) same_name_index: usize,
    /// How many chips share this name.
    pub(crate) same_name_count: usize,
}

/// Lists all hwmon chips under `root`, sorted by directory name.
///
/// Chips sharing one `name` (for example two `NVMe` drive controllers) are
/// numbered so ids stay unique and stable across reboots.
pub(crate) fn hwmon_chips(root: &Path) -> Result<Vec<HwmonChip>, CoreError> {
    let mut entries = list_dir_paths(root)?;
    entries.sort();

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for dir in &entries {
        *counts.entry(chip_name(dir)).or_default() += 1;
    }
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut chips = Vec::new();
    for dir in entries {
        let name = chip_name(&dir);
        let total = counts.get(&name).copied().unwrap_or(1);
        let index = seen.entry(name.clone()).or_default();
        let same_name_index = *index;
        *index += 1;
        chips.push(HwmonChip {
            name,
            dir,
            same_name_index,
            same_name_count: total,
        });
    }
    Ok(chips)
}

/// Names every file in a directory, sorted.
pub(crate) fn list_dir_names(dir: &Path) -> Result<Vec<String>, CoreError> {
    let mut names = Vec::new();
    for path in list_dir_paths(dir)? {
        names.push(file_name(&path));
    }
    names.sort();
    Ok(names)
}

fn list_dir_paths(dir: &Path) -> Result<Vec<PathBuf>, CoreError> {
    let read = std::fs::read_dir(dir).map_err(|source| CoreError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    let mut paths = Vec::new();
    for entry in read {
        let entry = entry.map_err(|source| CoreError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        paths.push(entry.path());
    }
    Ok(paths)
}

/// Reads a small sysfs file and trims it; `None` if missing or unreadable.
pub(crate) fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|content| content.trim().to_string())
        .filter(|content| !content.is_empty())
}

/// Reads an integer sysfs value, e.g. millidegrees or RPM.
pub(crate) fn read_int(path: &Path) -> Result<i64, CoreError> {
    let content = std::fs::read_to_string(path).map_err(|source| CoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    content.trim().parse::<i64>().map_err(|_| CoreError::Parse {
        path: path.to_path_buf(),
    })
}

fn chip_name(dir: &Path) -> String {
    read_trimmed(&dir.join("name")).unwrap_or_else(|| file_name(dir))
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
}

/// Extracts `M` from a hwmon file named `tempM_input`.
fn temp_input_index(name: &str) -> Option<usize> {
    let middle = name.strip_prefix("temp")?.strip_suffix("_input")?;
    middle.parse::<usize>().ok()
}

/// A temperature input remembered from the last discovery run.
#[derive(Clone, Debug)]
struct CachedInput {
    id: SensorId,
    input_path: PathBuf,
}

/// Reads temperature sensors from the Linux hwmon sysfs class.
#[derive(Debug)]
pub struct HwmonSource {
    root: PathBuf,
    cache: Vec<CachedInput>,
}

impl HwmonSource {
    /// Source pointing at the default hwmon class directory.
    #[must_use]
    pub fn new() -> Self {
        Self::with_root("/sys/class/hwmon")
    }

    /// Source rooted somewhere else, which lets tests fake a hwmon tree.
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            cache: Vec::new(),
        }
    }

    /// Builds a unique, stable id for one temp input of one chip.
    fn input_id(chip: &HwmonChip, index: usize) -> SensorId {
        if chip.same_name_count > 1 {
            SensorId::new(format!(
                "{}-{}-t{}",
                chip.name,
                chip.same_name_index + 1,
                index
            ))
        } else {
            SensorId::new(format!("{}-t{}", chip.name, index))
        }
    }
}

impl Default for HwmonSource {
    fn default() -> Self {
        Self::new()
    }
}

impl SensorSource for HwmonSource {
    fn kind(&self) -> SensorSourceKind {
        SensorSourceKind::Hwmon
    }

    fn discover(&mut self) -> Result<Vec<SensorDescriptor>, CoreError> {
        self.cache.clear();
        let mut descriptors = Vec::new();
        for chip in hwmon_chips(&self.root)? {
            for name in list_dir_names(&chip.dir)? {
                let Some(index) = temp_input_index(&name) else {
                    continue;
                };
                let input_path = chip.dir.join(&name);
                // Inputs whose value cannot be read are not real sensors;
                // skip them so dead placeholders never reach the UI.
                if read_int(&input_path).is_err() {
                    continue;
                }
                let label =
                    read_trimmed(&chip.dir.join(format!("temp{index}_label"))).unwrap_or_default();
                let default_name = default_sensor_name(&chip.name, index, &label);
                let raw_label = if label.is_empty() {
                    chip.name.clone()
                } else {
                    format!("{} {}", chip.name, label)
                };
                let id = Self::input_id(&chip, index);
                self.cache.push(CachedInput {
                    id: id.clone(),
                    input_path,
                });
                descriptors.push(SensorDescriptor {
                    id,
                    kind: SensorSourceKind::Hwmon,
                    raw_label,
                    default_name,
                });
            }
        }
        Ok(descriptors)
    }

    fn read_all(&mut self) -> Result<BTreeMap<SensorId, f64>, CoreError> {
        let mut readings = BTreeMap::new();
        for input in &self.cache {
            // Individual read failures are expected occasionally (a driver
            // may briefly not answer); those sensors are simply absent from
            // the map and the control loop treats them as failed reads.
            if let Ok(millidegrees) = read_int(&input.input_path) {
                #[expect(clippy::cast_precision_loss, reason = "millidegree values are tiny")]
                let temp_c = millidegrees as f64 / 1000.0;
                readings.insert(input.id.clone(), temp_c);
            }
        }
        Ok(readings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(raw: &str) -> SensorId {
        SensorId::new(raw)
    }

    fn chip_dir(root: &Path, hwmon: &str, name: &str) -> PathBuf {
        let dir = root.join(hwmon);
        std::fs::create_dir_all(&dir).expect("fixture dir");
        std::fs::write(dir.join("name"), format!("{name}\n")).expect("chip name");
        dir
    }

    #[test]
    fn discovers_sensors_with_human_names() {
        let root = tempfile::tempdir().expect("tempdir");
        let root = root.path();
        let k10 = chip_dir(root, "hwmon0", "k10temp");
        std::fs::write(k10.join("temp1_input"), "45000\n").expect("temp1");
        std::fs::write(k10.join("temp1_label"), "Tctl\n").expect("label1");
        std::fs::write(k10.join("temp2_input"), "47000\n").expect("temp2");
        std::fs::write(k10.join("temp2_label"), "Tccd1\n").expect("label2");
        // Broken inputs must be skipped.
        std::fs::write(k10.join("temp3_input"), "not a number\n").expect("temp3");

        let mut source = HwmonSource::with_root(root);
        let found = source.discover().expect("discovery");
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].id.as_str(), "k10temp-t1");
        assert_eq!(found[0].default_name, "CPU temperature");
        assert_eq!(found[0].raw_label, "k10temp Tctl");
        assert_eq!(found[1].default_name, "CPU CCD 1 temperature");

        let readings = source.read_all().expect("read_all");
        assert_eq!(readings[&sid("k10temp-t1")], 45.0);
        assert_eq!(readings[&sid("k10temp-t2")], 47.0);
    }

    #[test]
    fn negative_temperatures_read_correctly() {
        let root = tempfile::tempdir().expect("tempdir");
        let chip = chip_dir(root.path(), "hwmon0", "k10temp");
        std::fs::write(chip.join("temp1_input"), "-5250\n").expect("temp1");

        let mut source = HwmonSource::with_root(root.path());
        source.discover().expect("discovery");
        let readings = source.read_all().expect("read_all");
        assert_eq!(readings[&sid("k10temp-t1")], -5.25);
    }

    #[test]
    fn duplicate_chip_names_get_unique_ids() {
        let root = tempfile::tempdir().expect("tempdir");
        let first = chip_dir(root.path(), "hwmon0", "nvme");
        std::fs::write(first.join("temp1_input"), "40000\n").expect("temp1");
        let second = chip_dir(root.path(), "hwmon1", "nvme");
        std::fs::write(second.join("temp1_input"), "44000\n").expect("temp1");

        let mut source = HwmonSource::with_root(root.path());
        let found = source.discover().expect("discovery");
        let ids: Vec<&str> = found.iter().map(|sensor| sensor.id.as_str()).collect();
        assert_eq!(ids, ["nvme-1-t1", "nvme-2-t1"]);
    }

    #[test]
    fn missing_root_reports_io_error() {
        let mut source = HwmonSource::with_root("/definitely/not/a/hwmon/root");
        let error = source.discover().expect_err("should fail");
        assert!(matches!(error, CoreError::Io { .. }));
    }
}
