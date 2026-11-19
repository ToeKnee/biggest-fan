//! GTK application window: live case view on the left, fan controls and
//! sensor readouts on the right, with a configurable refresh rate.

use crate::case_view;
use crate::dbus;
use crate::fan_controls::{FAN_FLOW_CLASS, FAN_TILE_CSS, FanControls};
use biggest_fan_core as core;
use glib::{ControlFlow, SourceId};
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Box as GtkBox, Button, ComboBoxText, FlowBox, HeaderBar, Label,
    ListBox, ListBoxRow, Orientation, PolicyType, ScrolledWindow, SpinButton, gio,
};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

/// Everything the periodic tick needs to touch.
struct GuiState {
    engine: Rc<RefCell<core::Engine>>,
    /// Fan rows, in panel order; "Add fan" appends more at runtime.
    fan_controls: RefCell<Vec<FanControls>>,
    /// The flow of fan row tiles (new rows are inserted into it).
    fans_flow: FlowBox,
    /// Unmanaged headers offered by "Add fan", and its button.
    unclaimed_combo: ComboBoxText,
    add_button: Button,
    /// Display name and value label for each sensor row.
    sensor_rows: BTreeMap<core::SensorId, (String, Label)>,
    case_area: gtk4::DrawingArea,
    status_label: Label,
    /// Handle on the running root daemon; every change goes through it.
    client: dbus::Client,
    /// Answers from that daemon, drained on every tick.
    service_events: Rc<RefCell<mpsc::Receiver<dbus::Event>>>,
}

/// Starts the GTK app; `engine` mirrors the service's config and hardware
/// state but never writes to the headers.
///
/// Fan changes are forwarded to the running root daemon over D-Bus (its
/// handle, `client`), and its answers arrive as events on every tick.
/// Handing the headers back to hardware control at exit is a no-op here:
/// the daemon owns them.
#[must_use]
pub fn run(
    engine: core::Engine,
    client: dbus::Client,
    service_events: mpsc::Receiver<dbus::Event>,
) -> glib::ExitCode {
    let application = Application::builder()
        .application_id("net.ynottony.ten.BiggestFan")
        // Each launch gets its own window and engine mirror. Without this,
        // GApplication would remote-activate an already-running instance —
        // and hang waiting for it if that one is wedged.
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let engine_rc = Rc::new(RefCell::new(engine));
    let activate_engine = Rc::clone(&engine_rc);
    let activate_events = Rc::new(RefCell::new(service_events));
    application.connect_activate(move |application| {
        build_window(
            application,
            &activate_engine,
            &client,
            Rc::clone(&activate_events),
        );
    });
    let code = application.run_with_args(&["biggest-fan"]);
    // A mirror never claimed the headers, so this only matters if the
    // engine somehow did (it cannot today); keep it for safety.
    if let Err(error) = engine_rc.borrow_mut().release_control() {
        eprintln!("biggest-fan: could not return fans to hardware control: {error}");
    }
    code
}

/// Installs the app's stylesheet (the fan-row tiles from
/// `fan_controls::FAN_TILE_CSS`) on the display this window lives on.
fn install_stylesheet() {
    let provider = gtk4::CssProvider::new();
    // A broken selector would be reported by GTK itself; the rows stay
    // usable plain either way. (`load_from_data`; `load_from_string`
    // would need the gtk `v4_12` feature.)
    provider.load_from_data(FAN_TILE_CSS);
    // The window exists before this runs, so a display always does.
    if let Some(display) = gtk4::gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

fn build_window(
    application: &Application,
    engine_rc: &Rc<RefCell<core::Engine>>,
    client: &dbus::Client,
    service_events: Rc<RefCell<mpsc::Receiver<dbus::Event>>>,
) {
    let window = ApplicationWindow::builder()
        .application(application)
        .title("Biggest Fan")
        // Start large (1080p-friendly) and maximised: the case drawing
        // and the fan rows both want room, and a small window buries the
        // sensors below the fold.
        .default_width(1600)
        .default_height(900)
        .build();
    window.set_maximized(true);
    install_stylesheet();

    let header = HeaderBar::new();
    let save_button = Button::builder().label("Save config").build();
    let detect_button = Button::builder().label("Detect fans").build();
    // `pack_end` stacks right-to-left, so this puts Detect left of Save.
    header.pack_end(&save_button);
    header.pack_end(&detect_button);
    window.set_titlebar(Some(&header));

    // Left: the case drawing plus a short legend.
    let case_area = case_view::install(Rc::clone(engine_rc));
    let legend = Label::builder()
        .label(
            "Circles: blue intake · red exhaust · yellow internal.\nA rectangle is a whole graphics card.\nThe wedge shows commanded speed.",
        )
        .wrap(true)
        .max_width_chars(38)
        .build();
    let left = GtkBox::new(Orientation::Vertical, 8);
    left.append(&case_area);
    left.append(&legend);
    let left_scroll = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .child(&left)
        .build();

    // Right: fan control rows and the sensor list.
    let right = GtkBox::new(Orientation::Vertical, 12);
    let fan_panel = append_fans_panel(&right, engine_rc, &case_area, client);
    let sensor_rows = append_sensors_panel(&right, engine_rc);
    let right_scroll = ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(PolicyType::Never)
        .child(&right)
        .build();

    let paned = gtk4::Paned::builder()
        .orientation(Orientation::Horizontal)
        .vexpand(true)
        .build();
    paned.set_start_child(Some(&left_scroll));
    paned.set_end_child(Some(&right_scroll));

    // Bottom bar: refresh-rate control and the live status line. The
    // status line caps its requested width and ellipsizes long messages
    // so notes never stretch the window.
    let status_label = Label::builder()
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .max_width_chars(48)
        .build();
    let interval_spin = SpinButton::builder()
        .digits(1)
        .adjustment(&gtk4::Adjustment::new(1.0, 0.1, 30.0, 0.1, 1.0, 0.0))
        .build();
    let poll_ms = engine_rc.borrow().poll_interval_ms();
    #[expect(
        clippy::cast_precision_loss,
        reason = "poll interval is tiny compared to u64::MAX"
    )]
    interval_spin.set_value(poll_ms as f64 / 1000.0);
    let bottom = GtkBox::new(Orientation::Horizontal, 8);
    bottom.append(&Label::builder().label("update every (s):").build());
    bottom.append(&interval_spin);
    bottom.append(&status_label);

    let root = GtkBox::new(Orientation::Vertical, 6);
    // Keep panels off the window edges.
    root.set_margin_start(12);
    root.set_margin_end(12);
    root.set_margin_top(12);
    root.set_margin_bottom(12);
    root.append(&paned);
    root.append(&bottom);
    window.set_child(Some(&root));

    let state = Rc::new(GuiState {
        engine: Rc::clone(engine_rc),
        fan_controls: RefCell::new(fan_panel.controls),
        fans_flow: fan_panel.fans_flow,
        unclaimed_combo: fan_panel.unclaimed_combo,
        add_button: fan_panel.add_button,
        sensor_rows,
        case_area,
        status_label,
        client: client.clone(),
        service_events,
    });
    let tick_source = Rc::new(RefCell::new(None::<SourceId>));
    schedule_tick(&state, &tick_source);

    // Add fan: take a header the machine exposes but no fan drives yet
    // (a parked one, or a graphics-card cooler missing from an older
    // config) under management.
    let add_state = Rc::clone(&state);
    state
        .add_button
        .connect_clicked(move |_| on_add_fan_clicked(&add_state));

    // Detect fans: spin every header up briefly and disable the ones with
    // nothing connected (no RPM) or whose driver refuses the spin-up.
    // Rows of disabled headers disappear; the change is kept only after
    // Save config.
    let detect_state = Rc::clone(&state);
    detect_button.connect_clicked(move |_| on_detect_clicked(&detect_state));

    // Save button: write the current in-memory config (mode changes from
    // the controls) back to the config file — or through the service, in
    // whose hands the live config is when connected.
    let save_state = Rc::clone(&state);
    save_button.connect_clicked(move |_| on_save_clicked(&save_state));

    // Refresh-rate spin: update the engine and reschedule the tick.
    wire_interval_spin(&interval_spin, engine_rc, &state, &tick_source);

    window.present();
}

/// One round of fan detection, run by the service: it spins every header
/// up briefly and parks the ones with nothing connected (no RPM) or whose
/// driver refuses the spin-up; the findings arrive as a `Detected` event
/// (see `drain_service_events`) and park the matching rows here. The
/// change is kept only after Save config.
fn on_detect_clicked(state: &GuiState) {
    state.client.send(dbus::FanRequest::Detect);
    state
        .status_label
        .set_text("the service is spinning every header up…");
}

/// Hides the control rows of fans that left the managed set.
fn hide_control_rows(state: &GuiState, ids: &[core::FanId]) {
    for controls in state.fan_controls.borrow().iter() {
        if ids.contains(controls.fan_id()) {
            controls.hide_row();
        }
    }
    state.case_area.queue_draw();
}

/// Takes a discovered-but-unmanaged header (for example the
/// graphics-card cooler missing from a config written before GPU
/// coolers existed, or a header detection had parked) under
/// management: the mirror engine adopts it at once, the service is
/// asked to do the same and save, and a control row appears
/// immediately.
fn on_add_fan_clicked(state: &GuiState) {
    let Some(header_id) = state.unclaimed_combo.active_id() else {
        return;
    };
    let fan_id = core::FanId::new(header_id.to_string());
    let display_name = {
        let mut engine = state.engine.borrow_mut();
        match engine.add_fan(&fan_id) {
            Ok(()) => engine
                .managed_fans()
                .into_iter()
                .find(|(managed_id, _)| *managed_id == fan_id)
                .map_or_else(|| header_id.to_string(), |(_, name)| name),
            Err(error) => {
                state
                    .status_label
                    .set_text(&format!("could not add {fan_id}: {error}"));
                return;
            }
        }
    };
    state.client.send(dbus::FanRequest::AddFan {
        fan: fan_id.to_string(),
    });
    let controls = FanControls::new(
        &state.engine,
        &fan_id,
        &display_name,
        &state.case_area,
        &state.client,
    );
    state.fans_flow.insert(&controls.container, -1);
    state.fan_controls.borrow_mut().push(controls);
    populate_addable_headers(
        &state.unclaimed_combo,
        &state.add_button,
        &state.engine.borrow(),
    );
    state.case_area.queue_draw();
    state.status_label.set_text(&format!(
        "{display_name} added — the service has taken over its header"
    ));
}

/// Asks the service to apply and save the GUI's current config (mode and
/// grouping changes from the controls, placement from the canvas); the
/// service's config file is the live one.
fn on_save_clicked(state: &GuiState) {
    let engine = state.engine.borrow();
    match core::config::to_text(&engine.current_config()) {
        Ok(toml_text) => {
            state.client.send(dbus::FanRequest::Save { toml_text });
            state
                .status_label
                .set_text("the service is applying and saving the config…");
        }
        Err(error) => state
            .status_label
            .set_text(&format!("could not serialise the config: {error}")),
    }
}

/// Applies whatever the service answered since the last tick.
fn drain_service_events(state: &GuiState) {
    let guard = state.service_events.borrow_mut();
    while let Ok(event) = guard.try_recv() {
        match event {
            dbus::Event::Status(message) => state.status_label.set_text(&message),
            dbus::Event::Detected(ids) => {
                let ids: Vec<core::FanId> = ids.into_iter().map(core::FanId::new).collect();
                state.engine.borrow_mut().park_fans(&ids);
                hide_control_rows(state, &ids);
                if ids.is_empty() {
                    state.status_label.set_text(
                        "the service found nothing to disable — every managed header spun up",
                    );
                } else {
                    let headers = ids
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ");
                    state.status_label.set_text(&format!(
                        "{headers} disabled by the service — press Save config to keep them out"
                    ));
                }
            }
        }
    }
}

/// Makes the update-rate spin button retune the engine's control loop.
fn wire_interval_spin(
    interval_spin: &gtk4::SpinButton,
    engine_rc: &Rc<RefCell<core::Engine>>,
    state: &Rc<GuiState>,
    tick_source: &Rc<RefCell<Option<SourceId>>>,
) {
    let spin_engine = Rc::clone(engine_rc);
    let spin_state = Rc::clone(state);
    let spin_source = Rc::clone(tick_source);
    interval_spin.connect_value_changed(move |spin| {
        let seconds_ms = (spin.value() * 1000.0).round();
        #[expect(
            clippy::cast_sign_loss,
            clippy::cast_possible_truncation,
            reason = "the spin range is 0.1–30.0 s, so the value is a small positive number"
        )]
        let ms = seconds_ms as u64;
        spin_engine.borrow_mut().set_poll_interval_ms(ms);
        spin_state.client.send(dbus::FanRequest::SetInterval { ms });
        schedule_tick(&spin_state, &spin_source);
    });
}

/// The "Fans" panel: one control row per managed fan group (in a flow
/// new rows can be inserted into) with an "add a fan header" row
/// underneath.
struct FanPanel {
    controls: Vec<FanControls>,
    fans_flow: FlowBox,
    unclaimed_combo: ComboBoxText,
    add_button: Button,
}

/// Appends the fan section (header, one control row per managed fan
/// group, then the add row under them all) to `panel`.
fn append_fans_panel(
    panel: &GtkBox,
    engine_rc: &Rc<RefCell<core::Engine>>,
    case_area: &gtk4::DrawingArea,
    client: &dbus::Client,
) -> FanPanel {
    panel.append(
        &Label::builder()
            .label("<b>Fans</b>")
            .use_markup(true)
            .xalign(0.0)
            .build(),
    );
    // The fan rows are flow tiles: one per line on a 1080p-sized panel,
    // two side by side once the panel is wide enough for a 4K screen.
    // Each tile's minimum content width (the two graphs side by side)
    // is what decides where the flow breaks.
    let fans_flow = FlowBox::new();
    fans_flow.add_css_class(FAN_FLOW_CLASS);
    fans_flow.set_min_children_per_line(1);
    fans_flow.set_max_children_per_line(2);
    fans_flow.set_selection_mode(gtk4::SelectionMode::None);
    panel.append(&fans_flow);
    let fan_ids_and_names = engine_rc.borrow().managed_fans();
    let mut fan_controls = Vec::new();
    for (fan_id, name) in fan_ids_and_names {
        let controls = FanControls::new(engine_rc, &fan_id, &name, case_area, client);
        fans_flow.insert(&controls.container, -1);
        fan_controls.push(controls);
    }
    // The add row sits below the existing fans: new rows join the list
    // above it, and adding is a rare setup action, not the panel's focus.
    let add_row = GtkBox::new(Orientation::Horizontal, 8);
    let unclaimed_combo = ComboBoxText::new();
    let add_button = Button::builder()
        .label("Add fan")
        .tooltip_text("take over a fan header nothing drives yet (also revives parked ones)")
        .build();
    add_row.append(&Label::builder().label("add:").build());
    add_row.append(&unclaimed_combo);
    add_row.append(&add_button);
    panel.append(&add_row);
    let panel = FanPanel {
        controls: fan_controls,
        fans_flow,
        unclaimed_combo,
        add_button,
    };
    populate_addable_headers(
        &panel.unclaimed_combo,
        &panel.add_button,
        &engine_rc.borrow(),
    );
    panel
}

/// Refills the "add fan" dropdown with headers no managed fan drives
/// yet; the button only works while there is something to add.
fn populate_addable_headers(combo: &ComboBoxText, button: &Button, engine: &core::Engine) {
    combo.remove_all();
    let headers = engine.unclaimed_headers();
    for header in &headers {
        combo.append(Some(header.id.as_str()), &header.default_name);
    }
    if !headers.is_empty() {
        combo.set_active(Some(0));
    }
    button.set_sensitive(!headers.is_empty());
}

/// Appends the sensor section (header + readout per discovered sensor)
/// to `panel`, returning display name and value label per sensor id.
fn append_sensors_panel(
    panel: &GtkBox,
    engine_rc: &Rc<RefCell<core::Engine>>,
) -> BTreeMap<core::SensorId, (String, Label)> {
    panel.append(
        &Label::builder()
            .label("<b>Sensors</b>")
            .use_markup(true)
            .xalign(0.0)
            .build(),
    );
    let sensor_list = ListBox::new();
    sensor_list.set_selection_mode(gtk4::SelectionMode::None);
    let mut sensor_rows = BTreeMap::new();
    let sensors = engine_rc.borrow().sensors().to_vec();
    for sensor in sensors {
        let name = engine_rc.borrow().sensor_name(&sensor.id);
        let row = ListBoxRow::new();
        let label = Label::builder().xalign(0.0).build();
        label.set_text(&format!("{name}  —"));
        row.set_child(Some(&label));
        sensor_list.append(&row);
        sensor_rows.insert(sensor.id, (name, label));
    }
    panel.append(&sensor_list);
    sensor_rows
}

/// (Re)arms the periodic control-loop tick at the engine's current poll
/// interval, replacing any previously pending timeout.
fn schedule_tick(state: &Rc<GuiState>, source: &Rc<RefCell<Option<SourceId>>>) {
    if let Some(old) = source.borrow_mut().take() {
        old.remove();
    }
    let interval = Duration::from_millis(state.engine.borrow().poll_interval_ms());
    let tick_state = Rc::clone(state);
    let id = glib::timeout_add_local(interval, move || {
        tick_once(&tick_state);
        ControlFlow::Continue
    });
    *source.borrow_mut() = Some(id);
}

/// One control-loop iteration, pushing results into the widgets.
fn tick_once(state: &GuiState) {
    drain_service_events(state);
    let report = match state.engine.borrow_mut().tick() {
        Ok(report) => report,
        Err(error) => {
            state
                .status_label
                .set_text(&format!("control loop error: {error}"));
            return;
        }
    };

    let mut notes = Vec::new();
    for controls in state.fan_controls.borrow().iter() {
        match report.fans.iter().find(|fan| fan.id == *controls.fan_id()) {
            Some(status) => {
                controls.update(status);
                if let Some(note) = &status.note {
                    notes.push(note.clone());
                }
            }
            None => {
                // The control loop parked this fan (for example its driver
                // refused writes), so its row leaves the panel too.
                controls.hide_row();
            }
        }
    }
    for status in &report.sensors {
        if let Some((name, label)) = state.sensor_rows.get(&status.id) {
            let temp = status
                .temp_c
                .map_or_else(|| "no reading".to_string(), |temp| format!("{temp:.1} °C"));
            label.set_text(&format!("{name}  {temp}"));
        }
    }
    state.case_area.queue_draw();

    if !notes.is_empty() {
        state.status_label.set_text(&notes.join(" · "));
    }
}
