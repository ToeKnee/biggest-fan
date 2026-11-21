//! `stress-lab`: the developer bench for tuning fan settings against real
//! workloads. It writes its own stressors on purpose — no stress-ng:
//!
//! - `cpu` spins every core on AVX-512 hot instructions (chaotic-map FMA
//!   chains, lane-crossing shuffles, ternary logic).
//! - `gpu` keeps the graphics card busy with a Vulkan compute hot loop
//!   (WGSL, compiled at runtime by wgpu).
//! - `monitor` logs every temperature, fan RPM and GPU reading to a CSV so
//!   fan settings changed mid-run can be compared against what they did.
//!
//! The CPU and GPU stressors deliberately run as separate commands so the
//! experiment harness can start them together (`stress-lab cpu & stress-lab
//! gpu`) and stop them independently. All stressors carry a thermal guard:
//! a k10temp (and GPU, when NVML answers) temperature over `--max-temp-c`
//! stops the stress with exit code 3. The same stressors run inside the
//! GUI's Stress lab panel (they are a library — see `lib.rs`).
//!
//! Fan settings are *not* touched here: they change on the live daemon via
//! its D-Bus `SetFanMode` method (see the README "Stress lab" section), and
//! restarting the service puts the saved config back.

use biggest_fan_stress_lab::{cpu, gpu, monitor, thermal};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "stress-lab",
    about = "Fan-tuning bench: AVX-512 CPU stress, Vulkan GPU stress, hardware CSV logging."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Burn every core with AVX-512 hot instructions.
    Cpu(cpu::CpuArgs),
    /// Keep the GPU busy with a Vulkan compute hot loop.
    Gpu(gpu::GpuArgs),
    /// Log temperatures, fans and GPU readings to a CSV file.
    Monitor(monitor::MonitorArgs),
}

fn main() {
    let cli = Cli::parse();
    let outcome = match cli.command {
        Command::Cpu(args) => cpu::run(&args),
        Command::Gpu(args) => gpu::run(&args),
        Command::Monitor(args) => monitor::run(&args).map(|()| 0),
    };
    match outcome {
        Ok(code) => std::process::exit(code),
        Err(thermal::LabError::Thermal(message)) => {
            eprintln!("stress-lab: {message}");
            std::process::exit(3);
        }
        Err(thermal::LabError::Failed(message)) => {
            eprintln!("stress-lab: {message}");
            std::process::exit(1);
        }
    }
}
