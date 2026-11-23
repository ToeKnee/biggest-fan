//! The "Stress lab" panel: runs the stress lab's stressors (the library
//! behind the `stress-lab` CLI, see `crates/stress-lab`) in-process, so
//! the case view reacts live while real heat is being made.
//!
//! Each stressor runs on its own background thread and ends when its stop
//! flag goes up — the toggle going off, the duration running out, or the
//! window closing.
//!
//! A thermal guard (k10temp + GPU temperature) aborts the load if the
//! machine gets hot, so experimenting can never turn into cooking: the
//! guard trips, the thread wraps up, and the panel's own status line
//! says which temperature hit the limit.

use crate::app::{SECTION_CLASS, accessible_name, section_title};
use biggest_fan_stress_lab::{cpu, gpu, thermal};
use gtk4::glib::{self, ControlFlow};
use gtk4::prelude::*;
use gtk4::{Box as GtkBox, Label, Orientation, ProgressBar, SpinButton, ToggleButton};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// A live stress thread: the external stop flag, and the join handle
/// once it finishes so the result can be read back onto the panel.
struct StressRun {
    stop: Arc<AtomicBool>,
    started: Instant,
    /// Planned length in seconds, captured at start so the progress bar
    /// can show elapsed/duration even if the spin changes mid-run.
    duration_secs: u64,
    worker: Option<std::thread::JoinHandle<Result<thermal::StressReport, thermal::LabError>>>,
}

/// Which stressors are running now, plus the one-line outcome of each
/// finished run (shown until the next run starts).
#[derive(Default)]
struct StressRuns {
    cpu: Option<StressRun>,
    gpu: Option<StressRun>,
    cpu_note: Option<String>,
    gpu_note: Option<String>,
}

/// Widgets and live state of one window's Stress lab section.
pub struct StressPanel {
    cpu_button: ToggleButton,
    gpu_button: ToggleButton,
    duration_spin: SpinButton,
    guard_spin: SpinButton,
    /// One bar per stressor: hidden unless that stressor is running.
    cpu_progress: ProgressBar,
    gpu_progress: ProgressBar,
    /// The last result, shown inline under the buttons (ongoing state
    /// stays in the progress bars, not in a toast).
    status: Label,
    runs: RefCell<StressRuns>,
}

impl StressPanel {
    /// Ends the stressors (called when the window closes).
    pub fn stop_all(&self) {
        let runs = self.runs.borrow();
        for run in [runs.cpu.as_ref(), runs.gpu.as_ref()].into_iter().flatten() {
            run.stop.store(true, Ordering::Relaxed);
        }
    }

    fn start_cpu(&self, args: &cpu::CpuArgs) {
        let args = args.clone();
        let duration_secs = args.secs;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = std::thread::Builder::new()
            .name("stress-lab-cpu".to_string())
            .spawn(move || cpu::stress(&args, worker_stop, Arc::new(AtomicBool::new(false))))
            .expect("spawn the CPU stress thread");
        let mut runs = self.runs.borrow_mut();
        // A stale finished run (or a quickly re-toggled one) leaves here;
        // raise its stop flag so the old thread drains instead of piling on.
        if let Some(old) = runs.cpu.replace(StressRun {
            stop,
            started: Instant::now(),
            duration_secs,
            worker: Some(worker),
        }) {
            old.stop.store(true, Ordering::Relaxed);
        }
        runs.cpu_note = None;
    }

    fn start_gpu(&self, args: &gpu::GpuArgs) {
        let args = args.clone();
        let duration_secs = args.secs;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = std::thread::Builder::new()
            .name("stress-lab-gpu".to_string())
            .spawn(move || gpu::stress(&args, worker_stop, Arc::new(AtomicBool::new(false))))
            .expect("spawn the GPU stress thread");
        let mut runs = self.runs.borrow_mut();
        if let Some(old) = runs.gpu.replace(StressRun {
            stop,
            started: Instant::now(),
            duration_secs,
            worker: Some(worker),
        }) {
            old.stop.store(true, Ordering::Relaxed);
        }
        runs.gpu_note = None;
    }

    /// Manual-toggle arguments: durations and guard limit from the spins.
    fn manual_cpu_args(&self) -> cpu::CpuArgs {
        cpu::CpuArgs {
            secs: spin_seconds(&self.duration_spin),
            max_temp_c: self.guard_spin.value(),
            ..cpu::CpuArgs::default()
        }
    }

    fn manual_gpu_args(&self) -> gpu::GpuArgs {
        gpu::GpuArgs {
            secs: spin_seconds(&self.duration_spin),
            max_temp_c: self.guard_spin.value(),
            ..gpu::GpuArgs::default()
        }
    }

    /// Raises the stop flag of a live run; the status line updates on the
    /// next poll once the thread has actually wrapped up.
    fn stop_run(runs: &StressRuns, which: Which) {
        let run = match which {
            Which::Cpu => runs.cpu.as_ref(),
            Which::Gpu => runs.gpu.as_ref(),
        };
        if let Some(run) = run {
            run.stop.store(true, Ordering::Relaxed);
        }
    }

    /// Half-second refresh: collect finished runs into notes and keep live
    /// runs' progress on their bars.
    fn poll(&self) {
        let mut parts: Vec<String> = Vec::new();
        // Clearing a toggle re-enters the widget handlers, which borrow
        // `runs` themselves — so the borrow must be gone by then.
        let mut cpu_finished = false;
        let mut gpu_finished = false;
        {
            let runs = &mut *self.runs.borrow_mut();
            if collect(&mut runs.cpu, "CPU", &mut runs.cpu_note, &self.cpu_progress) {
                cpu_finished = true;
            }
            if collect(&mut runs.gpu, "GPU", &mut runs.gpu_note, &self.gpu_progress) {
                gpu_finished = true;
            }
            if let Some(note) = &runs.cpu_note {
                parts.push(note.clone());
            }
            if let Some(note) = &runs.gpu_note {
                parts.push(note.clone());
            }
        }
        if cpu_finished {
            self.cpu_button.set_active(false);
        }
        if gpu_finished {
            self.gpu_button.set_active(false);
        }
        self.status.set_text(&if parts.is_empty() {
            String::new()
        } else {
            parts.join("  ·  ")
        });
        self.status.set_visible(!parts.is_empty());
    }
}

/// Whole seconds from a spin, clamped well inside u64.
fn spin_seconds(spin: &SpinButton) -> u64 {
    #[expect(
        clippy::cast_sign_loss,
        reason = "every spin's minimum is a positive number"
    )]
    #[expect(
        clippy::cast_possible_truncation,
        reason = "whole seconds, far below 2^53"
    )]
    let seconds = spin.value().max(0.0) as u64;
    seconds
}

#[derive(Clone, Copy)]
enum Which {
    Cpu,
    Gpu,
}

/// Moves a finished run's outcome onto the status parts and hides its
/// bar; an unfinished run updates its bar with elapsed/duration instead.
/// Returns whether the run just finished (the caller then clears its
/// toggle, outside the `runs` borrow).
fn collect(
    run: &mut Option<StressRun>,
    kind: &str,
    note_slot: &mut Option<String>,
    progress: &ProgressBar,
) -> bool {
    let Some(current) = run.as_mut() else {
        progress.set_visible(false);
        return false;
    };
    let finished = current
        .worker
        .as_ref()
        .is_some_and(std::thread::JoinHandle::is_finished);
    if !finished {
        let elapsed = current.started.elapsed().as_secs();
        #[expect(
            clippy::cast_precision_loss,
            reason = "durations are minutes, not exponents"
        )]
        let fraction = (elapsed as f64 / current.duration_secs as f64).min(1.0);
        progress.set_fraction(fraction);
        progress.set_text(Some(&format!(
            "{kind} stress {elapsed} s of {} s",
            current.duration_secs
        )));
        progress.set_visible(true);
        return false;
    }
    progress.set_visible(false);
    let worker = current
        .worker
        .take()
        .expect("the finished run still owns its join handle");
    let outcome = worker.join().unwrap_or_else(|_| {
        Err(thermal::LabError::Failed(
            // A panic inside the stressor thread lands here instead of
            // taking the GUI down with it.
            "the stress thread panicked".to_string(),
        ))
    });
    *note_slot = Some(match outcome {
        Ok(report) if report.tripped => format!(
            "{kind} stress tripped the thermal guard after {:.0} s",
            report.elapsed_secs
        ),
        Ok(report) => format!("{kind} stress finished: {}", report.summary),
        Err(thermal::LabError::Thermal(message) | thermal::LabError::Failed(message)) => {
            format!("{kind} stress failed: {message}")
        }
    });
    *run = None;
    true
}

/// One stressor's progress bar: hidden until the run starts, with the
/// elapsed/duration countdown as its text.
fn stress_progress_bar(accessible: &str) -> ProgressBar {
    let bar = ProgressBar::builder()
        .show_text(true)
        .visible(false)
        .build();
    accessible_name(&bar, accessible);
    bar
}

/// Appends the "Stress lab" section (header, note, the stressor row,
/// per-stressor progress bars and a result line) to `panel`, wiring the
/// toggles and the half-second status tick.
///
/// The returned panel is needed to stop the stressors when the window
/// closes.
#[must_use]
pub fn append_stress_panel(panel: &GtkBox) -> Rc<StressPanel> {
    panel.append(&section_title("Stress lab"));
    let section = GtkBox::new(Orientation::Vertical, 8);
    section.add_css_class(SECTION_CLASS);
    section.append(
        &Label::builder()
            .label(
                "Heat up the machine and watch the fans respond; a thermal guard stops the \
                 load if things get too hot. Progress and results appear under the buttons.",
            )
            .wrap(true)
            .xalign(0.0)
            .build(),
    );

    let row = GtkBox::new(Orientation::Horizontal, 8);
    let cpu_button = ToggleButton::builder()
        .label("CPU load")
        .tooltip_text("Burn every core with AVX-512 hot instructions")
        .build();
    let gpu_button = ToggleButton::builder()
        .label("GPU load")
        .tooltip_text("Keep the graphics card busy with its Vulkan compute hot loop")
        .build();
    let duration_spin = SpinButton::builder()
        .digits(0)
        .adjustment(&gtk4::Adjustment::new(120.0, 5.0, 6000.0, 5.0, 30.0, 0.0))
        .build();
    let guard_spin = SpinButton::builder()
        .digits(0)
        .adjustment(&gtk4::Adjustment::new(88.0, 55.0, 95.0, 1.0, 5.0, 0.0))
        .build();
    // HIG: spin button numbers are right-justified.
    duration_spin.set_alignment(1.0);
    guard_spin.set_alignment(1.0);
    let duration_label = Label::builder()
        .label("_Stop after (s)")
        .use_underline(true)
        .mnemonic_widget(&duration_spin)
        .build();
    let guard_label = Label::builder()
        .label("_Guard at (°C)")
        .use_underline(true)
        .mnemonic_widget(&guard_spin)
        .build();
    accessible_name(&duration_spin, "Stop after seconds");
    accessible_name(&guard_spin, "Thermal guard temperature");
    row.append(&cpu_button);
    row.append(&gpu_button);
    row.append(&duration_label);
    row.append(&duration_spin);
    row.append(&guard_label);
    row.append(&guard_spin);

    let status = Label::builder()
        .xalign(0.0)
        .wrap(true)
        .visible(false)
        .build();
    // One bar per stressor, hidden until that stressor runs; the bar
    // text carries the elapsed/duration countdown.
    let cpu_progress = stress_progress_bar("CPU stress progress");
    let gpu_progress = stress_progress_bar("GPU stress progress");
    let stress_panel = Rc::new(StressPanel {
        cpu_button: cpu_button.clone(),
        gpu_button: gpu_button.clone(),
        duration_spin,
        guard_spin,
        cpu_progress: cpu_progress.clone(),
        gpu_progress: gpu_progress.clone(),
        status,
        runs: RefCell::new(StressRuns::default()),
    });

    // Toggling on starts the stressor (and replaces any previous run);
    // toggling off raises the stop flag.
    let cpu_panel = Rc::clone(&stress_panel);
    cpu_button.connect_toggled(move |button| {
        if button.is_active() {
            cpu_panel.start_cpu(&cpu_panel.manual_cpu_args());
        } else {
            StressPanel::stop_run(&cpu_panel.runs.borrow(), Which::Cpu);
        }
    });
    let gpu_panel = Rc::clone(&stress_panel);
    gpu_button.connect_toggled(move |button| {
        if button.is_active() {
            gpu_panel.start_gpu(&gpu_panel.manual_gpu_args());
        } else {
            StressPanel::stop_run(&gpu_panel.runs.borrow(), Which::Gpu);
        }
    });

    section.append(&row);
    section.append(&cpu_progress);
    section.append(&gpu_progress);
    section.append(&stress_panel.status);
    panel.append(&section);

    // Half-second status tick: elapsed time while a stressor runs, and the
    // result once its thread has wrapped up. Like the control-loop tick,
    // it lives as long as the main loop does.
    let poll_panel = Rc::clone(&stress_panel);
    glib::timeout_add_local(Duration::from_millis(500), move || {
        poll_panel.poll();
        ControlFlow::Continue
    });
    stress_panel
}
