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
use biggest_fan::power;
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

    /// Same as `quiet = true` in the config: print the fan speed table
    /// only at startup and whenever a fan has a warning, instead of once
    /// per tick. The first table proves the control loop is running;
    /// after that the journal stays quiet.
    #[arg(long)]
    quiet: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(core::config::default_config_path);
    let options = core::EngineOptions {
        hwmon_root: PathBuf::from("/sys/class/hwmon"),
        // NVIDIA management is always attempted: a machine without an
        // NVIDIA GPU simply ends up with no NVIDIA sensors or coolers.
        use_nvidia: true,
        apply_writes: !cli.dry_run,
    };

    match engine_setup::engine_from_config(&config_path, &options).and_then(|engine| {
        // Either the flag or the config's `quiet = true` quiets the table.
        let quiet = cli.quiet || engine.quiet();
        run(engine, quiet)
    }) {
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
fn run(mut engine: core::Engine, quiet: bool) -> Result<(), core::CoreError> {
    let stopping = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&stopping))
            .map_err(|error| core::CoreError::Config(format!("signal handler: {error}")))?;
    }
    // GUI requests queue up here and are applied on this — the engine's
    // only — thread, between ticks and inside the sleep quanta.
    let (command_tx, command_rx) = std::sync::mpsc::channel();
    dbus::spawn_server(command_tx, Arc::clone(&stopping));
    // A wake-up from suspend or hibernation arrives here as one message,
    // so the hardware gets re-read on the engine's own thread.
    let (wake_tx, wake_rx) = std::sync::mpsc::channel();
    power::spawn_resume_watcher(wake_tx);
    let mut first_tick = true;
    while !stopping.load(Ordering::Relaxed) {
        dbus::drain_commands(&mut engine, &command_rx);
        if wake_rx.try_recv().is_ok() {
            resync_after_resume(&mut engine);
        }
        match engine.tick() {
            Ok(report) => {
                for line in status_table(quiet, first_tick, &report) {
                    println!("{line}");
                }
                first_tick = false;
                // What the background nudger decided, when it decided
                // something: kept or reverted trims are rare and worth a
                // line in the journal.
                for note in engine.drain_nudge_notes() {
                    println!("nudge: {note}");
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

/// Re-reads the hardware and re-asserts every fan after the machine wakes
/// from sleep, then reports what changed. Printed regardless of `--quiet`:
/// resumes are rare, and this is the journal's only record that the
/// headers were re-bound.
fn resync_after_resume(engine: &mut core::Engine) {
    match engine.resync_hardware() {
        Ok(report) => {
            println!(
                "resume: re-bound {} fan header(s) and re-asserted every fan",
                report.rebound
            );
            if !report.missing_headers.is_empty() {
                eprintln!(
                    "biggest-fan-daemon: configured headers are not back yet: {}",
                    report.missing_headers.join(", ")
                );
            }
            if !report.unclaimed_headers.is_empty() {
                eprintln!(
                    "biggest-fan-daemon: headers the config has no fan for: {}",
                    report.unclaimed_headers.join(", ")
                );
            }
        }
        // The engine kept its old bindings, so the control loop carries on
        // exactly as it was before the sleep.
        Err(error) => {
            eprintln!("biggest-fan-daemon: hardware re-read after resume failed: {error}");
        }
    }
    std::io::stdout().flush().ok();
}

/// The tick's fan status table, one line per fan worth showing. Normally
/// every fan every tick; with `quiet` only the first table (the startup
/// proof) and fans carrying a warning.
fn status_table(quiet: bool, first_tick: bool, report: &core::TickReport) -> Vec<String> {
    report
        .fans
        .iter()
        .filter(|fan| !quiet || first_tick || fan.note.is_some())
        .map(fan_status_line)
        .collect()
}

/// One fan's line in the status table.
fn fan_status_line(fan: &core::FanStatus) -> String {
    let mode = match &fan.mode {
        core::FanMode::Off => "off",
        core::FanMode::Fixed { .. } => "fixed",
        core::FanMode::Auto { .. } => "auto",
    };
    // What the hardware reports: RPM where there is a tachometer,
    // otherwise the header's own speed reading (a GPU cooler has no tach,
    // so this shows it parking).
    let rpm = match (fan.measured_rpm, fan.measured_percent) {
        (Some(rpm), _) => format!("rpm {rpm:.0}"),
        (None, Some(percent)) => format!("hdr {:.0}%", percent * 100.0),
        (None, None) => "rpm -".to_string(),
    };
    format!(
        "{:<24} {:>4.0}%  {rpm:<9} [{mode}] {}",
        fan.name,
        fan.commanded_speed * 100.0,
        fan.note.clone().unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvidia_handling_needs_no_option() {
        // `--no-nvidia` is gone: machines without an NVIDIA GPU need no
        // explicit configuration, and the flag is rejected outright.
        assert!(Cli::try_parse_from(["biggest-fan-daemon", "--no-nvidia"]).is_err());
        assert!(Cli::try_parse_from(["biggest-fan-daemon", "--dry-run"]).is_ok());
    }

    #[test]
    fn quiet_flag_parses() {
        assert!(Cli::try_parse_from(["biggest-fan-daemon", "--quiet"]).is_ok());
    }

    /// A status fixture: two plain fans and one with a warning note.
    fn report() -> core::TickReport {
        let fan = |id: &str, name: &str, mode, rpm, percent, note| core::FanStatus {
            id: core::FanId::new(id),
            name: name.to_string(),
            mode,
            commanded_speed: 0.5,
            measured_percent: percent,
            measured_rpm: rpm,
            governing_temp_c: None,
            note,
        };
        core::TickReport {
            sensors: Vec::new(),
            fans: vec![
                // A header with a tachometer.
                fan(
                    "nct6795-p1",
                    "Rear exhaust",
                    core::FanMode::Auto {
                        sensor: core::SensorId::new("k10temp-t1"),
                        curve: core::FanCurve::default(),
                    },
                    Some(1000.0),
                    None,
                    None,
                ),
                // A parked GPU cooler: no tach, header at 0 %.
                fan(
                    "nvidia-gpu0-cooler",
                    "GPU cooler",
                    core::FanMode::Off,
                    None,
                    Some(0.0),
                    None,
                ),
                // A fan with something to say.
                fan(
                    "it87-p2",
                    "Case front",
                    core::FanMode::Fixed { speed: 0.5 },
                    None,
                    None,
                    Some("driver refused the write".to_string()),
                ),
            ],
        }
    }

    #[test]
    fn status_lines_show_mode_reading_and_note() {
        let lines = status_table(false, true, &report());
        assert_eq!(lines.len(), 3, "normally every fan logs every tick");
        assert!(lines[0].contains("Rear exhaust"), "name first");
        assert!(
            lines[0].contains("50%") && lines[0].contains("rpm 1000"),
            "{}",
            lines[0]
        );
        assert!(lines[0].contains("[auto]"), "mode tag");
        assert!(
            lines[1].contains("hdr 0%") && lines[1].contains("[off]"),
            "{}",
            lines[1]
        );
        assert!(lines[2].contains("rpm -"), "nothing readable");
        assert!(
            lines[2].contains("driver refused the write"),
            "notes ride the same line"
        );
    }

    #[test]
    fn quiet_logs_the_startup_table_then_only_warnings() {
        let report = report();
        assert_eq!(
            status_table(true, true, &report).len(),
            3,
            "the first table proves the loop is running"
        );
        let later = status_table(true, false, &report);
        assert_eq!(later.len(), 1, "after startup only the warned fan logs");
        assert!(later[0].contains("driver refused the write"));
        assert_eq!(
            status_table(false, false, &report).len(),
            3,
            "without --quiet nothing changes"
        );
    }
}
