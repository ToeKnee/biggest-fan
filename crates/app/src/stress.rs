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
//! guard trips, the thread wraps up, and the status bar at the bottom
//! of the window says which temperature hit the limit.

use biggest_fan_stress_lab::{cpu, gpu, thermal};
use gtk4::glib::{self, ControlFlow};
use gtk4::prelude::*;
use gtk4::{Box as GtkBox, Label, Orientation, SpinButton, ToggleButton};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// A live stress thread: the external stop flag, and the join handle
/// once it finishes so the result can be read back onto the status bar.
struct StressRun {
    stop: Arc<AtomicBool>,
    started: Instant,
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
    /// What this panel wants on the window's status bar right now (its
    /// own `poll` fills it in; the app's tick decides whether it gets
    /// shown — see [`StressPanel::status_line`]).
    status_line: RefCell<Option<String>>,
    runs: RefCell<StressRuns>,
}

impl StressPanel {
    /// The line this panel would like on the window's status bar: live
    /// stressor progress, or the last result — `None` while the lab has
    /// nothing to say, leaving the bar to the control loop and the
    /// service.
    #[must_use]
    pub fn status_line(&self) -> Option<String> {
        self.status_line.borrow().clone()
    }

    /// Ends the stressors (called when the window closes).
    pub fn stop_all(&self) {
        let runs = self.runs.borrow();
        for run in [runs.cpu.as_ref(), runs.gpu.as_ref()].into_iter().flatten() {
            run.stop.store(true, Ordering::Relaxed);
        }
    }

    fn start_cpu(&self, args: &cpu::CpuArgs) {
        let args = args.clone();
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
            worker: Some(worker),
        }) {
            old.stop.store(true, Ordering::Relaxed);
        }
        runs.cpu_note = None;
    }

    fn start_gpu(&self, args: &gpu::GpuArgs) {
        let args = args.clone();
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
    /// runs' elapsed time in the status line.
    fn poll(&self) {
        let mut parts: Vec<String> = Vec::new();
        // Clearing a toggle re-enters the widget handlers, which borrow
        // `runs` themselves — so the borrow must be gone by then.
        let mut cpu_finished = false;
        let mut gpu_finished = false;
        {
            let runs = &mut *self.runs.borrow_mut();
            if collect(&mut runs.cpu, "CPU", &mut runs.cpu_note, &mut parts) {
                cpu_finished = true;
            }
            if collect(&mut runs.gpu, "GPU", &mut runs.gpu_note, &mut parts) {
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
        // The bar is shared: with nothing to report the lab stays quiet
        // rather than hiding a fan note or a service answer.
        *self.status_line.borrow_mut() = if parts.is_empty() {
            None
        } else {
            Some(parts.join("  ·  "))
        };
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

/// Moves a finished run's outcome onto the status parts, clearing its
/// entry; an unfinished run contributes its elapsed time. Returns whether
/// the run just finished (the caller then clears its toggle, outside the
/// `runs` borrow).
fn collect(
    run: &mut Option<StressRun>,
    kind: &str,
    note_slot: &mut Option<String>,
    parts: &mut Vec<String>,
) -> bool {
    let Some(current) = run.as_mut() else {
        return false;
    };
    let finished = current
        .worker
        .as_ref()
        .is_some_and(std::thread::JoinHandle::is_finished);
    if !finished {
        let elapsed = current.started.elapsed().as_secs();
        parts.push(format!("{kind} stress running ({elapsed} s)"));
        return false;
    }
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

/// Appends the "Stress lab" section (header, note and the stressor row)
/// to `panel`, wiring the toggles and the half-second status tick.
///
/// The lab has no status label of its own: its progress and results go
/// to the window's status bar through [`StressPanel::status_line`].
#[must_use]
pub fn append_stress_panel(panel: &GtkBox) -> Rc<StressPanel> {
    panel.append(
        &Label::builder()
            .label("<b>Stress lab</b>")
            .use_markup(true)
            .xalign(0.0)
            .build(),
    );
    panel.append(
        &Label::builder()
            .label(
                "Heat up the machine and watch the fans respond; a thermal guard stops the \
                 load if things get too hot. Progress and results appear in the status bar \
                 at the bottom of the window.",
            )
            .wrap(true)
            .xalign(0.0)
            .build(),
    );

    let row = GtkBox::new(Orientation::Horizontal, 8);
    let cpu_button = ToggleButton::builder()
        .label("CPU load")
        .tooltip_text("burn every core with AVX-512 hot instructions")
        .build();
    let gpu_button = ToggleButton::builder()
        .label("GPU load")
        .tooltip_text("keep the graphics card busy with its Vulkan compute hot loop")
        .build();
    let duration_spin = SpinButton::builder()
        .digits(0)
        .adjustment(&gtk4::Adjustment::new(120.0, 5.0, 6000.0, 5.0, 30.0, 0.0))
        .build();
    let guard_spin = SpinButton::builder()
        .digits(0)
        .adjustment(&gtk4::Adjustment::new(88.0, 55.0, 95.0, 1.0, 5.0, 0.0))
        .build();
    row.append(&cpu_button);
    row.append(&gpu_button);
    row.append(&Label::builder().label("stop after (s):").build());
    row.append(&duration_spin);
    row.append(&Label::builder().label("guard at (°C):").build());
    row.append(&guard_spin);

    let stress_panel = Rc::new(StressPanel {
        cpu_button: cpu_button.clone(),
        gpu_button: gpu_button.clone(),
        duration_spin,
        guard_spin,
        status_line: RefCell::new(None),
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

    panel.append(&row);

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
