//! A per-fan curve graph: fan speed (vertical) against sensor
//! temperature (horizontal), with a marker at the fan's current
//! operating point.
//!
//! The marker is the governing sensor's temperature and the speed being
//! commanded. A dot away from the line shows a ramp or hold band at
//! work; a dot on the line means the fan is exactly where its curve
//! asks.
//!
//! All fans share the same temperature domain, so their curves can be
//! compared straight down the list.

use crate::case_view::draw_centered;
use biggest_fan_core as core;
use gtk4::DrawingArea;
use gtk4::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

/// Hottest and coldest temperatures the graph covers, in °C. Wide enough
/// for every preset (Quiet tops out at 95 °C) and fixed so the graphs in
/// different rows can be compared.
const TEMP_MIN_C: f64 = 20.0;
const TEMP_MAX_C: f64 = 100.0;
const TEMP_SPAN_C: f64 = TEMP_MAX_C - TEMP_MIN_C;

/// Where the fan is right now: the governing sensor's temperature and
/// the speed fraction currently commanded.
type OperatingPoint = (f64, f64);

/// The plot rectangle inside the widget (everything outside is label
/// margins).
#[derive(Clone, Copy, Debug)]
struct Plot {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

/// The x pixel for a temperature (temperatures outside the domain
/// clamp to the edges, so a marker never leaves the plot).
const fn x_for(plot: Plot, temp_c: f64) -> f64 {
    let fraction = (temp_c.clamp(TEMP_MIN_C, TEMP_MAX_C) - TEMP_MIN_C) / TEMP_SPAN_C;
    fraction.mul_add(plot.width, plot.x)
}

/// The y pixel for a speed fraction (0.0 at the bottom, 1.0 at the top).
const fn y_for(plot: Plot, speed: f64) -> f64 {
    (-speed).mul_add(plot.height, plot.y + plot.height)
}

/// The temperature at an x pixel (the inverse of [`x_for`], for
/// sampling the curve across the plot).
const fn temp_at(plot: Plot, px: f64) -> f64 {
    ((px - plot.x) / plot.width).mul_add(TEMP_SPAN_C, TEMP_MIN_C)
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

/// The curve sampled one point per pixel column, as plot coordinates.
fn curve_points(curve: &core::FanCurve, plot: Plot) -> Vec<(f64, f64)> {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the width is a small positive pixel count"
    )]
    let columns = (plot.width as usize).clamp(2, 2000);
    (0..=columns)
        .map(|column| {
            #[expect(
                clippy::cast_precision_loss,
                reason = "column counts are far below f64's exact-integer range"
            )]
            let px = (column as f64 / columns as f64).mul_add(plot.width, plot.x);
            (px, y_for(plot, curve.speed_at(temp_at(plot, px))))
        })
        .collect()
}

/// Draws the graph: frame, grid, the curve with a filled area under it,
/// and the live operating point as a dashed temperature line plus a dot.
fn draw(
    cr: &cairo::Context,
    width: f64,
    height: f64,
    curve: Option<&core::FanCurve>,
    operating: Option<OperatingPoint>,
) {
    cr.set_source_rgb(0.13, 0.13, 0.15);
    let _ = cr.paint();

    let plot = plot_box(width, height);
    cr.set_line_width(1.0);
    cr.set_source_rgba(0.55, 0.55, 0.58, 0.5);
    cr.rectangle(plot.x, plot.y, plot.width, plot.height);
    let _ = cr.stroke();

    // Grid at the round temperatures; the frame already marks the edges.
    cr.set_source_rgba(0.55, 0.55, 0.58, 0.2);
    for temp_c in [40.0, 60.0, 80.0] {
        let x = x_for(plot, temp_c);
        cr.move_to(x, plot.y);
        cr.line_to(x, plot.y + plot.height);
    }
    let _ = cr.stroke();

    // Corner labels: the domain ends and the speed range.
    cr.select_font_face(
        "sans-serif",
        cairo::FontSlant::Normal,
        cairo::FontWeight::Normal,
    );
    cr.set_font_size(9.0);
    cr.set_source_rgb(0.85, 0.85, 0.88);
    let _ = draw_centered(
        cr,
        plot.x,
        plot.y + plot.height + 11.0,
        &format!("{TEMP_MIN_C:.0} °C"),
    );
    let _ = draw_centered(
        cr,
        plot.x + plot.width,
        plot.y + plot.height + 11.0,
        &format!("{TEMP_MAX_C:.0} °C"),
    );
    let _ = draw_centered(cr, plot.x - 12.0, plot.y + 4.0, "100%");
    let _ = draw_centered(cr, plot.x - 12.0, plot.y + plot.height + 4.0, "0%");

    let Some(curve) = curve else {
        return;
    };

    let points = curve_points(curve, plot);
    if let (Some(&(first_x, first_y)), Some(&(last_x, _))) = (points.first(), points.last()) {
        cr.move_to(first_x, first_y);
        for &(x, y) in points.iter().skip(1) {
            cr.line_to(x, y);
        }
        cr.set_source_rgb(0.20, 0.50, 0.90);
        cr.set_line_width(1.6);
        let _ = cr.stroke_preserve();
        // Close down to the plot floor and fill the area under the curve.
        let floor = plot.y + plot.height;
        cr.line_to(last_x, floor);
        cr.line_to(first_x, floor);
        cr.close_path();
        cr.set_source_rgba(0.20, 0.50, 0.90, 0.12);
        let _ = cr.fill();
    }

    if let Some((temp_c, commanded)) = operating {
        let x = x_for(plot, temp_c);
        cr.set_source_rgba(0.95, 0.75, 0.20, 0.45);
        cr.set_line_width(1.0);
        cr.set_dash(&[3.0, 3.0], 0.0);
        cr.move_to(x, plot.y);
        cr.line_to(x, plot.y + plot.height);
        let _ = cr.stroke();
        cr.set_dash(&[], 0.0);
        cr.set_source_rgb(0.95, 0.75, 0.20);
        cr.arc(x, y_for(plot, commanded), 3.0, 0.0, std::f64::consts::TAU);
        let _ = cr.fill();
    }
}

/// The per-fan curve graph widget. Its draw closure reads the fan's
/// curve straight from the engine, so a preset change needs only a
/// `queue_draw` to appear.
pub struct CurveGraph {
    area: DrawingArea,
    operating: Rc<RefCell<Option<OperatingPoint>>>,
}

impl CurveGraph {
    /// Builds the graph for one fan group; `engine_rc` is consulted on
    /// every draw (missing or non-auto fans draw the empty frame only).
    #[must_use]
    pub fn new(engine_rc: &Rc<RefCell<core::Engine>>, fan_id: &core::FanId) -> Self {
        let area = DrawingArea::new();
        area.set_width_request(260);
        area.set_height_request(90);
        area.set_hexpand(true);
        area.set_tooltip_text(Some(
            "this fan's curve: speed against sensor temperature; the dot is where the fan \
             is now",
        ));
        let operating = Rc::new(RefCell::new(None));

        let draw_engine = Rc::clone(engine_rc);
        let draw_fan_id = fan_id.clone();
        let draw_operating = Rc::clone(&operating);
        area.set_draw_func(move |_area, cr, width, height| {
            // Take a copy of what is needed and release the engine
            // before drawing (same care as the case canvas).
            let curve = draw_engine.try_borrow().ok().and_then(|engine| {
                match engine.fan_mode(&draw_fan_id) {
                    Some(core::FanMode::Auto { curve, .. }) => Some(curve),
                    _ => None,
                }
            });
            draw(
                cr,
                f64::from(width),
                f64::from(height),
                curve.as_ref(),
                *draw_operating.borrow(),
            );
        });
        Self { area, operating }
    }

    /// The widget to add to the fan row.
    #[must_use]
    pub const fn widget(&self) -> &DrawingArea {
        &self.area
    }

    /// Moves the live marker to the fan's current governing temperature
    /// (if the sensor reads at all) and commanded speed, and schedules a
    /// redraw.
    pub fn set_operating_point(&self, temp_c: Option<f64>, commanded_speed: f64) {
        *self.operating.borrow_mut() = temp_c.map(|temp| (temp, commanded_speed));
        self.area.queue_draw();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use biggest_fan_core::{CurvePoint, FanCurve};

    fn plot() -> Plot {
        plot_box(260.0, 90.0)
    }

    fn ramp_curve() -> FanCurve {
        FanCurve::new(vec![
            CurvePoint {
                temp_c: 40.0,
                speed: 0.2,
            },
            CurvePoint {
                temp_c: 80.0,
                speed: 1.0,
            },
        ])
        .expect("curve")
    }

    #[test]
    fn axes_map_domain_ends_to_plot_edges() {
        let p = plot();
        assert!((x_for(p, TEMP_MIN_C) - p.x).abs() < 1e-9);
        assert!((x_for(p, TEMP_MAX_C) - (p.x + p.width)).abs() < 1e-9);
        assert!((y_for(p, 1.0) - p.y).abs() < 1e-9);
        assert!((y_for(p, 0.0) - (p.y + p.height)).abs() < 1e-9);
    }

    #[test]
    fn x_and_temp_are_inverses() {
        let p = plot();
        for temp_c in [25.0, 47.5, 60.0, 99.0] {
            assert!((temp_at(p, x_for(p, temp_c)) - temp_c).abs() < 1e-9);
        }
    }

    #[test]
    fn markers_clamp_to_the_plot_edges() {
        let p = plot();
        assert!((x_for(p, -50.0) - p.x).abs() < 1e-9);
        assert!((x_for(p, 500.0) - (p.x + p.width)).abs() < 1e-9);
    }

    #[test]
    fn curve_points_span_the_plot_and_follow_the_curve() {
        let p = plot();
        let points = curve_points(&ramp_curve(), p);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the plot width is a small positive pixel count"
        )]
        let columns = p.width as usize;
        assert_eq!(
            points.len(),
            columns + 1,
            "one point per pixel column plus the closing edge"
        );
        assert!((points[0].0 - p.x).abs() < 1e-6);
        assert!((points.last().expect("last").0 - (p.x + p.width)).abs() < 1e-6);
        // Below the curve's first point the speed is flat at 0.2; at
        // 60 °C the ramp is at its midpoint 0.6.
        assert!((points[0].1 - y_for(p, 0.2)).abs() < 1e-6);
        let middle = points
            .iter()
            .find(|(px, _)| (px - x_for(p, 60.0)).abs() < 1.0)
            .expect("the 60 °C column");
        assert!((middle.1 - y_for(p, 0.6)).abs() < 0.01);
        assert!((points.last().expect("last").1 - y_for(p, 1.0)).abs() < 1e-6);
    }

    #[test]
    fn a_tiny_widget_still_gets_a_usable_plot() {
        let p = plot_box(10.0, 5.0);
        assert!(p.width >= 1.0 && p.height >= 1.0);
    }
}
