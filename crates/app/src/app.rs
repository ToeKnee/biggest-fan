//! GTK application window: live case view on the left, fan controls and
//! sensor readouts on the right, with a configurable refresh rate.

use crate::case_view;
use crate::dbus;
use crate::fan_controls::{FAN_TILE_CSS, FanControls};
use crate::sparkline::Sparkline;
use crate::stress;
use biggest_fan_core as core;
use glib::{ControlFlow, Propagation, SourceId};
use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, FlowBox, FlowBoxChild, Label, ListBox, ListBoxRow, Orientation,
    PolicyType, ScrolledWindow, SpinButton, gio,
};
use libadwaita as adw;
use libadwaita::prelude::*;
use std::cell::Cell;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

/// Everything the periodic tick needs to touch.
struct GuiState {
    engine: Rc<RefCell<core::Engine>>,
    /// Fan rows, in panel order; showing a hidden fan appends more at runtime.
    fan_controls: RefCell<Vec<FanControls>>,
    /// The flow of fan row tiles (new rows are inserted into it).
    fans_flow: FlowBox,
    /// Shown while the fan list has no visible rows (HIG placeholder:
    /// icon, heading and guidance).
    fans_placeholder: GtkBox,
    /// Fans parked as `hidden` (by Detect or a lost driver): a reveal
    /// button and one row per parked fan with a "Show" button, the way
    /// the sensor list treats hidden temperatures.
    hidden_fans_list: ListBox,
    hidden_fans_reveal: Button,
    showing_hidden_fans: Cell<bool>,
    /// The sensor readout list with its hide/show plumbing.
    sensor_panel: Rc<SensorPanel>,
    case_area: gtk4::DrawingArea,
    /// One-shot results (detect, save, add, hide/show, errors) appear
    /// here as transient toasts; there is no status bar (HIG).
    toasts: adw::ToastOverlay,
    /// Header spinner shown while a Detect is in flight; the Detect
    /// button is insensitive until the answer arrives.
    detect_spinner: gtk4::Spinner,
    detect_button: Button,
    detect_pending: Cell<bool>,
    /// Handle on the running root daemon; every change goes through it.
    client: dbus::Client,
    /// Answers from that daemon, drained on every tick.
    service_events: Rc<RefCell<mpsc::Receiver<dbus::Event>>>,
    /// Tick counter behind the periodic service-config poll (the service
    /// owns the live config, so this mirror follows it).
    config_poll: Cell<u32>,
    /// The service config text last applied to this mirror; an identical
    /// poll answer is skipped, so the mirror is not rebuilt every poll.
    service_config: RefCell<String>,
}

/// Control-loop ticks between polls of the service's config.
const SERVICE_CONFIG_POLL_TICKS: u32 = 5;

/// One tile of the "Sensors" readout flow.
struct SensorRow {
    /// Resolved display name, quoted in the hide/show toasts.
    name: String,
    /// Live temperature label ("42.5 °C").
    value: Label,
    /// The tile's inline history of this temperature.
    spark: Sparkline,
    /// The tile itself — hidden sensors leave the flow until shown again.
    row: FlowBoxChild,
    /// This sensor's hide/show icon button.
    toggle: Button,
}

/// The "Sensors" readout tiles and their hide/show plumbing. Hiding a
/// sensor is a display-only config flag ([`core::Engine::set_sensor_hidden`]):
/// the engine keeps reading hidden sensors, so fans bound to them keep
/// working, and the flag only reaches the service on the next Save
/// config — same as hiding a detected fan.
struct SensorPanel {
    engine: Rc<RefCell<core::Engine>>,
    toasts: adw::ToastOverlay,
    rows: BTreeMap<core::SensorId, SensorRow>,
    /// "N hidden — show" button; reveals the hidden rows so a mistake
    /// can be undone.
    reveal: Button,
    showing_hidden: Cell<bool>,
}

impl SensorPanel {
    /// Hides or shows one sensor and toasts the result with an **Undo**
    /// button — the HIG's preferred pattern for reversible actions.
    /// Needs the `Rc` so the toast's Undo callback can reach the panel.
    fn set_hidden(self: &Rc<Self>, id: &core::SensorId, hide: bool) {
        if !self.apply_hidden(id, hide) {
            return;
        }
        let Some(row) = self.rows.get(id) else {
            return;
        };
        let toast = adw::Toast::new(&if hide {
            format!("{} hidden — press Save config to keep it hidden", row.name)
        } else {
            format!(
                "{} shown again — press Save config to keep it visible",
                row.name
            )
        });
        toast.set_button_label(Some("Undo"));
        let panel = Rc::clone(self);
        let undo_id = id.clone();
        toast.connect_button_clicked(move |_| {
            // Reverting quietly (no second toast) keeps the undo trail
            // from stacking up on the overlay.
            panel.apply_hidden(&undo_id, !hide);
        });
        self.toasts.add_toast(toast);
    }

    /// The hide/show plumbing on its own: the flag goes to the mirror
    /// engine (and from there into the config on the next save), the row
    /// follows. False when the engine refused (already toasted). The Undo
    /// button calls this directly, skipping the toast.
    fn apply_hidden(&self, id: &core::SensorId, hide: bool) -> bool {
        if let Err(error) = self.engine.borrow_mut().set_sensor_hidden(id, hide) {
            show_toast(&self.toasts, &format!("Could not change {id}: {error}"));
            return false;
        }
        let Some(row) = self.rows.get(id) else {
            return false;
        };
        // The icon shows the action: an eye-with-slash to hide a visible
        // sensor, a plain eye to bring a hidden one back.
        row.toggle.set_icon_name(if hide {
            "view-reveal-symbolic"
        } else {
            "view-conceal-symbolic"
        });
        row.toggle.set_tooltip_text(Some(if hide {
            "Show This Temperature"
        } else {
            "Hide This Temperature"
        }));
        accessible_name(
            &row.toggle,
            &format!("{} {}", if hide { "Show" } else { "Hide" }, row.name),
        );
        row.row.set_visible(!hide || self.showing_hidden.get());
        self.refresh_reveal();
        true
    }

    /// Flips the hidden rows' visibility as a group (the row-by-row
    /// buttons still un-hide individual sensors while they are shown).
    fn flip_reveal(&self) {
        let show = !self.showing_hidden.get();
        self.showing_hidden.set(show);
        for (id, row) in &self.rows {
            if self.engine.borrow().sensor_hidden(id) {
                row.row.set_visible(show);
            }
        }
        self.refresh_reveal();
    }

    /// Keeps the reveal button's label and visibility in step with the
    /// hidden count — with nothing hidden there is nothing to reveal,
    /// so the button leaves the panel entirely.
    fn refresh_reveal(&self) {
        let hidden = self
            .rows
            .keys()
            .filter(|id| self.engine.borrow().sensor_hidden(id))
            .count();
        self.reveal.set_visible(hidden > 0);
        self.reveal.set_label(&match hidden {
            1 if self.showing_hidden.get() => "Hide the shown sensor".to_string(),
            1 => "Show 1 hidden temperature".to_string(),
            n if self.showing_hidden.get() => format!("Hide the {n} shown sensors"),
            n => format!("Show {n} hidden temperatures"),
        });
    }
}

/// Starts the GTK app; `engine` mirrors the service's config and hardware
/// state but never writes to the headers.
///
/// Fan changes are forwarded to the running root daemon over D-Bus (its
/// handle, `client`), and its answers arrive as events on every tick.
/// Handing the headers back to hardware control at exit is a no-op here:
/// the daemon owns them.
///
/// # Panics
///
/// Never in practice: the only `expect` guards the `activate` signal
/// firing on the Adwaita application built right above.
#[must_use]
pub fn run(
    engine: core::Engine,
    client: dbus::Client,
    service_events: mpsc::Receiver<dbus::Event>,
) -> glib::ExitCode {
    let application = adw::Application::builder()
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
        add_packaged_icon_search_path();
        // The signal comes from the application built above, so the
        // downcast cannot fail.
        let application = application
            .downcast_ref::<adw::Application>()
            .expect("activate fires on the Adwaita application we built");
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

/// The section heading class: larger bold text with a rule underneath.
///
/// Sections — Case, Fans, Sensors, Stress lab — read as separate blocks
/// down their columns instead of one long stack, with each body (see
/// [`SECTION_CLASS`]) sitting indented under its heading.
pub const SECTION_TITLE_CLASS: &str = "section-title";
/// The section body class: content indented under its heading.
pub const SECTION_CLASS: &str = "section";

/// The stylesheet the section classes expect, loaded next to the fan-tile
/// one. The title rule spans the panel width; the top margin separates
/// sections, except for the first heading in a column which needs none.
/// The colours are libadwaita's own (the app runs under `adw::Application`).
const SECTION_CSS: &str = "
.section-title {
    margin-top: 16px;
    padding-bottom: 4px;
    border-bottom: 1px solid alpha(@window_fg_color, 0.35);
}

.section-title:first-child {
    margin-top: 0;
}

.section {
    padding-left: 8px;
}
";

/// The sensor-tile class: the fan tiles' soft card, sized down for a
/// chip's name and live reading.
pub const SENSOR_TILE_CLASS: &str = "sensor-tile";

/// The stylesheet [`SENSOR_TILE_CLASS`] expects.
const SENSOR_TILE_CSS: &str = "
.sensor-tile {
    background-color: alpha(@window_fg_color, 0.05);
    border: 1px solid alpha(@window_fg_color, 0.16);
    border-radius: 8px;
    padding: 6px 10px;
}
";

/// A section heading label: libadwaita's `title-4` typographic class
/// does the larger bold text, the stylesheet the spacing and the rule
/// underneath.
#[must_use]
pub fn section_title(text: &str) -> Label {
    let title = Label::builder().label(text).xalign(0.0).build();
    title.add_css_class("title-4");
    title.add_css_class(SECTION_TITLE_CLASS);
    title
}

/// Names a control for assistive technology: the visible labels are only
/// visual neighbours, so the accessible name has to be set explicitly.
pub(crate) fn accessible_name(widget: &impl IsA<gtk4::Accessible>, name: &str) {
    widget.update_property(&[gtk4::accessible::Property::Label(name)]);
}

/// Shows a transient toast: the HIG home for one-shot results (ongoing
/// state stays inline in the panels instead).
fn show_toast(toasts: &adw::ToastOverlay, message: &str) {
    toasts.add_toast(adw::Toast::new(message));
}

/// The app's own symbolic icons ship in `packaging/icons` and reach the
/// system icon theme through the .deb. When running from the repo
/// (`cargo run`) they are not installed, so add that directory to the
/// icon search path; installed runs already find them and skip this.
fn add_packaged_icon_search_path() {
    let icons = std::env::current_dir()
        .unwrap_or_default()
        .join("packaging/icons");
    if icons.is_dir()
        && let Some(display) = gtk4::gdk::Display::default()
    {
        gtk4::IconTheme::for_display(&display).add_search_path(icons.to_string_lossy().as_ref());
    }
}

/// The HIG placeholder for an empty section: a dim symbolic icon, a
/// heading and one line of guidance, instead of a bare text line.
fn placeholder(icon_name: &str, heading: &str, guidance: &str) -> GtkBox {
    let section = GtkBox::new(Orientation::Vertical, 4);
    let icon = gtk4::Image::from_icon_name(icon_name);
    icon.set_pixel_size(32);
    icon.set_halign(Align::Start);
    icon.add_css_class("dim-label");
    let title = Label::builder().label(heading).xalign(0.0).build();
    title.add_css_class("heading");
    let body = Label::builder()
        .label(guidance)
        .wrap(true)
        .xalign(0.0)
        .build();
    body.add_css_class("dim-label");
    section.append(&icon);
    section.append(&title);
    section.append(&body);
    section
}

/// Installs the app's stylesheet (the fan-row tiles from
/// `fan_controls::FAN_TILE_CSS` and the section and sensor-tile classes
/// defined above) on the display this window lives on.
fn install_stylesheet() {
    let provider = gtk4::CssProvider::new();
    // A broken selector would be reported by GTK itself; the rows stay
    // usable plain either way. (`load_from_data`; `load_from_string`
    // would need the gtk `v4_12` feature.)
    provider.load_from_data(FAN_TILE_CSS);
    provider.load_from_data(SECTION_CSS);
    provider.load_from_data(SENSOR_TILE_CSS);
    // The window exists before this runs, so a display always does.
    if let Some(display) = gtk4::gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

/// Builds the window's header bar (HIG: few controls, actions at one
/// end, menus at the other, everything tooltipped): Detect and Save as
/// flat icon+label buttons, a spinner that only appears while a Detect
/// is in flight, and a primary menu holding the rarely-touched
/// refresh-rate spin.
fn build_header_bar(
    engine_rc: &Rc<RefCell<core::Engine>>,
) -> (
    adw::HeaderBar,
    Button,
    Button,
    SpinButton,
    gtk4::Spinner,
    gtk4::MenuButton,
) {
    let header = adw::HeaderBar::new();
    let detect_button = Button::builder()
        .label("Detect fans")
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Spin Up Headers and Park Unconnected Fans")
        .build();
    let save_button = Button::builder()
        .label("Save config")
        .icon_name("document-save-symbolic")
        .tooltip_text("Write Settings to the Service Config (Ctrl+S)")
        .build();
    // The wait for Detect is short and has no result of its own: a
    // spinner next to the button shows it instead of a status message.
    let detect_spinner = gtk4::Spinner::new();
    detect_spinner.set_visible(false);
    // The refresh-rate spin is a setup control, not a per-visit action:
    // it lives in the primary menu, keeping the bar itself spare.
    let interval_spin = SpinButton::builder()
        .digits(1)
        .adjustment(&gtk4::Adjustment::new(1.0, 0.1, 30.0, 0.1, 1.0, 0.1))
        .build();
    interval_spin.set_alignment(1.0); // HIG: right-justified numbers.
    let poll_ms = engine_rc.borrow().poll_interval_ms();
    #[expect(
        clippy::cast_precision_loss,
        reason = "poll interval is tiny compared to u64::MAX"
    )]
    interval_spin.set_value(poll_ms as f64 / 1000.0);
    let settings_box = GtkBox::new(Orientation::Horizontal, 8);
    settings_box.set_margin_start(12);
    settings_box.set_margin_end(12);
    settings_box.set_margin_top(8);
    settings_box.set_margin_bottom(8);
    let interval_label = Label::builder()
        .label("_Update every (s)")
        .use_underline(true)
        .mnemonic_widget(&interval_spin)
        .build();
    settings_box.append(&interval_label);
    settings_box.append(&interval_spin);
    // A popover menu (HIG primary menu): the refresh spin as a custom
    // row, then the standard About item the HIG expects in every
    // primary menu (its "app.about" action is added in `build_window`).
    let menu_model = gio::Menu::new();
    let settings_section = gio::Menu::new();
    // A menu item with the `custom` attribute reserves a slot that
    // `PopoverMenu::add_child` fills with a real widget. (A `custom.<id>`
    // *action* would only make an empty button, not a slot.)
    let settings_item = gio::MenuItem::new(None, None);
    settings_item.set_attribute_value("custom", Some(&"settings".to_variant()));
    settings_section.append_item(&settings_item);
    let about_section = gio::Menu::new();
    about_section.append(Some("About Biggest Fan"), Some("app.about"));
    menu_model.append_section(None, &settings_section);
    menu_model.append_section(None, &about_section);
    let popover = gtk4::PopoverMenu::from_model(Some(&menu_model));
    // If this ever returns false the spin row silently vanishes from the
    // menu, so fail loudly in debug builds.
    let settings_row_placed = popover.add_child(&settings_box, "settings");
    debug_assert!(
        settings_row_placed,
        "settings row must match the custom menu item"
    );
    let menu = gtk4::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .tooltip_text("Main Menu (F10)")
        .popover(&popover)
        .build();
    accessible_name(&menu, "Main Menu");
    // `pack_end` stacks right-to-left: menu outermost, then Save, Detect,
    // spinner.
    header.pack_end(&menu);
    header.pack_end(&save_button);
    header.pack_end(&detect_button);
    header.pack_end(&detect_spinner);
    (
        header,
        save_button,
        detect_button,
        interval_spin,
        detect_spinner,
        menu,
    )
}

/// The left column: the case drawing under a "Case" heading with a
/// short legend, inside its own scrolled pane. Returns the pane and the
/// drawing area the fan rows repaint.
fn build_left_column(engine_rc: &Rc<RefCell<core::Engine>>) -> (ScrolledWindow, gtk4::DrawingArea) {
    let case_area = case_view::install(Rc::clone(engine_rc));
    let legend = Label::builder()
        .label(
            "Circles: blue intake · red exhaust · yellow internal.\nA rectangle is a whole graphics card.\nThe wedge shows commanded speed.",
        )
        .wrap(true)
        .max_width_chars(38)
        .build();
    let left = GtkBox::new(Orientation::Vertical, 8);
    // Each column keeps breathing room inside its pane and a wide gutter
    // around the divider: content touching the split reads as cut off.
    left.set_margin_start(12);
    left.set_margin_end(24);
    left.set_margin_bottom(8);
    left.append(&section_title("Case"));
    let case_section = GtkBox::new(Orientation::Vertical, 8);
    case_section.add_css_class(SECTION_CLASS);
    case_section.append(&case_area);
    case_section.append(&legend);
    left.append(&case_section);
    let left_scroll = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .child(&left)
        .build();
    (left_scroll, case_area)
}

/// Pin a paned's divider so the case side gets a third of its width.
///
/// An unset `Paned` position tracks the children's natural widths, which
/// change as readings, fan rows and hidden lists come and go — the divider
/// visibly drifts. Pinning at `width / 3` keeps the case stable and gives
/// the controls the wider share. The pin re-applies whenever the width
/// changes (window resize or maximize) and stops for good once the user
/// drags the divider: a position change at an unchanged width can only
/// come from the handle.
///
/// A poll watches for width changes because `notify::width` does not fire
/// reliably on this paned (verified on GTK 4.24 under Wayland/GNOME).
fn pin_divider_to_third(paned: &gtk4::Paned) {
    let divider_auto = Rc::new(Cell::new(true));
    let divider_expected = Rc::new(Cell::new(-1));
    let divider_pinned_width = Rc::new(Cell::new(-1));
    let poll_paned = paned.clone();
    let (poll_auto, poll_expected, poll_pinned) = (
        Rc::clone(&divider_auto),
        Rc::clone(&divider_expected),
        Rc::clone(&divider_pinned_width),
    );
    glib::timeout_add_local(Duration::from_millis(250), move || {
        if !poll_auto.get() {
            return ControlFlow::Break;
        }
        let width = gtk4::prelude::WidgetExt::width(&poll_paned);
        if width > 0 && width != poll_pinned.get() {
            let position = width / 3;
            poll_expected.set(position);
            poll_pinned.set(width);
            poll_paned.set_position(position);
        }
        ControlFlow::Continue
    });
    paned.connect_notify_local(Some("position"), move |object, _| {
        let paned = object
            .downcast_ref::<gtk4::Paned>()
            .expect("the position signal is detailed to this paned");
        // A position change at the width we pinned at can only be the
        // user grabbing the handle; their choice then wins for good.
        if divider_auto.get()
            && gtk4::prelude::WidgetExt::width(paned) == divider_pinned_width.get()
            && paned.position() != divider_expected.get()
        {
            divider_auto.set(false);
        }
    });
}

fn build_window(
    application: &adw::Application,
    engine_rc: &Rc<RefCell<core::Engine>>,
    client: &dbus::Client,
    service_events: Rc<RefCell<mpsc::Receiver<dbus::Event>>>,
) {
    let window = adw::ApplicationWindow::builder()
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

    let (header, save_button, detect_button, interval_spin, detect_spinner, menu_button) =
        build_header_bar(engine_rc);

    // Left: the case drawing plus a short legend.
    let (left_scroll, case_area) = build_left_column(engine_rc);

    // Right: fan control rows and the sensor list.
    let right = GtkBox::new(Orientation::Vertical, 12);
    right.set_margin_start(24);
    right.set_margin_end(12);
    right.set_margin_bottom(8);
    let fan_panel = append_fans_panel(&right, engine_rc, &case_area, client);
    let toast_overlay = adw::ToastOverlay::new();
    let sensor_panel = append_sensors_panel(&right, engine_rc, &toast_overlay);
    wire_stress_panel(&window, &right);
    let right_scroll = ScrolledWindow::builder()
        .vexpand(true)
        // `Automatic`, not `Never`: a `Never` pane propagates the content's
        // minimum width to the Paned, which then clamps the divider to it —
        // and that minimum breathes with the per-tick RPM/temperature text,
        // so the divider drifts. A horizontal scrollbar only appears when
        // the pane is genuinely too narrow.
        .hscrollbar_policy(PolicyType::Automatic)
        .child(&right)
        .build();

    let paned = gtk4::Paned::builder()
        .orientation(Orientation::Horizontal)
        .vexpand(true)
        .build();
    paned.set_start_child(Some(&left_scroll));
    paned.set_end_child(Some(&right_scroll));
    pin_divider_to_third(&paned);

    let root = GtkBox::new(Orientation::Vertical, 6);
    // Keep panels off the window edges.
    root.set_margin_start(12);
    root.set_margin_end(12);
    root.set_margin_top(12);
    root.set_margin_bottom(12);
    root.append(&paned);
    // `AdwApplicationWindow` has no titlebar slot — calling
    // `gtk_window_set_titlebar()` on one aborts the process — so the
    // header bar is the first child of the content and draws its own
    // window controls.
    let content = GtkBox::new(Orientation::Vertical, 0);
    content.append(&header);
    content.append(&root);
    // The toast overlay covers the whole window so toasts float at the
    // bottom centre, wherever the panels are (HIG feedback pattern).
    // `AdwApplicationWindow` takes its content through `set_content`;
    // the plain `set_child` aborts on it, like `set_titlebar` does.
    toast_overlay.set_child(Some(&content));
    window.set_content(Some(&toast_overlay));

    let state = Rc::new(GuiState {
        engine: Rc::clone(engine_rc),
        fan_controls: RefCell::new(fan_panel.controls),
        fans_flow: fan_panel.fans_flow,
        fans_placeholder: fan_panel.fans_placeholder,
        hidden_fans_list: fan_panel.hidden_fans_list,
        hidden_fans_reveal: fan_panel.hidden_fans_reveal,
        showing_hidden_fans: Cell::new(false),
        sensor_panel,
        case_area,
        toasts: toast_overlay,
        detect_spinner,
        detect_button: detect_button.clone(),
        detect_pending: Cell::new(false),
        client: client.clone(),
        service_events,
        config_poll: Cell::new(0),
        service_config: RefCell::new(String::new()),
    });
    let tick_source = Rc::new(RefCell::new(None::<SourceId>));
    schedule_tick(&state, &tick_source);

    // An empty Fans section gets the HIG placeholder instead of blank space.
    refresh_fans_placeholder(&state);

    // Hidden fans: the reveal line shows or hides the parked-fan rows,
    // each of which has a Show button that revives its fan.
    refresh_hidden_fans(&state);
    let hidden_state = Rc::clone(&state);
    state.hidden_fans_reveal.connect_clicked(move |_| {
        hidden_state
            .showing_hidden_fans
            .set(!hidden_state.showing_hidden_fans.get());
        refresh_hidden_fans(&hidden_state);
    });

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

    wire_window_actions(application, &window, &state, &menu_button);

    window.present();
}

/// Wires the window-level actions the HIG expects: the primary menu's
/// standard About item, and the standard shortcuts — F10 opens the
/// primary menu, Ctrl+S saves the config, Ctrl+W closes the window,
/// Ctrl+Q quits.
fn wire_window_actions(
    application: &adw::Application,
    window: &adw::ApplicationWindow,
    state: &Rc<GuiState>,
    menu_button: &gtk4::MenuButton,
) {
    let about_action = gio::SimpleAction::new("about", None);
    let about_window = window.clone();
    about_action.connect_activate(move |_, _| {
        // `AboutDialog` (libadwaita 1.6) replaces the deprecated
        // `AboutWindow`; it presents over the main window.
        let about = adw::AboutDialog::builder()
            .application_name("Biggest Fan")
            .application_icon("net.ynottony.ten.BiggestFan")
            .version(env!("CARGO_PKG_VERSION"))
            .comments(
                "Watch every temperature your machine reports — CPU, GPU, memory, drives and more — and drive each fan header from the sensor that matters, with editable curves and hysteresis, so the fans run no faster than they need to.",
            )
            .build();
        about.present(Some(&about_window));
    });
    application.add_action(&about_action);

    let keys = gtk4::EventControllerKey::new();
    let shortcut_state = Rc::clone(state);
    let shortcut_window = window.clone();
    let shortcut_menu = menu_button.clone();
    keys.connect_key_pressed(move |_, key, _keycode, modifiers| {
        // HIG: F10 opens the primary menu, wherever focus is.
        if key == gtk4::gdk::Key::F10 {
            shortcut_menu.set_active(true);
            return Propagation::Stop;
        }
        if !modifiers.contains(gtk4::gdk::ModifierType::CONTROL_MASK) {
            return Propagation::Proceed;
        }
        match key {
            gtk4::gdk::Key::s => {
                on_save_clicked(&shortcut_state);
                Propagation::Stop
            }
            gtk4::gdk::Key::w | gtk4::gdk::Key::q => {
                shortcut_window.close();
                Propagation::Stop
            }
            _ => Propagation::Proceed,
        }
    });
    window.add_controller(keys);
}

/// One round of fan detection, run by the service: it spins every header
/// up briefly and parks the ones with nothing connected (no RPM) or whose
/// driver refuses the spin-up; the findings arrive as a `Detected` event
/// (see `drain_service_events`) and park the matching rows here. The
/// change is kept only after Save config. While it runs, the header
/// spinner turns and the Detect button is insensitive.
fn on_detect_clicked(state: &GuiState) {
    state.detect_pending.set(true);
    state.detect_button.set_sensitive(false);
    state.detect_spinner.start();
    state.detect_spinner.set_visible(true);
    state.client.send(dbus::FanRequest::Detect);
}

/// Ends the Detect wait once its answer (or an error) has arrived.
fn detect_finished(state: &GuiState) {
    if !state.detect_pending.replace(false) {
        return;
    }
    state.detect_spinner.stop();
    state.detect_spinner.set_visible(false);
    state.detect_button.set_sensitive(true);
}

/// Hides the control rows of fans that left the managed set.
fn hide_control_rows(state: &GuiState, ids: &[core::FanId]) {
    for controls in state.fan_controls.borrow().iter() {
        if ids.contains(controls.fan_id()) {
            controls.hide_row();
        }
    }
    refresh_fans_placeholder(state);
    state.case_area.queue_draw();
}

/// Revives a fan parked as `hidden` (the hidden-fans row's Show button):
/// the engine restores the settings detection left behind, so the fan
/// comes back exactly as it was configured.
fn on_show_fan_clicked(state: &Rc<GuiState>, fan_id: &core::FanId) {
    add_fan_to_panel(state, fan_id);
}

/// Puts one parked header back under management: the mirror engine adopts
/// it at once, the service is asked to do the same and save, and a control
/// row appears immediately ([`core::Engine::add_fan`] revives a parked
/// config with its settings).
fn add_fan_to_panel(state: &Rc<GuiState>, fan_id: &core::FanId) {
    let display_name = {
        let mut engine = state.engine.borrow_mut();
        match engine.add_fan(fan_id) {
            Ok(()) => engine
                .managed_fans()
                .into_iter()
                .find(|(managed_id, _)| managed_id == fan_id)
                .map_or_else(|| fan_id.to_string(), |(_, name)| name),
            Err(error) => {
                show_toast(&state.toasts, &format!("Could not add {fan_id}: {error}"));
                return;
            }
        }
    };
    state.client.send(dbus::FanRequest::AddFan {
        fan: fan_id.to_string(),
    });
    let controls = FanControls::new(
        &state.engine,
        fan_id,
        &display_name,
        &state.case_area,
        &state.client,
    );
    state.fans_flow.insert(&controls.container, -1);
    state.fan_controls.borrow_mut().push(controls);
    refresh_hidden_fans(state);
    refresh_fans_placeholder(state);
    state.case_area.queue_draw();
    show_toast(
        &state.toasts,
        &format!("{display_name} added — the service has taken over its header"),
    );
}

/// Shows the Fans placeholder while no control row is visible (the HIG
/// placeholder pattern for an empty list).
fn refresh_fans_placeholder(state: &GuiState) {
    let any_visible = state
        .fan_controls
        .borrow()
        .iter()
        .any(|controls| controls.container.is_visible());
    state.fans_placeholder.set_visible(!any_visible);
}

/// Rebuilds the hidden-fans rows and the reveal line from the engine's
/// parked set — the fans-panel twin of the sensor panel's hidden plumbing.
fn refresh_hidden_fans(state: &Rc<GuiState>) {
    while let Some(child) = state.hidden_fans_list.last_child() {
        state.hidden_fans_list.remove(&child);
    }
    let hidden = state.engine.borrow().hidden_fans();
    for (fan_id, name) in &hidden {
        let row_box = GtkBox::new(Orientation::Horizontal, 8);
        row_box.append(
            &Label::builder()
                .label(name)
                .xalign(0.0)
                .hexpand(true)
                .build(),
        );
        let show = Button::builder()
            .label("Show")
            .tooltip_text("Take This Fan Back Under Control")
            .build();
        row_box.append(&show);
        let row = ListBoxRow::new();
        row.set_child(Some(&row_box));
        state.hidden_fans_list.append(&row);
        let show_state = Rc::clone(state);
        let show_id = fan_id.clone();
        show.connect_clicked(move |_| on_show_fan_clicked(&show_state, &show_id));
    }
    let count = hidden.len();
    state
        .hidden_fans_list
        .set_visible(state.showing_hidden_fans.get() && count > 0);
    // With nothing parked there is nothing to reveal: the line leaves
    // the panel until the count is back above zero.
    state.hidden_fans_reveal.set_visible(count > 0);
    state.hidden_fans_reveal.set_label(&match count {
        1 if state.showing_hidden_fans.get() => "Hide the shown fan".to_string(),
        1 => "Show 1 hidden fan".to_string(),
        n if state.showing_hidden_fans.get() => format!("Hide the {n} shown fans"),
        n => format!("Show {n} hidden fans"),
    });
}

/// Asks the service to apply and save the GUI's current config (mode and
/// name changes from the controls, placement from the canvas); the
/// service's config file is the live one. The confirmation arrives as a
/// `Status` event and lands in a toast.
fn on_save_clicked(state: &GuiState) {
    let engine = state.engine.borrow();
    match core::config::to_text(&engine.current_config()) {
        Ok(toml_text) => state.client.send(dbus::FanRequest::Save { toml_text }),
        Err(error) => show_toast(
            &state.toasts,
            &format!("Could not serialise the config: {error}"),
        ),
    }
}

/// Applies whatever the service answered since the last tick.
fn drain_service_events(state: &Rc<GuiState>) {
    let guard = state.service_events.borrow_mut();
    while let Ok(event) = guard.try_recv() {
        match event {
            dbus::Event::Status(message) => {
                // A Status answer while a Detect is in flight is its
                // failure path (the success path answers `Detected`).
                detect_finished(state);
                show_toast(&state.toasts, &message);
            }
            dbus::Event::Detected(ids) => {
                detect_finished(state);
                let ids: Vec<core::FanId> = ids.into_iter().map(core::FanId::new).collect();
                state.engine.borrow_mut().park_fans(&ids);
                hide_control_rows(state, &ids);
                refresh_hidden_fans(state);
                if ids.is_empty() {
                    show_toast(
                        &state.toasts,
                        "Nothing to disable — every managed header spun up",
                    );
                } else {
                    let headers = ids
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ");
                    let toast = adw::Toast::new(&format!(
                        "{headers} disabled — press Save config to keep them out"
                    ));
                    toast.set_button_label(Some("Undo"));
                    let undo_state = Rc::clone(state);
                    toast.connect_button_clicked(move |_| {
                        // Undo takes every parked header back under
                        // management; each `add_fan_to_panel` confirms
                        // its own fan with a toast of its own.
                        for id in &ids {
                            add_fan_to_panel(&undo_state, id);
                        }
                    });
                    state.toasts.add_toast(toast);
                }
            }
            dbus::Event::Config(text) => {
                // The service's config is the live one (it parks fans whose
                // driver refuses writes, and its file can be hand-edited),
                // so the mirror adopts it to draw what the fans are really
                // doing. An unchanged answer changes nothing here.
                let mut applied = state.service_config.borrow_mut();
                if text == *applied {
                    continue;
                }
                applied.clone_from(&text);
                drop(applied);
                if let Err(error) = core::config::from_text(&text)
                    .and_then(|config| state.engine.borrow_mut().apply_config(config))
                {
                    *state.service_config.borrow_mut() = String::new();
                    show_toast(
                        &state.toasts,
                        &format!("Service config did not apply: {error}"),
                    );
                } else {
                    // A hand-edited service config can hide or show fans;
                    // the hidden-fans line follows it.
                    refresh_hidden_fans(state);
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

/// The "Fans" panel: one control row per managed fan (in a flow
/// new rows can be inserted into), a placeholder while none are visible,
/// and the parked-as-hidden fans behind a reveal line.
struct FanPanel {
    controls: Vec<FanControls>,
    fans_flow: FlowBox,
    fans_placeholder: GtkBox,
    hidden_fans_list: ListBox,
    hidden_fans_reveal: Button,
}

/// Appends the Stress lab panel to `right` and ties its stress threads
/// to the window: closing the window raises their stop flags, so no
/// load outlives the window that started it.
fn wire_stress_panel(window: &adw::ApplicationWindow, right: &GtkBox) {
    let stress_panel = stress::append_stress_panel(right);
    let close_panel = Rc::clone(&stress_panel);
    window.connect_close_request(move |_window| {
        close_panel.stop_all();
        Propagation::Proceed
    });
}

/// Appends the fan section (header, one control row per managed fan,
/// then the hidden-fans reveal line under them all) to `panel`.
fn append_fans_panel(
    panel: &GtkBox,
    engine_rc: &Rc<RefCell<core::Engine>>,
    case_area: &gtk4::DrawingArea,
    client: &dbus::Client,
) -> FanPanel {
    panel.append(&section_title("Fans"));
    // The fan rows are flow tiles: one per line on a 1080p-sized panel,
    // two side by side once the panel is wide enough for a 4K screen.
    // Each tile's minimum content width (the two graphs side by side)
    // is what decides where the flow breaks.
    let section = GtkBox::new(Orientation::Vertical, 8);
    section.add_css_class(SECTION_CLASS);
    let fans_flow = FlowBox::new();
    // The gap between tiles (and between tiles and the section edge):
    // the flow's own spacing, not child padding — padding on the flow's
    // cells proved too subtle next to the tiled borders.
    fans_flow.set_row_spacing(24);
    fans_flow.set_column_spacing(16);
    fans_flow.set_min_children_per_line(1);
    fans_flow.set_max_children_per_line(2);
    fans_flow.set_selection_mode(gtk4::SelectionMode::None);
    section.append(&fans_flow);
    // Empty state (HIG placeholder): shown while no fan row is visible,
    // so the section never reads as broken or still loading.
    let fans_placeholder = placeholder(
        "net.ynottony.ten.BiggestFan-symbolic",
        "No fans under control",
        "Press Detect fans to find them.",
    );
    section.append(&fans_placeholder);
    let fan_ids_and_names = engine_rc.borrow().managed_fans();
    let mut fan_controls = Vec::new();
    for (fan_id, name) in fan_ids_and_names {
        let controls = FanControls::new(engine_rc, &fan_id, &name, case_area, client);
        fans_flow.insert(&controls.container, -1);
        fan_controls.push(controls);
    }
    // Hidden fans get the hidden-sensors treatment: collapsed behind a
    // named count line, each with a Show button that takes it back under
    // control — no config editing needed.
    let hidden_fans_reveal = Button::builder().tooltip_text("Show Hidden Fans").build();
    let hidden_fans_list = ListBox::new();
    hidden_fans_list.set_selection_mode(gtk4::SelectionMode::None);
    hidden_fans_list.set_visible(false);
    section.append(&hidden_fans_reveal);
    section.append(&hidden_fans_list);
    panel.append(&section);
    FanPanel {
        controls: fan_controls,
        fans_flow,
        fans_placeholder,
        hidden_fans_list,
        hidden_fans_reveal,
    }
}

/// Appends the sensor section (header, a small tile with the live
/// reading and a hide button per discovered sensor, then the
/// hidden-count line) to `panel`.
fn append_sensors_panel(
    panel: &GtkBox,
    engine_rc: &Rc<RefCell<core::Engine>>,
    toasts: &adw::ToastOverlay,
) -> Rc<SensorPanel> {
    panel.append(&section_title("Sensors"));
    let section = GtkBox::new(Orientation::Vertical, 8);
    section.add_css_class(SECTION_CLASS);
    // Small tiles instead of full-width rows: a chip's name and reading
    // need little room, and a board can expose dozens of them.
    let sensor_flow = FlowBox::new();
    sensor_flow.set_selection_mode(gtk4::SelectionMode::None);
    sensor_flow.set_min_children_per_line(1);
    sensor_flow.set_max_children_per_line(8);
    sensor_flow.set_homogeneous(false);
    sensor_flow.set_column_spacing(8);
    sensor_flow.set_row_spacing(8);
    let mut rows = BTreeMap::new();
    let sensors = engine_rc.borrow().sensors().to_vec();
    for sensor in sensors {
        let name = engine_rc.borrow().sensor_name(&sensor.id);
        // A sensor hidden by the config starts out of the flow.
        let hidden = engine_rc.borrow().sensor_hidden(&sensor.id);
        let tile = FlowBoxChild::new();
        tile.add_css_class(SENSOR_TILE_CLASS);
        // Name and hide button share the top line (button at the right),
        // the live reading gets the line below to itself.
        let name_label = Label::builder()
            .label(&name)
            .xalign(0.0)
            .hexpand(true)
            .build();
        name_label.add_css_class("dim-label");
        let value = Label::builder().xalign(0.0).build();
        value.set_text("—");
        // Fixed budget for the live reading ("99.9 °C" -> "100.0 °C"):
        // a constant natural width keeps the sensor flow from re-wrapping
        // its tiles on every tick.
        value.set_width_chars(12);
        let spark = Sparkline::new();
        let toggle = Button::builder()
            .icon_name(if hidden {
                "view-reveal-symbolic"
            } else {
                "view-conceal-symbolic"
            })
            .tooltip_text("Hide This Temperature")
            .build();
        toggle.add_css_class("flat");
        accessible_name(
            &toggle,
            &format!("{} {name}", if hidden { "Show" } else { "Hide" }),
        );
        let top_row = GtkBox::new(Orientation::Horizontal, 6);
        top_row.append(&name_label);
        top_row.append(&toggle);
        // The live reading and its sparkline share the bottom line.
        let value_row = GtkBox::new(Orientation::Horizontal, 6);
        value_row.append(&value);
        value_row.append(spark.widget());
        let tile_box = GtkBox::new(Orientation::Vertical, 2);
        tile_box.append(&top_row);
        tile_box.append(&value_row);
        tile.set_child(Some(&tile_box));
        tile.set_visible(!hidden);
        sensor_flow.append(&tile);
        rows.insert(
            sensor.id,
            SensorRow {
                name,
                value,
                spark,
                row: tile,
                toggle,
            },
        );
    }
    section.append(&sensor_flow);
    if rows.is_empty() {
        // Empty state (HIG placeholder) for a machine whose temperature
        // chips the core could not read.
        let empty = placeholder(
            "net.ynottony.ten.BiggestFan-temperature-symbolic",
            "No temperature sensors found",
            "Check that the hardware-monitor (hwmon) kernel modules are loaded.",
        );
        section.append(&empty);
    }
    let reveal = Button::builder()
        .tooltip_text("Show Hidden Sensors")
        .build();
    section.append(&reveal);
    panel.append(&section);
    let sensor_panel = Rc::new(SensorPanel {
        engine: Rc::clone(engine_rc),
        toasts: toasts.clone(),
        rows,
        reveal: reveal.clone(),
        showing_hidden: Cell::new(false),
    });
    sensor_panel.refresh_reveal();
    let reveal_panel = Rc::clone(&sensor_panel);
    reveal.connect_clicked(move |_| reveal_panel.flip_reveal());
    for (id, row) in &sensor_panel.rows {
        let toggle_panel = Rc::clone(&sensor_panel);
        let toggle_id = id.clone();
        row.toggle.connect_clicked(move |_| {
            // The button label shows the action, so hide iff it says
            // "Hide" — i.e. the sensor is currently visible.
            let hide = !toggle_panel.engine.borrow().sensor_hidden(&toggle_id);
            toggle_panel.set_hidden(&toggle_id, hide);
        });
    }
    sensor_panel
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
fn tick_once(state: &Rc<GuiState>) {
    drain_service_events(state);

    // The service owns the live config — it parks fans whose driver
    // refuses writes, and its file can be hand-edited — so ask it for the
    // text now and then. A queued change sits in the same FIFO queue, so
    // an answer can be one poll behind.
    let poll_tick = state.config_poll.get();
    state.config_poll.set(poll_tick.wrapping_add(1));
    if poll_tick.is_multiple_of(SERVICE_CONFIG_POLL_TICKS) {
        state.client.send(dbus::FanRequest::GetConfig);
    }

    let report = match state.engine.borrow_mut().tick() {
        Ok(report) => report,
        Err(error) => {
            show_toast(&state.toasts, &format!("Control loop error: {error}"));
            return;
        }
    };

    for controls in state.fan_controls.borrow().iter() {
        match report.fans.iter().find(|fan| fan.id == *controls.fan_id()) {
            Some(status) => {
                // Notes (a fail-safe, a refused write) show on the row
                // itself — ongoing state stays inline, not in a toast.
                controls.update(status);
            }
            None => {
                // The control loop parked this fan (for example its driver
                // refused writes), so its row leaves the panel too.
                controls.hide_row();
            }
        }
    }
    for sensor in &report.sensors {
        if let Some(row) = state.sensor_panel.rows.get(&sensor.id) {
            let temp = sensor
                .temp_c
                .map_or_else(|| "no reading".to_string(), |temp| format!("{temp:.1} °C"));
            row.value.set_text(&temp);
            if let Some(temp_c) = sensor.temp_c {
                row.spark.record(temp_c);
            }
        }
    }
    refresh_fans_placeholder(state);
    state.case_area.queue_draw();
}
