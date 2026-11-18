//! Engine construction shared by the one-shot commands and the daemon.
//!
//! Loads the config file, writing a sensible default from live hardware
//! when it does not exist yet, then validates it against that hardware.

use biggest_fan_core as core;
use std::path::Path;

/// Loads the config (creating a default one when missing) and wires up a
/// [`core::Engine`] validated against the live hardware.
///
/// # Errors
///
/// [`core::CoreError::Io`] when the config file or discovery fails, or the
/// errors [`Engine::startup`] reports for a config naming hardware that
/// is not present.
///
/// [`Engine::startup`]: core::Engine::startup
pub fn engine_from_config(
    config_path: &Path,
    options: &core::EngineOptions,
) -> Result<core::Engine, core::CoreError> {
    let config = if config_path.exists() {
        core::config::load(config_path)?
    } else {
        let discovery = core::discover(&options.hwmon_root, options.use_nvidia)?;
        let config = core::default_config(&discovery);
        core::config::save(&config, config_path)?;
        eprintln!(
            "no config at {} — wrote a default one; review it before trusting it",
            config_path.display()
        );
        config
    };
    core::Engine::startup(config, config_path, options)
}
