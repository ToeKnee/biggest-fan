//! A sparkline: the tiny inline history chart on each sensor tile —
//! that temperature's last few minutes as one line, so a rising or
//! spiking sensor is visible at a glance without opening a graph.
//!
//! Unlike the fan graphs the sparkline has no axes, labels or frame:
//! the tile's name and live reading carry the numbers, and the line
//! only shows the shape of the recent history.

use crate::theme::{GraphPalette, Rgba};
use gtk4::DrawingArea;
use gtk4::prelude::*;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Instant;

/// How much history a sparkline keeps and draws, in seconds — the same
/// window as the fan trend graphs, so both answer "the last 5 minutes".
const SPARK_WINDOW_S: f64 = 300.0;
/// Hard sample cap: the fastest refresh the GUI offers (0.1 s) across
/// the window stays under this.
const MAX_SAMPLES: usize = 4000;

/// One sensor's rolling history: (elapsed seconds, temperature °C).
#[derive(Default)]
struct History {
    samples: VecDeque<(f64, f64)>,
}

impl History {
    /// Appends a reading and trims to the window (and the sample cap).
    fn push(&mut self, elapsed_s: f64, temp_c: f64) {
        self.samples.push_back((elapsed_s, temp_c));
        let cutoff = elapsed_s - SPARK_WINDOW_S;
        while self.samples.front().is_some_and(|&(t, _)| t < cutoff) {
            self.samples.pop_front();
        }
        while self.samples.len() > MAX_SAMPLES {
            self.samples.pop_front();
        }
    }
}

/// Draws the line: newest sample pinned to the right edge, the vertical
/// scale fitted to the window's own min/max (a flat history draws a
/// level line through the middle). The tile's own background shows
/// through — nothing is painted behind the line.
fn draw(
    cr: &cairo::Context,
    width: f64,
    height: f64,
    samples: &VecDeque<(f64, f64)>,
    colour: Rgba,
) {
    let Some(&(newest_t, _)) = samples.back() else {
        return;
    };
    let (mut lo, mut hi) = (f64::MAX, f64::MIN);
    for &(_, temp_c) in samples {
        lo = lo.min(temp_c);
        hi = hi.max(temp_c);
    }
    // A little padding so the line never rides the tile's edge.
    let (top, bottom) = (1.5, height - 1.5);
    let flat = hi - lo < 1e-9;
    let left = newest_t - SPARK_WINDOW_S;
    cr.set_line_width(1.5);
    cr.set_source_rgba(colour.0, colour.1, colour.2, colour.3);
    let mut started = false;
    let mut last = (0.0, 0.0);
    let mut visible = 0;
    for &(elapsed_s, temp_c) in samples {
        if elapsed_s < left {
            continue;
        }
        let x = (elapsed_s - left) / SPARK_WINDOW_S * width;
        let y = if flat {
            f64::midpoint(top, bottom)
        } else {
            // lo maps to `bottom`, hi maps to `top`.
            f64::mul_add((temp_c - lo) / (hi - lo), top - bottom, bottom)
        };
        if started {
            cr.line_to(x, y);
        } else {
            cr.move_to(x, y);
            started = true;
        }
        last = (x, y);
        visible += 1;
    }
    if visible == 1 {
        // A lone reading: a dot, so the sparkline is never blank.
        cr.arc(last.0, last.1, 1.5, 0.0, std::f64::consts::TAU);
        let _ = cr.fill();
    } else if started {
        let _ = cr.stroke();
    }
}

/// The sparkline widget for one sensor tile. Samples arrive through
/// [`Sparkline::record`] on every GUI refresh tick; the widget needs
/// nothing from the engine.
pub struct Sparkline {
    area: DrawingArea,
    history: Rc<RefCell<History>>,
    clock: Instant,
}

impl Sparkline {
    /// Builds an empty sparkline; the line appears as readings arrive.
    #[must_use]
    pub fn new() -> Self {
        let area = DrawingArea::new();
        area.set_width_request(64);
        area.set_height_request(16);
        area.set_tooltip_text(Some("Last 5 minutes"));
        let history = Rc::new(RefCell::new(History::default()));

        let draw_history = Rc::clone(&history);
        area.set_draw_func(move |_area, cr, width, height| {
            // Amber, the temperature colour the trend graphs already use.
            draw(
                cr,
                f64::from(width),
                f64::from(height),
                &draw_history.borrow().samples,
                GraphPalette::resolve().operating,
            );
        });
        Self {
            area,
            history,
            clock: Instant::now(),
        }
    }

    /// The widget to add to the sensor tile.
    #[must_use]
    pub const fn widget(&self) -> &DrawingArea {
        &self.area
    }

    /// Appends one reading, trimmed to the rolling window, and schedules
    /// a redraw. Unread ticks simply do not arrive, so the line bridges
    /// the gap.
    pub fn record(&self, temp_c: f64) {
        self.history
            .borrow_mut()
            .push(self.clock.elapsed().as_secs_f64(), temp_c);
        self.area.queue_draw();
    }
}

impl Default for Sparkline {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Paints `samples` onto a transparent surface and returns the
    /// painted pixel coordinates.
    fn painted_pixels(samples: &[(f64, f64)]) -> Vec<(u32, u32)> {
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 64, 16).expect("surface");
        {
            let cr = cairo::Context::new(&surface).expect("cairo context");
            let samples = VecDeque::from_iter(samples.iter().copied());
            draw(&cr, 64.0, 16.0, &samples, (1.0, 0.0, 0.0, 1.0));
        }
        let stride = usize::try_from(surface.stride()).expect("a positive stride");
        let mut pixels = Vec::new();
        surface
            .with_data(|data| {
                for (y, row) in data.chunks(stride.max(1)).enumerate() {
                    for (x, pixel) in row.as_chunks::<4>().0.iter().enumerate() {
                        // Premultiplied BGRA; alpha is the last byte.
                        if pixel[3] > 0 {
                            let (Ok(x), Ok(y)) = (u32::try_from(x), u32::try_from(y)) else {
                                continue;
                            };
                            pixels.push((x, y));
                        }
                    }
                }
            })
            .expect("a freshly drawn image surface still has its pixels");
        pixels
    }

    #[test]
    fn history_trims_to_the_window() {
        let mut history = History::default();
        for step in 0..=10 {
            history.push(f64::from(step) * 60.0, 40.0);
        }
        // Newest is 600 s; cutoff is 600 - 300 = 300 s.
        assert_eq!(history.samples.front().copied(), Some((300.0, 40.0)));
        assert_eq!(history.samples.back().copied(), Some((600.0, 40.0)));
        assert_eq!(history.samples.len(), 6, "300..=600 in whole minutes");
    }

    #[test]
    fn history_caps_at_max_samples() {
        let mut history = History::default();
        for step in 0..5000 {
            history.push(f64::from(step) * 0.01, 40.0);
        }
        assert_eq!(history.samples.len(), MAX_SAMPLES);
    }

    #[test]
    fn empty_history_draws_nothing() {
        assert_eq!(painted_pixels(&[]), [] as [(u32, u32); 0]);
    }

    #[test]
    fn flat_history_draws_a_level_line() {
        let samples = [(0.0, 40.0), (100.0, 40.0), (300.0, 40.0)];
        let pixels = painted_pixels(&samples);
        assert!(!pixels.is_empty(), "a flat history still draws");
        let mid = 8.0; // (1.5 + 14.5) / 2
        assert!(
            pixels
                .iter()
                .all(|&(_, y)| (f64::from(y) - mid).abs() <= 2.0),
            "the level line sits mid-height: {pixels:?}"
        );
    }

    #[test]
    fn rising_history_slopes_up_to_the_right() {
        // Cairo's y grows downwards, so a rising temperature means the
        // line's y decreases towards the right edge.
        let samples = [(0.0, 30.0), (150.0, 45.0), (300.0, 60.0)];
        let pixels = painted_pixels(&samples);
        let left_y = pixels
            .iter()
            .filter(|&&(x, _)| x < 4)
            .map(|&(_, y)| y)
            .max()
            .expect("painted at the left");
        let right_y = pixels
            .iter()
            .filter(|&&(x, _)| x > 60)
            .map(|&(_, y)| y)
            .min()
            .expect("painted at the right");
        assert!(
            right_y < left_y,
            "the line climbs: right y {right_y}, left y {left_y}"
        );
    }

    #[test]
    fn samples_outside_the_window_are_skipped() {
        // Only the newest point is inside the window (left edge is 100 s
        // before it), and a lone point draws as a dot at the right edge.
        let samples = [(0.0, 90.0), (50.0, 40.0), (400.0, 41.0)];
        let pixels = painted_pixels(&samples);
        assert!(!pixels.is_empty(), "the lone in-window point draws");
        assert!(
            pixels.iter().all(|&(x, _)| x >= 60),
            "off-window points never paint: {pixels:?}"
        );
    }
}
