//! Per-fan control rows in the GUI: name (rename), mode selector, speed
//! slider, sensor binding, curve preset, case-view placement and
//! tachometer correction.

use crate::app::accessible_name;
use crate::curve_graph::CurveGraph;
use crate::dbus;
use crate::trend_graph::TrendGraph;
use biggest_fan_core as core;
use gtk4::prelude::*;
use gtk4::{
    Box as GtkBox, DrawingArea, DropDown, Entry, Expander, Label, Orientation, Scale, SpinButton,
    StringList, ToggleButton,
};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

const MODE_OFF: &str = "off";
const MODE_FIXED: &str = "fixed";
const MODE_AUTO: &str = "auto";

/// Curve preset dropdown ids (plus "custom" for hand-made curves), in
/// the order the dropdown lists them.
const CURVE_QUIET: &str = "quiet";
const CURVE_BALANCED: &str = "balanced";
const CURVE_PERFORMANCE: &str = "performance";
const CURVE_CUSTOM: &str = "custom";
const PRESET_IDS: [&str; 4] = [CURVE_QUIET, CURVE_BALANCED, CURVE_PERFORMANCE, CURVE_CUSTOM];

/// Case-view slot shapes: a circle per fan header, or a rectangle for a
/// whole object such as a graphics-card cooler. The ids the shape
/// segmented buttons use.
const SHAPE_CIRCLE: &str = "circle";
const SHAPE_RECTANGLE: &str = "rectangle";
/// The airflow dropdown's ids, in order, and its display labels.
const AIRFLOW_IDS: [&str; 3] = ["intake", "exhaust", "internal"];
const AIRFLOW_LABELS: [&str; 3] = ["Intake", "Exhaust", "Internal"];

/// Style class that turns each fan row into a subtle tile. The colours
/// are translucent tints of the theme's own foreground, so the tile
/// reads softly on both light and dark themes.
pub const FAN_TILE_CLASS: &str = "fan-tile";

/// The stylesheet [`FAN_TILE_CLASS`] expects, installed on the GUI's
/// display when a window is built. The gap between tiles is the flow's
/// own row/column spacing, set in `app::append_fans_panel`.
pub const FAN_TILE_CSS: &str = "
.fan-tile {
    background-color: alpha(@window_fg_color, 0.05);
    border: 1px solid alpha(@window_fg_color, 0.16);
    border-radius: 10px;
    padding: 10px 12px;
}

/* Row status notes and errors: the theme's own error colour, so they
   follow light, dark and high-contrast styles. */
.status-error {
    color: @error_color;
}
";

/// Placement widgets bundled so any one change can push the whole slot.
struct PlacementWidgets {
    airflow: DropDown,
    shape: Segmented,
    diameter_spin: SpinButton,
    width_spin: SpinButton,
    height_spin: SpinButton,
}

impl PlacementWidgets {
    fn airflow_id(&self) -> Option<&'static str> {
        selected_id(&self.airflow, &AIRFLOW_IDS)
    }

    /// Whether this fan's slot is drawn as a whole-card rectangle.
    fn is_rectangle(&self) -> bool {
        self.shape.is_active(SHAPE_RECTANGLE)
    }

    fn set_shape_id(&self, id: &str) {
        self.shape.set_active_id(id);
    }

    fn set_airflow_id(&self, id: &str) {
        set_selected_id(&self.airflow, &AIRFLOW_IDS, id);
    }
}

/// The id a `DropDown` with a fixed id list currently selects (`None`
/// while nothing is selected).
fn selected_id<'a>(drop_down: &DropDown, ids: &[&'a str]) -> Option<&'a str> {
    usize::try_from(drop_down.selected())
        .ok()
        .and_then(|index| ids.get(index))
        .copied()
}

/// Selects `id` in a `DropDown` with a fixed id list.
fn set_selected_id(drop_down: &DropDown, ids: &[&str], id: &str) {
    if let Some(index) = ids.iter().position(|candidate| *candidate == id) {
        drop_down.set_selected(u32::try_from(index).unwrap_or(u32::MAX));
    }
}

/// The sensor-binding dropdown: display names in the model, the sensor
/// ids kept alongside so selections speak in ids.
#[derive(Clone)]
struct SensorDropDown {
    drop_down: DropDown,
    ids: Vec<core::SensorId>,
}

impl SensorDropDown {
    fn selected_id(&self) -> Option<core::SensorId> {
        usize::try_from(self.drop_down.selected())
            .ok()
            .and_then(|index| self.ids.get(index))
            .cloned()
    }

    fn set_selected_id(&self, id: &str) {
        if let Some(index) = self
            .ids
            .iter()
            .position(|candidate| candidate.as_str() == id)
        {
            self.drop_down
                .set_selected(u32::try_from(index).unwrap_or(u32::MAX));
        }
    }
}

/// Callbacks a [`Segmented`] runs when the user picks a different option.
type SegmentedChangedHandlers = Rc<RefCell<Vec<Rc<dyn Fn()>>>>;

/// A HIG segmented button: one linked group of toggles, so the current
/// option is visible at a glance and switching is a single click.
/// Exactly one option is always pressed. The HIG wants these instead of
/// drop-downs for short option sets, so both the fan's mode and the
/// two-option slot shape use it.
///
/// ```text
/// [ Off | Constant speed | Automatic ]   <- one pressed
/// ```
#[derive(Clone)]
struct Segmented {
    container: GtkBox,
    buttons: Vec<(String, ToggleButton)>,
    /// The option the user last chose; lets the change callback ignore
    /// no-op toggles (clicking the selected option again).
    current: Rc<RefCell<Option<String>>>,
    /// Set while the group adjusts itself, so handlers do not recurse.
    guard: Rc<Cell<bool>>,
    /// Run when the user picks a different option.
    changed: SegmentedChangedHandlers,
}

impl Segmented {
    /// Builds the group from `(id, label, tooltip)` triples.
    #[must_use]
    fn new(options: &[(&str, &str, &str)]) -> Self {
        let container = GtkBox::new(Orientation::Horizontal, 0);
        container.add_css_class("linked");
        let mut segmented = Self {
            container,
            buttons: Vec::new(),
            current: Rc::new(RefCell::new(None)),
            guard: Rc::new(Cell::new(false)),
            changed: Rc::new(RefCell::new(Vec::new())),
        };
        for (id, label, tooltip) in options {
            let button = ToggleButton::builder()
                .label(*label)
                .tooltip_text(*tooltip)
                .build();
            segmented.container.append(&button);
            segmented.buttons.push((id.to_string(), button));
        }
        // Keep exactly one button pressed; a real change runs the callbacks.
        for (id, button) in &segmented.buttons {
            let buttons = segmented.buttons.clone();
            let guard = Rc::clone(&segmented.guard);
            let current = Rc::clone(&segmented.current);
            let changed = Rc::clone(&segmented.changed);
            let id = id.clone();
            button.connect_toggled(move |button| {
                if guard.get() {
                    return;
                }
                guard.set(true);
                if button.is_active() {
                    for (other_id, other) in &buttons {
                        if *other_id != *id {
                            other.set_active(false);
                        }
                    }
                } else {
                    // Clicking the selected mode must not leave the fan
                    // with no mode: put it back.
                    button.set_active(true);
                }
                guard.set(false);
                if current.borrow().as_deref() != Some(id.as_str()) {
                    *current.borrow_mut() = Some(id.clone());
                    for on_changed in changed.borrow().iter() {
                        on_changed();
                    }
                }
            });
        }
        segmented
    }

    /// Whether the option with this id is the pressed one.
    fn is_active(&self, id: &str) -> bool {
        self.buttons
            .iter()
            .any(|(button_id, button)| button_id == id && button.is_active())
    }

    /// The id of the pressed option, if any.
    fn active_id(&self) -> Option<String> {
        self.buttons
            .iter()
            .find(|(_, button)| button.is_active())
            .map(|(id, _)| id.clone())
    }

    /// Presses one option without running the change callbacks (engine-
    /// driven initialisation).
    fn set_active_id(&self, id: &str) {
        self.guard.set(true);
        for (button_id, button) in &self.buttons {
            button.set_active(button_id == id);
        }
        self.guard.set(false);
        *self.current.borrow_mut() = Some(id.to_string());
    }

    /// Runs `on_changed` whenever the user picks a different option.
    fn connect_changed(&self, on_changed: impl Fn() + 'static) {
        self.changed.borrow_mut().push(Rc::new(on_changed));
    }
}

/// One fan's row: name, mode segmented buttons, speed slider, sensor
/// dropdown, curve preset, placement controls and a live status line.
pub struct FanControls {
    /// The row container, to be added to the window.
    pub container: GtkBox,
    fan_id: core::FanId,
    name_entry: Entry,
    mode: Segmented,
    speed_scale: Scale,
    /// The "Speed" label beside the slider; shares its visibility.
    speed_label: Label,
    sensor: SensorDropDown,
    preset: DropDown,
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
    /// Case canvas to repaint when placement changes.
    case_area: DrawingArea,
    /// Set while the tick updates widgets so signal handlers do not write
    /// the same value straight back to the engine.
    suppress: Rc<Cell<bool>>,
    /// The running root daemon; every fan change goes through it.
    client: dbus::Client,
}

impl FanControls {
    /// Builds the row for one managed fan; `engine_rc` is consulted
    /// by the signal handlers whenever the user changes a control,
    /// `case_area` is repainted when placement changes, and `client`
    /// mirrors mode changes to the running root service.
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
            .tooltip_text("Rename This Fan")
            .build();
        accessible_name(&name_entry, "Fan name");
        let mode = Segmented::new(&[
            (MODE_OFF, "Off", "Stop This Fan"),
            (MODE_FIXED, "Constant speed", "Hold This Fan at One Speed"),
            (MODE_AUTO, "Automatic", "Follow a Temperature Sensor"),
        ]);
        header.append(&name_entry);
        header.append(&mode.container);

        let (speed_scale, speed_label, speed_row) = Self::speed_slider();

        let sensor = sensor_dropdown(&engine_rc.borrow(), id);
        sensor.drop_down.set_tooltip_text(Some("Governing Sensor"));
        accessible_name(&sensor.drop_down, "Sensor");
        // Curve presets: one dropdown pick instead of editing the points
        // (by hand on the row's graph, or in the config). Picking one
        // replaces whatever curve the fan has; "Custom" is display-only —
        // shown while the curve is not one of these three.
        let preset_model = StringList::new(&[
            core::CurvePreset::Quiet.label(),
            core::CurvePreset::Balanced.label(),
            core::CurvePreset::Performance.label(),
            "Custom",
        ]);
        let preset = DropDown::builder().model(&preset_model).build();
        preset.set_tooltip_text(Some("Curve Preset"));
        accessible_name(&preset, "Curve preset");
        // Show what switching an off/fixed fan to Automatic will actually
        // apply: the first sensor in the (ranked) list and the default
        // curve. `init_from_engine` overwrites both for auto fans.
        sensor.drop_down.set_selected(0);
        set_selected_id(&preset, &PRESET_IDS, CURVE_BALANCED);
        let auto_row = GtkBox::new(Orientation::Horizontal, 8);
        auto_row.append(&Label::builder().label("Sensor").build());
        auto_row.append(&sensor.drop_down);
        auto_row.append(&Label::builder().label("Curve").build());
        auto_row.append(&preset);
        let temp_label = Label::builder().xalign(0.0).hexpand(true).build();
        // The live readout's digit count changes every tick ("999 RPM"
        // -> "1000 RPM"); a fixed character budget keeps the row's
        // natural width constant so the flow tile stops breathing. The
        // ellipsize is the valve when the pane is narrower than the row.
        temp_label.set_width_chars(28);
        temp_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        auto_row.append(&temp_label);
        // The sensor/curve row sits between the header and the graphs;
        // without its own margins it reads squished between them.
        auto_row.set_margin_top(6);
        auto_row.set_margin_bottom(6);

        let status_label = Label::builder().xalign(0.0).build();
        status_label.set_markup("<span size=\"small\">…</span>");

        // The fan's curve drawn against sensor temperature, with a dot
        // for where the fan is now; only shown in auto mode. Dragging its
        // points edits the fan's curve and reports trouble on the row's
        // status line.
        let curve_graph = CurveGraph::new(engine_rc, id, client, &status_label);

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
            &placement.airflow,
            &placement.shape,
            &diameter_label,
            &placement.diameter_spin,
            &size_row,
        );

        let rpm_spin = Self::tach_spin();
        let floor_spin = Self::floor_spin();
        let extra_expander = Self::extras_expander(&placement_row, &rpm_spin, &floor_spin);

        container.append(&header);
        container.append(&speed_row);
        container.append(&auto_row);
        container.append(&graphs_row);
        container.append(&status_label);
        container.append(&extra_expander);

        let controls = Self {
            container,
            fan_id: id.clone(),
            name_entry,
            mode,
            speed_scale,
            speed_label,
            sensor,
            preset,
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
            case_area: case_area.clone(),
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

    /// The fixed-mode speed slider with its "Speed" label (the HIG wants
    /// a text label to identify a slider, not just the drawn value).
    #[must_use]
    fn speed_slider() -> (Scale, Label, GtkBox) {
        let speed_scale = Scale::builder()
            .orientation(Orientation::Horizontal)
            .draw_value(true)
            .digits(0)
            .round_digits(0)
            .hexpand(true)
            .build();
        speed_scale.set_range(0.0, 100.0);
        speed_scale.set_tooltip_text(Some("Commanded Speed Percent"));
        accessible_name(&speed_scale, "Fan speed percent");
        let speed_label = Label::builder().label("Speed").build();
        let speed_row = GtkBox::new(Orientation::Horizontal, 8);
        speed_row.append(&speed_label);
        speed_row.append(&speed_scale);
        (speed_scale, speed_label, speed_row)
    }

    /// Builds the airflow, shape and size widgets for the placement row,
    /// with the labels and spin row whose visibility depends on the
    /// shape.
    #[must_use]
    fn placement_widgets() -> (PlacementWidgets, Label, GtkBox) {
        let airflow_model = StringList::new(&AIRFLOW_LABELS);
        let airflow = DropDown::builder().model(&airflow_model).build();
        airflow.set_tooltip_text(Some("Airflow Direction"));
        accessible_name(&airflow, "Airflow direction");
        // Two options: linked toggles, not a drop-down (HIG).
        let shape = Segmented::new(&[
            (SHAPE_CIRCLE, "Circle", "Circle For A Fan Header"),
            (SHAPE_RECTANGLE, "Rectangle", "Rectangle For A Whole Card"),
        ]);
        let diameter_label = Label::builder().label("Diameter").build();
        let diameter_spin = SpinButton::builder()
            .digits(0)
            .adjustment(&gtk4::Adjustment::new(120.0, 30.0, 300.0, 5.0, 10.0, 0.0))
            .build();
        diameter_spin.set_alignment(1.0); // HIG: right-justified numbers.
        diameter_spin.set_tooltip_text(Some("Fan Diameter"));
        accessible_name(&diameter_spin, "Fan diameter");
        let width_spin = Self::size_spin("Card Width", "Card width");
        let height_spin = Self::size_spin("Card Height", "Card height");
        let size_row = GtkBox::new(Orientation::Horizontal, 4);
        size_row.append(&Label::builder().label("Card W").build());
        size_row.append(&width_spin);
        size_row.append(&Label::builder().label("H").build());
        size_row.append(&height_spin);
        (
            PlacementWidgets {
                airflow,
                shape,
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
    fn size_spin(tooltip: &str, name: &str) -> SpinButton {
        let spin = SpinButton::builder()
            .digits(0)
            .adjustment(&gtk4::Adjustment::new(120.0, 10.0, 1200.0, 5.0, 10.0, 0.0))
            .build();
        spin.set_alignment(1.0); // HIG: right-justified numbers.
        spin.set_tooltip_text(Some(tooltip));
        accessible_name(&spin, name);
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
        spin.set_alignment(1.0); // HIG: right-justified numbers.
        spin.set_tooltip_text(Some("Tach Correction Factor"));
        accessible_name(&spin, "RPM reading factor");
        spin
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
        spin.set_alignment(1.0); // HIG: right-justified numbers.
        spin.set_tooltip_text(Some("Minimum Speed"));
        accessible_name(&spin, "Minimum speed percent");
        spin
    }

    /// Wraps the case-view placement, the tach correction and the speed
    /// floor in a collapsed section, keeping the main row to the
    /// controls every visit needs.
    #[must_use]
    fn extras_expander(
        placement_row: &GtkBox,
        rpm_spin: &SpinButton,
        floor_spin: &SpinButton,
    ) -> Expander {
        // Roomy enough to read as a small form: even spacing between the
        // rows, and the two spin labels padded to one width so the spins
        // line up underneath each other.
        let rpm_row = GtkBox::new(Orientation::Horizontal, 8);
        let rpm_label = Label::builder().label("RPM reading ×").xalign(0.0).build();
        rpm_label.set_width_chars(14);
        rpm_row.append(&rpm_label);
        rpm_row.append(rpm_spin);

        let floor_row = GtkBox::new(Orientation::Horizontal, 8);
        let floor_label = Label::builder().label("Never below %").xalign(0.0).build();
        floor_label.set_width_chars(14);
        floor_row.append(&floor_label);
        floor_row.append(floor_spin);

        let extra_box = GtkBox::new(Orientation::Vertical, 8);
        extra_box.set_margin_top(6);
        extra_box.set_margin_bottom(2);
        extra_box.set_margin_end(4);
        extra_box.append(placement_row);
        extra_box.append(&floor_row);
        extra_box.append(&rpm_row);
        let expander = Expander::builder().label("Extra settings").build();
        expander.set_child(Some(&extra_box));
        expander
    }

    /// Builds the placement editor row: airflow, slot shape, and the
    /// diameter (circle) or width/height (rectangle) widgets.
    #[must_use]
    fn placement_controls(
        airflow: &DropDown,
        shape: &Segmented,
        diameter_label: &Label,
        diameter_spin: &SpinButton,
        size_row: &GtkBox,
    ) -> GtkBox {
        let row = GtkBox::new(Orientation::Horizontal, 8);
        row.append(&Label::builder().label("Airflow").build());
        row.append(airflow);
        row.append(&Label::builder().label("Shape").build());
        row.append(&shape.container);
        row.append(diameter_label);
        row.append(diameter_spin);
        row.append(size_row);
        row
    }

    /// Syncs the widgets once the engine reports a fan's live state.
    pub fn update(&self, status: &core::FanStatus) {
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
        // What the hardware reports: RPM where it has a tachometer (sysfs
        // headers and, on a recent driver, a GPU cooler), and the speed the
        // header reads back where it has none — which is how an older driver
        // still shows a cooler parking.
        let rpm = match (status.measured_rpm, status.measured_percent) {
            (Some(rpm), _) => format!("{rpm:.0} RPM"),
            (None, Some(percent)) => format!("no tach, header at {:.0}%", percent * 100.0),
            (None, None) => "no tach".to_string(),
        };
        let routine = format!("{speed} · {rpm}");
        // In auto mode the live speed and rpm ride on the sensor/curve
        // row (after the temperature), so the line under the graphs is
        // spare for notes — and carries the routine line in the other
        // modes, where that row is hidden.
        let auto = matches!(status.mode, core::FanMode::Auto { .. });
        match &status.note {
            Some(note) => {
                self.status_label.set_visible(true);
                self.status_label.add_css_class("status-error");
                self.status_label.set_markup(&format!(
                    "<span size=\"small\">⚠ {} — engine at {routine}</span>",
                    escape_markup(note),
                ));
            }
            None if auto => self.status_label.set_visible(false),
            None => {
                self.status_label.set_visible(true);
                self.status_label.remove_css_class("status-error");
                self.status_label.set_markup(&format!(
                    "<span size=\"small\">{}</span>",
                    escape_markup(&routine),
                ));
            }
        }

        if let core::FanMode::Auto { curve, .. } = &status.mode {
            let temp = status.governing_temp_c.map_or_else(
                || "sensor unread".to_string(),
                |temp| format!("{temp:.1} °C"),
            );
            // A note below already states the speed and rpm, so the row
            // only carries them when there is no note.
            self.temp_label.set_text(&if status.note.is_some() {
                temp
            } else {
                format!("{temp} · {routine}")
            });
            self.curve_graph
                .set_operating_point(status.governing_temp_c, status.commanded_speed);
            // Keep the preset dropdown honest even when the curve changed
            // without the user touching it (a hand-edited config, for
            // example, shows as Custom).
            self.suppress.set(true);
            set_selected_id(&self.preset, &PRESET_IDS, preset_combo_id(curve));
            self.suppress.set(false);
        }
    }

    /// Sets the initial dropdown and placement state from the engine's
    /// current config.
    fn init_from_engine(&self, engine_rc: &Rc<RefCell<core::Engine>>) {
        let engine = engine_rc.borrow();
        let mode = engine.fan_mode(&self.fan_id).unwrap_or(core::FanMode::Off);
        match &mode {
            core::FanMode::Off => {
                self.mode.set_active_id(MODE_OFF);
            }
            core::FanMode::Fixed { speed } => {
                self.mode.set_active_id(MODE_FIXED);
                self.speed_scale.set_value(speed * 100.0);
            }
            core::FanMode::Auto { sensor, curve } => {
                self.mode.set_active_id(MODE_AUTO);
                // A hidden sensor this fan is bound to still gets a row
                // (see `sensor_dropdown`), so this always lands.
                self.sensor.set_selected_id(sensor.as_str());
                set_selected_id(&self.preset, &PRESET_IDS, preset_combo_id(curve));
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
            self.placement.set_airflow_id(airflow);
            self.placement.diameter_spin.set_value(slot.diameter);
            if let Some((width, height)) = slot.rect {
                self.placement.set_shape_id(SHAPE_RECTANGLE);
                self.placement.width_spin.set_value(width);
                self.placement.height_spin.set_value(height);
            } else {
                self.placement.set_shape_id(SHAPE_CIRCLE);
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
        let rectangle = self.placement.is_rectangle();
        self.diameter_label.set_visible(!rectangle);
        self.placement.diameter_spin.set_visible(!rectangle);
        self.size_row.set_visible(rectangle);
    }

    /// Shows only the controls relevant to the selected mode.
    fn refresh_visibility(&self) {
        let mode = self.mode.active_id();
        self.speed_scale
            .set_visible(mode.as_deref() == Some(MODE_FIXED));
        self.speed_label
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
        self.wire_placement_signals(engine_rc);
        self.wire_rpm_signal(engine_rc);
        self.wire_floor_signal(engine_rc);
    }

    /// Renaming updates the mirror (rows and case view follow the engine's
    /// display name) and tells the service so the saved config agrees.
    /// The HIG commits a text field on Return or when it loses focus —
    /// not on every keystroke.
    fn wire_name_signals(&self, engine_rc: &Rc<RefCell<core::Engine>>) {
        self.name_entry.connect_activate({
            let rc = Rc::clone(engine_rc);
            let suppress = Rc::clone(&self.suppress);
            let fan_id = self.fan_id.clone();
            let client = self.client.clone();
            let status_label = self.status_label.clone();
            move |entry| {
                apply_fan_name(
                    &rc,
                    &fan_id,
                    entry.text().as_ref(),
                    &status_label,
                    &client,
                    &suppress,
                );
            }
        });
        let focus = gtk4::EventControllerFocus::new();
        {
            let rc = Rc::clone(engine_rc);
            let suppress = Rc::clone(&self.suppress);
            let fan_id = self.fan_id.clone();
            let client = self.client.clone();
            let status_label = self.status_label.clone();
            focus.connect_leave(move |controller| {
                // The controller's widget is the entry it is attached to.
                let Some(widget) = controller.widget() else {
                    return;
                };
                let Ok(entry) = widget.downcast::<Entry>() else {
                    return;
                };
                apply_fan_name(
                    &rc,
                    &fan_id,
                    entry.text().as_ref(),
                    &status_label,
                    &client,
                    &suppress,
                );
            });
        }
        self.name_entry.add_controller(focus);
    }

    /// Connects the mode segmented buttons, speed slider and sensor dropdown.
    fn wire_mode_signals(&self, engine_rc: &Rc<RefCell<core::Engine>>) {
        // Mode buttons: rebuild the whole mode from all the controls.
        let rc = Rc::clone(engine_rc);
        let suppress = Rc::clone(&self.suppress);
        let mode = self.mode.clone();
        let scale = self.speed_scale.clone();
        let speed_label = self.speed_label.clone();
        let sensor = self.sensor.clone();
        let preset = self.preset.clone();
        let auto_row = self.auto_row.clone();
        let curve_area = self.curve_graph.widget().clone();
        let status_label = self.status_label.clone();
        let fan_id = self.fan_id.clone();
        let client = self.client.clone();
        self.mode.connect_changed(move || {
            if suppress.get() {
                return;
            }
            let active = mode.active_id();
            let curve_engine = Rc::clone(&rc);
            let curve_fan = fan_id.clone();
            let fan_mode = read_mode(
                active.as_deref(),
                scale.value(),
                sensor.selected_id().as_ref(),
                move || existing_curve(&curve_engine.borrow(), &curve_fan),
            );
            push_mode(&rc, &fan_id, fan_mode, &status_label, &client);
            scale.set_visible(active.as_deref() == Some(MODE_FIXED));
            speed_label.set_visible(active.as_deref() == Some(MODE_FIXED));
            auto_row.set_visible(active.as_deref() == Some(MODE_AUTO));
            curve_area.set_visible(active.as_deref() == Some(MODE_AUTO));
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
                set_selected_id(&preset, &PRESET_IDS, preset_id);
                suppress.set(false);
            }
        });

        // Speed slider: only meaningful (and only visible) in fixed mode.
        let rc = Rc::clone(engine_rc);
        let suppress = Rc::clone(&self.suppress);
        let mode = self.mode.clone();
        let sensor = self.sensor.clone();
        let status_label = self.status_label.clone();
        let fan_id = self.fan_id.clone();
        let client = self.client.clone();
        self.speed_scale.connect_value_changed(move |scale| {
            if suppress.get() || mode.active_id().as_deref() != Some(MODE_FIXED) {
                return;
            }
            let fan_mode = read_mode(
                Some(MODE_FIXED),
                scale.value(),
                sensor.selected_id().as_ref(),
                core::FanCurve::default,
            );
            push_mode(&rc, &fan_id, fan_mode, &status_label, &client);
        });

        // Sensor dropdown: rebinds an auto fan, keeping its curve.
        let rc = Rc::clone(engine_rc);
        let suppress = Rc::clone(&self.suppress);
        let mode = self.mode.clone();
        let scale = self.speed_scale.clone();
        let sensor = self.sensor.clone();
        let status_label = self.status_label.clone();
        let fan_id = self.fan_id.clone();
        let client = self.client.clone();
        self.sensor.drop_down.connect_selected_notify(move |_| {
            if suppress.get() || mode.active_id().as_deref() != Some(MODE_AUTO) {
                return;
            }
            let curve_engine = Rc::clone(&rc);
            let curve_fan = fan_id.clone();
            let fan_mode = read_mode(
                Some(MODE_AUTO),
                scale.value(),
                sensor.selected_id().as_ref(),
                move || existing_curve(&curve_engine.borrow(), &curve_fan),
            );
            push_mode(&rc, &fan_id, fan_mode, &status_label, &client);
        });
    }

    /// A preset replaces an auto fan's curve, keeping its sensor. Hand-
    /// made ("custom") curves are never overwritten until one is picked.
    fn wire_curve_preset_signal(&self, engine_rc: &Rc<RefCell<core::Engine>>) {
        let rc = Rc::clone(engine_rc);
        let suppress = Rc::clone(&self.suppress);
        let mode = self.mode.clone();
        let sensor = self.sensor.clone();
        let status_label = self.status_label.clone();
        let fan_id = self.fan_id.clone();
        let client = self.client.clone();
        let curve_area = self.curve_graph.widget().clone();
        self.preset.connect_selected_notify(move |preset| {
            if suppress.get() || mode.active_id().as_deref() != Some(MODE_AUTO) {
                return;
            }
            let preset_id = match selected_id(preset, &PRESET_IDS) {
                Some(CURVE_QUIET) => core::CurvePreset::Quiet,
                Some(CURVE_BALANCED) => core::CurvePreset::Balanced,
                Some(CURVE_PERFORMANCE) => core::CurvePreset::Performance,
                _ => return, // "custom": display-only, nothing to apply
            };
            let Some(sensor_id) = sensor.selected_id() else {
                return;
            };
            let mode = Some(core::FanMode::Auto {
                sensor: sensor_id,
                curve: preset_id.curve(),
            });
            push_mode(&rc, &fan_id, mode, &status_label, &client);
            // The draw closure re-reads the curve from the engine, so
            // one redraw shows the new preset at once.
            curve_area.queue_draw();
        });
    }

    /// Connects the placement controls to the fan's case-view slot.
    /// Every control pushes the whole slot, so switching shape (circle ↔
    /// card) converts the drawing in place. (Dragging on the canvas moves
    /// the shape itself; see `case_view`.)
    fn wire_placement_signals(&self, engine_rc: &Rc<RefCell<core::Engine>>) {
        self.placement.airflow.connect_selected_notify({
            let rc = Rc::clone(engine_rc);
            let placement = Rc::clone(&self.placement);
            let case_area = self.case_area.clone();
            let status_label = self.status_label.clone();
            let fan_id = self.fan_id.clone();
            move |_| {
                push_slot(&rc, &fan_id, &placement, &case_area, &status_label);
            }
        });
        self.placement.shape.connect_changed({
            let rc = Rc::clone(engine_rc);
            let placement = Rc::clone(&self.placement);
            let diameter_label = self.diameter_label.clone();
            let size_row = self.size_row.clone();
            let case_area = self.case_area.clone();
            let status_label = self.status_label.clone();
            let fan_id = self.fan_id.clone();
            move || {
                let rectangle = placement.is_rectangle();
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
}

/// Copies the placement widgets' values into the fan's case-view
/// slot, keeping the position chosen by dragging on the drawing.
fn push_slot(
    engine_rc: &Rc<RefCell<core::Engine>>,
    fan_id: &core::FanId,
    placement: &PlacementWidgets,
    case_area: &DrawingArea,
    status_label: &Label,
) {
    let airflow = match placement.airflow_id() {
        Some("intake") => core::Airflow::Intake,
        Some("exhaust") => core::Airflow::Exhaust,
        _ => core::Airflow::Internal,
    };
    let diameter = placement.diameter_spin.value().clamp(10.0, 400.0);
    let rect = placement.is_rectangle().then(|| {
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
        report_status(status_label, &error.to_string());
    }
    drop(engine);
    case_area.queue_draw();
}

/// Assembles the mode the controls currently describe: the pressed mode,
/// the slider's percent and the chosen sensor. `None` when no mode is
/// pressed or an auto fan has no sensor to follow.
fn read_mode(
    active: Option<&str>,
    speed_percent: f64,
    sensor_id: Option<&core::SensorId>,
    curve: impl Fn() -> core::FanCurve,
) -> Option<core::FanMode> {
    match active {
        Some(MODE_OFF) => Some(core::FanMode::Off),
        Some(MODE_FIXED) => Some(core::FanMode::Fixed {
            speed: speed_percent / 100.0,
        }),
        Some(MODE_AUTO) => sensor_id.map(|sensor| core::FanMode::Auto {
            sensor: sensor.clone(),
            curve: curve(),
        }),
        _ => None,
    }
}

/// Applies a fan rename: to the mirror engine first (rows and the case
/// view follow the display name), then to the service so the saved
/// config agrees. An empty name falls back to the hardware default.
fn apply_fan_name(
    engine_rc: &Rc<RefCell<core::Engine>>,
    fan_id: &core::FanId,
    name: &str,
    status_label: &Label,
    client: &dbus::Client,
    suppress: &Cell<bool>,
) {
    if suppress.get() {
        return;
    }
    let mut engine = engine_rc.borrow_mut();
    if let Err(error) = engine.set_fan_name(fan_id, name) {
        report_status(status_label, &error.to_string());
        return;
    }
    drop(engine);
    client.send(dbus::FanRequest::SetName {
        fan: fan_id.to_string(),
        name: name.to_string(),
    });
}

/// Reports a one-off problem on the row's status line, in red. The line
/// hides itself while an auto row has nothing to say, so a report has
/// to show it again.
pub(crate) fn report_status(status_label: &Label, problem: &str) {
    status_label.set_visible(true);
    status_label.add_css_class("status-error");
    status_label.set_markup(&format!(
        "<span size=\"small\">⚠ {}</span>",
        escape_markup(problem),
    ));
}

pub(crate) fn push_mode(
    engine_rc: &Rc<RefCell<core::Engine>>,
    fan_id: &core::FanId,
    mode: Option<core::FanMode>,
    status_label: &Label,
    client: &dbus::Client,
) {
    let Some(mode) = mode else { return };
    let mut engine = engine_rc.borrow_mut();
    if let Err(error) = engine.set_fan_mode(fan_id, &mode) {
        report_status(status_label, &error.to_string());
        return;
    }
    drop(engine);
    // The running root service drives the headers: mirror the change there.
    match serde_json::to_string(&mode) {
        Ok(mode_json) => client.send(dbus::FanRequest::SetFanMode {
            fan: fan_id.to_string(),
            mode_json,
        }),
        Err(error) => report_status(
            status_label,
            &format!("could not send mode to service: {error}"),
        ),
    }
}

/// Sensor binding dropdown: every temperature, minus the hidden ones
/// ([`core::Engine::sensor_hidden`]) except the sensor this fan is
/// already bound to — dropping that row would silently change a live
/// binding. Search is on: the list can run long on a big board.
fn sensor_dropdown(engine: &core::Engine, fan_id: &core::FanId) -> SensorDropDown {
    let bound = engine.fan_mode(fan_id).and_then(|mode| match mode {
        core::FanMode::Auto { sensor, .. } => Some(sensor),
        _ => None,
    });
    let mut ids = Vec::new();
    let mut names = Vec::new();
    for sensor in engine.sensors() {
        if engine.sensor_hidden(&sensor.id) && bound.as_ref() != Some(&sensor.id) {
            continue;
        }
        ids.push(sensor.id.clone());
        names.push(engine.sensor_name(&sensor.id));
    }
    let labels: Vec<&str> = names.iter().map(String::as_str).collect();
    let model = StringList::new(&labels);
    let drop_down = DropDown::builder()
        .model(&model)
        .enable_search(true)
        .build();
    SensorDropDown { drop_down, ids }
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

pub(crate) fn escape_markup(text: &str) -> String {
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
    fn read_mode_builds_each_mode_from_the_controls() {
        let cpu = core::SensorId::new("k10temp");
        // Happy paths: each pressed mode assembles its variant.
        assert_eq!(
            read_mode(Some(MODE_OFF), 40.0, Some(&cpu), core::FanCurve::default),
            Some(core::FanMode::Off)
        );
        assert_eq!(
            read_mode(Some(MODE_FIXED), 42.5, Some(&cpu), core::FanCurve::default),
            Some(core::FanMode::Fixed { speed: 0.425 })
        );
        let auto = read_mode(Some(MODE_AUTO), 0.0, Some(&cpu), core::FanCurve::default);
        match auto {
            Some(core::FanMode::Auto { sensor, curve }) => {
                assert_eq!(sensor.as_str(), "k10temp");
                assert_eq!(curve, core::FanCurve::default());
            }
            other => panic!("expected auto mode, got {other:?}"),
        }
        // Edge cases: no pressed mode, and auto without a sensor to follow.
        assert_eq!(
            read_mode(None, 40.0, Some(&cpu), core::FanCurve::default),
            None
        );
        assert_eq!(
            read_mode(Some(MODE_AUTO), 0.0, None, core::FanCurve::default),
            None
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
