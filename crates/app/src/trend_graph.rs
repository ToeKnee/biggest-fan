//! The trend graph beside a fan's curve graph: the fan's recent history.
//!
//! It plots the governing sensor's temperature and the fan's speed over
//! the last few minutes, so a speed change — and the temperature change
//! behind it — stays visible as a shape after the numbers have scrolled
//! past in the status line.

use crate::case_view::draw_centered;
use crate::theme::{GraphPalette, Rgba};
use biggest_fan_core as core;
use gtk4::DrawingArea;
use gtk4::prelude::*;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

/// How much history the graph keeps and draws, in seconds.
const HISTORY_WINDOW_S: f64 = 300.0;
/// Extra history kept past the left edge, so the drawn line runs into
/// the edge instead of starting short of it.
const HISTORY_MARGIN_S: f64 = 60.0;
/// Spacing of the time grid lines.
const GRID_STEP_S: f64 = 60.0;
/// Whole minutes across the drawn window: used for the "-5 min" corner
/// label and the grid lines (one per minute, the one at the far left
/// edge falling on the frame). Keep in step with [`HISTORY_WINDOW_S`].
const HISTORY_MINUTES: u32 = 5;
/// Hard sample cap: the fastest refresh the GUI offers (0.1 s) across
/// the whole kept window plus margin stays under this.
const MAX_SAMPLES: usize = 4000;

/// The shared vertical scale: temperatures (°C) and speed (%) both fit
/// a 0–100 axis, so the two series need no separate scales.
const SCALE_MAX: f64 = 100.0;

/// The speed series' dash pattern: the second channel that tells the
/// two series apart besides colour.
const SPEED_DASH: &[f64] = &[5.0, 3.0];

/// The plot rectangle inside the widget (everything outside is label
/// margins), shared shape with the curve graph's own copy.
#[derive(Clone, Copy, Debug)]
struct Plot {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

/// The plot rectangle, leaving room for the corner labels.
const fn plot_box(width: f64, height: f64) -> Plot {
    let left = 26.0;
    let right = 26.0;
    let top = 8.0;
    let bottom = 16.0;
    Plot {
        x: left,
        y: top,
        width: (width - left - right).max(1.0),
        height: (height - top - bottom).max(1.0),
    }
}

/// The x pixel for a sample time, with the right plot edge pinned to
/// `now_s` and the left edge at `now_s - HISTORY_WINDOW_S`. Times
/// outside the window clamp to the edges.
const fn x_for(plot: Plot, now_s: f64, elapsed_s: f64) -> f64 {
    let window_start = now_s - HISTORY_WINDOW_S;
    let fraction = ((elapsed_s - window_start) / HISTORY_WINDOW_S).clamp(0.0, 1.0);
    fraction.mul_add(plot.width, plot.x)
}

/// The y pixel for a scale value (0 at the bottom, 100 at the top).
const fn y_for(plot: Plot, value: f64) -> f64 {
    let fraction = 1.0 - value.clamp(0.0, SCALE_MAX) / SCALE_MAX;
    fraction.mul_add(plot.height, plot.y)
}

/// One control-loop tick of readings for one fan.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Sample {
    /// Seconds since the fan row's own clock started.
    elapsed_s: f64,
    /// Governing sensor temperature, if it read at all.
    temp_c: Option<f64>,
    /// Fan speed on the 0–100 scale (hardware-reported percent where
    /// available, else the commanded speed).
    percent: Option<f64>,
}

/// Rolling fan history, trimmed to the drawn window plus margin.
#[derive(Default)]
struct FanHistory {
    samples: VecDeque<Sample>,
}

impl FanHistory {
    fn push(&mut self, sample: Sample) {
        self.samples.push_back(sample);
        let cutoff = sample.elapsed_s - HISTORY_WINDOW_S - HISTORY_MARGIN_S;
        while self
            .samples
            .front()
            .is_some_and(|front| front.elapsed_s < cutoff)
        {
            self.samples.pop_front();
        }
        while self.samples.len() > MAX_SAMPLES {
            self.samples.pop_front();
        }
    }

    /// The time pinned to the right plot edge (the newest sample).
    fn now(&self) -> f64 {
        self.samples.back().map_or(0.0, |sample| sample.elapsed_s)
    }
}

/// Turns one series' samples into polyline segments (plot coordinates):
/// missing readings break the line, and the newest sample left of the
/// window becomes a bridge point on the left edge so the line runs in.
fn segments(
    samples: &VecDeque<Sample>,
    now_s: f64,
    plot: Plot,
    pick: fn(&Sample) -> Option<f64>,
) -> Vec<Vec<(f64, f64)>> {
    let window_start = now_s - HISTORY_WINDOW_S;
    let mut lines = Vec::new();
    let mut current: Vec<(f64, f64)> = Vec::new();
    let mut bridge: Option<(f64, f64)> = None;
    for sample in samples {
        let Some(value) = pick(sample) else {
            if current.len() > 1 {
                lines.push(std::mem::take(&mut current));
            }
            current.clear();
            bridge = None;
            continue;
        };
        if sample.elapsed_s < window_start {
            bridge = Some((plot.x, y_for(plot, value)));
            continue;
        }
        if let Some(point) = bridge.take() {
            current.push(point);
        }
        current.push((x_for(plot, now_s, sample.elapsed_s), y_for(plot, value)));
    }
    if current.len() > 1 {
        lines.push(current);
    }
    lines
}

/// Strokes every segment of one series. `dash` gives the series its
/// line style, so the two differ by more than colour (HIG accessibility).
fn draw_series(cr: &cairo::Context, lines: &[Vec<(f64, f64)>], colour: Rgba, dash: &[f64]) {
    cr.set_source_rgba(colour.0, colour.1, colour.2, colour.3);
    cr.set_line_width(1.4);
    cr.set_dash(dash, 0.0);
    for line in lines {
        if let (Some(&(first_x, first_y)), Some(_)) = (line.first(), line.get(1)) {
            cr.move_to(first_x, first_y);
            for &(x, y) in line.iter().skip(1) {
                cr.line_to(x, y);
            }
            let _ = cr.stroke();
        }
    }
    cr.set_dash(&[], 0.0);
}

/// Draws the graph: frame, minute grid, mid-line at 50, the two
/// histories (speed accent-blue dashed underneath, temperature
/// warning-amber solid on top, since the sensor trace is the one users
/// hunt for), the corner labels and a small style legend.
fn draw(
    cr: &cairo::Context,
    width: f64,
    height: f64,
    history: &FanHistory,
    palette: &GraphPalette,
) {
    let GraphPalette {
        background,
        text,
        curve,
        operating,
        relief: _,
    } = *palette;
    cr.set_source_rgba(background.0, background.1, background.2, background.3);
    let _ = cr.paint();

    let plot = plot_box(width, height);
    cr.set_line_width(1.0);
    cr.set_source_rgba(text.0, text.1, text.2, 0.5);
    cr.rectangle(plot.x, plot.y, plot.width, plot.height);
    let _ = cr.stroke();

    let now_s = history.now();

    // Grid: one vertical line per minute back from the right edge (the
    // line at the far left edge would sit on the frame), and a
    // horizontal line at the halfway scale point.
    cr.set_source_rgba(text.0, text.1, text.2, 0.2);
    for minute_back in 1..HISTORY_MINUTES {
        let grid_time = (-GRID_STEP_S).mul_add(f64::from(minute_back), now_s);
        let x = x_for(plot, now_s, grid_time);
        cr.move_to(x, plot.y);
        cr.line_to(x, plot.y + plot.height);
    }
    let middle = y_for(plot, SCALE_MAX / 2.0);
    cr.move_to(plot.x, middle);
    cr.line_to(plot.x + plot.width, middle);
    let _ = cr.stroke();

    // Corner labels: the time window below, the scale range at left
    // (one axis serves both °C and %).
    cr.select_font_face(
        "sans-serif",
        cairo::FontSlant::Normal,
        cairo::FontWeight::Normal,
    );
    cr.set_font_size(9.0);
    cr.set_source_rgba(text.0, text.1, text.2, text.3);
    let _ = draw_centered(
        cr,
        plot.x,
        plot.y + plot.height + 11.0,
        &format!("-{HISTORY_MINUTES} min"),
    );
    let _ = draw_centered(cr, plot.x + plot.width, plot.y + plot.height + 11.0, "now");
    let _ = draw_centered(cr, plot.x - 12.0, plot.y + 4.0, "100");
    let _ = draw_centered(cr, plot.x - 12.0, plot.y + plot.height + 4.0, "0");

    let samples = &history.samples;
    draw_series(
        cr,
        &segments(samples, now_s, plot, |sample| sample.percent),
        curve,
        SPEED_DASH,
    );
    draw_series(
        cr,
        &segments(samples, now_s, plot, |sample| sample.temp_c),
        operating,
        &[],
    );

    // Legend (top-right, over the traces): the line styles spelled out,
    // so the series are distinguishable without the colours.
    let legend_right = plot.x + plot.width;
    cr.set_line_width(1.4);
    cr.set_source_rgba(operating.0, operating.1, operating.2, operating.3);
    cr.move_to(legend_right - 34.0, plot.y + 7.0);
    cr.line_to(legend_right - 23.0, plot.y + 7.0);
    let _ = cr.stroke();
    cr.set_source_rgba(text.0, text.1, text.2, text.3);
    let _ = draw_centered(cr, legend_right - 14.0, plot.y + 10.0, "°C");
    cr.set_source_rgba(curve.0, curve.1, curve.2, curve.3);
    cr.set_dash(SPEED_DASH, 0.0);
    cr.move_to(legend_right - 34.0, plot.y + 17.0);
    cr.line_to(legend_right - 23.0, plot.y + 17.0);
    let _ = cr.stroke();
    cr.set_dash(&[], 0.0);
    cr.set_source_rgba(text.0, text.1, text.2, text.3);
    let _ = draw_centered(cr, legend_right - 14.0, plot.y + 20.0, "%");
}

/// The per-fan history graph. It records from each tick's `FanStatus`
/// and owns its rolling buffer, so it needs nothing from the engine.
pub struct TrendGraph {
    area: DrawingArea,
    history: Rc<RefCell<FanHistory>>,
}

impl TrendGraph {
    /// Builds an empty history graph; samples arrive through
    /// [`TrendGraph::record`] on every GUI refresh tick.
    #[must_use]
    pub fn new() -> Self {
        let area = DrawingArea::new();
        area.set_width_request(220);
        area.set_height_request(90);
        area.set_hexpand(true);
        area.set_tooltip_text(Some("Last 5 Minutes: Sensor °C Solid, Fan Speed % Dashed"));
        let history = Rc::new(RefCell::new(FanHistory::default()));

        let draw_history = Rc::clone(&history);
        area.set_draw_func(move |_area, cr, width, height| {
            draw(
                cr,
                f64::from(width),
                f64::from(height),
                &draw_history.borrow(),
                &GraphPalette::resolve(),
            );
        });
        Self { area, history }
    }

    /// The widget to add to the fan row.
    #[must_use]
    pub const fn widget(&self) -> &DrawingArea {
        &self.area
    }

    /// Appends this tick's readings, trimmed to the rolling window, and
    /// schedules a redraw. `elapsed_s` must keep increasing (it comes
    /// from one clock per fan row).
    pub fn record(&self, status: &core::FanStatus, elapsed_s: f64) {
        self.history
            .borrow_mut()
            .push(sample_for(status, elapsed_s));
        self.area.queue_draw();
    }
}

impl Default for TrendGraph {
    fn default() -> Self {
        Self::new()
    }
}

/// The history sample for one status line: the speed prefers what the
/// hardware reports (NVML percent, pwmN duty) and falls back to what
/// was commanded; both are fractions, scaled to the 0–100 axis.
fn sample_for(status: &core::FanStatus, elapsed_s: f64) -> Sample {
    let percent = status.measured_percent.unwrap_or(status.commanded_speed);
    Sample {
        elapsed_s,
        temp_c: status.governing_temp_c,
        percent: Some(percent * SCALE_MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn plot() -> Plot {
        plot_box(220.0, 90.0)
    }

    fn sample(elapsed_s: f64, temp_c: Option<f64>, percent: Option<f64>) -> Sample {
        Sample {
            elapsed_s,
            temp_c,
            percent,
        }
    }

    fn pick_percent(sample: &Sample) -> Option<f64> {
        sample.percent
    }

    #[test]
    fn axes_pin_now_to_the_right_edge_and_clamp() {
        let p = plot();
        let now = 1000.0;
        assert!((x_for(p, now, now) - (p.x + p.width)).abs() < 1e-9);
        assert!(
            (x_for(p, now, now - HISTORY_WINDOW_S) - p.x).abs() < 1e-9,
            "the left edge is one window back"
        );
        assert!((x_for(p, now, now + 500.0) - (p.x + p.width)).abs() < 1e-9);
        assert!((x_for(p, now, now - 900.0) - p.x).abs() < 1e-9);
        assert!((y_for(p, SCALE_MAX) - p.y).abs() < 1e-9);
        assert!((y_for(p, 0.0) - (p.y + p.height)).abs() < 1e-9);
        assert!((y_for(p, 50.0) - (p.y + p.height / 2.0)).abs() < 1e-9);
        assert!((y_for(p, 150.0) - p.y).abs() < 1e-9, "values clamp too");
    }

    #[test]
    fn history_trims_to_the_window_plus_margin() {
        let mut history = FanHistory::default();
        for step in 0..=10 {
            history.push(sample(f64::from(step) * 60.0, Some(40.0), Some(30.0)));
        }
        // Newest is 600 s; cutoff is 600 - 300 - 60 = 240 s.
        assert_eq!(history.now(), 600.0);
        let kept: Vec<f64> = history.samples.iter().map(|s| s.elapsed_s).collect();
        assert_eq!(kept.first().copied(), Some(240.0));
        assert_eq!(kept.len(), 7, "240..=600 in whole minutes");
    }

    #[test]
    fn history_caps_at_max_samples() {
        let mut history = FanHistory::default();
        // Fast ticks all inside the kept window: only the count cap can
        // trim here, and only after far more samples than the 0.1 s
        // refresh could ever produce.
        for step in 0..5000 {
            history.push(sample(f64::from(step) * 0.01, Some(40.0), Some(30.0)));
        }
        assert_eq!(history.samples.len(), MAX_SAMPLES);
    }

    #[test]
    fn missing_readings_break_the_line() {
        let p = plot();
        let samples = VecDeque::from([
            sample(280.0, Some(40.0), Some(20.0)),
            sample(290.0, Some(41.0), Some(22.0)),
            sample(295.0, None, Some(25.0)),
            sample(297.0, Some(42.0), Some(27.0)),
            sample(300.0, Some(43.0), Some(30.0)),
        ]);
        let temp_lines = segments(&samples, 300.0, p, |s| s.temp_c);
        assert_eq!(temp_lines.len(), 2, "the unread tick splits the trace");
        assert_eq!(temp_lines[0].len(), 2);
        assert_eq!(temp_lines[1].len(), 2);
        let speed_lines = segments(&samples, 300.0, p, pick_percent);
        assert_eq!(speed_lines.len(), 1);
        assert_eq!(speed_lines[0].len(), 5);
    }

    #[test]
    fn a_point_left_of_the_window_bridges_the_left_edge() {
        let p = plot();
        let samples = VecDeque::from([
            sample(0.0, Some(30.0), Some(10.0)), // 300 s before now: off the left edge
            sample(250.0, Some(40.0), Some(20.0)),
            sample(300.0, Some(50.0), Some(30.0)),
        ]);
        let lines = segments(&samples, 300.0, p, pick_percent);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].len(), 3, "the off-window point joins as a bridge");
        assert!(
            (lines[0][0].0 - p.x).abs() < 1e-9,
            "bridge sits on the edge"
        );
        assert!(
            (lines[0][1].0 - x_for(p, 300.0, 250.0)).abs() < 1e-9,
            "later points land on their own times"
        );
    }

    #[test]
    fn empty_history_draws_nothing_and_starts_at_zero() {
        let history = FanHistory::default();
        assert_eq!(history.now(), 0.0);
        let p = plot();
        let lines = segments(&history.samples, history.now(), p, pick_percent);
        let no_lines: [Vec<(f64, f64)>; 0] = [];
        assert_eq!(lines, no_lines, "no samples means no line segments");
    }

    #[test]
    fn samples_prefer_hardware_percent_and_scale_to_the_axis() {
        let status = core::FanStatus {
            id: core::FanId::new("nct6795-p1"),
            name: "CPU Fan".to_string(),
            mode: core::FanMode::Off,
            commanded_speed: 0.4,
            measured_percent: Some(0.6),
            measured_rpm: Some(800.0),
            governing_temp_c: Some(55.0),
            note: None,
        };
        let sample = sample_for(&status, 12.5);
        assert_eq!(sample.elapsed_s, 12.5);
        assert_eq!(sample.temp_c, Some(55.0));
        assert!((sample.percent.expect("percent") - 60.0).abs() < 1e-9);

        // No tach-style read-back (the engine gives one for every fan,
        // but be explicit): the commanded speed stands in.
        let blind = core::FanStatus {
            measured_percent: None,
            ..status
        };
        let sample = sample_for(&blind, 13.5);
        assert!((sample.percent.expect("percent") - 40.0).abs() < 1e-9);
    }
}
