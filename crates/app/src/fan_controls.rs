//! Per-fan control rows in the GUI: name (rename), mode selector, speed
//! slider, sensor binding, curve preset, case-view placement, tachometer
//! correction and fan grouping.

use crate::dbus;
use biggest_fan_core as core;
use gtk4::prelude::*;
use gtk4::{
    Box as GtkBox, CheckButton, ComboBoxText, DrawingArea, Entry, Expander, Label, Orientation,
    Scale, SpinButton,
};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const MODE_OFF: &str = "off";
const MODE_FIXED: &str = "fixed";
const MODE_AUTO: &str = "auto";

/// Curve preset dropdown ids (plus "custom" for hand-made curves).
const CURVE_QUIET: &str = "quiet";
const CURVE_BALANCED: &str = "balanced";
const CURVE_PERFORMANCE: &str = "performance";
const CURVE_CUSTOM: &str = "custom";

/// One fan group's row: name, mode dropdown, speed slider, sensor
/// dropdown, curve preset, placement controls and a live status line.
pub struct FanControls {
    /// The row container, to be added to the window.
    pub container: GtkBox,
    fan_id: core::FanId,
    name_entry: Entry,
    mode_combo: ComboBoxText,
    speed_scale: Scale,
    sensor_combo: ComboBoxText,
    preset_combo: ComboBoxText,
    auto_row: GtkBox,
    airflow_combo: ComboBoxText,
    diameter_spin: SpinButton,
    rpm_spin: SpinButton,
    temp_label: Label,
    status_label: Label,
    /// Case canvas to repaint when placement changes.
    case_area: DrawingArea,
    /// Per-header checkboxes for driving headers together as one fan.
    member_checks: Rc<Vec<(core::FanId, CheckButton)>>,
    /// Set while the tick updates widgets so signal handlers do not write
    /// the same value straight back to the engine.
    suppress: Rc<Cell<bool>>,
    /// The running root daemon; every fan change goes through it.
    client: dbus::Client,
}

impl FanControls {
    /// Builds the row for one managed fan group; `engine_rc` is consulted
    /// by the signal handlers whenever the user changes a control,
    /// `case_area` is repainted when placement changes, and `client`
    /// mirrors mode and grouping changes to the running root service.
    #[must_use]
    pub fn new(
        engine_rc: &Rc<RefCell<core::Engine>>,
        id: &core::FanId,
        name: &str,
        case_area: &DrawingArea,
        client: &dbus::Client,
    ) -> Self {
        let container = GtkBox::new(Orientation::Vertical, 4);

        let header = GtkBox::new(Orientation::Horizontal, 8);
        // The fan's name is editable straight where it is shown; an empty
        // name falls back to the hardware-derived default. Renaming reaches
        // the service at once, and is saved with "Save config".
        let name_entry = Entry::builder()
            .text(name)
            .hexpand(true)
            .tooltip_text("rename this fan")
            .build();
        let mode_combo = ComboBoxText::new();
        mode_combo.append(Some(MODE_OFF), "Off");
        mode_combo.append(Some(MODE_FIXED), "Constant speed");
        mode_combo.append(Some(MODE_AUTO), "Automatic");
        header.append(&name_entry);
        header.append(&mode_combo);

        let speed_scale = Scale::builder()
            .orientation(Orientation::Horizontal)
            .draw_value(true)
            .digits(0)
            .round_digits(0)
            .hexpand(true)
            .build();
        speed_scale.set_range(0.0, 100.0);

        let sensor_combo = ComboBoxText::new();
        {
            let engine = engine_rc.borrow();
            for sensor in engine.sensors() {
                sensor_combo.append(Some(sensor.id.as_str()), &engine.sensor_name(&sensor.id));
            }
        }
        // Curve presets: one dropdown pick instead of hand-writing points.
        // "Custom" is display-only — a hand-made curve shows up here until
        // a preset replaces it.
        let preset_combo = ComboBoxText::new();
        preset_combo.append(Some(CURVE_QUIET), core::CurvePreset::Quiet.label());
        preset_combo.append(Some(CURVE_BALANCED), core::CurvePreset::Balanced.label());
        preset_combo.append(
            Some(CURVE_PERFORMANCE),
            core::CurvePreset::Performance.label(),
        );
        preset_combo.append(Some(CURVE_CUSTOM), "Custom");
        let auto_row = GtkBox::new(Orientation::Horizontal, 8);
        auto_row.append(&Label::builder().label("sensor:").build());
        auto_row.append(&sensor_combo);
        auto_row.append(&Label::builder().label("curve:").build());
        auto_row.append(&preset_combo);
        let temp_label = Label::builder().xalign(0.0).hexpand(true).build();
        auto_row.append(&temp_label);

        // Placement on the case drawing: drag the fan circle itself to
        // move it; airflow direction and diameter are set here.
        let airflow_combo = ComboBoxText::new();
        airflow_combo.append(Some("intake"), "intake");
        airflow_combo.append(Some("exhaust"), "exhaust");
        airflow_combo.append(Some("internal"), "internal");
        let diameter_spin = SpinButton::builder()
            .digits(0)
            .adjustment(&gtk4::Adjustment::new(120.0, 30.0, 300.0, 5.0, 10.0, 0.0))
            .build();
        let placement_row = Self::placement_controls(&airflow_combo, &diameter_spin);

        let member_checks = Self::member_check_buttons(engine_rc, id);
        let rpm_spin = Self::tach_spin();
        let extra_expander = Self::extras_expander(&rpm_spin, &member_checks);

        let status_label = Label::builder().xalign(0.0).build();
        status_label.set_markup("<span size=\"small\">…</span>");

        container.append(&header);
        container.append(&speed_scale);
        container.append(&auto_row);
        container.append(&placement_row);
        container.append(&status_label);
        container.append(&extra_expander);

        let controls = Self {
            container,
            fan_id: id.clone(),
            name_entry,
            mode_combo,
            speed_scale,
            sensor_combo,
            preset_combo,
            auto_row,
            airflow_combo,
            diameter_spin,
            rpm_spin,
            temp_label,
            status_label,
            case_area: case_area.clone(),
            member_checks: Rc::new(member_checks),
            suppress: Rc::new(Cell::new(false)),
            client: client.clone(),
        };
        controls.init_from_engine(engine_rc);
        controls.wire_signals(engine_rc);
        controls
    }

    /// Which fan this row controls.
    #[must_use]
    pub const fn fan_id(&self) -> &core::FanId {
        &self.fan_id
    }

    /// Builds one checkbox per fan header for driving headers together as
    /// one fan: the group leader is checked and locked, and headers already
    /// owned by another group are locked to that group.
    #[must_use]
    fn member_check_buttons(
        engine_rc: &RefCell<core::Engine>,
        id: &core::FanId,
    ) -> Vec<(core::FanId, CheckButton)> {
        let engine = engine_rc.borrow();
        engine
            .fan_headers()
            .into_iter()
            .map(|header| {
                let check = CheckButton::builder().label(&header.default_name).build();
                if header.id == *id {
                    // The group leader is always part of its own group.
                    check.set_active(true);
                    check.set_sensitive(false);
                } else if let Some(owner) = engine.fan_owner(&header.id) {
                    check.set_active(owner == *id);
                    check.set_sensitive(owner == *id);
                }
                (header.id, check)
            })
            .collect()
    }

    /// Builds the tachometer-correction spinner. Some tachometers report
    /// exactly double (a pulses-per-revolution mismatch): 0.5 halves the
    /// reading without touching hardware.
    #[must_use]
    fn tach_spin() -> SpinButton {
        let spin = SpinButton::builder()
            .digits(2)
            .adjustment(&gtk4::Adjustment::new(1.0, 0.05, 8.0, 0.05, 0.5, 0.0))
            .build();
        spin.set_tooltip_text(Some(
            "tach correction: multiply the RPM reading (0.5 halves a doubled tachometer)",
        ));
        spin
    }

    /// Wraps the tach correction and the group member checkboxes in a
    /// collapsed section, keeping the main row simple.
    #[must_use]
    fn extras_expander(
        rpm_spin: &SpinButton,
        member_checks: &[(core::FanId, CheckButton)],
    ) -> Expander {
        let rpm_row = GtkBox::new(Orientation::Horizontal, 8);
        rpm_row.append(&Label::builder().label("rpm reading ×").build());
        rpm_row.append(rpm_spin);

        let extra_box = GtkBox::new(Orientation::Vertical, 2);
        extra_box.append(&rpm_row);
        extra_box.append(
            &Label::builder()
                .label("drive extra headers as one fan")
                .xalign(0.0)
                .build(),
        );
        for (_, check) in member_checks {
            extra_box.append(check);
        }
        let expander = Expander::builder().label("extra settings").build();
        expander.set_child(Some(&extra_box));
        expander
    }

    /// Builds the placement editor row for airflow and diameter widgets.
    #[must_use]
    fn placement_controls(airflow_combo: &ComboBoxText, diameter_spin: &SpinButton) -> GtkBox {
        let row = GtkBox::new(Orientation::Horizontal, 8);
        row.append(&Label::builder().label("airflow:").build());
        row.append(airflow_combo);
        row.append(&Label::builder().label("diameter:").build());
        row.append(diameter_spin);
        row.append(
            &Label::builder()
                .label("· drag the circle on the drawing to move")
                .build(),
        );
        row
    }

    /// Syncs the widgets once the engine reports a fan's live state.
    pub fn update(&self, status: &core::FanStatus) {
        if matches!(status.mode, core::FanMode::Fixed { .. }) {
            self.suppress.set(true);
            self.speed_scale.set_value(status.commanded_speed * 100.0);
            self.suppress.set(false);
        }
        let speed = format!("{:.0}%", status.commanded_speed * 100.0);
        let rpm = status
            .measured_rpm
            .map_or_else(|| "no tach".to_string(), |rpm| format!("{rpm:.0} RPM"));
        let (line, colour) = status.note.as_deref().map_or_else(
            || (format!("{speed} · {rpm}"), None),
            |note| {
                (
                    format!("⚠ {note} — engine at {speed} · {rpm}"),
                    Some("#c0392b"),
                )
            },
        );
        let line = escape_markup(&line);
        let markup = colour.map_or_else(
            || format!("<span size=\"small\">{line}</span>"),
            |colour| format!("<span size=\"small\" foreground=\"{colour}\">{line}</span>"),
        );
        self.status_label.set_markup(&markup);

        if let core::FanMode::Auto { .. } = status.mode {
            let temp = status.governing_temp_c.map_or_else(
                || "sensor unread".to_string(),
                |temp| format!("{temp:.1} °C"),
            );
            self.temp_label.set_text(&temp);
        }
    }

    /// Sets the initial dropdown and placement state from the engine's
    /// current config.
    fn init_from_engine(&self, engine_rc: &Rc<RefCell<core::Engine>>) {
        let engine = engine_rc.borrow();
        let mode = engine.fan_mode(&self.fan_id).unwrap_or(core::FanMode::Off);
        match &mode {
            core::FanMode::Off => {
                self.mode_combo.set_active_id(Some(MODE_OFF));
            }
            core::FanMode::Fixed { speed } => {
                self.mode_combo.set_active_id(Some(MODE_FIXED));
                self.speed_scale.set_value(speed * 100.0);
            }
            core::FanMode::Auto { sensor, curve } => {
                self.mode_combo.set_active_id(Some(MODE_AUTO));
                self.sensor_combo.set_active_id(Some(sensor.as_str()));
                let preset_id = match core::CurvePreset::matching(curve) {
                    Some(core::CurvePreset::Quiet) => CURVE_QUIET,
                    Some(core::CurvePreset::Balanced) => CURVE_BALANCED,
                    Some(core::CurvePreset::Performance) => CURVE_PERFORMANCE,
                    None => CURVE_CUSTOM,
                };
                self.preset_combo.set_active_id(Some(preset_id));
            }
        }
        if let Some(factor) = engine.fan_rpm_factor(&self.fan_id) {
            self.rpm_spin.set_value(factor);
        }
        if let Some(slot) = engine.fan_slot(&self.fan_id) {
            let airflow = match slot.airflow {
                core::Airflow::Intake => "intake",
                core::Airflow::Exhaust => "exhaust",
                core::Airflow::Internal => "internal",
            };
            self.airflow_combo.set_active_id(Some(airflow));
            self.diameter_spin.set_value(slot.diameter);
        }
        drop(engine);
        self.refresh_visibility();
    }

    /// Shows only the controls relevant to the selected mode.
    fn refresh_visibility(&self) {
        let mode = self.mode_combo.active_id().map(|id| id.to_string());
        self.speed_scale
            .set_visible(mode.as_deref() == Some(MODE_FIXED));
        self.auto_row
            .set_visible(mode.as_deref() == Some(MODE_AUTO));
    }

    /// Connects all user controls to engine updates.
    fn wire_signals(&self, engine_rc: &Rc<RefCell<core::Engine>>) {
        self.wire_name_signals(engine_rc);
        self.wire_mode_signals(engine_rc);
        self.wire_curve_preset_signal(engine_rc);
        self.wire_placement_signals(engine_rc);
        self.wire_group_signals(engine_rc);
        self.wire_rpm_signal(engine_rc);
    }

    /// Renaming updates the mirror (rows and case view follow the engine's
    /// display name) and tells the service so the saved config agrees.
    fn wire_name_signals(&self, engine_rc: &Rc<RefCell<core::Engine>>) {
        let rc = Rc::clone(engine_rc);
        let suppress = Rc::clone(&self.suppress);
        let fan_id = self.fan_id.clone();
        let client = self.client.clone();
        let status_label = self.status_label.clone();
        self.name_entry.connect_changed(move |entry| {
            if suppress.get() {
                return;
            }
            let name = entry.text().to_string();
            let mut engine = rc.borrow_mut();
            if let Err(error) = engine.set_fan_name(&fan_id, &name) {
                status_label.set_markup(&format!(
                    "<span size=\"small\" foreground=\"#c0392b\">⚠ {}</span>",
                    escape_markup(&error.to_string())
                ));
                return;
            }
            drop(engine);
            client.send(dbus::FanRequest::SetName {
                fan: fan_id.to_string(),
                name,
            });
        });
    }

    /// Connects the mode dropdown, speed slider and sensor dropdown.
    fn wire_mode_signals(&self, engine_rc: &Rc<RefCell<core::Engine>>) {
        // Mode dropdown: rebuild the whole mode from all three widgets.
        let rc = Rc::clone(engine_rc);
        let suppress = Rc::clone(&self.suppress);
        let mode_combo = self.mode_combo.clone();
        let scale = self.speed_scale.clone();
        let sensor_combo = self.sensor_combo.clone();
        let auto_row = self.auto_row.clone();
        let status_label = self.status_label.clone();
        let fan_id = self.fan_id.clone();
        let client = self.client.clone();
        self.mode_combo.connect_changed(move |_| {
            if suppress.get() {
                return;
            }
            let curve_engine = Rc::clone(&rc);
            let curve_fan = fan_id.clone();
            let mode = read_mode(&mode_combo, &scale, &sensor_combo, move || {
                existing_curve(&curve_engine.borrow(), &curve_fan)
            });
            push_mode(&rc, &fan_id, mode, &status_label, &client);
            scale.set_visible(mode_combo.active_id().as_deref() == Some(MODE_FIXED));
            auto_row.set_visible(mode_combo.active_id().as_deref() == Some(MODE_AUTO));
        });

        // Speed slider: only meaningful (and only visible) in fixed mode.
        let rc = Rc::clone(engine_rc);
        let suppress = Rc::clone(&self.suppress);
        let mode_combo = self.mode_combo.clone();
        let sensor_combo = self.sensor_combo.clone();
        let status_label = self.status_label.clone();
        let fan_id = self.fan_id.clone();
        let client = self.client.clone();
        self.speed_scale.connect_value_changed(move |scale| {
            if suppress.get() || mode_combo.active_id().as_deref() != Some(MODE_FIXED) {
                return;
            }
            let mode = read_mode(&mode_combo, scale, &sensor_combo, core::FanCurve::default);
            push_mode(&rc, &fan_id, mode, &status_label, &client);
        });

        // Sensor dropdown: rebinds an auto fan, keeping its curve.
        let rc = Rc::clone(engine_rc);
        let suppress = Rc::clone(&self.suppress);
        let mode_combo = self.mode_combo.clone();
        let scale = self.speed_scale.clone();
        let sensor_combo = self.sensor_combo.clone();
        let status_label = self.status_label.clone();
        let fan_id = self.fan_id.clone();
        let client = self.client.clone();
        self.sensor_combo.connect_changed(move |_| {
            if suppress.get() || mode_combo.active_id().as_deref() != Some(MODE_AUTO) {
                return;
            }
            let curve_engine = Rc::clone(&rc);
            let curve_fan = fan_id.clone();
            let mode = read_mode(&mode_combo, &scale, &sensor_combo, move || {
                existing_curve(&curve_engine.borrow(), &curve_fan)
            });
            push_mode(&rc, &fan_id, mode, &status_label, &client);
        });
    }

    /// A preset replaces an auto fan's curve, keeping its sensor. Hand-
    /// made ("custom") curves are never overwritten until one is picked.
    fn wire_curve_preset_signal(&self, engine_rc: &Rc<RefCell<core::Engine>>) {
        let rc = Rc::clone(engine_rc);
        let suppress = Rc::clone(&self.suppress);
        let mode_combo = self.mode_combo.clone();
        let sensor_combo = self.sensor_combo.clone();
        let status_label = self.status_label.clone();
        let fan_id = self.fan_id.clone();
        let client = self.client.clone();
        self.preset_combo.connect_changed(move |combo| {
            if suppress.get() || mode_combo.active_id().as_deref() != Some(MODE_AUTO) {
                return;
            }
            let preset = match combo.active_id().as_deref() {
                Some(CURVE_QUIET) => core::CurvePreset::Quiet,
                Some(CURVE_BALANCED) => core::CurvePreset::Balanced,
                Some(CURVE_PERFORMANCE) => core::CurvePreset::Performance,
                _ => return, // "custom": display-only, nothing to apply
            };
            let Some(sensor_id) = sensor_combo.active_id() else {
                return;
            };
            let mode = Some(core::FanMode::Auto {
                sensor: core::SensorId::new(sensor_id.to_string()),
                curve: preset.curve(),
            });
            push_mode(&rc, &fan_id, mode, &status_label, &client);
        });
    }

    /// Connects the placement controls to the fan group's case-view slot.
    /// (Dragging on the canvas moves the circle itself; see `case_view`.)
    fn wire_placement_signals(&self, engine_rc: &Rc<RefCell<core::Engine>>) {
        self.airflow_combo.connect_changed({
            let rc = Rc::clone(engine_rc);
            let airflow_combo = self.airflow_combo.clone();
            let diameter_spin = self.diameter_spin.clone();
            let case_area = self.case_area.clone();
            let status_label = self.status_label.clone();
            let fan_id = self.fan_id.clone();
            move |_| {
                push_slot(
                    &rc,
                    &fan_id,
                    &airflow_combo,
                    &diameter_spin,
                    &case_area,
                    &status_label,
                );
            }
        });
        self.diameter_spin.connect_value_changed({
            let rc = Rc::clone(engine_rc);
            let airflow_combo = self.airflow_combo.clone();
            let case_area = self.case_area.clone();
            let status_label = self.status_label.clone();
            let fan_id = self.fan_id.clone();
            move |spin| {
                push_slot(
                    &rc,
                    &fan_id,
                    &airflow_combo,
                    spin,
                    &case_area,
                    &status_label,
                );
            }
        });
    }

    /// Connects the tachometer correction spin: measured RPM gets
    /// multiplied by the factor, both here and at the service.
    fn wire_rpm_signal(&self, engine_rc: &Rc<RefCell<core::Engine>>) {
        let rc = Rc::clone(engine_rc);
        let fan_id = self.fan_id.clone();
        let client = self.client.clone();
        self.rpm_spin.connect_value_changed(move |spin| {
            let factor = spin.value();
            let mut engine = rc.borrow_mut();
            if engine.set_fan_rpm_factor(&fan_id, factor).is_err() {
                return;
            }
            drop(engine);
            client.send(dbus::FanRequest::SetRpmFactor {
                fan: fan_id.to_string(),
                factor,
            });
        });
    }

    /// Connects the group checkboxes: whatever is ticked drives this fan
    /// as one logical fan.
    fn wire_group_signals(&self, engine_rc: &Rc<RefCell<core::Engine>>) {
        for (_header_id, check) in self.member_checks.iter() {
            let rc = Rc::clone(engine_rc);
            let checks = Rc::clone(&self.member_checks);
            let status_label = self.status_label.clone();
            let fan_id = self.fan_id.clone();
            let client = self.client.clone();
            check.connect_toggled(move |_| {
                let members: Vec<core::FanId> = checks
                    .iter()
                    .filter(|(_, check)| check.is_active())
                    .map(|(member_id, _)| member_id.clone())
                    .collect();
                let mut engine = rc.borrow_mut();
                if let Err(error) = engine.set_fan_members(&fan_id, &members) {
                    status_label.set_markup(&format!(
                        "<span size=\"small\" foreground=\"#c0392b\">⚠ {}</span>",
                        escape_markup(&error.to_string())
                    ));
                    return;
                }
                drop(engine);
                client.send(dbus::FanRequest::SetMembers {
                    leader: fan_id.to_string(),
                    members: members.iter().map(ToString::to_string).collect(),
                });
            });
        }
    }
}

/// Copies the placement widgets' values into the fan group's case-view
/// slot, keeping the position chosen by dragging on the drawing.
fn push_slot(
    engine_rc: &Rc<RefCell<core::Engine>>,
    fan_id: &core::FanId,
    airflow_combo: &ComboBoxText,
    diameter_spin: &SpinButton,
    case_area: &DrawingArea,
    status_label: &Label,
) {
    let airflow = match airflow_combo.active_id().as_deref() {
        Some("intake") => core::Airflow::Intake,
        Some("exhaust") => core::Airflow::Exhaust,
        _ => core::Airflow::Internal,
    };
    let diameter = diameter_spin.value().clamp(10.0, 400.0);
    let mut engine = engine_rc.borrow_mut();
    let Some(current) = engine.fan_slot(fan_id) else {
        return;
    };
    let slot = core::FanSlot {
        x: current.x,
        y: current.y,
        diameter,
        airflow,
    };
    if let Err(error) = engine.set_fan_slot(fan_id, slot) {
        status_label.set_markup(&format!(
            "<span size=\"small\" foreground=\"#c0392b\">⚠ {}</span>",
            escape_markup(&error.to_string())
        ));
    }
    drop(engine);
    case_area.queue_draw();
}

/// Assembles the mode the dropdowns currently describe.
fn read_mode(
    mode_combo: &ComboBoxText,
    scale: &Scale,
    sensor_combo: &ComboBoxText,
    curve: impl Fn() -> core::FanCurve,
) -> Option<core::FanMode> {
    match mode_combo.active_id().as_deref() {
        Some(MODE_OFF) => Some(core::FanMode::Off),
        Some(MODE_FIXED) => Some(core::FanMode::Fixed {
            speed: scale.value() / 100.0,
        }),
        Some(MODE_AUTO) => sensor_combo.active_id().map(|id| core::FanMode::Auto {
            sensor: core::SensorId::new(id.to_string()),
            curve: curve(),
        }),
        _ => None,
    }
}

fn push_mode(
    engine_rc: &Rc<RefCell<core::Engine>>,
    fan_id: &core::FanId,
    mode: Option<core::FanMode>,
    status_label: &Label,
    client: &dbus::Client,
) {
    let Some(mode) = mode else { return };
    let mut engine = engine_rc.borrow_mut();
    if let Err(error) = engine.set_fan_mode(fan_id, &mode) {
        status_label.set_markup(&format!(
            "<span size=\"small\" foreground=\"#c0392b\">⚠ {}</span>",
            escape_markup(&error.to_string())
        ));
        return;
    }
    drop(engine);
    // The running root service drives the headers: mirror the change there.
    match serde_json::to_string(&mode) {
        Ok(mode_json) => client.send(dbus::FanRequest::SetFanMode {
            fan: fan_id.to_string(),
            mode_json,
        }),
        Err(error) => status_label.set_markup(&format!(
            "<span size=\"small\" foreground=\"#c0392b\">⚠ could not send mode to service: {error}</span>",
        )),
    }
}

fn existing_curve(engine: &core::Engine, fan_id: &core::FanId) -> core::FanCurve {
    engine
        .fan_mode(fan_id)
        .and_then(|mode| match mode {
            core::FanMode::Auto { curve, .. } => Some(curve),
            _ => None,
        })
        .unwrap_or_default()
}

fn escape_markup(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_markup_neutralises_pango_tags() {
        assert_eq!(
            escape_markup("Corsair <H100i> & RGB"),
            "Corsair &lt;H100i&gt; &amp; RGB"
        );
    }
}
