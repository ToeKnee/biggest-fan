//! Shared parts of the stress lab: the `stress-lab` CLI (see `main.rs`)
//! and the GUI's Stress lab panel both call these stressors directly.
//!
//! Each stressor has two entry points:
//!
//! - [`cpu::stress`] / [`gpu::stress`] — the library call. The caller owns
//!   the stop/trip flags and gets a [`thermal::StressReport`] back; nothing
//!   is printed, so a GUI can show the result wherever it likes.
//! - [`cpu::run`] / [`gpu::run`] — the CLI wrappers: fresh flags, print,
//!   map a thermal trip onto exit code 3.
//!
//! All stressors carry the shared thermal guard (see [`thermal`]): k10temp
//! or GPU over the limit stops the stress instead of cooking the machine.

pub mod cpu;
pub mod gpu;
pub mod monitor;
pub mod thermal;
