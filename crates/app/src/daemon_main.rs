//! Biggest Fan daemon — the root-privileged fan control loop.
//!
//! Runs headless until SIGINT/SIGTERM, driving every managed `pwm` header
//! from the config file, and owns the `net.ynottony.ten.biggestfan` name
//! on the system bus so the (rootless) `biggest-fan gui` can watch and
//! change fan settings through it. Installed and started by the .deb as
//! `biggest-fan.service`; typically run only by systemd.

#![warn(missing_docs)]
#![warn(clippy::all)]
#![warn(clippy::pedantic)]
#![warn(clippy::complexity)]
#![warn(clippy::nursery)]
#![warn(clippy::perf)]
#![warn(clippy::style)]

use biggest_fan::dbus;
use biggest_fan::engine_setup;
use biggest_fan_core as core;
use clap::Parser;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "biggest-fan-daemon",
    version,
    about = "Biggest Fan control loop: drives hwmon PWM fan headers and serves the GUI over D-Bus. Needs root for header writes."
)]
struct Cli {
    /// Config file (default: ~/.config/biggest-fan/config.toml; the
    /// systemd unit passes /etc/biggest-fan/config.toml)
    #[arg(short, long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Compute fan speeds but never write them to the fan headers
    #[arg(long, global = true)]
    dry_run: bool,

    /// Skip NVIDIA GPU reading through NVML
    #[arg(long, global = true)]
    no_nvidia: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(core::config::default_config_path);
    let options = core::EngineOptions {
        hwmon_root: PathBuf::from("/sys/class/hwmon"),
        use_nvidia: !cli.no_nvidia,
        apply_writes: !cli.dry_run,
    };

    match engine_setup::engine_from_config(&config_path, &options).and_then(run) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("biggest-fan-daemon: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Runs the control loop until SIGINT/SIGTERM, then hands the fan headers
/// back to the motherboard's automatic control. While it runs, the GUI
/// can reach it without root over D-Bus (see `biggest_fan::dbus`).
fn run(mut engine: core::Engine) -> Result<(), core::CoreError> {
    let stopping = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&stopping))
            .map_err(|error| core::CoreError::Config(format!("signal handler: {error}")))?;
    }
    // GUI requests queue up here and are applied on this — the engine's
    // only — thread, between ticks and inside the sleep quanta.
    let (command_tx, command_rx) = std::sync::mpsc::channel();
    dbus::spawn_server(command_tx, Arc::clone(&stopping));
    while !stopping.load(Ordering::Relaxed) {
        dbus::drain_commands(&mut engine, &command_rx);
        match engine.tick() {
            Ok(report) => {
                for fan in &report.fans {
                    let mode = match &fan.mode {
                        core::FanMode::Off => "off",
                        core::FanMode::Fixed { .. } => "fixed",
                        core::FanMode::Auto { .. } => "auto",
                    };
                    // What the hardware reports: RPM where there is a
                    // tachometer, otherwise the header's own speed reading (a
                    // GPU cooler has no tach, so this shows it parking).
                    let rpm = match (fan.measured_rpm, fan.measured_percent) {
                        (Some(rpm), _) => format!("rpm {rpm:.0}"),
                        (None, Some(percent)) => format!("hdr {:.0}%", percent * 100.0),
                        (None, None) => "rpm -".to_string(),
                    };
                    println!(
                        "{:<24} {:>4.0}%  {rpm:<9} [{mode}] {}",
                        fan.name,
                        fan.commanded_speed * 100.0,
                        fan.note.clone().unwrap_or_default()
                    );
                }
                std::io::stdout().flush().ok();
            }
            Err(error) => {
                // Transient write failures must not kill the daemon; log and
                // retry next tick.
                eprintln!("biggest-fan-daemon: control loop error: {error}");
                if matches!(
                    error,
                    core::CoreError::Io { source, .. }
                        if source.kind() == std::io::ErrorKind::PermissionDenied
                ) {
                    eprintln!(
                        "hint: fan header writes need root — run as root (the .deb does) or pass --dry-run"
                    );
                }
            }
        }
        // Sleep the poll interval in short quanta so a stop signal — and
        // GUI requests — land promptly.
        let total_ms = engine.poll_interval_ms();
        let mut slept_ms = 0;
        while slept_ms < total_ms && !stopping.load(Ordering::Relaxed) {
            let step_ms = (total_ms - slept_ms).min(100);
            std::thread::sleep(Duration::from_millis(step_ms));
            dbus::drain_commands(&mut engine, &command_rx);
            slept_ms += step_ms;
        }
    }
    eprintln!("biggest-fan-daemon: stopping — returning fans to hardware control");
    engine.release_control()
}
