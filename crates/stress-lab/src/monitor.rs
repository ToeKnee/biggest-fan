//! The experiment recorder: one CSV line per sample with every hardware
//! reading worth comparing across fan-settings phases — all hwmon temps,
//! fan tachometers and PWM duties, plus GPU temperature/fan/utilisation/
//! power/thermal-throttle flags through NVML when the driver answers.
//!
//! A *phase file* lets the harness mark what it was doing at any moment:
//! each sample copies the file's current single line (e.g.
//! `front-fans-full`) into the CSV, so the later analysis groups samples
//! by phase instead of by clock time.
//!
//! The logger never touches the fans: settings change through the daemon's
//! D-Bus `SetFanMode`; the CSV just records what followed.

use crate::thermal::{self, GpuMonitor, LabError};
use clap::Args;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Args, Debug)]
pub struct MonitorArgs {
    /// CSV file to write (created/truncated).
    pub out: PathBuf,
    /// Sample for this many seconds; 0 runs until killed.
    #[arg(long, default_value_t = 600)]
    pub secs: u64,
    /// Seconds between samples.
    #[arg(long, default_value_t = 1.0)]
    pub every_secs: f64,
    /// File whose current line names the phase for each sample.
    #[arg(long)]
    pub phase_file: Option<PathBuf>,
    /// hwmon root to scan (test seam).
    #[arg(long, default_value = "/sys/class/hwmon")]
    pub hwmon_root: PathBuf,
    /// k10temp/GPU temperature that stops the recording early; 0 leaves
    /// the recorder passive (default) so it never fights a stress run.
    #[arg(long, default_value_t = 0.0)]
    pub max_temp_c: f64,
}

/// One recorded signal: where to read it, and what the CSV column is
/// called.
struct Column {
    /// CSV header, e.g. `k10temp/Tctl` or `nct6799/fan4`.
    header: String,
    /// The sysfs input file this value is read from.
    input_path: PathBuf,
    /// What kind of sysfs input this is (scaling differs per kind).
    kind: ValueKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ValueKind {
    /// `tempN_input`, millidegrees.
    TempMillidegrees,
    /// `fanN_input`, rpm.
    FanRpm,
    /// `pwmN`, raw 0–255 duty.
    PwmDuty,
}

/// sysfs pseudo-files report size 0 and return nothing when re-sought, so
/// open, read, and trim — never `fs::read` (it trusts the size).
fn read_sysfs(path: &Path) -> Option<f64> {
    let text = std::fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_string())?;
    text.parse::<f64>().ok()
}

/// Every sysfs column available under `hwmon_root`, in a stable order:
/// chips by directory, then temps, fans, PWM duties within a chip.
/// Chips sharing a name get their hwmon directory prepended so headers
/// stay unique.
fn discover_columns(hwmon_root: &Path) -> Vec<Column> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(hwmon_root)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();

    let mut columns = Vec::new();
    // Read every chip name first: when several chips share one name (the
    // phantom nct6798 beside the real nct6799, for example) *every*
    // duplicate gets its hwmon directory prepended, so headers never
    // depend on scan order.
    let chips: Vec<(PathBuf, String)> = dirs
        .into_iter()
        .map(|chip_dir| {
            let raw_name = std::fs::read_to_string(chip_dir.join("name"))
                .ok()
                .map(|name| name.trim().to_string())
                .unwrap_or_else(|| {
                    chip_dir
                        .file_name()
                        .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
                });
            (chip_dir, raw_name)
        })
        .collect();
    for (chip_dir, raw_name) in &chips {
        let duplicate = chips.iter().filter(|(_, other)| other == raw_name).count() > 1;
        let chip_prefix = if duplicate {
            let dir_label = chip_dir
                .file_name()
                .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
            format!("{dir_label}-{raw_name}")
        } else {
            raw_name.clone()
        };

        let mut entries: Vec<(u32, ValueKind, PathBuf)> = std::fs::read_dir(chip_dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|entry| {
                        let file_name = entry.file_name();
                        let file_name = file_name.to_string_lossy().into_owned();
                        classify_input(&file_name)
                            .map(|(number, kind)| (number, kind, chip_dir.join(entry.file_name())))
                    })
                    .collect()
            })
            .unwrap_or_default();
        entries.sort_by_key(|(number, kind, _)| (*kind, *number));
        for (number, kind, input_path) in entries {
            let raw_label = match kind {
                ValueKind::TempMillidegrees => std::fs::read_to_string(
                    input_path.with_file_name(format!("temp{number}_label")),
                )
                .ok()
                .map(|label| label.trim().to_string())
                .filter(|label| !label.is_empty()),
                _ => None,
            };
            let stem = match kind {
                ValueKind::TempMillidegrees => format!("temp{number}"),
                ValueKind::FanRpm => format!("fan{number}"),
                ValueKind::PwmDuty => format!("pwm{number}"),
            };
            let header = match raw_label {
                Some(label) => format!("{chip_prefix}/{stem}({})", sanitize(label)),
                None => format!("{chip_prefix}/{stem}"),
            };
            columns.push(Column {
                header,
                input_path,
                kind,
            });
        }
    }
    columns
}

/// Recognise `tempN_input` / `fanN_input` / `pwmN` file names; everything
/// else (labels, alarms, `pwmN_enable`, …) is not a value column.
fn classify_input(file_name: &str) -> Option<(u32, ValueKind)> {
    if let Some(rest) = file_name
        .strip_prefix("temp")
        .and_then(|r| r.strip_suffix("_input"))
    {
        return rest
            .parse::<u32>()
            .ok()
            .zip(Some(ValueKind::TempMillidegrees));
    }
    if let Some(rest) = file_name
        .strip_prefix("fan")
        .and_then(|r| r.strip_suffix("_input"))
    {
        return rest.parse::<u32>().ok().zip(Some(ValueKind::FanRpm));
    }
    let rest = file_name.strip_prefix("pwm")?;
    rest.parse::<u32>().ok().zip(Some(ValueKind::PwmDuty))
}

/// Scale one raw sysfs reading to CSV units (°C / rpm / duty).
const fn scaled(value: f64, kind: ValueKind) -> f64 {
    match kind {
        ValueKind::TempMillidegrees => value / 1000.0,
        ValueKind::FanRpm | ValueKind::PwmDuty => value,
    }
}

/// Keep phase names CSV-safe: a phase line becomes a bare token.
fn sanitize(text: String) -> String {
    text.trim()
        .chars()
        .map(|c| match c {
            ',' | '"' | '\n' | '\r' => ';',
            c => c,
        })
        .collect()
}

pub fn run(args: &MonitorArgs) -> Result<(), LabError> {
    if args.every_secs <= 0.0 {
        return Err(LabError::Failed(
            "--every-secs must be positive".to_string(),
        ));
    }
    let columns = discover_columns(&args.hwmon_root);
    if columns.is_empty() {
        return Err(LabError::Failed(format!(
            "no hwmon channels found under {}",
            args.hwmon_root.display()
        )));
    }
    let gpu = GpuMonitor::try_open();
    if gpu.is_none() {
        eprintln!("stress-lab monitor: NVML not answering — logging hwmon channels only");
    }

    // Optional safety net for experiments: with `--max-temp-c` set, the
    // shared guard thread stops the recorder too when something runs hot.
    let stop = Arc::new(AtomicBool::new(false));
    let tripped = Arc::new(AtomicBool::new(false));
    let guard = thermal::spawn_guard(
        thermal::k10temp_path(&args.hwmon_root),
        args.max_temp_c,
        Arc::clone(&stop),
        Arc::clone(&tripped),
    );
    println!(
        "stress-lab monitor: {} hwmon channels, thermal guard: {}",
        columns.len(),
        if args.max_temp_c > 0.0 {
            format!("{:.0} °C", args.max_temp_c)
        } else {
            "off".to_string()
        },
    );

    let mut file = std::fs::File::create(&args.out).map_err(|error| {
        LabError::Failed(format!("cannot write {}: {error}", args.out.display()))
    })?;
    let mut header = String::from("epoch_ms,phase,");
    header.push_str(
        &columns
            .iter()
            .map(|c| c.header.clone())
            .collect::<Vec<_>>()
            .join(","),
    );
    if gpu.is_some() {
        header.push_str(",gpu_temp_c,gpu_fan_pct,gpu_util_pct,gpu_power_w,gpu_throttle_thermal");
    }
    writeln!(file, "{header}")
        .map_err(|error| LabError::Failed(format!("cannot write CSV: {error}")))?;

    let start = Instant::now();
    let deadline = if args.secs == 0 {
        None
    } else {
        Some(start + Duration::from_secs(args.secs))
    };
    let interval = Duration::from_secs_f64(args.every_secs);
    let mut samples = 0u64;
    while deadline.is_none_or(|until| Instant::now() < until) && !stop.load(Ordering::Relaxed) {
        let phase = args
            .phase_file
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .map(sanitize)
            .unwrap_or_default();
        let mut line = format!("{},{}", thermal::epoch_millis(), phase);
        for column in &columns {
            let value = read_sysfs(&column.input_path).map(|raw| scaled(raw, column.kind));
            match value {
                Some(value) => line.push_str(&format!(",{value}")),
                None => line.push_str(",nan"),
            }
        }
        if let Some(gpu) = &gpu {
            let temp = gpu.temperature_c();
            line.push_str(&format!(
                ",{},{},{},{},{}",
                temp.map_or_else(|| "nan".to_string(), |v| v.to_string()),
                gpu.fan_percent()
                    .map_or_else(|| "nan".to_string(), |v| v.to_string()),
                gpu.gpu_util_percent()
                    .map_or_else(|| "nan".to_string(), |v| v.to_string()),
                gpu.power_watts()
                    .map_or_else(|| "nan".to_string(), |v| v.to_string()),
                u8::from(gpu.thermal_throttling()),
            ));
        }
        writeln!(file, "{line}")
            .map_err(|error| LabError::Failed(format!("cannot write CSV: {error}")))?;
        // Flush every sample: losing the tail of a lab run is worse than
        // a little disk chatter.
        file.flush()
            .map_err(|error| LabError::Failed(format!("cannot flush CSV: {error}")))?;
        samples += 1;

        // A compact progress line so an interactive run reads well
        // without tailing the CSV.
        if samples.is_multiple_of(10) {
            let cpu_temp = thermal::k10temp_path(&args.hwmon_root)
                .and_then(|path| thermal::read_millidegrees(&path))
                .map_or_else(|| "n/a".to_string(), |temp| format!("{temp:.0} °C"));
            let gpu_temp = gpu
                .as_ref()
                .and_then(GpuMonitor::temperature_c)
                .map_or_else(|| "n/a".to_string(), |temp| format!("{temp:.0} °C"));
            println!(
                "sample {samples}: k10temp {cpu_temp}, GPU {gpu_temp}, phase {phase:?} → {}",
                args.out.display()
            );
        }
        std::thread::sleep(interval);
    }
    if let Some(guard) = guard {
        let _ = guard.join();
    }
    println!(
        "stress-lab monitor: {samples} samples in {}",
        args.out.display()
    );
    if tripped.load(Ordering::Relaxed) {
        return Err(LabError::Thermal(format!(
            "recording stopped by the thermal guard after {samples} samples"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "stress-lab-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let cpu = root.join("hwmon0");
        std::fs::create_dir_all(&cpu).expect("cpu dir");
        std::fs::write(cpu.join("name"), "k10temp\n").expect("name");
        std::fs::write(cpu.join("temp1_input"), "45000\n").expect("temp");
        std::fs::write(cpu.join("temp1_label"), "Tctl\n").expect("label");

        let superio = root.join("hwmon1");
        std::fs::create_dir_all(&superio).expect("superio dir");
        std::fs::write(superio.join("name"), "nct6795\n").expect("name");
        std::fs::write(superio.join("temp3_input"), "38000\n").expect("temp");
        std::fs::write(superio.join("fan1_input"), "1200\n").expect("fan");
        std::fs::write(superio.join("pwm1"), "128\n").expect("pwm");
        std::fs::write(superio.join("pwm1_enable"), "1\n").expect("enable");
        root
    }

    #[test]
    fn classify_input_picks_value_files_only() {
        assert_eq!(
            classify_input("temp7_input"),
            Some((7, ValueKind::TempMillidegrees))
        );
        assert_eq!(classify_input("fan12_input"), Some((12, ValueKind::FanRpm)));
        assert_eq!(classify_input("pwm3"), Some((3, ValueKind::PwmDuty)));
        assert_eq!(classify_input("pwm3_enable"), None);
        assert_eq!(classify_input("temp7_label"), None);
        assert_eq!(classify_input("temp7_crit"), None);
        assert_eq!(classify_input("name"), None);
    }

    #[test]
    fn discovery_labels_temps_and_keeps_order() {
        let root = fixture("discovery");
        let columns = discover_columns(&root);
        let headers: Vec<&str> = columns.iter().map(|c| c.header.as_str()).collect();
        assert!(headers.contains(&"k10temp/temp1(Tctl)"), "{headers:?}");
        assert!(headers.contains(&"nct6795/temp3"), "{headers:?}");
        assert!(headers.contains(&"nct6795/fan1"), "{headers:?}");
        assert!(headers.contains(&"nct6795/pwm1"), "{headers:?}");
        assert!(!headers.iter().any(|h| h.contains("enable")), "{headers:?}");
        std::fs::remove_dir_all(&root).expect("clean up");
    }

    #[test]
    fn duplicate_chip_names_get_unique_headers() {
        let root = std::env::temp_dir().join(format!(
            "stress-lab-dup-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        for (dir, pwm_value) in [("hwmon2", "1\n"), ("hwmon4", "2\n")] {
            let chip = root.join(dir);
            std::fs::create_dir_all(&chip).expect("chip dir");
            std::fs::write(chip.join("name"), "same\n").expect("name");
            std::fs::write(chip.join("pwm1"), pwm_value).expect("pwm");
        }
        let columns = discover_columns(&root);
        let headers: Vec<&str> = columns.iter().map(|c| c.header.as_str()).collect();
        assert!(headers.contains(&"hwmon2-same/pwm1"), "{headers:?}");
        assert!(headers.contains(&"hwmon4-same/pwm1"), "{headers:?}");
        std::fs::remove_dir_all(&root).expect("clean up");
    }

    #[test]
    fn scaling_and_sanitising() {
        assert!((scaled(45_000.0, ValueKind::TempMillidegrees) - 45.0).abs() < 1e-9);
        assert!((scaled(1200.0, ValueKind::FanRpm) - 1200.0).abs() < 1e-9);
        assert_eq!(
            sanitize("quiet \"eco\", slow\r\n".to_string()),
            "quiet ;eco;; slow"
        );
    }

    #[test]
    fn end_to_end_writes_a_parseable_csv_with_phase() {
        let root = fixture("e2e");
        let phase_file = root.join("phase");
        std::fs::write(&phase_file, "idle baseline\n").expect("phase");
        let out = root.join("log.csv");
        let args = MonitorArgs {
            out: out.clone(),
            secs: 1,
            every_secs: 0.2,
            phase_file: Some(phase_file),
            hwmon_root: root.clone(),
            max_temp_c: 0.0,
        };
        run(&args).expect("monitor run");
        let text = std::fs::read_to_string(&out).expect("csv");
        let mut lines = text.lines();
        let header = lines.next().expect("header");
        assert!(
            header.starts_with("epoch_ms,phase,k10temp/temp1(Tctl)"),
            "{header}"
        );
        let first_data = lines.next().expect("a sample");
        assert!(first_data.contains("idle baseline"), "{first_data}");
        assert!(
            first_data.contains("45"),
            "Celsius-scaled temp in row: {first_data}"
        );
        assert!(first_data.contains("1200"), "rpm in row: {first_data}");
        assert!(
            first_data.split(',').count() == header.split(',').count(),
            "row width matches header"
        );
        std::fs::remove_dir_all(&root).expect("clean up");
    }

    /// With `--max-temp-c` armed, a hot fixture must abort the run. The
    /// fixture's "k10temp" chip reads 45 °C and the guard watches once a
    /// second, so arming at 40 °C ends the 5-second run almost at once
    /// (the GPU side, if NVML answers, is idle-cool and never trips
    /// before the CPU fixture does on this machine's test runs).
    #[test]
    fn thermal_guard_stops_a_hot_recording_early() {
        let root = fixture("guard");
        let out = root.join("log.csv");
        let args = MonitorArgs {
            out: out.clone(),
            secs: 5,
            every_secs: 0.2,
            phase_file: None,
            hwmon_root: root.clone(),
            max_temp_c: 40.0,
        };
        let outcome = run(&args);
        std::fs::remove_dir_all(&root).expect("clean up");
        assert!(
            matches!(outcome, Err(LabError::Thermal(_))),
            "guard at 40 °C must abort a 45 °C fixture, got {outcome:?}"
        );
    }
}
