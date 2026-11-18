//! Reading and writing the TOML config file.

use crate::config::model::AppConfig;
use crate::error::CoreError;
use std::path::{Path, PathBuf};

/// Where the config lives when not passed on the command line:
/// `$XDG_CONFIG_HOME/biggest-fan/config.toml` (falling back to `~/.config`).
#[must_use]
pub fn default_config_path() -> PathBuf {
    let config_root = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map_or_else(
            || {
                let home =
                    std::env::var_os("HOME").unwrap_or_else(|| std::ffi::OsString::from("."));
                PathBuf::from(home).join(".config")
            },
            PathBuf::from,
        );
    config_root.join("biggest-fan").join("config.toml")
}

/// Loads and parses a config file.
///
/// # Errors
///
/// [`CoreError::Io`] when the file cannot be read; [`CoreError::Config`]
/// when it is not valid Biggest Fan configuration.
pub fn load(path: &Path) -> Result<AppConfig, CoreError> {
    let text = std::fs::read_to_string(path).map_err(|source| CoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    from_text(&text).map_err(|error| CoreError::Config(format!("{}: {error}", path.display())))
}

/// Parses configuration from its TOML text.
///
/// # Errors
///
/// [`CoreError::Config`] when the text is not valid Biggest Fan
/// configuration.
pub fn from_text(text: &str) -> Result<AppConfig, CoreError> {
    toml::from_str(text).map_err(|error| CoreError::Config(error.to_string()))
}

/// Renders a configuration as TOML text (the form `save` writes).
///
/// # Errors
///
/// [`CoreError::Config`] when serialization fails.
pub fn to_text(config: &AppConfig) -> Result<String, CoreError> {
    toml::to_string_pretty(config).map_err(|error| CoreError::Config(error.to_string()))
}

/// Writes the config back out, creating parent directories as needed.
///
/// # Errors
///
/// [`CoreError::Io`] when the file cannot be written; [`CoreError::Config`]
/// when serialization fails.
pub fn save(config: &AppConfig, path: &Path) -> Result<(), CoreError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && std::fs::create_dir_all(parent).is_err()
        && !parent.is_dir()
    {
        return Err(CoreError::Io {
            path: parent.to_path_buf(),
            source: std::io::Error::other("could not create config directory"),
        });
    }
    let text = to_text(config)?;
    std::fs::write(path, text).map_err(|source| CoreError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_then_loads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("config.toml");
        let config = AppConfig {
            poll_interval_ms: 250,
            ..AppConfig::default()
        };
        save(&config, &path).expect("save");
        let loaded = load(&path).expect("load");
        assert_eq!(loaded, config);
    }

    #[test]
    fn loading_missing_file_reports_io_error() {
        let error = load(Path::new("/nonexistent/bf-config.toml")).expect_err("must fail");
        assert!(matches!(error, CoreError::Io { .. }));
    }

    #[test]
    fn loading_garbage_reports_config_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "this is = = not toml").expect("write");
        let error = load(&path).expect_err("must fail");
        assert!(matches!(error, CoreError::Config(_)));
    }
}
