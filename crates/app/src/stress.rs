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
//! guard trips, the thread wraps up, and the status line says which
//! temperature hit the limit.
//!
//! "Run full test" starts the auto-tune (see [`crate::tune`]): warm-up
//! under full stress, then each fan's speed is searched downward until a
//! temperature climbs, and the slowest curves each fan can hold are
//! suggested (kept only if *Save config* is pressed afterwards).

use crate::dbus::{self, FanRequest};
use crate::tune;
use biggest_fan_core as core;
use biggest_fan_stress_lab::{cpu, gpu, thermal};
use gtk4::glib::{self, ControlFlow};
use gtk4::prelude::*;
use gtk4::{Box as GtkBox, Button, Label, Orientation, SpinButton, ToggleButton};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// A tune keeps the stressors alive as long as a day; the stop flag
/// normally ends them long before that.
const TUNE_STRESS_SECS: u64 = 24 * 60 * 60;

/// One live stress thread: the external stop flag, and the join handle
/// once it finishes so the result can be read back onto the status line.
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
    tune_button: Button,
    warmup_spin: SpinButton,
    restore_button: Button,
    status: Label,
    engine: Rc<RefCell<core::Engine>>,
    client: dbus::Client,
    /// Live sensor temperatures, refreshed by the control-loop tick.
    temps: Rc<RefCell<BTreeMap<core::SensorId, f64>>>,
    runs: RefCell<StressRuns>,
    tune: RefCell<Option<tune::Tune>>,
    /// The last tune report or restore confirmation.
    tune_note: RefCell<Option<String>>,
}

impl StressPanel {
    /// Ends any live tune and stressors (called when the window closes).
    pub fn stop_all(&self) {
        self.stop_tune();
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

    fn start_tune(&self) {
        let config = self.engine.borrow().current_config();
        let warmup = Duration::from_secs(spin_seconds(&self.warmup_spin) * 60);
        let tuner = tune::Tune::new(&config, warmup, Instant::now());
        // Detect first so a connected-but-hidden header joins the lineup,
        // then pin every candidate fan at full speed for the warm-up.
        self.client.send(FanRequest::Detect);
        for request in tuner.warmup_requests() {
            self.client.send(request);
        }
        let guard = self.guard_spin.value();
        self.start_cpu(&cpu::CpuArgs {
            secs: TUNE_STRESS_SECS,
            max_temp_c: guard,
            ..cpu::CpuArgs::default()
        });
        self.start_gpu(&gpu::GpuArgs {
            secs: TUNE_STRESS_SECS,
            max_temp_c: guard,
            ..gpu::GpuArgs::default()
        });
        self.tune_button.set_label("Stop tune");
        self.set_manual_sensitive(false);
        self.tune_note.borrow_mut().take();
        *self.tune.borrow_mut() = Some(tuner);
    }

    /// Stops a live tune (restoring the pre-tune modes) and its
    /// stressors; a no-op on widgets and state when nothing is tuning.
    fn stop_tune(&self) {
        if let Some(mut tuner) = self.tune.borrow_mut().take() {
            tuner.abort(&mut |request| {
                self.client.send(request);
            });
            *self.tune_note.borrow_mut() = Some(tuner.report());
            Self::stop_run(&self.runs.borrow(), Which::Cpu);
            Self::stop_run(&self.runs.borrow(), Which::Gpu);
        }
        self.tune_button.set_label("Run full test");
        self.set_manual_sensitive(true);
    }

    /// Hands every visible fan the mode from the saved config file —
    /// what a daemon restart would apply.
    fn restore_saved(&self) {
        let path = self.engine.borrow().config_path().to_path_buf();
        let outcome = std::fs::read_to_string(&path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))
            .and_then(|text| core::config::from_text(&text).map_err(|error| error.to_string()));
        match outcome {
            Ok(config) => {
                let mut restored = 0;
                for fan in config.fans.iter().filter(|fan| !fan.hidden) {
                    if let Ok(mode_json) = serde_json::to_string(&fan.mode) {
                        self.client.send(FanRequest::SetFanMode {
                            fan: fan.id.as_str().to_string(),
                            mode_json,
                        });
                        restored += 1;
                    }
                }
                *self.tune_note.borrow_mut() = Some(format!(
                    "restored {restored} fan modes from the saved config"
                ));
            }
            Err(problem) => {
                *self.tune_note.borrow_mut() = Some(format!("could not restore: {problem}"));
            }
        }
    }

    fn set_manual_sensitive(&self, enabled: bool) {
        self.cpu_button.set_sensitive(enabled);
        self.gpu_button.set_sensitive(enabled);
        self.duration_spin.set_sensitive(enabled);
        self.guard_spin.set_sensitive(enabled);
        self.warmup_spin.set_sensitive(enabled);
        self.restore_button.set_sensitive(enabled);
    }

    /// Half-second refresh: collect finished runs into notes, keep live
    /// runs' elapsed time on the label, and step a live tune.
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
        self.step_tune(&mut parts);
        if let Some(note) = self.tune_note.borrow().as_ref() {
            parts.push(note.clone());
        }
        if parts.is_empty() {
            self.status
                .set_text("Heat up the machine and watch the fans respond; a thermal guard stops the load if things get too hot.");
        } else {
            self.status.set_text(&parts.join("  ·  "));
        }
    }

    /// Advances a live tune; aborts it if its stressors died on their own
    /// (a thermal-guard trip, most likely).
    fn step_tune(&self, parts: &mut Vec<String>) {
        let mut slot = self.tune.borrow_mut();
        let Some(tuner) = slot.as_mut() else { return };
        let alive = {
            let runs = self.runs.borrow();
            let live = |run: &Option<StressRun>| {
                run.as_ref()
                    .is_some_and(|run| run.worker.as_ref().is_some_and(|w| !w.is_finished()))
            };
            live(&runs.cpu) && live(&runs.gpu)
        };
        if !alive {
            tuner.abort(&mut |request| {
                self.client.send(request);
            });
        }
        tuner.step(Instant::now(), &self.temps.borrow(), &mut |request| {
            self.client.send(request);
        });
        parts.push(tuner.status(Instant::now()));
        if tuner.finished() {
            *self.tune_note.borrow_mut() = Some(tuner.report());
            self.tune_button.set_label("Run full test");
            self.set_manual_sensitive(true);
            slot.take();
        }
    }
}

/// The auto-tune row: its buttons and warm-up spin, ready to append.
struct TuneRow {
    row: GtkBox,
    tune_button: Button,
    warmup_spin: SpinButton,
    restore_button: Button,
}

impl TuneRow {
    #[must_use]
    fn new() -> Self {
        let row = GtkBox::new(Orientation::Horizontal, 8);
        let tune_button = Button::builder()
            .label("Run full test")
            .tooltip_text(
                "warm up under full stress, then search each fan downward until a \
                 temperature climbs, and suggest the slowest curves each fan can hold",
            )
            .build();
        let warmup_spin = SpinButton::builder()
            .digits(0)
            .adjustment(&gtk4::Adjustment::new(10.0, 2.0, 120.0, 1.0, 5.0, 0.0))
            .build();
        let restore_button = Button::builder()
            .label("Restore saved modes")
            .tooltip_text("put every fan back in the mode its saved config says")
            .build();
        row.append(&tune_button);
        row.append(&Label::builder().label("warm-up (min):").build());
        row.append(&warmup_spin);
        row.append(&restore_button);
        Self {
            row,
            tune_button,
            warmup_spin,
            restore_button,
        }
    }

    fn append_to(&self, panel: &GtkBox) {
        panel.append(&self.row);
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

/// Appends the "Stress lab" section (header, note, controls, full-test
/// row and status line) to `panel`, wiring the toggles, the tune button
/// and the half-second status tick.
#[must_use]
pub fn append_stress_panel(
    panel: &GtkBox,
    engine_rc: &Rc<RefCell<core::Engine>>,
    client: &dbus::Client,
    temps: Rc<RefCell<BTreeMap<core::SensorId, f64>>>,
) -> Rc<StressPanel> {
    panel.append(
        &Label::builder()
            .label("<b>Stress lab</b>")
            .use_markup(true)
            .xalign(0.0)
            .build(),
    );
    let status = Label::builder()
        .label("Heat up the machine and watch the fans respond; a thermal guard stops the load if things get too hot.")
        .wrap(true)
        .xalign(0.0)
        .build();

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

    // The auto-tune row: warm-up minutes is how long the machine soaks
    // at full speed before the sweep starts — air-cooled machines get by
    // with a few, AIO and custom loops want the longer ends.
    let tune_row = TuneRow::new();
    tune_row.append_to(panel);

    let stress_panel = Rc::new(StressPanel {
        cpu_button: cpu_button.clone(),
        gpu_button: gpu_button.clone(),
        duration_spin,
        guard_spin,
        tune_button: tune_row.tune_button.clone(),
        warmup_spin: tune_row.warmup_spin.clone(),
        restore_button: tune_row.restore_button.clone(),
        status: status.clone(),
        engine: Rc::clone(engine_rc),
        client: client.clone(),
        temps,
        runs: RefCell::new(StressRuns::default()),
        tune: RefCell::new(None),
        tune_note: RefCell::new(None),
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

    // "Run full test" starts the auto-tune and becomes "Stop tune".
    let tune_panel = Rc::clone(&stress_panel);
    tune_row.tune_button.connect_clicked(move |_| {
        if tune_panel.tune.borrow().is_some() {
            tune_panel.stop_tune();
        } else {
            tune_panel.start_tune();
        }
    });
    let restore_panel = Rc::clone(&stress_panel);
    tune_row
        .restore_button
        .connect_clicked(move |_| restore_panel.restore_saved());

    panel.append(&row);
    panel.append(&status);

    // Half-second status tick: elapsed time while running, results once
    // a thread has wrapped up, and the live tune's state machine. Like
    // the control-loop tick, it lives as long as the main loop does.
    let poll_panel = Rc::clone(&stress_panel);
    glib::timeout_add_local(Duration::from_millis(500), move || {
        poll_panel.poll();
        ControlFlow::Continue
    });
    stress_panel
}
