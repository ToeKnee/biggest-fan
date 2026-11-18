//! Fan control through hwmon sysfs `pwm` files (fancontrol-style).
//!
//! A motherboard fan header is controlled by writing to `pwmN` (speed) and,
//! when the chip exposes one, `pwmN_enable` (0 = stopped, 1 = manual,
//! 2 = hardware-automatic). We always drive headers in manual mode so the
//! software curve, hysteresis and fail-safes decide the speed, and remember
//! each header's original enable mode so [`SysfsFan::restore_control`] can
//! hand it back (usually to hardware-automatic control) when Biggest Fan
//! exits. Manual-only controllers take `pwmN` writes directly and have no
//! enable file — for example the ARCTIC Fan Controller (hwmon `arctic_fan`,
//! a USB device that never changes speed on its own). Writing needs root or
//! matching udev permissions.

use crate::error::CoreError;
use crate::fans::model::{FanDescriptor, FanId};
use crate::sensors::hwmon::{hwmon_chips, list_dir_names, read_int, read_trimmed};
use crate::sensors::naming::friendly_chip_name;
use std::fs::File;
use std::io::ErrorKind;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Speed scale assumed when a chip does not expose `pwmN_max`.
const DEFAULT_PWM_MAX: i64 = 255;

/// A controllable motherboard fan header (hwmon `pwmN` plus optional tach).
#[derive(Clone, Debug)]
pub struct SysfsFan {
    id: FanId,
    default_name: String,
    chip: String,
    dir: PathBuf,
    index: usize,
    pwm_max: f64,
    supports_rpm: bool,
    /// `pwmN_enable` as found at discovery; Biggest Fan restores it on exit.
    original_enable: Option<u8>,
}

impl SysfsFan {
    /// Finds every fan header (a `pwmN` file) under an hwmon class root,
    /// ordered by controller name then header number.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Io`] when the hwmon root cannot be read.
    pub fn discover_all(root: &Path) -> Result<Vec<Self>, CoreError> {
        let mut fans = Vec::new();
        for chip in hwmon_chips(root)? {
            let display_chip = friendly_chip_name(&chip.name);
            for name in list_dir_names(&chip.dir)? {
                let Some(index) = pwm_file_index(&name) else {
                    continue;
                };
                let pwm_max =
                    read_int(&chip.dir.join(format!("pwm{index}_max"))).unwrap_or(DEFAULT_PWM_MAX);
                let supports_rpm = chip.dir.join(format!("fan{index}_input")).exists();
                let label = read_trimmed(&chip.dir.join(format!("pwm{index}_label")))
                    .or_else(|| read_trimmed(&chip.dir.join(format!("fan{index}_label"))));
                let default_name = label.map_or_else(
                    || format!("Fan {index} on {display_chip}"),
                    |label| format!("{label} on {display_chip}"),
                );
                let id = if chip.same_name_count > 1 {
                    FanId::new(format!(
                        "{}-{}-p{}",
                        chip.name,
                        chip.same_name_index + 1,
                        index
                    ))
                } else {
                    FanId::new(format!("{}-p{}", chip.name, index))
                };
                #[expect(clippy::cast_precision_loss, reason = "pwm ranges are tiny")]
                let pwm_max_f64 = pwm_max as f64;
                let original_enable = read_int(&chip.dir.join(format!("pwm{index}_enable")))
                    .ok()
                    .and_then(|value| u8::try_from(value).ok());
                fans.push(Self {
                    id,
                    default_name,
                    chip: chip.name.clone(),
                    dir: chip.dir.clone(),
                    index,
                    pwm_max: pwm_max_f64,
                    supports_rpm,
                    original_enable,
                });
            }
        }
        // Lexical file order puts `pwm10` before `pwm2`; present headers in
        // controller-name then header-number order instead.
        fans.sort_by(|a, b| a.chip.cmp(&b.chip).then(a.index.cmp(&b.index)));
        Ok(fans)
    }

    /// Stable id of this header, usable from configuration.
    #[must_use]
    pub const fn id(&self) -> &FanId {
        &self.id
    }

    /// Description used by discovery and the UI.
    #[must_use]
    pub fn descriptor(&self) -> FanDescriptor {
        FanDescriptor {
            id: self.id.clone(),
            default_name: self.default_name.clone(),
            rpm_supported: self.supports_rpm,
        }
    }

    /// Best-guess human readable name; configuration overrides it via the
    /// fan's `name` field.
    #[must_use]
    pub fn default_name(&self) -> &str {
        &self.default_name
    }

    /// Writes a speed fraction (0.0..=1.0) in manual mode.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Io`] if the pwm files cannot be written; a
    /// missing `pwmN_enable` file is tolerated (some chips have none).
    pub fn apply(&mut self, speed_fraction: f64) -> Result<(), CoreError> {
        let fraction = speed_fraction.clamp(0.0, 1.0);
        self.write_enable(1)?;
        let value = (fraction * self.pwm_max).round().clamp(0.0, self.pwm_max);
        write_sysfs(&self.pwm_path(), &format!("{value:.0}"))
    }

    /// Stops the fan: hardware-off if supported, otherwise manual zero.
    ///
    /// # Errors
    ///
    /// As for [`SysfsFan::apply`].
    pub fn stop(&mut self) -> Result<(), CoreError> {
        self.write_enable(0)?;
        write_sysfs(&self.pwm_path(), "0")
    }

    /// Raw duty value currently in `pwmN`, `None` when unreadable.
    #[must_use]
    pub fn read_pwm(&self) -> Option<i64> {
        read_int(&self.pwm_path()).ok()
    }

    /// The duty currently in `pwmN` as a fraction of this header's maximum
    /// (0.0–1.0), `None` when the register cannot be read. A header the
    /// motherboard controls shows the motherboard's duty, not ours.
    #[must_use]
    pub fn read_percent(&self) -> Option<f64> {
        #[expect(
            clippy::cast_precision_loss,
            reason = "duty values are at most a few thousand"
        )]
        let duty = self.read_pwm()? as f64;
        Some((duty / self.pwm_max).clamp(0.0, 1.0))
    }

    /// Measured RPM from the matching tachometer, `None` when unavailable.
    #[must_use]
    pub fn read_rpm(&self) -> Option<f64> {
        if !self.supports_rpm {
            return None;
        }
        let rpm = read_int(&self.dir.join(format!("fan{}_input", self.index))).ok()?;
        #[expect(clippy::cast_precision_loss, reason = "rpm values are tiny")]
        let rpm_f64 = rpm as f64;
        Some(rpm_f64)
    }

    /// Name of the chip driver that exposes this header.
    #[must_use]
    pub fn chip(&self) -> &str {
        &self.chip
    }

    /// Index `N` of this header's `pwmN` files within its chip.
    #[must_use]
    pub const fn pwm_index(&self) -> usize {
        self.index
    }

    /// Whether this process could open the speed register for writing
    /// (nothing is actually written). Drivers whose attributes have no
    /// usable store method — headers wired as thermal sensors, or
    /// read-only duty registers — reject writes with a permission error
    /// even for root; this probe tells that apart from the process simply
    /// lacking privileges.
    #[must_use]
    pub fn open_writable(&self) -> bool {
        std::fs::OpenOptions::new()
            .write(true)
            .open(self.pwm_path())
            .is_ok()
    }

    /// Whether a tachometer (`fanN_input`) exists to verify this header.
    #[must_use]
    pub const fn tach_supported(&self) -> bool {
        self.supports_rpm
    }

    /// Writes `pwmN_enable` back to the mode found at discovery, handing
    /// the header back to the motherboard (usually hardware-automatic fan
    /// control). A header with no enable file accepted direct `pwmN`
    /// writes and has nothing to restore.
    ///
    /// # Errors
    ///
    /// [`CoreError::Io`] when the enable file cannot be written.
    pub fn restore_control(&mut self) -> Result<(), CoreError> {
        let Some(mode) = self.original_enable else {
            return Ok(());
        };
        self.write_enable(mode)
    }

    fn pwm_path(&self) -> PathBuf {
        self.dir.join(format!("pwm{}", self.index))
    }

    fn enable_path(&self) -> PathBuf {
        self.dir.join(format!("pwm{}_enable", self.index))
    }

    fn write_enable(&self, mode: u8) -> Result<(), CoreError> {
        let path = self.enable_path();
        // Open deliberately *without* create: `std::fs::write` adds
        // O_CREAT, and on sysfs a missing attribute (chips with no enable
        // file, or a hot-unplugged controller) surfaces as a failed
        // create — a permission error even for root — instead of the
        // "no enable file" signal we need to tolerate.
        let mut file = match std::fs::OpenOptions::new().write(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(source) => return Err(CoreError::Io { path, source }),
        };
        write_replacing(&path, &mut file, &mode.to_string())
    }
}

/// Writes `content` to an already-opened value file, replacing whatever
/// was there. sysfs drivers take over the whole attribute from the first
/// write, but regular files (test fixtures) keep trailing bytes, so they
/// are truncated first — an ftruncate sysfs rejects is ignored.
fn write_replacing(path: &Path, file: &mut File, content: &str) -> Result<(), CoreError> {
    let _ = file.set_len(0);
    file.write_all(content.as_bytes())
        .map_err(|source| CoreError::Io {
            path: path.to_path_buf(),
            source,
        })
}

/// Writes a small value file such as a hwmon pwm register. Opening
/// without create matches sysfs semantics: value files always exist or
/// the device is gone (ENOENT), and a create attempt would be rejected
/// with a misleading permission error.
fn write_sysfs(path: &Path, content: &str) -> Result<(), CoreError> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|source| CoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    write_replacing(path, &mut file, content)
}

/// Extracts `N` from an hwmon file named `pwmN` (ignoring `pwmN_*` helpers).
fn pwm_file_index(name: &str) -> Option<usize> {
    let digits = name.strip_prefix("pwm")?;
    digits.parse::<usize>().ok().filter(|index| *index > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_chip(root: &Path, hwmon: &str, name: &str) -> PathBuf {
        let dir = root.join(hwmon);
        std::fs::create_dir_all(&dir).expect("fixture dir");
        std::fs::write(dir.join("name"), format!("{name}\n")).expect("chip name");
        dir
    }

    #[test]
    fn discovers_pwm_headers_and_reads_rpm() {
        let root = tempfile::tempdir().expect("tempdir");
        let chip = fixture_chip(root.path(), "hwmon0", "nct6795");
        std::fs::write(chip.join("pwm1"), "128\n").expect("pwm1");
        std::fs::write(chip.join("pwm1_enable"), "1\n").expect("enable");
        std::fs::write(chip.join("fan1_input"), "1200\n").expect("fan1");
        // Helper files must not be mistaken for headers.
        std::fs::write(chip.join("pwm2_freq"), "25000\n").expect("pwm2_freq");

        let fans = SysfsFan::discover_all(root.path()).expect("discovery");
        assert_eq!(fans.len(), 1);
        assert_eq!(fans[0].index, 1);
        assert_eq!(fans[0].chip(), "nct6795");

        let descriptor = fans[0].descriptor();
        assert_eq!(descriptor.id.as_str(), "nct6795-p1");
        assert_eq!(descriptor.default_name, "Fan 1 on NCT 6795");
        assert!(descriptor.rpm_supported);
        assert_eq!(fans[0].read_rpm(), Some(1200.0));
    }

    #[test]
    fn headers_are_ordered_by_chip_then_header_number() {
        let root = tempfile::tempdir().expect("tempdir");
        let it87 = fixture_chip(root.path(), "hwmon0", "it87");
        for index in [1, 2, 10] {
            std::fs::write(it87.join(format!("pwm{index}")), "0\n").expect("pwm");
        }
        let nct = fixture_chip(root.path(), "hwmon1", "nct6795");
        std::fs::write(nct.join("pwm3"), "0\n").expect("pwm");

        let fans = SysfsFan::discover_all(root.path()).expect("discovery");
        let ids: Vec<String> = fans.iter().map(|fan| fan.id().to_string()).collect();
        assert_eq!(ids, ["it87-p1", "it87-p2", "it87-p10", "nct6795-p3"]);
    }

    #[test]
    fn apply_writes_scaled_manual_pwm() {
        let root = tempfile::tempdir().expect("tempdir");
        let chip = fixture_chip(root.path(), "hwmon0", "it87");
        std::fs::write(chip.join("pwm1"), "0\n").expect("pwm1");
        std::fs::write(chip.join("pwm1_enable"), "2\n").expect("enable");

        let mut fans = SysfsFan::discover_all(root.path()).expect("discovery");
        let mut fan = fans.pop().expect("one fan");
        fan.apply(0.5).expect("apply half");
        assert_eq!(read_trimmed(&chip.join("pwm1")).as_deref(), Some("128"));
        assert_eq!(
            read_trimmed(&chip.join("pwm1_enable")).as_deref(),
            Some("1")
        );

        fan.apply(2.0).expect("apply clamps high");
        assert_eq!(read_trimmed(&chip.join("pwm1")).as_deref(), Some("255"));
    }

    #[test]
    fn read_percent_reports_the_duty_the_chip_holds() {
        let root = tempfile::tempdir().expect("tempdir");
        let chip = fixture_chip(root.path(), "hwmon0", "it87");
        std::fs::write(chip.join("pwm1"), "0\n").expect("pwm1");
        std::fs::write(chip.join("pwm1_enable"), "1\n").expect("enable");

        let mut fans = SysfsFan::discover_all(root.path()).expect("discovery");
        let mut fan = fans.pop().expect("one fan");
        assert_eq!(fan.read_percent(), Some(0.0), "a stopped duty reads zero");

        fan.apply(0.5).expect("apply half");
        let percent = fan.read_percent().expect("duty is readable");
        assert!((percent - 0.5).abs() < 0.01, "128 of 255 reads {percent}");

        fan.stop().expect("stop");
        assert_eq!(fan.read_percent(), Some(0.0), "the zeroed duty reads back");
    }

    #[test]
    fn stop_writes_zero_and_tolerates_missing_enable_file() {
        let root = tempfile::tempdir().expect("tempdir");
        let chip = fixture_chip(root.path(), "hwmon0", "nvidia");
        std::fs::write(chip.join("pwm1"), "200\n").expect("pwm1");

        let mut fans = SysfsFan::discover_all(root.path()).expect("discovery");
        let mut fan = fans.pop().expect("one fan");
        fan.stop().expect("stop without enable file still works");
        assert_eq!(read_trimmed(&chip.join("pwm1")).as_deref(), Some("0"));
        assert_eq!(fan.read_rpm(), None);
    }

    #[test]
    fn apply_drives_manual_only_chips_without_creating_an_enable_file() {
        // The ARCTIC Fan Controller (mainline `arctic_fan` driver) is
        // manual-only and exposes no pwmN_enable; sysfs would also reject
        // any attempt to create one. Duty control must still work.
        let root = tempfile::tempdir().expect("tempdir");
        let chip = fixture_chip(root.path(), "hwmon0", "arctic_fan");
        std::fs::write(chip.join("pwm1"), "0\n").expect("pwm1");
        std::fs::write(chip.join("fan1_input"), "800\n").expect("fan1");

        let mut fans = SysfsFan::discover_all(root.path()).expect("discovery");
        let mut fan = fans.pop().expect("one fan");
        fan.apply(0.5).expect("duty-only control");
        assert_eq!(read_trimmed(&chip.join("pwm1")).as_deref(), Some("128"));
        assert!(
            !chip.join("pwm1_enable").exists(),
            "no enable file may be created"
        );
        assert_eq!(fan.read_rpm(), Some(800.0));
        // Nothing was found to restore at discovery, so release is a no-op.
        fan.restore_control().expect("nothing to restore");
    }

    #[test]
    fn honors_chip_reported_pwm_max() {
        let root = tempfile::tempdir().expect("tempdir");
        let chip = fixture_chip(root.path(), "hwmon0", "applesmc");
        std::fs::write(chip.join("pwm1"), "0\n").expect("pwm1");
        std::fs::write(chip.join("pwm1_max"), "254\n").expect("pwm_max");

        let mut fans = SysfsFan::discover_all(root.path()).expect("discovery");
        let mut fan = fans.pop().expect("one fan");
        fan.apply(0.5).expect("apply");
        assert_eq!(read_trimmed(&chip.join("pwm1")).as_deref(), Some("127"));
    }

    #[test]
    fn restore_control_hands_header_back_to_original_mode() {
        let root = tempfile::tempdir().expect("tempdir");
        let chip = fixture_chip(root.path(), "hwmon0", "nct6795");
        std::fs::write(chip.join("pwm1"), "0\n").expect("pwm1");
        std::fs::write(chip.join("pwm1_enable"), "2\n").expect("enable");

        let mut fans = SysfsFan::discover_all(root.path()).expect("discovery");
        let mut fan = fans.pop().expect("one fan");
        fan.apply(0.5).expect("apply takes manual control");
        assert_eq!(
            read_trimmed(&chip.join("pwm1_enable")).as_deref(),
            Some("1")
        );
        fan.restore_control().expect("restore");
        assert_eq!(
            read_trimmed(&chip.join("pwm1_enable")).as_deref(),
            Some("2")
        );
    }

    #[test]
    fn restore_control_tolerates_missing_enable_file() {
        let root = tempfile::tempdir().expect("tempdir");
        let chip = fixture_chip(root.path(), "hwmon0", "applesmc");
        std::fs::write(chip.join("pwm1"), "0\n").expect("pwm1");

        let mut fans = SysfsFan::discover_all(root.path()).expect("discovery");
        let mut fan = fans.pop().expect("one fan");
        fan.apply(0.5).expect("apply");
        fan.restore_control().expect("nothing to restore");
        assert_eq!(read_trimmed(&chip.join("pwm1")).as_deref(), Some("128"));
    }

    #[test]
    fn open_writable_tracks_write_permission_on_the_register() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().expect("tempdir");
        let chip = fixture_chip(root.path(), "hwmon0", "nct6795");
        std::fs::write(chip.join("pwm1"), "0\n").expect("pwm1");

        let fans = SysfsFan::discover_all(root.path()).expect("discovery");
        let fan = fans.first().expect("one fan");
        let normal_mode_openable = fan.open_writable();
        assert!(
            normal_mode_openable,
            "a 0644 register should open for write"
        );

        std::fs::set_permissions(chip.join("pwm1"), std::fs::Permissions::from_mode(0o444))
            .expect("chmod");
        // Self-consistent with the kernel: however this environment
        // evaluates permissions (root ignores mode bits), discovery agrees.
        let still_openable = std::fs::OpenOptions::new()
            .write(true)
            .open(chip.join("pwm1"))
            .is_ok();
        assert_eq!(fan.open_writable(), still_openable);
    }
}
