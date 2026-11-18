//! A controllable fan header behind whichever driver exposes it.

use crate::error::CoreError;
use crate::fans::model::{FanDescriptor, FanId};
use crate::fans::nvidia::NvidiaFan;
use crate::fans::sysfs::SysfsFan;
use std::io::ErrorKind;

/// One controllable fan header: either a `pwmN` register on an hwmon chip
/// or the fans of an NVIDIA GPU driven through NVML. Both take speed
/// fractions (0.0..=1.0), so the engine drives them uniformly.
#[derive(Clone, Debug)]
pub enum FanDriver {
    /// A `pwmN`/`fanN_input` header on an hwmon chip.
    Sysfs(SysfsFan),
    /// The fans of one NVIDIA GPU, driven together as a single cooler.
    Nvidia(NvidiaFan),
}

impl FanDriver {
    /// Stable id of this header (the config addresses fans by it).
    #[must_use]
    pub const fn id(&self) -> &FanId {
        match self {
            Self::Sysfs(fan) => fan.id(),
            Self::Nvidia(fan) => fan.id(),
        }
    }

    /// Hardware-derived display name.
    #[must_use]
    pub fn default_name(&self) -> &str {
        match self {
            Self::Sysfs(fan) => fan.default_name(),
            Self::Nvidia(fan) => fan.default_name(),
        }
    }

    /// Pre-discovery descriptor for listings and group editing.
    #[must_use]
    pub fn descriptor(&self) -> FanDescriptor {
        match self {
            Self::Sysfs(fan) => fan.descriptor(),
            Self::Nvidia(fan) => fan.descriptor(),
        }
    }

    /// The speed fraction this header can actually run for `requested`,
    /// given its current state.
    ///
    /// NVIDIA coolers spin only at 30 % or above and park below 25 %, so a
    /// request inside that band keeps the cooler doing what it already was
    /// ([`crate::fans::nvidia::MIN_SPIN_FRACTION`] and
    /// [`crate::fans::nvidia::SPIN_DOWN_FRACTION`]); sysfs headers take any
    /// fraction. [`NvidiaFan::apply`] realises the same mapping on the wire,
    /// so asking for this after a write is stable; asking without a write
    /// (a dry run) reports what a write would do from the current state.
    #[must_use]
    pub fn effective_speed(&self, requested: f64) -> f64 {
        match self {
            Self::Sysfs(_) => requested,
            Self::Nvidia(fan) => fan.effective_speed(requested),
        }
    }

    /// Commands a speed fraction (0.0..=1.0).
    ///
    /// # Errors
    ///
    /// [`CoreError::Io`] or [`CoreError::Nvidia`] when the write fails.
    pub fn apply(&mut self, speed_fraction: f64) -> Result<(), CoreError> {
        match self {
            Self::Sysfs(fan) => fan.apply(speed_fraction),
            Self::Nvidia(fan) => fan.apply(speed_fraction),
        }
    }

    /// Stops the fan.
    ///
    /// # Errors
    ///
    /// As for [`FanDriver::apply`].
    pub fn stop(&mut self) -> Result<(), CoreError> {
        match self {
            Self::Sysfs(fan) => fan.stop(),
            Self::Nvidia(fan) => fan.stop(),
        }
    }

    /// Hands the header back to hardware-automatic control (a sysfs
    /// `pwmN_enable` restore, or NVML's "back to default" for GPUs).
    ///
    /// # Errors
    ///
    /// As for [`FanDriver::apply`].
    pub fn restore_control(&mut self) -> Result<(), CoreError> {
        match self {
            Self::Sysfs(fan) => fan.restore_control(),
            Self::Nvidia(fan) => fan.restore_control(),
        }
    }

    /// Measured RPM when this header has a tachometer (NVML headers
    /// never do — they report percent only).
    #[must_use]
    pub fn read_rpm(&self) -> Option<f64> {
        match self {
            Self::Sysfs(fan) => fan.read_rpm(),
            Self::Nvidia(fan) => fan.read_rpm(),
        }
    }

    /// The speed fraction the hardware reports for this header right now,
    /// when it reports one at all: NVML's fan percent for a GPU cooler, the
    /// `pwmN` duty for a sysfs header. `None` when nothing can be read.
    ///
    /// A GPU cooler has no tachometer, so this is how a parked cooler becomes
    /// visible: once the card has actually stopped its fans they read back at
    /// 0 %.
    #[must_use]
    pub fn read_percent(&self) -> Option<f64> {
        match self {
            Self::Sysfs(fan) => fan.read_percent(),
            Self::Nvidia(fan) => fan.read_percent(),
        }
    }

    /// A warning that the hardware did not do what the commanded speed asked,
    /// where that can be told at all. GPU coolers check NVML's fan-speed
    /// read-back (a stop the driver refused, or a cooler that never started
    /// turning); sysfs headers have no equivalent read-back note, because a
    /// tachometer plus fan detection verifies those instead.
    #[must_use]
    pub fn speed_note(&self, commanded: f64) -> Option<String> {
        match self {
            Self::Sysfs(_) => None,
            Self::Nvidia(fan) => fan.speed_note(commanded),
        }
    }

    /// Whether a tachometer exists to verify this header.
    #[must_use]
    pub const fn tach_supported(&self) -> bool {
        match self {
            Self::Sysfs(fan) => fan.tach_supported(),
            Self::Nvidia(_) => false,
        }
    }

    /// Ordering key: controller name, then header number.
    #[must_use]
    pub fn sort_key(&self) -> (String, usize) {
        match self {
            Self::Sysfs(fan) => (fan.chip().to_string(), fan.pwm_index()),
            Self::Nvidia(fan) => (fan.chip(), 0),
        }
    }

    /// Whether a failed write proves the *driver* refuses software control
    /// rather than this process merely lacking privileges right now.
    ///
    /// For sysfs headers that is a permission error although the register
    /// opens for writing (the driver has no usable store method). Every
    /// NVML error is a semantic refusal (unsupported control, GPU lost);
    /// NVML's insufficient-privileges case is turned into a permission
    /// [`CoreError::Io`] by [`NvidiaFan::apply`], so it propagates as a
    /// plain error exactly like the sysfs equivalent.
    #[must_use]
    pub fn write_refused(&self, error: &CoreError) -> bool {
        match (self, error) {
            (Self::Sysfs(fan), CoreError::Io { source, .. }) => {
                source.kind() == ErrorKind::PermissionDenied && fan.open_writable()
            }
            (Self::Nvidia(_), CoreError::Nvidia(_)) => true,
            _ => false,
        }
    }
}
