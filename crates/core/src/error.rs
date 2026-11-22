//! Error type shared by the whole core crate.

use std::path::PathBuf;
use thiserror::Error;

/// Errors produced by sensor discovery, fan control and configuration.
#[derive(Debug, Error)]
pub enum CoreError {
    /// A sysfs or configuration file could not be read or written.
    #[error("could not access {}: {source}", .path.display())]
    Io {
        /// The file involved.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A sysfs value file did not contain a usable number.
    #[error("expected a number in {}, but content was not readable", .path.display())]
    Parse {
        /// The file involved.
        path: PathBuf,
    },
    /// A fan id in the config does not match any discovered fan header.
    #[error("unknown fan id '{0}' — run `biggest-fan scan` to list fan headers")]
    UnknownFan(String),
    /// A sensor id in the config does not match any discovered sensor.
    #[error("unknown sensor id '{0}' — run `biggest-fan scan` to list sensors")]
    UnknownSensor(String),
    /// Duplicate fan ids in the config.
    #[error("duplicate fan id '{0}' in the config")]
    DuplicateFan(String),
    /// A fan curve is not usable (empty, unsorted or out-of-range speeds).
    #[error("invalid fan curve: {0}")]
    InvalidCurve(String),
    /// The configuration file is invalid or could not be serialized.
    #[error("config error: {0}")]
    Config(String),
    /// The NVIDIA NVML library reported an error.
    #[error("nvidia NVML error: {0}")]
    Nvidia(String),
}

impl From<nvml_wrapper::error::NvmlError> for CoreError {
    fn from(source: nvml_wrapper::error::NvmlError) -> Self {
        Self::Nvidia(source.to_string())
    }
}
