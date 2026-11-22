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
//! Hovering a handle prints its speed and temperature beside it — on the
//! plot rather than in a GTK tooltip, which would cover the curve.
//! A drag is a draft: it is drawn in place of the fan's real curve and
//! only becomes the fan's mode — here and on the service — when the
//! pointer is released, so a half-dragged curve never drives a fan.
//!
//! All fans share the same temperature domain, so their curves can be
//! compared straight down the list.

use crate::case_view::draw_centered;
use crate::dbus;
use crate::fan_controls::{escape_markup, push_mode};
use biggest_fan_core as core;
use gtk4::prelude::*;
use gtk4::{DrawingArea, Label};
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

/// Draws the graph: frame, grid, the curve with a filled area under it,
/// the curve's points as handles (the one the pointer is over or holding
/// stands out, with its values beside it), and the live operating point
/// as a dashed temperature line plus a dot.
fn draw(
    cr: &cairo::Context,
    width: f64,
    height: f64,
    curve: Option<&core::FanCurve>,
    handles: Option<&[core::CurvePoint]>,
    focus: Option<usize>,
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

    // The curve's own points: hover one to read it, drag it to reshape
    // the curve. The focused handle gets a halo and a readout of what it
    // asks for — its temperature and the speed there.
    if let Some(handles) = handles {
        for (index, point) in handles.iter().enumerate() {
            let x = x_for(plot, point.temp_c);
            let y = y_for(plot, point.speed);
            let focused = focus == Some(index);
            if focused {
                cr.set_source_rgb(1.0, 1.0, 1.0);
                cr.arc(x, y, 5.0, 0.0, std::f64::consts::TAU);
                let _ = cr.fill();
            }
            cr.set_source_rgb(0.75, 0.88, 1.0);
            cr.arc(x, y, 3.2, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
            cr.set_source_rgb(0.10, 0.25, 0.45);
            cr.set_line_width(1.0);
            let _ = cr.stroke();
            if focused {
                draw_readout(cr, plot, x, y, point);
            }
        }
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

/// The values of the focused handle: "45 % at 60 °C" in a dim box, so it
/// stays readable over the curve, and kept inside the frame whatever the
/// point asks for.
///
/// The wording is ASCII apart from °C: cairo's toy text API (used by
/// [`draw_centered`]) picks one font with no glyph fallback, so anything
/// outside that font draws as an empty box — an arrow like U+2192 is not
/// in every sans-serif.
fn draw_readout(cr: &cairo::Context, plot: Plot, x: f64, y: f64, point: &core::CurvePoint) {
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
        cr.set_source_rgb(1.0, 1.0, 1.0);
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
    cr.set_source_rgba(0.07, 0.08, 0.10, 0.9);
    cr.rectangle(
        text_x - half_box,
        baseline + extents.y_bearing() - 2.0,
        extents.width() + 6.0,
        extents.height() + 4.0,
    );
    let _ = cr.fill();
    cr.set_source_rgb(1.0, 1.0, 1.0);
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
            status_label.set_markup(&format!(
                "<span size=\"small\" foreground=\"#c0392b\">⚠ curve not changed: {}</span>",
                escape_markup(&error.to_string())
            ));
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
/// fan's mode only when the gesture ends.
pub struct CurveGraph {
    area: DrawingArea,
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
        let operating = Rc::new(RefCell::new(None));
        let draft: Draft = Rc::new(RefCell::new(None));
        let grabbed: Rc<Cell<Option<usize>>> = Rc::new(Cell::new(None));
        // Which handle the pointer is resting on (no drag involved).
        let hovered: Rc<Cell<Option<usize>>> = Rc::new(Cell::new(None));

        let draw_engine = Rc::clone(engine_rc);
        let draw_fan_id = fan_id.clone();
        let draw_operating = Rc::clone(&operating);
        let draw_draft = Rc::clone(&draft);
        let draw_grabbed = Rc::clone(&grabbed);
        let draw_hovered = Rc::clone(&hovered);
        area.set_draw_func(move |_area, cr, width, height| {
            // Take a copy of what is needed and release the engine
            // before drawing (same care as the case canvas).
            let draft = draw_draft.borrow().clone();
            let (curve, handles) = draw_engine.try_borrow().map_or((None, None), |engine| {
                match engine.fan_mode(&draw_fan_id) {
                    Some(core::FanMode::Auto { curve, .. }) => {
                        let handles = draft.clone().unwrap_or_else(|| curve.points().to_vec());
                        (Some(curve), Some(handles))
                    }
                    _ => (None, None),
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
                drafted.as_ref().or(curve.as_ref()),
                handles.as_deref(),
                // A held handle outranks a hovered one.
                draw_grabbed.get().or_else(|| draw_hovered.get()),
                *draw_operating.borrow(),
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
            add_status.set_markup(
                "<span size=\"small\" foreground=\"#c0392b\">⚠ too close to another point on \
                 the curve</span>",
            );
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
            del_status.set_markup(
                "<span size=\"small\" foreground=\"#c0392b\">⚠ a curve keeps at least two \
                 points — pick a preset for a simpler one</span>",
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
                Some(&curve),
                Some(curve.points()),
                focus,
                None,
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
}
