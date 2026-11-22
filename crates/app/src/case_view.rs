//! Draws the PC case: fan shapes whose fill reflects each fan's
//! commanded speed, coloured by airflow direction — circles for fan
//! headers, a card-like rectangle for a whole graphics cooler.
//!
//! Dragging a fan shape moves it on the canvas (kept in the engine and
//! persisted by Save config).

use biggest_fan_core as core;
use gtk4::DrawingArea;
use gtk4::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

const TAU: f64 = std::f64::consts::TAU;

/// The fan grabbed at drag start, with the slot it had then; `None`
/// while a drag is not moving a fan.
type DragState = Rc<RefCell<Option<(core::FanId, core::FanSlot)>>>;

/// Builds the drawing area showing the live case view.
#[must_use]
pub fn install(engine_rc: Rc<RefCell<core::Engine>>) -> DrawingArea {
    let area = DrawingArea::new();
    area.set_width_request(330);
    area.set_height_request(470);
    area.set_vexpand(true);
    area.set_hexpand(true);
    // The draw closure takes its own handle of the engine; the gesture
    // closures below use this clone.
    let gesture_engine = Rc::clone(&engine_rc);
    area.set_draw_func(move |_area, cr, width, height| {
        // Try (not borrow): a panic inside a draw callback would be worse
        // than skipping one frame while a handler holds the engine.
        let Ok(engine) = engine_rc.try_borrow() else {
            return;
        };
        draw(&engine, cr, f64::from(width), f64::from(height));
    });

    let drag: DragState = Rc::new(RefCell::new(None));
    let gesture = gtk4::GestureDrag::new();

    let begin_area = area.clone();
    let begin_engine = Rc::clone(&gesture_engine);
    let begin_drag = Rc::clone(&drag);
    gesture.connect_drag_begin(move |_gesture, x, y| {
        let Ok(engine) = begin_engine.try_borrow() else {
            return;
        };
        let widget_width = f64::from(begin_area.width());
        let widget_height = f64::from(begin_area.height());
        let (scale, offset_x, offset_y) =
            fit_transform(widget_width, widget_height, engine.case_config());
        let canvas_x = (x - offset_x) / scale;
        let canvas_y = (y - offset_y) / scale;
        if let Some(view) = engine.case_fans().into_iter().find(|view| {
            let dx = view.slot.x - canvas_x;
            let dy = view.slot.y - canvas_y;
            if view.slot.is_rectangular() {
                let (half_width, half_height) = view.slot.half_extents();
                dx.abs() <= half_width && dy.abs() <= half_height
            } else {
                dx.hypot(dy) <= view.slot.diameter / 2.0
            }
        }) {
            *begin_drag.borrow_mut() = Some((view.id, view.slot));
        }
    });

    let update_area = area.clone();
    let update_engine = gesture_engine;
    let update_drag = Rc::clone(&drag);
    gesture.connect_drag_update(move |_gesture, offset_x, offset_y| {
        // Drag offsets are measured from the grab point, so the slot is
        // always recomputed from where it was when the drag started.
        let Some((fan_id, start)) = update_drag.borrow().clone() else {
            return;
        };
        let Ok(mut engine) = update_engine.try_borrow_mut() else {
            return;
        };
        let case = engine.case_config();
        let (scale, _offset_x, _offset_y) = fit_transform(
            f64::from(update_area.width()),
            f64::from(update_area.height()),
            case,
        );
        // The engine's `set_fan_slot` clamps the slot back inside the
        // padded case walls, so the drag just chases the pointer.
        let slot = core::FanSlot {
            x: start.x + offset_x / scale,
            y: start.y + offset_y / scale,
            diameter: start.diameter,
            rect: start.rect,
            airflow: start.airflow,
        };
        if engine.set_fan_slot(&fan_id, slot).is_err() {
            return;
        }
        drop(engine);
        update_area.queue_draw();
    });

    let end_drag = Rc::clone(&drag);
    gesture.connect_drag_end(move |_gesture, _x, _y| {
        *end_drag.borrow_mut() = None;
    });
    let cancel_drag = Rc::clone(&drag);
    gesture.connect_cancel(move |_gesture, _sequence| {
        *cancel_drag.borrow_mut() = None;
    });
    // `add_controller` takes the reference the widget needs; the local
    // `gesture` can go afterwards because the widget keeps it alive.
    area.add_controller(gesture.clone());
    drop(gesture);
    area
}

/// The case canvas is centred in the widget and scaled to fit a margin.
/// Drawing and drag hit-testing must share this mapping.
///
/// Returns `(scale, offset_x, offset_y)`: canvas coordinates map to
/// widget pixels as `offset + scale * canvas`.
fn fit_transform(widget_width: f64, widget_height: f64, case: core::CaseConfig) -> (f64, f64, f64) {
    let margin = 10.0;
    let usable_width = (-2.0_f64).mul_add(margin, widget_width);
    let usable_height = (-2.0_f64).mul_add(margin, widget_height);
    let scale = (usable_width / case.width).min(usable_height / case.height);
    let offset_x = (-case.width).mul_add(scale, widget_width) / 2.0;
    let offset_y = (-case.height).mul_add(scale, widget_height) / 2.0;
    (scale, offset_x, offset_y)
}

/// Airflow colours: blue intake, red exhaust, yellow internal.
const fn airflow_colour(airflow: core::Airflow) -> (f64, f64, f64) {
    match airflow {
        core::Airflow::Intake => (0.20, 0.50, 0.90),
        core::Airflow::Exhaust => (0.90, 0.30, 0.25),
        core::Airflow::Internal => (0.90, 0.72, 0.15),
    }
}

fn draw(engine: &core::Engine, cr: &cairo::Context, width: f64, height: f64) {
    let case_size = engine.case_config();

    cr.set_source_rgb(0.13, 0.13, 0.15);
    let _ = cr.paint();

    // Fit the case canvas inside the widget, then work in canvas units.
    let (scale, offset_x, offset_y) = fit_transform(width, height, case_size);
    let _ = cr.save();
    cr.translate(offset_x, offset_y);
    cr.scale(scale, scale);

    // The case outline.
    cr.set_source_rgb(0.20, 0.20, 0.23);
    rounded_rect(cr, 0.0, 0.0, case_size.width, case_size.height, 14.0);
    let _ = cr.fill_preserve();
    cr.set_source_rgb(0.55, 0.55, 0.58);
    cr.set_line_width(1.6);
    let _ = cr.stroke();

    for view in engine.case_fans() {
        draw_fan(cr, &view);
    }
    let _ = cr.restore();
}

fn draw_fan(cr: &cairo::Context, view: &core::CaseFanView) {
    if view.slot.is_rectangular() {
        draw_card(cr, view);
        return;
    }
    let (red, green, blue) = airflow_colour(view.slot.airflow);
    let (cx, cy) = (view.slot.x, view.slot.y);
    let radius = view.slot.diameter / 2.0;

    // Housing circle, brighter as the fan spins faster.
    cr.set_source_rgba(red, green, blue, 0.45_f64.mul_add(view.speed, 0.15));
    circle(cr, cx, cy, radius);
    let _ = cr.fill();

    // Speed wedge sweeping clockwise from the top.
    if view.speed > 0.005 {
        cr.set_source_rgba(red, green, blue, 0.9);
        cr.move_to(cx, cy);
        cr.arc(
            cx,
            cy,
            radius * 0.72,
            -std::f64::consts::FRAC_PI_2,
            TAU.mul_add(view.speed, -std::f64::consts::FRAC_PI_2),
        );
        cr.close_path();
        let _ = cr.fill();
    }

    // Hub and outline.
    cr.set_source_rgb(0.15, 0.15, 0.17);
    circle(cr, cx, cy, radius * 0.16);
    let _ = cr.fill();
    cr.set_source_rgb(red, green, blue);
    cr.set_line_width(1.2);
    circle(cr, cx, cy, radius);
    let _ = cr.stroke();

    // Labels: name above the hub, percent below it, RPM under the circle.
    cr.select_font_face(
        "sans-serif",
        cairo::FontSlant::Normal,
        cairo::FontWeight::Normal,
    );
    cr.set_source_rgb(0.85, 0.85, 0.88);
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to a small positive range first"
    )]
    let name_chars = (radius / 3.0).clamp(4.0, 18.0) as usize;
    let short_name = clip(&view.name, name_chars);
    cr.set_font_size((radius / 5.0).clamp(8.0, 14.0));
    let _ = draw_centered(cr, cx, 0.55_f64.mul_add(-radius, cy), &short_name);
    let percent = format!("{:.0}%", view.speed * 100.0);
    cr.set_font_size((radius / 4.0).clamp(9.0, 16.0));
    let _ = draw_centered(cr, cx, 0.55_f64.mul_add(radius, cy), &percent);
    if let Some(rpm) = view.rpm {
        cr.set_font_size(10.0);
        let _ = draw_centered(cr, cx, cy + radius + 12.0, &format!("{rpm:.0} RPM"));
    }
    if view.note.is_some() {
        cr.set_source_rgb(0.95, 0.75, 0.20);
        cr.set_font_size(12.0);
        let _ = draw_centered(cr, cx, cy - radius - 6.0, "⚠");
    }
}

/// Draws a whole rectangular object (a graphics-card cooler): the card
/// body brightens with the commanded speed, decorative cooler fans sit
/// across it, and the name/percent ride in the middle.
fn draw_card(cr: &cairo::Context, view: &core::CaseFanView) {
    let (red, green, blue) = airflow_colour(view.slot.airflow);
    let (width, height) = view
        .slot
        .rect
        .unwrap_or((view.slot.diameter, view.slot.diameter));
    let (left, top) = (view.slot.x - width / 2.0, view.slot.y - height / 2.0);

    // Card body, brighter as the cooler spins faster.
    cr.set_source_rgba(red, green, blue, 0.45_f64.mul_add(view.speed, 0.15));
    rounded_rect(cr, left, top, width, height, 8.0);
    let _ = cr.fill_preserve();
    cr.set_source_rgb(red, green, blue);
    cr.set_line_width(1.4);
    let _ = cr.stroke();

    // Decorative cooler fans, spaced across the card's width.
    let fan_count = (width / 110.0).round().clamp(1.0, 3.0);
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "already clamped to 1..=3"
    )]
    let cooler_fans = fan_count as usize;
    let fan_radius = (height / 2.0 - 10.0).max(4.0);
    for index in 0..cooler_fans {
        #[expect(
            clippy::cast_precision_loss,
            reason = "index and fan count are tiny integers"
        )]
        let fraction = (index as f64 + 0.5) / fan_count;
        let centre_x = fraction.mul_add(width, left);
        // Re-arm the fill every circle: the wedge and stroke below leave
        // their own source behind, which would otherwise freeze the
        // later circles at the wedge colour instead of tracking speed.
        cr.set_source_rgba(red, green, blue, 0.5_f64.mul_add(view.speed, 0.2));
        circle(cr, centre_x, view.slot.y, fan_radius.min(width / 2.0 - 4.0));
        let _ = cr.fill_preserve();
        cr.set_source_rgb(red, green, blue);
        cr.set_line_width(1.0);
        let _ = cr.stroke();
        // Same speed wedge as a plain fan circle, so one glance shows
        // how fast the cooler is running.
        if view.speed > 0.005 && fan_radius > 8.0 {
            cr.set_source_rgba(red, green, blue, 0.9);
            cr.move_to(centre_x, view.slot.y);
            cr.arc(
                centre_x,
                view.slot.y,
                fan_radius * 0.72,
                -std::f64::consts::FRAC_PI_2,
                TAU.mul_add(view.speed, -std::f64::consts::FRAC_PI_2),
            );
            cr.close_path();
            let _ = cr.fill();
        }
    }

    // Name near the top edge, percent near the bottom.
    cr.select_font_face(
        "sans-serif",
        cairo::FontSlant::Normal,
        cairo::FontWeight::Normal,
    );
    cr.set_source_rgb(0.85, 0.85, 0.88);
    let font_size = (height / 5.0).clamp(9.0, 15.0);
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to a small positive range first"
    )]
    let name_chars = (width / 7.0).clamp(4.0, 32.0) as usize;
    cr.set_font_size(font_size);
    let short_name = clip(&view.name, name_chars);
    let _ = draw_centered(cr, view.slot.x, top + font_size + 4.0, &short_name);
    let percent = format!("{:.0}%", view.speed * 100.0);
    let _ = draw_centered(cr, view.slot.x, top + height - 6.0, &percent);
    if let Some(rpm) = view.rpm {
        cr.set_font_size(10.0);
        let _ = draw_centered(
            cr,
            view.slot.x,
            top + height + 12.0,
            &format!("{rpm:.0} RPM"),
        );
    }
    if view.note.is_some() {
        cr.set_source_rgb(0.95, 0.75, 0.20);
        cr.set_font_size(12.0);
        let _ = draw_centered(cr, view.slot.x, top - 6.0, "⚠");
    }
}

/// Writes `text` centred on (`cx`, `baseline`); shared by the case view
/// and the curve graphs.
///
/// # Errors
///
/// [`cairo::Error`] if the text extents or drawing fails.
pub fn draw_centered(
    cr: &cairo::Context,
    cx: f64,
    baseline: f64,
    text: &str,
) -> Result<(), cairo::Error> {
    let extents = cr.text_extents(text)?;
    cr.move_to(cx - extents.width() / 2.0 - extents.x_bearing(), baseline);
    cr.show_text(text)?;
    Ok(())
}

fn circle(cr: &cairo::Context, cx: f64, cy: f64, radius: f64) {
    cr.new_path();
    cr.arc(cx, cy, radius.max(1.0), 0.0, TAU);
}

/// Draws a rounded rectangle path (does not fill or stroke).
fn rounded_rect(
    cr: &cairo::Context,
    origin_x: f64,
    origin_y: f64,
    rect_width: f64,
    rect_height: f64,
    corner_radius: f64,
) {
    cr.new_path();
    let r = corner_radius.min(rect_width / 2.0).min(rect_height / 2.0);
    let (x, y) = (origin_x, origin_y);
    cr.move_to(x + r, y);
    cr.arc(
        x + rect_width - r,
        y + r,
        r,
        -std::f64::consts::FRAC_PI_2,
        0.0,
    );
    cr.arc(
        x + rect_width - r,
        y + rect_height - r,
        r,
        0.0,
        std::f64::consts::FRAC_PI_2,
    );
    cr.arc(
        x + r,
        y + rect_height - r,
        r,
        std::f64::consts::FRAC_PI_2,
        std::f64::consts::PI,
    );
    cr.arc(
        x + r,
        y + r,
        r,
        std::f64::consts::PI,
        -std::f64::consts::FRAC_PI_2,
    );
    cr.close_path();
}

fn clip(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let mut short: String = text.chars().take(max_chars.saturating_sub(1)).collect();
        short.push('…');
        short
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_shortens_long_names_with_ellipsis() {
        assert_eq!(clip("Front 120mm intake", 6), "Front…");
        assert_eq!(clip("Rear", 10), "Rear");
    }

    #[test]
    fn clip_handles_multibyte_names() {
        assert_eq!(clip("Fan ünïcode name", 5), "Fan …");
    }

    #[test]
    fn fit_transform_centres_the_canvas_at_exact_fit() {
        let case = core::CaseConfig::default();
        let (scale, offset_x, offset_y) = fit_transform(320.0, 480.0, case);
        assert!((scale - 1.0).abs() < 1e-12);
        assert!((offset_x - 10.0).abs() < 1e-12);
        assert!((offset_y - 10.0).abs() < 1e-12);
    }

    #[test]
    fn fit_transform_scales_down_and_stays_centred() {
        let case = core::CaseConfig::default();
        let (scale, offset_x, offset_y) = fit_transform(160.0, 240.0, case);
        // Width binds: 140/300 < 220/460.
        assert!((scale - 140.0 / 300.0).abs() < 1e-9);
        assert!((offset_x - 10.0).abs() < 1e-9);
        assert!((offset_y - 460.0f64.mul_add(-scale, 240.0) / 2.0).abs() < 1e-9);
    }
}
