//! Biggest Fan — the GUI and one-shot hardware commands.
//!
//! Reads temperatures from lm-sensors-style hwmon chips and NVIDIA GPUs,
//! and controls motherboard fan headers through the `biggest-fan-daemon`
//! service: the GUI itself never writes to the headers, it joins the
//! running daemon over D-Bus and forwards every change to it (see
//! `biggest_fan::dbus`).

#![warn(missing_docs)]
#![warn(clippy::all)]
#![warn(clippy::pedantic)]
#![warn(clippy::complexity)]
#![warn(clippy::nursery)]
#![warn(clippy::perf)]
#![warn(clippy::style)]

use biggest_fan::app;
use biggest_fan::dbus;
use biggest_fan::engine_setup;
use biggest_fan_core as core;
use clap::{Parser, Subcommand};
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "biggest-fan",
    version,
    about = "Smart fan controller: reads lm-sensors and NVIDIA GPU temperatures, drives PWM fan headers through the biggest-fan-daemon service."
)]
struct Cli {
    /// Config file (default: ~/.config/biggest-fan/config.toml)
    #[arg(short, long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Compute fan speeds but never write them to the fan headers
    #[arg(long, global = true)]
    dry_run: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Show the case view and fan controls (the default)
    Gui,
    /// Print discovered temperature sensors and fan headers
    Scan,
    /// Write a starter config file based on discovered hardware
    InitConfig {
        /// Overwrite an existing config file
        #[arg(long)]
        force: bool,
    },
    /// Spin up every managed header and disable the ones with nothing connected
    DetectFans,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(core::config::default_config_path);
    let options = core::EngineOptions {
        hwmon_root: PathBuf::from("/sys/class/hwmon"),
        // NVIDIA management is always attempted: a machine without an
        // NVIDIA GPU simply ends up with no NVIDIA sensors or coolers.
        use_nvidia: true,
        apply_writes: !cli.dry_run,
    };

    let result = match cli.command {
        Some(Command::Scan) => scan(&options),
        Some(Command::InitConfig { force }) => init_config(&config_path, &options, force),
        Some(Command::DetectFans) => {
            engine_setup::engine_from_config(&config_path, &options).and_then(detect_fans)
        }
        Some(Command::Gui) | None => gui(&config_path, &options),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("biggest-fan: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Runs the GUI. The GUI never controls the headers itself: it joins the
/// running daemon (the systemd service, or `sudo biggest-fan-daemon`)
/// over D-Bus, mirrors its config, and forwards every change to it — so
/// a normal user can drive the fans. Without a reachable service the GUI
/// reports the problem and exits; see [`dbus::ConnectError`].
fn gui(config_path: &Path, options: &core::EngineOptions) -> Result<(), core::CoreError> {
    let (client, events, service_config) = dbus::Client::connect().map_err(|problem| {
        core::CoreError::Config(format!(
            "{problem}. The GUI needs the Biggest Fan service: install the .deb and start it with `sudo systemctl enable --now biggest-fan.service`, or run `sudo biggest-fan-daemon` in a terminal."
        ))
    })?;
    let config = core::config::from_text(&service_config)?;
    let mirror_options = core::EngineOptions {
        hwmon_root: options.hwmon_root.clone(),
        use_nvidia: options.use_nvidia,
        // The service owns the headers; this process only reads.
        apply_writes: false,
    };
    let engine = core::Engine::startup(config, config_path.to_path_buf(), &mirror_options)?;
    finish_gui(app::run(engine, client, events))
}

fn finish_gui(code: glib::ExitCode) -> Result<(), core::CoreError> {
    if code == glib::ExitCode::SUCCESS {
        Ok(())
    } else {
        Err(core::CoreError::Config(format!(
            "GUI exited with code {}",
            code.get()
        )))
    }
}

fn scan(options: &core::EngineOptions) -> Result<(), core::CoreError> {
    let discovery = core::discover(&options.hwmon_root, options.use_nvidia)?;
    println!("Temperature sensors:");
    for sensor in &discovery.sensors {
        println!(
            "  {:<24} {:<38} ({})",
            sensor.id, sensor.default_name, sensor.raw_label
        );
    }
    println!("\nFan headers (pwm):");
    if discovery.fans.is_empty() {
        println!("  none found — running as root may reveal more");
    }
    for fan in &discovery.fans {
        let tach = if fan.rpm_supported { "tach" } else { "no tach" };
        println!("  {:<24} {:<38} [{tach}]", fan.id, fan.default_name);
    }
    Ok(())
}

fn init_config(
    config_path: &Path,
    options: &core::EngineOptions,
    force: bool,
) -> Result<(), core::CoreError> {
    if config_path.exists() && !force {
        eprintln!(
            "{} already exists; pass --force to replace it",
            config_path.display()
        );
        return Ok(());
    }
    let discovery = core::discover(&options.hwmon_root, options.use_nvidia)?;
    let config = core::default_config(&discovery);
    core::config::save(&config, config_path)?;
    println!("wrote {}", config_path.display());
    Ok(())
}

/// Spins every managed header up briefly and disables the ones whose
/// tachometer stays silent or whose driver refuses the spin-up; the
/// result is saved to the config file. Needs root (the headers are
/// written) — normally done from the GUI, which forwards this to the
/// root daemon instead.
fn detect_fans(mut engine: core::Engine) -> Result<(), core::CoreError> {
    let hidden = engine.detect_unconnected_fans(Duration::from_millis(900))?;
    if hidden.is_empty() {
        println!("nothing to disable — every managed header spun up");
    } else {
        for id in &hidden {
            println!("disabled {id} (no spin-up or the driver refuses writes)");
        }
        println!(
            "to re-enable one, set its `hidden = false` in {} and re-run",
            engine.config_path().display()
        );
    }
    engine.save_config()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvidia_handling_needs_no_option() {
        // `--no-nvidia` is gone: machines without an NVIDIA GPU need no
        // explicit configuration, and the flag is rejected outright.
        assert!(Cli::try_parse_from(["biggest-fan", "--no-nvidia", "scan"]).is_err());
        assert!(Cli::try_parse_from(["biggest-fan", "--dry-run", "scan"]).is_ok());
    }
}
