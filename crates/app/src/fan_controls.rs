//! Per-fan control rows in the GUI: name (rename), mode selector, speed
//! slider, sensor binding, curve preset, case-view placement, tachometer
//! correction and fan grouping.

use crate::curve_graph::CurveGraph;
use crate::dbus;
use crate::trend_graph::TrendGraph;
use biggest_fan_core as core;
use gtk4::prelude::*;
use gtk4::{
    Box as GtkBox, Button, CheckButton, ComboBoxText, DrawingArea, Entry, Expander, Label,
    Orientation, Scale, SpinButton,
};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

const MODE_OFF: &str = "off";
const MODE_FIXED: &str = "fixed";
const MODE_AUTO: &str = "auto";

/// Curve preset dropdown ids (plus "custom" for hand-made curves).
const CURVE_QUIET: &str = "quiet";
const CURVE_BALANCED: &str = "balanced";
const CURVE_PERFORMANCE: &str = "performance";
const CURVE_CUSTOM: &str = "custom";

/// Case-view slot shapes: a circle per fan header, or a rectangle for a
/// whole object such as a graphics-card cooler.
const SHAPE_CIRCLE: &str = "circle";
const SHAPE_RECTANGLE: &str = "rectangle";

/// Style class that turns each fan row into a subtle tile. The colours
/// are translucent tints of the theme's own foreground, so the tile
/// reads softly on both light and dark themes.
pub const FAN_TILE_CLASS: &str = "fan-tile";

/// Style class for the panel container that lays the fan tiles out as
/// a flow: one tile per line on a 1080p-sized panel, two side by side
/// once it is wide enough (the two graphs per tile set the break).
pub const FAN_FLOW_CLASS: &str = "fan-flow";

/// The stylesheet [`FAN_TILE_CLASS`] and [`FAN_FLOW_CLASS`] expect,
/// installed on the GUI's display when a window is built.
pub const FAN_TILE_CSS: &str = "
.fan-tile {
    background-color: alpha(@theme_fg_color, 0.05);
    border: 1px solid alpha(@theme_fg_color, 0.16);
    border-radius: 10px;
    padding: 10px 12px;
}

.fan-flow > flowboxchild {
    padding: 6px;
}
";

/// Placement widgets bundled so any one change can push the whole slot.
struct PlacementWidgets {
    airflow_combo: ComboBoxText,
    shape_combo: ComboBoxText,
    diameter_spin: SpinButton,
    width_spin: SpinButton,
    height_spin: SpinButton,
}

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
    /// Curve graph shown under the auto controls: the fan's curve plus
    /// its live operating point.
    curve_graph: CurveGraph,
    /// History graph beside the curve graph: temperature and speed over
    /// the last few minutes.
    trend_graph: TrendGraph,
    /// Monotonic clock behind the trend graph's sample times (every
    /// sample for this row shares it, so they stay ordered).
    clock: Instant,
    /// Airflow, shape and size widgets for the case-view slot.
    placement: Rc<PlacementWidgets>,
    /// Shape-dependent visibility partners of the placement widgets.
    diameter_label: Label,
    size_row: GtkBox,
    rpm_spin: SpinButton,
    /// Speed floor spin: a fan can never be commanded below this.
    floor_spin: SpinButton,
    temp_label: Label,
    status_label: Label,
    /// What the nudger has learned for this fan, and the button that
    /// folds that learning into the curve; hidden while there is nothing
    /// learned.
    nudge_row: GtkBox,
    nudge_label: Label,
    fold_button: Button,
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
        container.add_css_class(FAN_TILE_CLASS);

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

        let sensor_combo = sensor_dropdown(&engine_rc.borrow(), id);
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
        // Show what switching an off/fixed fan to Automatic will actually
        // apply: the first sensor in the (ranked) list and the default
        // curve. `init_from_engine` overwrites both for auto fans.
        sensor_combo.set_active(Some(0));
        preset_combo.set_active_id(Some(CURVE_BALANCED));
        let auto_row = GtkBox::new(Orientation::Horizontal, 8);
        auto_row.append(&Label::builder().label("Sensor:").build());
        auto_row.append(&sensor_combo);
        auto_row.append(&Label::builder().label("Curve:").build());
        auto_row.append(&preset_combo);
        let temp_label = Label::builder().xalign(0.0).hexpand(true).build();
        auto_row.append(&temp_label);

        // The fan's curve drawn against sensor temperature, with a dot
        // for where the fan is now; only shown in auto mode.
        let curve_graph = CurveGraph::new(engine_rc, id);

        // The fan's recent history beside the curve: speed % and
        // governing sensor °C over the last few minutes, every mode.
        let trend_graph = TrendGraph::new();
        let graphs_row = GtkBox::new(Orientation::Horizontal, 8);
        graphs_row.append(curve_graph.widget());
        graphs_row.append(trend_graph.widget());

        // Placement on the case drawing: drag the shape itself to move
        // it; airflow, shape (circle ↔ whole graphics card) and size are
        // set here.
        let (placement, diameter_label, size_row) = Self::placement_widgets();
        let placement_row = Self::placement_controls(
            &placement.airflow_combo,
            &placement.shape_combo,
            &diameter_label,
            &placement.diameter_spin,
            &size_row,
        );

        let member_checks = Self::member_check_buttons(engine_rc, id);
        let rpm_spin = Self::tach_spin();
        let floor_spin = Self::floor_spin();
        let extra_expander = Self::extras_expander(&rpm_spin, &floor_spin, &member_checks);

        let status_label = Label::builder().xalign(0.0).build();
        status_label.set_markup("<span size=\"small\">…</span>");

        // The nudger's learning for this fan: how far below its curve the
        // fan now runs, and the offer to make that part of the curve.
        let (nudge_row, nudge_label, fold_button) = Self::nudge_controls();

        container.append(&header);
        container.append(&speed_scale);
        container.append(&auto_row);
        container.append(&graphs_row);
        container.append(&nudge_row);
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
            curve_graph,
            trend_graph,
            clock: Instant::now(),
            placement: Rc::new(placement),
            diameter_label,
            size_row,
            rpm_spin,
            floor_spin,
            temp_label,
            status_label,
            nudge_row,
            nudge_label,
            fold_button,
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

    /// Takes this fan's row out of the panel: inside the flow the cell
    /// wrapper is what occupies a layout slot, so hide that too.
    pub fn hide_row(&self) {
        self.container.set_visible(false);
        if let Some(cell) = self
            .container
            .parent()
            .filter(|parent| parent.downcast_ref::<gtk4::FlowBoxChild>().is_some())
        {
            cell.set_visible(false);
        }
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

    /// Builds the airflow, shape and size widgets for the placement row,
    /// with the labels and spin row whose visibility depends on the
    /// shape.
    #[must_use]
    fn placement_widgets() -> (PlacementWidgets, Label, GtkBox) {
        let airflow_combo = ComboBoxText::new();
        airflow_combo.append(Some("intake"), "intake");
        airflow_combo.append(Some("exhaust"), "exhaust");
        airflow_combo.append(Some("internal"), "internal");
        let shape_combo = ComboBoxText::new();
        shape_combo.append(Some(SHAPE_CIRCLE), "circle");
        shape_combo.append(Some(SHAPE_RECTANGLE), "rectangle");
        shape_combo.set_tooltip_text(Some(
            "a rectangle draws a whole object (graphics card cooler) sized by width and height",
        ));
        let diameter_label = Label::builder().label("Diameter:").build();
        let diameter_spin = SpinButton::builder()
            .digits(0)
            .adjustment(&gtk4::Adjustment::new(120.0, 30.0, 300.0, 5.0, 10.0, 0.0))
            .build();
        let width_spin = Self::size_spin("card width on the drawing");
        let height_spin = Self::size_spin("card height on the drawing");
        let size_row = GtkBox::new(Orientation::Horizontal, 4);
        size_row.append(&Label::builder().label("Card w:").build());
        size_row.append(&width_spin);
        size_row.append(&Label::builder().label("H:").build());
        size_row.append(&height_spin);
        (
            PlacementWidgets {
                airflow_combo,
                shape_combo,
                diameter_spin,
                width_spin,
                height_spin,
            },
            diameter_label,
            size_row,
        )
    }

    /// A width or height spin for a rectangular (graphics-card) slot.
    #[must_use]
    fn size_spin(tooltip: &str) -> SpinButton {
        let spin = SpinButton::builder()
            .digits(0)
            .adjustment(&gtk4::Adjustment::new(120.0, 10.0, 1200.0, 5.0, 10.0, 0.0))
            .build();
        spin.set_tooltip_text(Some(tooltip));
        spin
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

    /// The learned-trim caption and the button that folds the learning
    /// into the curve; the row starts hidden until there is a trim.
    fn nudge_controls() -> (GtkBox, Label, Button) {
        let nudge_label = Label::builder().xalign(0.0).hexpand(true).build();
        let fold_button = Button::builder()
            .label("Apply learned trim to curve")
            .tooltip_text(
                "write the learned speed reduction into this fan's curve and forget it; \
                 the fan keeps running at exactly the same speed",
            )
            .build();
        let nudge_row = GtkBox::new(Orientation::Horizontal, 8);
        nudge_row.append(&nudge_label);
        nudge_row.append(&fold_button);
        nudge_row.set_visible(false);
        (nudge_row, nudge_label, fold_button)
    }

    /// Builds the speed-floor spinner, in whole percents. A fan with a floor
    /// — an AIO pump, or a case fan that must keep the airflow going — can
    /// never be taken below it by a curve or a ramp, though switching it to
    /// _off_ still stops it.
    #[must_use]
    fn floor_spin() -> SpinButton {
        let spin = SpinButton::builder()
            .digits(0)
            .numeric(true)
            .adjustment(&gtk4::Adjustment::new(0.0, 0.0, 100.0, 5.0, 10.0, 0.0))
            .build();
        spin.set_tooltip_text(Some(
            "never command this fan below this speed — set it for an AIO pump; _off_ still stops it",
        ));
        spin
    }

    /// Wraps the tach correction, the speed floor and the group member
    /// checkboxes in a collapsed section, keeping the main row simple.
    #[must_use]
    fn extras_expander(
        rpm_spin: &SpinButton,
        floor_spin: &SpinButton,
        member_checks: &[(core::FanId, CheckButton)],
    ) -> Expander {
        let rpm_row = GtkBox::new(Orientation::Horizontal, 8);
        rpm_row.append(&Label::builder().label("RPM reading ×").build());
        rpm_row.append(rpm_spin);

        let floor_row = GtkBox::new(Orientation::Horizontal, 8);
        floor_row.append(&Label::builder().label("Never below %").build());
        floor_row.append(floor_spin);

        let extra_box = GtkBox::new(Orientation::Vertical, 2);
        extra_box.append(&floor_row);
        extra_box.append(&rpm_row);
        extra_box.append(
            &Label::builder()
                .label("Drive extra headers as one fan")
                .xalign(0.0)
                .build(),
        );
        for (_, check) in member_checks {
            extra_box.append(check);
        }
        let expander = Expander::builder().label("Extra settings").build();
        expander.set_child(Some(&extra_box));
        expander
    }

    /// Builds the placement editor row: airflow, slot shape, and the
    /// diameter (circle) or width/height (rectangle) widgets.
    #[must_use]
    fn placement_controls(
        airflow_combo: &ComboBoxText,
        shape_combo: &ComboBoxText,
        diameter_label: &Label,
        diameter_spin: &SpinButton,
        size_row: &GtkBox,
    ) -> GtkBox {
        let row = GtkBox::new(Orientation::Horizontal, 8);
        row.append(&Label::builder().label("Airflow:").build());
        row.append(airflow_combo);
        row.append(&Label::builder().label("Shape:").build());
        row.append(shape_combo);
        row.append(diameter_label);
        row.append(diameter_spin);
        row.append(size_row);
        row.append(
            &Label::builder()
                .label("· drag the shape on the drawing to move")
                .build(),
        );
        row
    }

    /// Syncs the widgets once the engine reports a fan's live state.
    /// `engine` is the GUI's mirror, consulted for the nudger's learning.
    pub fn update(&self, status: &core::FanStatus, engine: &core::Engine) {
        self.trend_graph
            .record(status, self.clock.elapsed().as_secs_f64());
        if let core::FanMode::Fixed { speed } = &status.mode {
            // Follow the requested speed, not `commanded_speed`: NVIDIA
            // coolers only run at 30 % or above and park under 25 %, and
            // the slider must not snap the user's setting down to 0 while
            // they are watching it.
            self.suppress.set(true);
            self.speed_scale.set_value(speed.clamp(0.0, 1.0) * 100.0);
            self.suppress.set(false);
        }
        let speed = format!("{:.0}%", status.commanded_speed * 100.0);
        // What the hardware reports: RPM where there is a tachometer, and the
        // speed the header reads back where there is none (a GPU cooler's
        // parking becomes visible this way, since it has no tach).
        let rpm = match (status.measured_rpm, status.measured_percent) {
            (Some(rpm), _) => format!("{rpm:.0} RPM"),
            (None, Some(percent)) => format!("no tach, header at {:.0}%", percent * 100.0),
            (None, None) => "no tach".to_string(),
        };
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

        if let core::FanMode::Auto { curve, .. } = &status.mode {
            let temp = status.governing_temp_c.map_or_else(
                || "sensor unread".to_string(),
                |temp| format!("{temp:.1} °C"),
            );
            self.temp_label.set_text(&temp);
            self.curve_graph
                .set_operating_point(status.governing_temp_c, status.commanded_speed);
            // Keep the preset dropdown honest even when the curve changed
            // without the user touching it (a folded nudge trim makes a
            // Custom curve).
            self.suppress.set(true);
            self.preset_combo
                .set_active_id(Some(preset_combo_id(curve)));
            self.suppress.set(false);
            self.show_nudge_trim(engine);
        } else {
            self.nudge_row.set_visible(false);
        }
    }

    /// Shows how far below its curve the nudger has this fan running, and
    /// offers to fold that learning into the curve.
    fn show_nudge_trim(&self, engine: &core::Engine) {
        let trim = engine.fan_nudge_trim(&self.fan_id);
        if trim <= 0.0 {
            self.nudge_row.set_visible(false);
            return;
        }
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the engine's trim is a fraction of speed between 0 and 1, so this is a small positive percent"
        )]
        let percent = (trim * 100.0).round() as u32;
        self.nudge_label.set_markup(&format!(
            "<span size=\"small\">{percent}% slower than its curve</span>"
        ));
        self.fold_button
            .set_label(&format!("Apply learned −{percent}% to curve"));
        self.nudge_row.set_visible(true);
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
                // A hidden sensor this fan is bound to still gets a row
                // (see `sensor_dropdown`), so this always lands.
                self.sensor_combo.set_active_id(Some(sensor.as_str()));
                self.preset_combo
                    .set_active_id(Some(preset_combo_id(curve)));
            }
        }
        if let Some(factor) = engine.fan_rpm_factor(&self.fan_id) {
            self.rpm_spin.set_value(factor);
        }
        if let Some(floor) = engine.fan_min_speed(&self.fan_id) {
            self.floor_spin.set_value(floor * 100.0);
        }
        if let Some(slot) = engine.fan_slot(&self.fan_id) {
            let airflow = match slot.airflow {
                core::Airflow::Intake => "intake",
                core::Airflow::Exhaust => "exhaust",
                core::Airflow::Internal => "internal",
            };
            self.placement.airflow_combo.set_active_id(Some(airflow));
            self.placement.diameter_spin.set_value(slot.diameter);
            if let Some((width, height)) = slot.rect {
                self.placement
                    .shape_combo
                    .set_active_id(Some(SHAPE_RECTANGLE));
                self.placement.width_spin.set_value(width);
                self.placement.height_spin.set_value(height);
            } else {
                self.placement.shape_combo.set_active_id(Some(SHAPE_CIRCLE));
                // Keep the size spins sensible if the user switches
                // this fan to a card later.
                self.placement.width_spin.set_value(slot.diameter);
                self.placement.height_spin.set_value(slot.diameter);
            }
        }
        drop(engine);
        self.refresh_visibility();
        self.refresh_placement_visibility();
    }

    /// Shows only the size widgets the selected shape uses.
    fn refresh_placement_visibility(&self) {
        let rectangle = self.placement.shape_combo.active_id().as_deref() == Some(SHAPE_RECTANGLE);
        self.diameter_label.set_visible(!rectangle);
        self.placement.diameter_spin.set_visible(!rectangle);
        self.size_row.set_visible(rectangle);
    }

    /// Shows only the controls relevant to the selected mode.
    fn refresh_visibility(&self) {
        let mode = self.mode_combo.active_id().map(|id| id.to_string());
        self.speed_scale
            .set_visible(mode.as_deref() == Some(MODE_FIXED));
        self.auto_row
            .set_visible(mode.as_deref() == Some(MODE_AUTO));
        self.curve_graph
            .widget()
            .set_visible(mode.as_deref() == Some(MODE_AUTO));
    }

    /// Connects all user controls to engine updates.
    fn wire_signals(&self, engine_rc: &Rc<RefCell<core::Engine>>) {
        self.wire_name_signals(engine_rc);
        self.wire_mode_signals(engine_rc);
        self.wire_curve_preset_signal(engine_rc);
        self.wire_nudge_signal(engine_rc);
        self.wire_placement_signals(engine_rc);
        self.wire_group_signals(engine_rc);
        self.wire_rpm_signal(engine_rc);
        self.wire_floor_signal(engine_rc);
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
        let preset_combo = self.preset_combo.clone();
        let auto_row = self.auto_row.clone();
        let curve_area = self.curve_graph.widget().clone();
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
            curve_area.set_visible(mode_combo.active_id().as_deref() == Some(MODE_AUTO));
            // An auto fan may have been given the default curve just now
            // (off → auto) or lost a hand-made one (off → auto after an
            // earlier custom curve); show the preset that is really in
            // force so the dropdown never lies about the profile.
            let applied = match rc.borrow().fan_mode(&fan_id) {
                Some(core::FanMode::Auto { curve, .. }) => Some(preset_combo_id(&curve)),
                _ => None,
            };
            if let Some(preset_id) = applied {
                suppress.set(true);
                preset_combo.set_active_id(Some(preset_id));
                suppress.set(false);
            }
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
        let curve_area = self.curve_graph.widget().clone();
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
            // The draw closure re-reads the curve from the engine, so
            // one redraw shows the new preset at once.
            curve_area.queue_draw();
        });
    }

    /// "Apply learned −X% to curve" moves the nudger's learning into the
    /// mirror's curve straight away (the graph and preset follow at once),
    /// then asks the service to do the same and save it; the periodic
    /// config poll brings the service's answer back to this mirror.
    fn wire_nudge_signal(&self, engine_rc: &Rc<RefCell<core::Engine>>) {
        let rc = Rc::clone(engine_rc);
        let fan_id = self.fan_id.clone();
        let client = self.client.clone();
        let status_label = self.status_label.clone();
        let curve_area = self.curve_graph.widget().clone();
        let nudge_row = self.nudge_row.clone();
        self.fold_button.connect_clicked(move |_| {
            // A tick may own the engine borrow; skipping this click is
            // fine — the row is still there for the next one.
            let Ok(mut engine) = rc.try_borrow_mut() else {
                return;
            };
            if let Err(error) = engine.fold_fan_nudge_trim(&fan_id) {
                status_label.set_markup(&format!(
                    "<span size=\"small\" foreground=\"#c0392b\">⚠ {}</span>",
                    escape_markup(&error.to_string())
                ));
                return;
            }
            drop(engine);
            nudge_row.set_visible(false);
            curve_area.queue_draw();
            client.send(dbus::FanRequest::FoldNudgeTrim {
                fan: fan_id.to_string(),
            });
        });
    }

    /// Connects the placement controls to the fan group's case-view slot.
    /// Every control pushes the whole slot, so switching shape (circle ↔
    /// card) converts the drawing in place. (Dragging on the canvas moves
    /// the shape itself; see `case_view`.)
    fn wire_placement_signals(&self, engine_rc: &Rc<RefCell<core::Engine>>) {
        self.placement.airflow_combo.connect_changed({
            let rc = Rc::clone(engine_rc);
            let placement = Rc::clone(&self.placement);
            let case_area = self.case_area.clone();
            let status_label = self.status_label.clone();
            let fan_id = self.fan_id.clone();
            move |_| {
                push_slot(&rc, &fan_id, &placement, &case_area, &status_label);
            }
        });
        self.placement.shape_combo.connect_changed({
            let rc = Rc::clone(engine_rc);
            let placement = Rc::clone(&self.placement);
            let diameter_label = self.diameter_label.clone();
            let size_row = self.size_row.clone();
            let case_area = self.case_area.clone();
            let status_label = self.status_label.clone();
            let fan_id = self.fan_id.clone();
            move |_| {
                let rectangle =
                    placement.shape_combo.active_id().as_deref() == Some(SHAPE_RECTANGLE);
                diameter_label.set_visible(!rectangle);
                size_row.set_visible(rectangle);
                push_slot(&rc, &fan_id, &placement, &case_area, &status_label);
            }
        });
        for spin in [
            &self.placement.diameter_spin,
            &self.placement.width_spin,
            &self.placement.height_spin,
        ] {
            spin.connect_value_changed({
                let rc = Rc::clone(engine_rc);
                let placement = Rc::clone(&self.placement);
                let case_area = self.case_area.clone();
                let status_label = self.status_label.clone();
                let fan_id = self.fan_id.clone();
                move |_spin| {
                    push_slot(&rc, &fan_id, &placement, &case_area, &status_label);
                }
            });
        }
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

    /// Connects the speed-floor spin: the engine may never command this fan
    /// below the floor, both here and at the service.
    fn wire_floor_signal(&self, engine_rc: &Rc<RefCell<core::Engine>>) {
        let rc = Rc::clone(engine_rc);
        let fan_id = self.fan_id.clone();
        let client = self.client.clone();
        self.floor_spin.connect_value_changed(move |spin| {
            let speed = spin.value() / 100.0;
            let mut engine = rc.borrow_mut();
            if engine.set_fan_min_speed(&fan_id, speed).is_err() {
                return;
            }
            drop(engine);
            client.send(dbus::FanRequest::SetMinSpeed {
                fan: fan_id.to_string(),
                speed,
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
    placement: &PlacementWidgets,
    case_area: &DrawingArea,
    status_label: &Label,
) {
    let airflow = match placement.airflow_combo.active_id().as_deref() {
        Some("intake") => core::Airflow::Intake,
        Some("exhaust") => core::Airflow::Exhaust,
        _ => core::Airflow::Internal,
    };
    let diameter = placement.diameter_spin.value().clamp(10.0, 400.0);
    let rect = (placement.shape_combo.active_id().as_deref() == Some(SHAPE_RECTANGLE)).then(|| {
        (
            placement.width_spin.value().clamp(10.0, 1200.0),
            placement.height_spin.value().clamp(10.0, 1200.0),
        )
    });
    let mut engine = engine_rc.borrow_mut();
    let Some(current) = engine.fan_slot(fan_id) else {
        return;
    };
    let slot = core::FanSlot {
        x: current.x,
        y: current.y,
        diameter,
        rect,
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

/// Sensor binding dropdown: every temperature, minus the hidden ones
/// ([`core::Engine::sensor_hidden`]) except the sensor this fan is
/// already bound to — dropping that row would silently change a live
/// binding.
fn sensor_dropdown(engine: &core::Engine, fan_id: &core::FanId) -> ComboBoxText {
    let combo = ComboBoxText::new();
    let bound = engine.fan_mode(fan_id).and_then(|mode| match mode {
        core::FanMode::Auto { sensor, .. } => Some(sensor),
        _ => None,
    });
    for sensor in engine.sensors() {
        if engine.sensor_hidden(&sensor.id) && bound.as_ref() != Some(&sensor.id) {
            continue;
        }
        combo.append(Some(sensor.id.as_str()), &engine.sensor_name(&sensor.id));
    }
    combo
}

/// The preset dropdown row that describes `curve` ("custom" for a
/// hand-made one).
fn preset_combo_id(curve: &core::FanCurve) -> &'static str {
    match core::CurvePreset::matching(curve) {
        Some(core::CurvePreset::Quiet) => CURVE_QUIET,
        Some(core::CurvePreset::Balanced) => CURVE_BALANCED,
        Some(core::CurvePreset::Performance) => CURVE_PERFORMANCE,
        None => CURVE_CUSTOM,
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

    #[test]
    fn preset_combo_id_names_the_real_curve() {
        for (preset, id) in [
            (core::CurvePreset::Quiet, CURVE_QUIET),
            (core::CurvePreset::Balanced, CURVE_BALANCED),
            (core::CurvePreset::Performance, CURVE_PERFORMANCE),
        ] {
            assert_eq!(preset_combo_id(&preset.curve()), id);
        }
        // The curve an off fan gets when switched to Automatic is the
        // default one, so the dropdown must show Balanced, not Quiet.
        assert_eq!(preset_combo_id(&core::FanCurve::default()), CURVE_BALANCED);
        let hand_made = core::FanCurve::new(vec![core::CurvePoint {
            temp_c: 40.0,
            speed: 0.5,
        }])
        .expect("valid");
        assert_eq!(preset_combo_id(&hand_made), CURVE_CUSTOM);
    }
}
