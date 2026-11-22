//! Configuration loading, saving and the on-disk model.

pub mod file;
pub mod model;

pub use file::{default_config_path, from_text, load, save, to_text};
pub use model::{AppConfig, FanConfig, HysteresisConfig};
