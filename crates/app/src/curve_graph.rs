//! A per-fan curve graph: fan speed (vertical) against sensor
//! temperature (horizontal), with a marker at where the fan is now and
//! the curve's own points as handles you can edit straight on it.
//!
//! The marker is the governing sensor's temperature and the speed being
//! commanded. A dot away from the line shows a ramp or hold band at
//! work; a dot on the line means the fan is exactly where its curve
//! asks.
//!
//! Editing: **drag a handle** to move it (it snaps to whole degrees and
//! whole percents, and can never cross its neighbours), **double-click**
//! an empty spot to add a point, **right-click a handle** to delete one.
//! The same edits have a keyboard path: Tab to the graph, **Left/Right**
//! pick a handle, **Up/Down** nudge its speed, **Shift+Left/Right** its
//! temperature, **Plus** adds a point and **Minus** removes one.
//! Hovering a handle prints its speed and temperature beside it — on the
//! plot rather than in a GTK tooltip, which would cover the curve.
//! A drag is a draft: it is drawn in place of the fan's real curve and
//! only becomes the fan's mode — here and on the service — when the
//! pointer is released, so a half-dragged curve never drives a fan.
//!
//! All fans share the same temperature domain, so their curves can be
//! compared straight down the list.
//!
//! While GPU throttle relief lifts a cooler above its saved curve, the
//! curve actually running is drawn over the saved one in hot orange, and
//! a "Keep this curve" button offers to make it permanent.

use crate::case_view::draw_centered;
use crate::dbus;
use crate::fan_controls::{push_mode, report_status};
use crate::theme::{GraphPalette, Rgba};
use biggest_fan_core as core;
use glib::Propagation;
use gtk4::prelude::*;
use gtk4::{Box as GtkBox, Button, DrawingArea, Label, Orientation};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Hottest and coldest temperatures the graph covers, in °C. Wide enough
/// for every preset (Quiet tops out at 95 °C) and fixed so the graphs in
/// different rows can be compared.
const TEMP_MIN_C: f64 = 20.0;
const TEMP_MAX_C: f64 = 100.0;
const TEMP_SPAN_C: f64 = TEMP_MAX_C - TEMP_MIN_C;

/// How close the pointer has to come (pixels) to grab or delete a point.
const GRAB_RADIUS_PX: f64 = 9.0;

/// The smallest gap two neighbouring points may have, in °C. Enough to
/// see and grab each handle, and it keeps a dragged point legal (points
/// must be strictly increasing in temperature).
const MIN_POINT_GAP_C: f64 = 2.0;

/// A hand-edited point snaps to whole degrees and whole percents: curves
/// land on round numbers, both on screen and in the saved config.
const TEMP_SNAP_C: f64 = 1.0;
const SPEED_SNAP: f64 = 0.01;

/// The fewest points an editable curve may keep. A single point would be
/// a flat speed, which is what _constant speed_ mode is for.
const MIN_POINTS: usize = 2;

/// Text size (pixels) for the "45 % at 60 °C" readout — bigger than the
/// 9 px axis labels, because it is the number the user came to read.
const READOUT_FONT_PX: f64 = 18.0;

/// Where the fan is right now: the governing sensor's temperature and
/// the speed fraction currently commanded.
type OperatingPoint = (f64, f64);

/// The curve points a live edit works on, shared between the draw
/// closure and the gestures.
type Draft = Rc<RefCell<Option<Vec<core::CurvePoint>>>>;

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

/// The speed fraction at a y pixel (the inverse of [`y_for`], for
/// placing an edited point under the pointer).
const fn speed_at(plot: Plot, py: f64) -> f64 {
    ((plot.y + plot.height - py) / plot.height).clamp(0.0, 1.0)
}

/// Rounds to the edit grid.
const fn snap(value: f64, grid: f64) -> f64 {
    (value / grid).round() * grid
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

/// The handle the pointer is nearest to, within [`GRAB_RADIUS_PX`].
fn nearest_point(plot: Plot, points: &[core::CurvePoint], x: f64, y: f64) -> Option<usize> {
    points
        .iter()
        .enumerate()
        .map(|(index, point)| {
            let dx = x_for(plot, point.temp_c) - x;
            let dy = y_for(plot, point.speed) - y;
            (index, dx.hypot(dy))
        })
        .filter(|(_index, distance)| *distance <= GRAB_RADIUS_PX)
        .min_by(|(_, a), (_, b)| a.total_cmp(b))
        .map(|(index, _distance)| index)
}

/// Moves one point to the pointer, snapping it to the edit grid and
/// keeping it inside the domain and strictly between its neighbours, so
/// the result is always a curve the core accepts. Returns whether the
/// point moved.
///
/// A point squeezed between two neighbours less than twice
/// [`MIN_POINT_GAP_C`] apart cannot move horizontally at all; its speed
/// can still be dragged.
fn move_point(points: &mut [core::CurvePoint], index: usize, plot: Plot, x: f64, y: f64) -> bool {
    let mut low = TEMP_MIN_C;
    let mut high = TEMP_MAX_C;
    if let Some(previous) = index.checked_sub(1).and_then(|lower| points.get(lower)) {
        low = previous.temp_c + MIN_POINT_GAP_C;
    }
    if let Some(next) = points.get(index + 1) {
        high = next.temp_c - MIN_POINT_GAP_C;
    }
    let Some(point) = points.get_mut(index) else {
        return false;
    };
    let wanted = snap(temp_at(plot, x), TEMP_SNAP_C);
    let temp_c = if low <= high {
        wanted.clamp(low, high)
    } else {
        point.temp_c
    };
    let speed = snap(speed_at(plot, y), SPEED_SNAP);
    let moved = !((point.temp_c - temp_c).abs() < f64::EPSILON
        && (point.speed - speed).abs() < f64::EPSILON);
    point.temp_c = temp_c;
    point.speed = speed;
    moved
}

/// Moves one point by a keyboard step, under the same rules a drag
/// obeys: snapped to the edit grid, inside the domain, and strictly
/// between its neighbours. Returns whether the point moved.
fn nudge_point(points: &mut [core::CurvePoint], index: usize, d_temp_c: f64, d_speed: f64) -> bool {
    let Some(point) = points.get(index) else {
        return false;
    };
    let mut low = TEMP_MIN_C;
    let mut high = TEMP_MAX_C;
    if let Some(previous) = index.checked_sub(1).and_then(|lower| points.get(lower)) {
        low = previous.temp_c + MIN_POINT_GAP_C;
    }
    if let Some(next) = points.get(index + 1) {
        high = next.temp_c - MIN_POINT_GAP_C;
    }
    let temp_c = if low <= high {
        snap(point.temp_c + d_temp_c, TEMP_SNAP_C).clamp(low, high)
    } else {
        point.temp_c
    };
    let speed = snap(point.speed + d_speed, SPEED_SNAP).clamp(0.0, 1.0);
    let Some(point) = points.get_mut(index) else {
        return false;
    };
    let moved = !((point.temp_c - temp_c).abs() < f64::EPSILON
        && (point.speed - speed).abs() < f64::EPSILON);
    point.temp_c = temp_c;
    point.speed = speed;
    moved
}

/// Adds a point under the pointer. Refused (returning `false`, leaving
/// the points alone) when the spot is closer than [`MIN_POINT_GAP_C`] to
/// an existing point, which is what keeps handles grabbable.
fn insert_point(points: &mut Vec<core::CurvePoint>, plot: Plot, x: f64, y: f64) -> bool {
    let point = core::CurvePoint {
        temp_c: snap(temp_at(plot, x), TEMP_SNAP_C),
        speed: snap(speed_at(plot, y), SPEED_SNAP),
    };
    if points
        .iter()
        .any(|other| (other.temp_c - point.temp_c).abs() < MIN_POINT_GAP_C)
    {
        return false;
    }
    let position = points
        .iter()
        .position(|other| other.temp_c > point.temp_c)
        .unwrap_or(points.len());
    points.insert(position, point);
    true
}

/// Deletes the point nearest the pointer, unless that would leave fewer
/// than [`MIN_POINTS`]. Returns whether one was deleted.
fn remove_point(points: &mut Vec<core::CurvePoint>, plot: Plot, x: f64, y: f64) -> bool {
    if points.len() <= MIN_POINTS {
        return false;
    }
    let Some(index) = nearest_point(plot, points, x, y) else {
        return false;
    };
    points.remove(index);
    true
}

/// What one frame shows: the fan's curve (its auto curve, or the draft
/// under the pointer), the handles to paint, the hovered/held handle,
/// the live operating point, and the throttle-relief curve running
/// above the saved one (if any).
struct Frame<'a> {
    curve: Option<&'a core::FanCurve>,
    handles: Option<&'a [core::CurvePoint]>,
    focus: Option<usize>,
    /// Whether the graph widget itself has keyboard focus (draws a ring
    /// around the plot; the keyboard editing keys are in `wire_keyboard`).
    focused: bool,
    operating: Option<OperatingPoint>,
    relief: Option<&'a core::FanCurve>,
}

/// Draws the graph: frame, grid, the curve with a filled area under it,
/// the curve's points as handles (the one the pointer is over or holding
/// stands out, with its values beside it), the live operating point as a
/// dashed temperature line plus a dot, and — while GPU throttle relief
/// runs the fan above its curve — the curve actually running, over the
/// saved one in hot orange.
fn draw(cr: &cairo::Context, width: f64, height: f64, frame: &Frame<'_>, palette: &GraphPalette) {
    let GraphPalette {
        background,
        text,
        curve: curve_colour,
        operating: operating_colour,
        relief: relief_colour,
    } = *palette;
    cr.set_source_rgba(background.0, background.1, background.2, background.3);
    let _ = cr.paint();

    let plot = plot_box(width, height);
    draw_grid(cr, plot, text);

    // Keyboard focus ring: the graph is a focusable editing surface, so
    // it has to show when it holds focus.
    if frame.focused {
        cr.set_line_width(2.0);
        cr.set_source_rgba(curve_colour.0, curve_colour.1, curve_colour.2, 0.9);
        cr.rectangle(
            plot.x - 1.5,
            plot.y - 1.5,
            plot.width + 3.0,
            plot.height + 3.0,
        );
        let _ = cr.stroke();
    }

    let Some(curve) = frame.curve else {
        return;
    };

    draw_curve(cr, plot, curve, curve_colour);

    // The throttle-relief curve the fan is really running, over the
    // saved one in hot orange (the live marker lands on this line).
    draw_relief(cr, plot, frame.relief, relief_colour);

    // The curve's own points: hover one to read it, drag it to reshape
    // the curve.
    if let Some(handles) = frame.handles {
        draw_handles(cr, plot, handles, frame.focus, palette);
    }

    if let Some(operating) = frame.operating {
        draw_operating(cr, plot, operating, operating_colour);
    }
}

/// The plot's frame, gridlines at the round temperatures, and corner
/// labels marking the domain ends and the speed range.
fn draw_grid(cr: &cairo::Context, plot: Plot, text: Rgba) {
    cr.set_line_width(1.0);
    cr.set_source_rgba(text.0, text.1, text.2, 0.5);
    cr.rectangle(plot.x, plot.y, plot.width, plot.height);
    let _ = cr.stroke();

    // Grid at the round temperatures; the frame already marks the edges.
    cr.set_source_rgba(text.0, text.1, text.2, 0.2);
    for temp_c in [40.0, 60.0, 80.0] {
        let x = x_for(plot, temp_c);
        cr.move_to(x, plot.y);
        cr.line_to(x, plot.y + plot.height);
    }
    let _ = cr.stroke();

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
}

/// The saved curve, stroked with a filled area under it down to the plot
/// floor.
fn draw_curve(cr: &cairo::Context, plot: Plot, curve: &core::FanCurve, colour: Rgba) {
    let points = curve_points(curve, plot);
    if let (Some(&(first_x, first_y)), Some(&(last_x, _))) = (points.first(), points.last()) {
        cr.move_to(first_x, first_y);
        for &(x, y) in points.iter().skip(1) {
            cr.line_to(x, y);
        }
        cr.set_source_rgb(colour.0, colour.1, colour.2);
        cr.set_line_width(1.6);
        let _ = cr.stroke_preserve();
        // Close down to the plot floor and fill the area under the curve.
        let floor = plot.y + plot.height;
        cr.line_to(last_x, floor);
        cr.line_to(first_x, floor);
        cr.close_path();
        cr.set_source_rgba(colour.0, colour.1, colour.2, 0.12);
        let _ = cr.fill();
    }
}

/// The curve's points as handles. The one the pointer is over or holding
/// gets a halo and a readout of what it asks for — its temperature and
/// the speed there.
fn draw_handles(
    cr: &cairo::Context,
    plot: Plot,
    handles: &[core::CurvePoint],
    focus: Option<usize>,
    palette: &GraphPalette,
) {
    let GraphPalette {
        background,
        text,
        curve: curve_colour,
        ..
    } = *palette;
    for (index, point) in handles.iter().enumerate() {
        let x = x_for(plot, point.temp_c);
        let y = y_for(plot, point.speed);
        let focused = focus == Some(index);
        if focused {
            cr.set_source_rgba(text.0, text.1, text.2, text.3);
            cr.arc(x, y, 5.0, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
        }
        cr.set_source_rgb(curve_colour.0, curve_colour.1, curve_colour.2);
        cr.arc(x, y, 3.2, 0.0, std::f64::consts::TAU);
        let _ = cr.fill();
        cr.set_source_rgb(background.0, background.1, background.2);
        cr.set_line_width(1.0);
        let _ = cr.stroke();
        if focused {
            draw_readout(cr, plot, x, y, point, palette);
        }
    }
}

/// The live operating point: a dashed temperature line plus a dot at the
/// commanded speed.
fn draw_operating(cr: &cairo::Context, plot: Plot, operating: OperatingPoint, colour: Rgba) {
    let (temp_c, commanded) = operating;
    let x = x_for(plot, temp_c);
    cr.set_source_rgba(colour.0, colour.1, colour.2, 0.45);
    cr.set_line_width(1.0);
    cr.set_dash(&[3.0, 3.0], 0.0);
    cr.move_to(x, plot.y);
    cr.line_to(x, plot.y + plot.height);
    let _ = cr.stroke();
    cr.set_dash(&[], 0.0);
    cr.set_source_rgb(colour.0, colour.1, colour.2);
    cr.arc(x, y_for(plot, commanded), 3.0, 0.0, std::f64::consts::TAU);
    let _ = cr.fill();
}

/// The curve GPU throttle relief is actually running, stroked in hot
/// orange over the fan's saved curve.
fn draw_relief(cr: &cairo::Context, plot: Plot, relief: Option<&core::FanCurve>, colour: Rgba) {
    let Some(relief) = relief else {
        return;
    };
    let points = curve_points(relief, plot);
    if let Some(&(first_x, first_y)) = points.first() {
        cr.move_to(first_x, first_y);
        for &(x, y) in points.iter().skip(1) {
            cr.line_to(x, y);
        }
        cr.set_source_rgb(colour.0, colour.1, colour.2);
        cr.set_line_width(2.0);
        let _ = cr.stroke();
    }
}

/// The values of the focused handle: "45 % at 60 °C" in a dim box, so it
/// stays readable over the curve, and kept inside the frame whatever the
/// point asks for.
///
/// The wording is ASCII apart from °C: cairo's toy text API (used by
/// [`draw_centered`]) picks one font with no glyph fallback, so anything
/// outside that font draws as an empty box — an arrow like U+2192 is not
/// in every sans-serif.
fn draw_readout(
    cr: &cairo::Context,
    plot: Plot,
    x: f64,
    y: f64,
    point: &core::CurvePoint,
    palette: &GraphPalette,
) {
    // The frame's axis labels left cairo at 9 px; this readout is the
    // number the user is here for, so it gets its own bigger size (the
    // next frame selects the small one again).
    cr.set_font_size(READOUT_FONT_PX);
    let label = format!("{:.0} % at {:.0} °C", point.speed * 100.0, point.temp_c);
    // Above the handle normally, below when there is no room up there.
    let half_font = READOUT_FONT_PX / 2.0;
    let baseline = if y > plot.y + READOUT_FONT_PX + half_font {
        y - half_font
    } else {
        y + READOUT_FONT_PX + half_font
    };
    let Ok(extents) = cr.text_extents(&label) else {
        // No metrics to place a box by; the text alone still says the job.
        cr.set_source_rgba(
            palette.text.0,
            palette.text.1,
            palette.text.2,
            palette.text.3,
        );
        let _ = draw_centered(cr, x, baseline, &label);
        return;
    };
    // Keep the whole box inside the frame, whatever the point asks for.
    let half_box = extents.width() / 2.0 + 3.0;
    let text_x = if half_box * 2.0 > plot.width {
        plot.x + plot.width / 2.0
    } else {
        x.clamp(plot.x + half_box, plot.x + plot.width - half_box)
    };
    // The box is the plot background again: it erases the curve behind
    // the text on any theme.
    cr.set_source_rgba(
        palette.background.0,
        palette.background.1,
        palette.background.2,
        0.9,
    );
    cr.rectangle(
        text_x - half_box,
        baseline + extents.y_bearing() - 2.0,
        extents.width() + 6.0,
        extents.height() + 4.0,
    );
    let _ = cr.fill();
    cr.set_source_rgba(
        palette.text.0,
        palette.text.1,
        palette.text.2,
        palette.text.3,
    );
    let _ = draw_centered(cr, text_x, baseline, &label);
}

/// Turns edited points back into the fan's automatic mode: the curve it
/// already follows keeps its sensor, and the new curve goes to this
/// process (so the row and case view react) and to the service (which
/// drives the headers). A point set the core rejects is reported on the
/// row's status line and changes nothing.
fn commit_points(
    engine_rc: &Rc<RefCell<core::Engine>>,
    fan_id: &core::FanId,
    points: &[core::CurvePoint],
    status_label: &Label,
    client: &dbus::Client,
) {
    let sensor = engine_rc
        .borrow()
        .fan_mode(fan_id)
        .and_then(|mode| match mode {
            core::FanMode::Auto { sensor, .. } => Some(sensor),
            _ => None,
        });
    let Some(sensor) = sensor else { return };
    let mode = match core::FanCurve::new(points.to_vec()) {
        Ok(curve) => core::FanMode::Auto { sensor, curve },
        Err(error) => {
            report_status(status_label, &format!("curve not changed: {error}"));
            return;
        }
    };
    push_mode(engine_rc, fan_id, Some(mode), status_label, client);
}

/// The per-fan curve graph widget. Its draw closure reads the fan's
/// curve straight from the engine, so a preset change needs only a
/// `queue_draw` to appear.
///
/// An edit works the same way: it writes a draft here, and commits the
/// fan's mode only when the gesture ends. While GPU throttle relief
/// lifts a cooler above its saved curve, the boosted curve is drawn
/// over the saved one and the "Keep this curve" row offers to make it
/// permanent.
pub struct CurveGraph {
    area: DrawingArea,
    /// The graph with the (normally hidden) keep offer under it.
    container: GtkBox,
    save_row: GtkBox,
    engine: Rc<RefCell<core::Engine>>,
    fan_id: core::FanId,
    operating: Rc<RefCell<Option<OperatingPoint>>>,
}

impl CurveGraph {
    /// Builds the editable graph for one fan; `engine_rc` is
    /// consulted on every draw (missing or non-auto fans draw the empty
    /// frame only), and an edit goes through `client` to the service,
    /// reporting trouble on `status_label`.
    #[must_use]
    pub fn new(
        engine_rc: &Rc<RefCell<core::Engine>>,
        fan_id: &core::FanId,
        client: &dbus::Client,
        status_label: &Label,
    ) -> Self {
        let area = DrawingArea::new();
        area.set_width_request(260);
        area.set_height_request(90);
        area.set_hexpand(true);
        // Deliberately no tooltip: the popup lands on top of the curve and
        // under the pointer while editing. Hovering a handle prints what it
        // asks for on the plot instead, and the gestures are in the README.
        // The graph is focusable so the curve can be edited without a
        // pointer (HIG: every pointer action needs a keyboard path).
        area.set_focusable(true);
        accessible_curve_keys(&area);
        let operating = Rc::new(RefCell::new(None));
        let draft: Draft = Rc::new(RefCell::new(None));
        let grabbed: Rc<Cell<Option<usize>>> = Rc::new(Cell::new(None));
        // Which handle the pointer is resting on (no drag involved).
        let hovered: Rc<Cell<Option<usize>>> = Rc::new(Cell::new(None));
        // Which handle the keyboard picked, and whether the graph itself
        // has keyboard focus (the focus ring).
        let selected: Rc<Cell<Option<usize>>> = Rc::new(Cell::new(None));
        let has_focus = Rc::new(Cell::new(false));
        let save_row = relief_save_row(engine_rc, fan_id, status_label, client);
        let container = GtkBox::new(Orientation::Vertical, 2);
        container.append(&area);
        container.append(&save_row);
        container.set_hexpand(true);

        let draw_engine = Rc::clone(engine_rc);
        let draw_fan_id = fan_id.clone();
        let draw_operating = Rc::clone(&operating);
        let draw_draft = Rc::clone(&draft);
        let draw_grabbed = Rc::clone(&grabbed);
        let draw_hovered = Rc::clone(&hovered);
        let draw_selected = Rc::clone(&selected);
        let draw_has_focus = Rc::clone(&has_focus);
        area.set_draw_func(move |_area, cr, width, height| {
            // Take a copy of what is needed and release the engine
            // before drawing (same care as the case canvas).
            let draft = draw_draft.borrow().clone();
            let (curve, handles, relief) =
                draw_engine
                    .try_borrow()
                    .map_or((None, None, None), |engine| {
                        match engine.fan_mode(&draw_fan_id) {
                            Some(core::FanMode::Auto { curve, .. }) => {
                                let handles =
                                    draft.clone().unwrap_or_else(|| curve.points().to_vec());
                                // No overlay while a draft is being dragged:
                                // the draft is the curve under the pointer.
                                let relief = draft
                                    .is_none()
                                    .then(|| engine.relieved_fan_curve(&draw_fan_id))
                                    .flatten();
                                (Some(curve), Some(handles), relief)
                            }
                            _ => (None, None, None),
                        }
                    });
            // A drag draws its draft instead of the fan's real curve.
            let drafted = draft
                .as_ref()
                .and_then(|points| core::FanCurve::new(points.clone()).ok());
            draw(
                cr,
                f64::from(width),
                f64::from(height),
                &Frame {
                    curve: drafted.as_ref().or(curve.as_ref()),
                    handles: handles.as_deref(),
                    // A held handle outranks a keyboard-selected one,
                    // which outranks a hovered one.
                    focus: draw_grabbed
                        .get()
                        .or_else(|| draw_selected.get())
                        .or_else(|| draw_hovered.get()),
                    focused: draw_has_focus.get(),
                    operating: *draw_operating.borrow(),
                    relief: relief.as_ref(),
                },
                &GraphPalette::resolve(),
            );
        });

        wire_drag(
            &area,
            engine_rc,
            fan_id,
            client,
            status_label,
            &draft,
            &grabbed,
        );
        wire_point_edits(&area, engine_rc, fan_id, client, status_label);
        wire_hover(&area, engine_rc, fan_id, &draft, &hovered);
        wire_keyboard(
            &area,
            engine_rc,
            fan_id,
            client,
            status_label,
            &selected,
            &has_focus,
        );

        Self {
            area,
            container,
            save_row,
            engine: Rc::clone(engine_rc),
            fan_id: fan_id.clone(),
            operating,
        }
    }

    /// The widget to add to the fan row.
    #[must_use]
    pub const fn widget(&self) -> &GtkBox {
        &self.container
    }

    /// Moves the live marker to the fan's current governing temperature
    /// (if the sensor reads at all) and commanded speed, shows or hides
    /// the "Keep this curve" offer to match the engine's live throttle
    /// relief, and schedules a redraw.
    pub fn set_operating_point(&self, temp_c: Option<f64>, commanded_speed: f64) {
        *self.operating.borrow_mut() = temp_c.map(|temp| (temp, commanded_speed));
        let relieved = self
            .engine
            .try_borrow()
            .is_ok_and(|engine| engine.relieved_fan_curve(&self.fan_id).is_some());
        self.save_row.set_visible(relieved);
        self.area.queue_draw();
    }
}

/// The keep offer shown while throttle relief lifts a fan's curve;
/// `set_operating_point` (every tick) shows or hides it.
fn relief_save_row(
    engine_rc: &Rc<RefCell<core::Engine>>,
    fan_id: &core::FanId,
    status_label: &Label,
    client: &dbus::Client,
) -> GtkBox {
    let save_row = GtkBox::new(Orientation::Horizontal, 6);
    let save_button = Button::builder()
        .label("Keep this curve")
        .tooltip_text(
            "make the curve the fan is actually running its saved curve; \
             relief keeps lifting from there while the card throttles, \
             and \"Save config\" writes it to disk",
        )
        .build();
    {
        let rc = Rc::clone(engine_rc);
        let fan_id = fan_id.clone();
        let status_label = status_label.clone();
        let client = client.clone();
        save_button.connect_clicked(move |_| {
            save_relief_curve(&rc, &fan_id, &status_label, &client);
        });
    }
    save_row.append(
        &Label::builder()
            .label("GPU relief is running this curve")
            .build(),
    );
    save_row.append(&save_button);
    save_row.set_visible(false);
    save_row
}

/// Keeps a throttle-lifted curve as the fan's own: the curve the fan is
/// actually running (relief included) replaces its configured curve,
/// through the same path as a drag commit so the service and the case
/// view follow. Relief continues from the new baseline while the card
/// still throttles; writing the curve to disk stays the "Save config"
/// button's job.
fn save_relief_curve(
    engine_rc: &Rc<RefCell<core::Engine>>,
    fan_id: &core::FanId,
    status_label: &Label,
    client: &dbus::Client,
) {
    let engine = engine_rc.borrow();
    let Some(core::FanMode::Auto { sensor, .. }) = engine.fan_mode(fan_id) else {
        return;
    };
    let Some(relief) = engine.relieved_fan_curve(fan_id) else {
        return;
    };
    drop(engine);
    push_mode(
        engine_rc,
        fan_id,
        Some(core::FanMode::Auto {
            sensor,
            curve: relief,
        }),
        status_label,
        client,
    );
}

/// Wires the drag-a-handle gesture onto the graph. The press grabs the
/// nearest point and copies the fan's curve into `draft`; every move
/// reshapes the copy, and releasing commits it. A cancel drops the copy
/// and leaves the fan's curve exactly as it was.
fn wire_drag(
    area: &DrawingArea,
    engine_rc: &Rc<RefCell<core::Engine>>,
    fan_id: &core::FanId,
    client: &dbus::Client,
    status_label: &Label,
    draft: &Draft,
    grabbed: &Rc<Cell<Option<usize>>>,
) {
    // Primary button only, so the click gestures can use the others.
    let begin_engine = Rc::clone(engine_rc);
    let begin_fan_id = fan_id.clone();
    let begin_area = area.clone();
    let begin_draft = Rc::clone(draft);
    let begin_grabbed = Rc::clone(grabbed);
    let changed = Rc::new(Cell::new(false));
    let begin_changed = Rc::clone(&changed);
    // Where the press happened: `GtkGestureDrag` reports later moves as
    // offsets from it, so adding them gives the pointer back.
    let origin = Rc::new(Cell::new((0.0_f64, 0.0_f64)));
    let begin_origin = Rc::clone(&origin);
    let gesture = gtk4::GestureDrag::new();
    gesture.set_button(1);
    gesture.connect_drag_begin(move |_gesture, x, y| {
        let Some(plot) = plot_of(&begin_area) else {
            return;
        };
        let Some(points) = auto_points(&begin_engine, &begin_fan_id) else {
            return;
        };
        let Some(index) = nearest_point(plot, &points, x, y) else {
            return;
        };
        // The engine keeps its real curve until the drag commits; the
        // draft starts as an exact copy so grabbing alone changes nothing
        // (a hand-written 43.7 °C point stays where it is).
        begin_origin.set((x, y));
        *begin_draft.borrow_mut() = Some(points);
        begin_grabbed.set(Some(index));
        begin_changed.set(false);
        begin_area.queue_draw();
    });
    let update_area = area.clone();
    let update_draft = Rc::clone(draft);
    let update_grabbed = Rc::clone(grabbed);
    let update_changed = Rc::clone(&changed);
    let update_origin = Rc::clone(&origin);
    gesture.connect_drag_update(move |_gesture, offset_x, offset_y| {
        let Some(plot) = plot_of(&update_area) else {
            return;
        };
        let Some(index) = update_grabbed.get() else {
            return;
        };
        let (start_x, start_y) = update_origin.get();
        let pointer_x = start_x + offset_x;
        let pointer_y = start_y + offset_y;
        let Some(mut points) = update_draft.borrow().clone() else {
            return;
        };
        if move_point(&mut points, index, plot, pointer_x, pointer_y) {
            update_changed.set(true);
        }
        *update_draft.borrow_mut() = Some(points);
        update_area.queue_draw();
    });
    let end_area = area.clone();
    let end_engine = Rc::clone(engine_rc);
    let end_fan_id = fan_id.clone();
    let end_draft = Rc::clone(draft);
    let end_grabbed = Rc::clone(grabbed);
    let end_changed = Rc::clone(&changed);
    let end_status = status_label.clone();
    let end_client = client.clone();
    gesture.connect_drag_end(move |_gesture, _x, _y| {
        let committed = end_draft.borrow_mut().take();
        let moved = end_changed.get();
        end_grabbed.set(None);
        end_changed.set(false);
        if let (true, Some(points)) = (moved, committed) {
            commit_points(&end_engine, &end_fan_id, &points, &end_status, &end_client);
        }
        end_area.queue_draw();
    });
    let cancel_draft = Rc::clone(draft);
    let cancel_grabbed = Rc::clone(grabbed);
    let cancel_area = area.clone();
    gesture.connect_cancel(move |_gesture, _sequence| {
        cancel_draft.borrow_mut().take();
        cancel_grabbed.set(None);
        changed.set(false);
        cancel_area.queue_draw();
    });
    area.add_controller(gesture);
}

/// Wires the two click gestures: double-click adds a point under the
/// pointer, right-click deletes the nearest one. Both commit at once —
/// a click has no half-finished state to keep off a real fan.
fn wire_point_edits(
    area: &DrawingArea,
    engine_rc: &Rc<RefCell<core::Engine>>,
    fan_id: &core::FanId,
    client: &dbus::Client,
    status_label: &Label,
) {
    let add_engine = Rc::clone(engine_rc);
    let add_fan_id = fan_id.clone();
    let add_area = area.clone();
    let add_status = status_label.clone();
    let add_client = client.clone();
    let add_gesture = gtk4::GestureClick::new();
    add_gesture.set_button(1);
    add_gesture.connect_pressed(move |_click, n_press, x, y| {
        if n_press != 2 {
            return;
        }
        let Some(plot) = plot_of(&add_area) else {
            return;
        };
        let Some(mut points) = auto_points(&add_engine, &add_fan_id) else {
            return;
        };
        if !insert_point(&mut points, plot, x, y) {
            report_status(&add_status, "too close to another point on the curve");
            return;
        }
        commit_points(&add_engine, &add_fan_id, &points, &add_status, &add_client);
        add_area.queue_draw();
    });
    area.add_controller(add_gesture);

    let del_engine = Rc::clone(engine_rc);
    let del_fan_id = fan_id.clone();
    let del_area = area.clone();
    let del_status = status_label.clone();
    let del_client = client.clone();
    let del_gesture = gtk4::GestureClick::new();
    del_gesture.set_button(3);
    del_gesture.connect_pressed(move |_click, _n_press, x, y| {
        let Some(plot) = plot_of(&del_area) else {
            return;
        };
        let Some(mut points) = auto_points(&del_engine, &del_fan_id) else {
            return;
        };
        if points.len() <= MIN_POINTS {
            report_status(
                &del_status,
                "a curve keeps at least two points — pick a preset for a simpler one",
            );
            return;
        }
        if !remove_point(&mut points, plot, x, y) {
            return;
        }
        commit_points(&del_engine, &del_fan_id, &points, &del_status, &del_client);
        del_area.queue_draw();
    });
    area.add_controller(del_gesture);
}

/// Wires the hover readout: the handle nearest the pointer (within
/// [`GRAB_RADIUS_PX`]) has its values drawn beside it, and the readout
/// goes away when the pointer moves off it or leaves the graph.
fn wire_hover(
    area: &DrawingArea,
    engine_rc: &Rc<RefCell<core::Engine>>,
    fan_id: &core::FanId,
    draft: &Draft,
    hovered: &Rc<Cell<Option<usize>>>,
) {
    let motion_engine = Rc::clone(engine_rc);
    let motion_fan_id = fan_id.clone();
    let motion_draft = Rc::clone(draft);
    let motion_area = area.clone();
    let motion_hovered = Rc::clone(hovered);
    let motion = gtk4::EventControllerMotion::new();
    motion.connect_motion(move |_motion, x, y| {
        let Some(plot) = plot_of(&motion_area) else {
            return;
        };
        let Some(points) = shown_points(&motion_engine, &motion_fan_id, &motion_draft) else {
            return;
        };
        let found = nearest_point(plot, &points, x, y);
        // Redraw only when the answer changes: a motion event arrives for
        // every pixel of pointer movement.
        if motion_hovered.get() != found {
            motion_hovered.set(found);
            motion_area.queue_draw();
        }
    });
    let leave_area = area.clone();
    let leave_hovered = Rc::clone(hovered);
    motion.connect_leave(move |_motion| {
        if leave_hovered.take().is_some() {
            leave_area.queue_draw();
        }
    });
    area.add_controller(motion);
}

/// Names the graph for assistive technology and spells out its keyboard
/// editing keys as the accessible description.
fn accessible_curve_keys(area: &DrawingArea) {
    area.update_property(&[gtk4::accessible::Property::Label("Curve graph")]);
    area.update_property(&[gtk4::accessible::Property::Description(
        "Left and Right select a curve point, Up and Down change its speed, \
         Shift with Left or Right changes its temperature, Plus adds a point \
         beside the selected one, Minus removes it",
    )]);
}

/// Wires the keyboard editing path (HIG: every pointer action needs a
/// keyboard equivalent). Tab reaches the graph; Left/Right pick a
/// handle, Up/Down nudge its speed a percent, Shift+Left/Right nudge its
/// temperature a degree, Plus inserts a point halfway to the next handle
/// and Minus removes the selected one. Like the click gestures, every
/// edit commits at once — a keypress has no half-finished state.
fn wire_keyboard(
    area: &DrawingArea,
    engine_rc: &Rc<RefCell<core::Engine>>,
    fan_id: &core::FanId,
    client: &dbus::Client,
    status_label: &Label,
    selected: &Rc<Cell<Option<usize>>>,
    has_focus: &Rc<Cell<bool>>,
) {
    // The focus ring follows the widget's focus; the selection goes away
    // with it so a stale handle is never quietly nudged next visit.
    let enter_area = area.clone();
    let enter_flag = Rc::clone(has_focus);
    let leave_area = area.clone();
    let leave_flag = Rc::clone(has_focus);
    let leave_selected = Rc::clone(selected);
    let focus = gtk4::EventControllerFocus::new();
    focus.connect_enter(move |_| {
        enter_flag.set(true);
        enter_area.queue_draw();
    });
    focus.connect_leave(move |_| {
        leave_flag.set(false);
        leave_selected.take();
        leave_area.queue_draw();
    });
    area.add_controller(focus);

    let editor = CurveEditor {
        area: area.clone(),
        engine: Rc::clone(engine_rc),
        fan_id: fan_id.clone(),
        client: client.clone(),
        status: status_label.clone(),
        selected: Rc::clone(selected),
    };
    let keys = gtk4::EventControllerKey::new();
    keys.connect_key_pressed(move |_controller, key, _keycode, modifiers| {
        editor.key_press(key, modifiers.contains(gtk4::gdk::ModifierType::SHIFT_MASK))
    });
    area.add_controller(keys);
}

/// Everything a keyboard curve edit touches: the graph to redraw, the
/// engine to read and commit through, and the selected handle.
struct CurveEditor {
    area: DrawingArea,
    engine: Rc<RefCell<core::Engine>>,
    fan_id: core::FanId,
    client: dbus::Client,
    status: Label,
    selected: Rc<Cell<Option<usize>>>,
}

impl CurveEditor {
    /// One keypress on the focused graph: the keyboard curve edits (see
    /// [`wire_keyboard`]). Every edit commits at once, like the click
    /// gestures do.
    fn key_press(&self, key: gtk4::gdk::Key, shift: bool) -> Propagation {
        let area = &self.area;
        let engine_rc = &self.engine;
        let fan_id = &self.fan_id;
        let client = &self.client;
        let status_label = &self.status;
        let selected = &*self.selected;
        let Some(plot) = plot_of(area) else {
            return Propagation::Proceed;
        };
        let Some(mut points) = auto_points(engine_rc, fan_id) else {
            return Propagation::Proceed;
        };
        match key {
            gtk4::gdk::Key::Left | gtk4::gdk::Key::Right => {
                let backwards = key == gtk4::gdk::Key::Left;
                let Some(index) = selected.get() else {
                    // The first arrow picks an end handle.
                    selected.set(Some(if backwards {
                        points.len().saturating_sub(1)
                    } else {
                        0
                    }));
                    area.queue_draw();
                    return Propagation::Stop;
                };
                if shift {
                    let d_temp_c = if backwards { -TEMP_SNAP_C } else { TEMP_SNAP_C };
                    if nudge_point(&mut points, index, d_temp_c, 0.0) {
                        commit_points(engine_rc, fan_id, &points, status_label, client);
                    }
                } else {
                    let next = if backwards {
                        index.saturating_sub(1)
                    } else {
                        (index + 1).min(points.len().saturating_sub(1))
                    };
                    selected.set(Some(next));
                }
                area.queue_draw();
                Propagation::Stop
            }
            gtk4::gdk::Key::Up | gtk4::gdk::Key::Down => {
                let index = selected.get().unwrap_or(0);
                selected.set(Some(index));
                let d_speed = if key == gtk4::gdk::Key::Up {
                    SPEED_SNAP
                } else {
                    -SPEED_SNAP
                };
                if nudge_point(&mut points, index, 0.0, d_speed) {
                    commit_points(engine_rc, fan_id, &points, status_label, client);
                }
                area.queue_draw();
                Propagation::Stop
            }
            gtk4::gdk::Key::plus | gtk4::gdk::Key::KP_Add => self.insert_beside(&mut points, plot),
            gtk4::gdk::Key::minus | gtk4::gdk::Key::KP_Subtract => {
                let index = selected.get().unwrap_or(0);
                if points.len() <= MIN_POINTS {
                    report_status(
                        status_label,
                        "a curve keeps at least two points — pick a preset for a simpler one",
                    );
                    return Propagation::Stop;
                }
                points.remove(index);
                selected.set(Some(index.min(points.len() - 1)));
                commit_points(engine_rc, fan_id, &points, status_label, client);
                area.queue_draw();
                Propagation::Stop
            }
            gtk4::gdk::Key::Escape => {
                if selected.take().is_some() {
                    area.queue_draw();
                }
                Propagation::Stop
            }
            _ => Propagation::Proceed,
        }
    }

    /// Inserts a point halfway to the next handle (the previous one at
    /// the far end) and selects it.
    fn insert_beside(&self, points: &mut Vec<core::CurvePoint>, plot: Plot) -> Propagation {
        let index = self.selected.get().unwrap_or(0);
        let (lower, upper) = match points.get(index + 1) {
            Some(_) => (index, index + 1),
            None => (index.saturating_sub(1), index),
        };
        let (Some(low), Some(high)) = (points.get(lower), points.get(upper)) else {
            return Propagation::Proceed;
        };
        let mid_temp = f64::midpoint(low.temp_c, high.temp_c);
        let mid_speed = f64::midpoint(low.speed, high.speed);
        if !insert_point(points, plot, x_for(plot, mid_temp), y_for(plot, mid_speed)) {
            report_status(&self.status, "too close to another point on the curve");
            return Propagation::Stop;
        }
        // The new point lands at `upper` (it sorts between the two it
        // was placed between): select it.
        self.selected.set(Some(upper));
        commit_points(
            &self.engine,
            &self.fan_id,
            points,
            &self.status,
            &self.client,
        );
        self.area.queue_draw();
        Propagation::Stop
    }
}

/// The points the graph is showing as handles: the live draft while a
/// gesture is mid-drag, otherwise the fan's own curve. `None` for a fan
/// in another mode (there is nothing to read or edit).
fn shown_points(
    engine_rc: &Rc<RefCell<core::Engine>>,
    fan_id: &core::FanId,
    draft: &Draft,
) -> Option<Vec<core::CurvePoint>> {
    draft
        .borrow()
        .clone()
        .or_else(|| auto_points(engine_rc, fan_id))
}

/// The plot for the widget as it is on screen, or `None` before it has a
/// size (a gesture can arrive before the first draw).
fn plot_of(area: &DrawingArea) -> Option<Plot> {
    let width = f64::from(area.width());
    let height = f64::from(area.height());
    (width > 0.0 && height > 0.0).then(|| plot_box(width, height))
}

/// The fan's curve points while it is automatic; `None` for a fan in
/// another mode (there is nothing to edit).
fn auto_points(
    engine_rc: &Rc<RefCell<core::Engine>>,
    fan_id: &core::FanId,
) -> Option<Vec<core::CurvePoint>> {
    engine_rc
        .try_borrow()
        .ok()?
        .fan_mode(fan_id)
        .and_then(|mode| match mode {
            core::FanMode::Auto { curve, .. } => Some(curve.points().to_vec()),
            _ => None,
        })
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
    fn speed_and_y_are_inverses() {
        let p = plot();
        for speed in [0.0, 0.25, 0.6, 1.0] {
            assert!((speed_at(p, y_for(p, speed)) - speed).abs() < 1e-9);
        }
    }

    #[test]
    fn markers_clamp_to_the_plot_edges() {
        let p = plot();
        assert!((x_for(p, -50.0) - p.x).abs() < 1e-9);
        assert!((x_for(p, 500.0) - (p.x + p.width)).abs() < 1e-9);
        assert!((speed_at(p, -500.0) - 1.0).abs() < 1e-9, "above the plot");
        assert!((speed_at(p, 500.0) - 0.0).abs() < 1e-9, "below the plot");
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

    #[test]
    fn nearest_point_grabs_a_handle_and_lets_a_far_click_pass() {
        let p = plot();
        let points = ramp_curve().points().to_vec();
        let handle_x = x_for(p, 80.0);
        let handle_y = y_for(p, 1.0);
        assert_eq!(
            nearest_point(p, &points, handle_x + 3.0, handle_y + 3.0),
            Some(1)
        );
        assert_eq!(nearest_point(p, &points, handle_x - 3.0, handle_y), Some(1));
        assert_eq!(nearest_point(p, &points, handle_x, handle_y + 30.0), None);
    }

    #[test]
    fn nearest_point_chooses_the_closest_of_two() {
        let p = plot();
        let points = ramp_curve().points().to_vec();
        let between_x = f64::midpoint(x_for(p, 40.0), x_for(p, 80.0));
        let near_first = x_for(p, 40.0) + 6.0;
        assert_eq!(
            nearest_point(p, &points, near_first, y_for(p, 0.2)),
            Some(0)
        );
        assert_eq!(nearest_point(p, &points, between_x, y_for(p, 0.6)), None);
    }

    #[test]
    fn a_dragged_point_snaps_to_whole_degrees_and_percents() {
        let p = plot();
        let mut points = ramp_curve().points().to_vec();
        // Aim between grid lines: 61.6 °C-ish and ~44.7 %.
        let x = x_for(p, 61.6);
        let y = y_for(p, 0.447);
        assert!(move_point(&mut points, 0, p, x, y));
        assert!((points[0].temp_c - 62.0).abs() < 1e-9, "{:?}", points[0]);
        assert!((points[0].speed - 0.45).abs() < 1e-9, "{:?}", points[0]);
    }

    #[test]
    fn a_dragged_point_stays_between_its_neighbours() {
        let p = plot();
        let mut points = vec![
            CurvePoint {
                temp_c: 30.0,
                speed: 0.2,
            },
            CurvePoint {
                temp_c: 50.0,
                speed: 0.4,
            },
            CurvePoint {
                temp_c: 70.0,
                speed: 0.6,
            },
        ];
        // Hard right of the last point: the middle one stops 2 °C short.
        move_point(&mut points, 1, p, x_for(p, 99.0), y_for(p, 0.4));
        assert!(
            (points[1].temp_c - 68.0).abs() < 1e-9,
            "hard right: {:?}",
            points[1]
        );
        // The first point has no neighbour to its left, so it follows the
        // pointer all the way down to near the cold edge of the domain.
        move_point(&mut points, 0, p, x_for(p, 21.0), y_for(p, 0.2));
        assert!(
            (points[0].temp_c - 21.0).abs() < 1e-9,
            "hard left, nothing to stop it: {:?}",
            points[0]
        );
        // The result is still a curve the core accepts.
        FanCurve::new(points.clone()).expect("still legal");
    }

    #[test]
    fn the_first_and_last_points_reach_the_domain_ends() {
        let p = plot();
        let mut points = ramp_curve().points().to_vec();
        move_point(&mut points, 0, p, x_for(p, 15.0), y_for(p, 0.5));
        move_point(&mut points, 1, p, x_for(p, 105.0), y_for(p, 0.5));
        assert!((points[0].temp_c - TEMP_MIN_C).abs() < 1e-9);
        assert!((points[1].temp_c - TEMP_MAX_C).abs() < 1e-9);
    }

    #[test]
    fn a_squeezed_point_keeps_its_temperature_but_moves_its_speed() {
        let p = plot();
        // Neighbours 1 °C either side (only possible from a hand-written
        // config) leave no legal room for the middle point to move.
        let mut points = vec![
            CurvePoint {
                temp_c: 50.0,
                speed: 0.2,
            },
            CurvePoint {
                temp_c: 51.0,
                speed: 0.4,
            },
            CurvePoint {
                temp_c: 52.0,
                speed: 0.6,
            },
        ];
        move_point(&mut points, 1, p, x_for(p, 90.0), y_for(p, 0.9));
        assert!(
            (points[1].temp_c - 51.0).abs() < 1e-9,
            "temperature cannot move: {:?}",
            points[1]
        );
        assert!(
            (points[1].speed - 0.9).abs() < 1e-9,
            "speed still follows the pointer: {:?}",
            points[1]
        );
    }

    #[test]
    fn a_drag_that_reaches_the_same_place_reports_no_change() {
        let p = plot();
        let mut points = ramp_curve().points().to_vec();
        let at = (x_for(p, 40.0), y_for(p, 0.2));
        assert!(
            !move_point(&mut points, 0, p, at.0, at.1),
            "no movement, so nothing to commit"
        );
    }

    #[test]
    fn a_keyboard_nudge_snaps_and_stays_between_neighbours() {
        let mut points = vec![
            core::CurvePoint {
                temp_c: 40.0,
                speed: 0.2,
            },
            core::CurvePoint {
                temp_c: 60.0,
                speed: 0.5,
            },
            core::CurvePoint {
                temp_c: 80.0,
                speed: 0.8,
            },
        ];
        assert!(nudge_point(&mut points, 1, 1.0, 0.01));
        assert_eq!((points[1].temp_c, points[1].speed), (61.0, 0.51));
        for _ in 0..40 {
            nudge_point(&mut points, 1, 1.0, 0.0);
        }
        assert_eq!(points[1].temp_c, 78.0, "two degrees short of the next");
        for _ in 0..80 {
            nudge_point(&mut points, 1, -1.0, 0.0);
        }
        assert_eq!(points[1].temp_c, 42.0, "two degrees past the previous");
    }

    #[test]
    fn a_keyboard_speed_nudge_clamps_at_the_ends() {
        let mut points = vec![
            core::CurvePoint {
                temp_c: 40.0,
                speed: 0.0,
            },
            core::CurvePoint {
                temp_c: 60.0,
                speed: 1.0,
            },
        ];
        assert!(!nudge_point(&mut points, 0, 0.0, -0.01), "already at 0%");
        assert!(!nudge_point(&mut points, 1, 0.0, 0.01), "already at 100%");
        assert!(nudge_point(&mut points, 0, 0.0, 0.01));
        assert!((points[0].speed - 0.01).abs() < 1e-9);
    }

    #[test]
    fn a_squeezed_point_keeps_its_temperature_under_a_nudge() {
        let mut points = vec![
            core::CurvePoint {
                temp_c: 50.0,
                speed: 0.2,
            },
            core::CurvePoint {
                temp_c: 51.5,
                speed: 0.5,
            },
            core::CurvePoint {
                temp_c: 53.0,
                speed: 0.8,
            },
        ];
        // Neighbours closer than twice the gap: no room to move.
        assert!(nudge_point(&mut points, 1, 1.0, 0.05));
        assert_eq!(points[1].temp_c, 51.5, "the temperature cannot budge");
        assert!((points[1].speed - 0.55).abs() < 1e-9, "the speed can");
    }

    #[test]
    fn inserting_a_point_lands_in_temperature_order() {
        let p = plot();
        let mut points = ramp_curve().points().to_vec();
        assert!(insert_point(&mut points, p, x_for(p, 60.0), y_for(p, 0.5)));
        assert_eq!(points.len(), 3);
        assert_eq!(points[1].temp_c, 60.0);
        assert!((points[1].speed - 0.5).abs() < 1e-9);
        FanCurve::new(points.clone()).expect("still sorted and legal");
    }

    #[test]
    fn inserting_beside_an_existing_point_is_refused() {
        let p = plot();
        let mut points = ramp_curve().points().to_vec();
        assert!(
            !insert_point(&mut points, p, x_for(p, 41.0), y_for(p, 0.3)),
            "1 °C away is too close to be a usable handle"
        );
        assert_eq!(points, ramp_curve().points(), "left alone");
    }

    #[test]
    fn removing_a_point_takes_the_nearest_and_keeps_a_usable_curve() {
        let p = plot();
        let mut points = vec![
            CurvePoint {
                temp_c: 30.0,
                speed: 0.2,
            },
            CurvePoint {
                temp_c: 55.0,
                speed: 0.5,
            },
            CurvePoint {
                temp_c: 80.0,
                speed: 0.9,
            },
        ];
        assert!(remove_point(&mut points, p, x_for(p, 56.0), y_for(p, 0.5)));
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].temp_c, 30.0);
        assert_eq!(points[1].temp_c, 80.0);
    }

    #[test]
    fn removing_when_only_two_points_are_left_is_refused() {
        let p = plot();
        let mut points = ramp_curve().points().to_vec();
        assert!(!remove_point(&mut points, p, x_for(p, 40.0), y_for(p, 0.2)));
        assert_eq!(points, ramp_curve().points(), "left alone");
    }

    #[test]
    fn removing_far_from_every_point_does_nothing() {
        let p = plot();
        let mut points = vec![
            CurvePoint {
                temp_c: 30.0,
                speed: 0.2,
            },
            CurvePoint {
                temp_c: 55.0,
                speed: 0.5,
            },
            CurvePoint {
                temp_c: 80.0,
                speed: 0.9,
            },
        ];
        assert!(!remove_point(&mut points, p, x_for(p, 65.0), y_for(p, 0.5)));
        assert_eq!(points.len(), 3);
    }

    /// Counts the bright pixels a frame paints — axis labels, handles, and
    /// for a focused handle also its halo and readout text — so comparing
    /// a focused frame with an unfocused one shows whether the readout was
    /// drawn.
    fn bright_pixels(width: i32, height: i32, focus: Option<usize>) -> usize {
        let surface =
            cairo::ImageSurface::create(cairo::Format::ARgb32, width, height).expect("surface");
        {
            let cr = cairo::Context::new(&surface).expect("cairo context");
            let curve = ramp_curve();
            draw(
                &cr,
                f64::from(width),
                f64::from(height),
                &Frame {
                    curve: Some(&curve),
                    handles: Some(curve.points()),
                    focus,
                    focused: false,
                    operating: None,
                    relief: None,
                },
                &GraphPalette::DARK,
            );
        }
        let stride = usize::try_from(surface.stride()).expect("a positive stride");
        let mut bright = 0_usize;
        surface
            .with_data(|data| {
                bright = data
                    .chunks(stride.max(1))
                    .flat_map(|row| row.as_chunks::<4>().0)
                    // Premultiplied BGRA here; white reads the same either way.
                    .filter(|pixel| pixel[0] > 150 && pixel[1] > 150 && pixel[2] > 150)
                    .count();
            })
            .expect("a freshly drawn image surface still has its pixels");
        bright
    }

    #[test]
    fn a_focused_handle_paints_its_readout() {
        let plain = bright_pixels(260, 90, None);
        let focused = bright_pixels(260, 90, Some(0));
        // The halo is only a few dozen pixels, so the rest of the jump is
        // the "20 % at 40 °C" text (measured: about 220 plain, 445 focused).
        assert!(
            focused > plain + 100,
            "the readout did not draw: {plain} plain vs {focused} focused"
        );
    }

    #[test]
    fn a_tiny_graph_still_paints_a_focused_handle() {
        // The readout box is wider than this plot, so it centres rather
        // than clamps (clamping the wrong way round would panic).
        assert!(bright_pixels(40, 30, Some(1)) > 0);
    }

    /// Counts the hot-orange pixels only the throttle-relief overlay
    /// paints (premultiplied BGRA: high red, low blue — the blue curve,
    /// white text and handles all have a high blue channel).
    fn overlay_pixels(width: i32, height: i32, relief: Option<&core::FanCurve>) -> usize {
        let surface =
            cairo::ImageSurface::create(cairo::Format::ARgb32, width, height).expect("surface");
        {
            let cr = cairo::Context::new(&surface).expect("cairo context");
            let curve = ramp_curve();
            draw(
                &cr,
                f64::from(width),
                f64::from(height),
                &Frame {
                    curve: Some(&curve),
                    handles: Some(curve.points()),
                    focus: None,
                    focused: false,
                    operating: None,
                    relief,
                },
                &GraphPalette::DARK,
            );
        }
        let stride = usize::try_from(surface.stride()).expect("a positive stride");
        let mut orange = 0_usize;
        surface
            .with_data(|data| {
                orange = data
                    .chunks(stride.max(1))
                    .flat_map(|row| row.as_chunks::<4>().0)
                    .filter(|pixel| pixel[0] < 110 && pixel[1] < 200 && pixel[2] > 150)
                    .count();
            })
            .expect("a freshly drawn image surface still has its pixels");
        orange
    }

    #[test]
    fn the_relief_overlay_paints_over_the_curve() {
        assert_eq!(
            overlay_pixels(260, 90, None),
            0,
            "no overlay without throttle relief"
        );
        let lifted = overlay_pixels(260, 90, Some(&core::CurvePreset::Performance.curve()));
        assert!(lifted > 100, "the lifted-curve overlay did not draw");
    }
}
