# GNOME HIG review — Biggest Fan

A pass through the [GNOME Human Interface Guidelines](https://developer.gnome.org/hig/)
item by item, checking the GUI (`crates/app`) against each.

Legend:

- ✅ follows the HIG
- ⚠️ partially follows — improvement noted
- ❌ gap against an explicit HIG rule
- ➖ not applicable to this app

Pages marked _(quick pass)_ were reviewed from general HIG knowledge, not the
fetched page text; the rest were reviewed against the current page text
(Nov 2025). Actionable items are collected at the bottom.

## Design Principles

### Design for People

✅ Hardware-monitoring audience is served directly: friendly chip names, plain
language, no config editing needed for normal use.
✅ Curve editing now has a keyboard path (focus the graph; see Keyboard).
⚠️ Case-view fan placement is still pointer-only — the one remaining
keyboard gap (see Keyboard below).

### Make it Simple

✅ One job (fans + temperatures). Progressive disclosure: per-row
_Extra settings_ expander keeps the common controls forward, the rare ones back.
✅ Header bar holds only the two window-level actions.

### Reduce User Effort

✅ Detect fans auto-matches headers to sensors; defaults are sensible.
✅ Live numbers, trend graphs and notes keep state visible without memory.
✅ Renaming a fan commits on Return or focus-out, not on every keystroke.

### Be Considerate

✅ A half-dragged curve is never committed; drags only land on release.
✅ Hiding a sensor or fan toasts with an **Undo** button — the HIG's
preferred pattern ("allow destructive actions to be undone instead of
asking for confirmation"). The reveal lines stay as the group fallback.

## Guidelines

### App Naming

✅ "Biggest Fan": two simple nouns, domain-related, short, header
capitalization, no trademarks or made-up words.

### App Icons

✅ Full-color icon exists (SVG + PNG sizes, installed by the .deb) with a
fan metaphor, and a symbolic app icon now ships with it
(`hicolor/symbolic/apps/net.ynottony.ten.BiggestFan-symbolic.svg`, plus a
temperature symbolic icon for the sensors placeholder).
⚠️ Visual style of the full-color icon (128px grid, baseline, profile) not
checked — do it with App Icon Preview.

### Pointer & Touch _(quick pass)_

✅ Direct manipulation is used well: drag the fan shape on the case view,
drag curve handles, double-click to add, right-click to delete.
✅ Curve editing has a full keyboard equivalent (see Keyboard); case-view
placement is the remaining pointer-only interaction.

### Keyboard

✅ Ctrl+S (save), Ctrl+W (close) and Ctrl+Q (quit) — standard, Ctrl+letter.
✅ F10 opens the primary menu — wired explicitly in `wire_window_actions`
(GTK4 has no default F10 binding; the earlier "GTK default" assumption
was wrong and F10 did nothing).
✅ Esc closes menus/popovers (GTK default), and clears the curve-graph
selection while the graph has focus.
✅ Curve editing has a keyboard path: the graph is focusable (Tab),
Left/Right select a handle, Up/Down nudge its speed, Shift+Left/Right its
temperature, Plus adds a point and Minus removes one; a focus ring shows
when the graph holds focus.
⚠️ Case-view placement (moving a fan shape) is still pointer-only.
➖ Mnemonic decision (documented deviation): access keys live only on the
window-unique labels (Update every, Stop after, Guard at). Per-row controls
deliberately have none — identical Alt-keys would repeat across every fan
tile and collide, and the HIG's "unique per window" rule cannot be met with
repeated rows; the per-row controls carry accessible names instead.

### UI Icons

✅ Symbolic style throughout (`view-refresh-symbolic`,
`document-save-symbolic`, `open-menu-symbolic`, `view-reveal/conceal-symbolic`),
all from the standard set.
✅ Icon-only where recognition is conventional (menu, eye); labels elsewhere.

### UI Styling

✅ Follows the system light/dark style via `AdwStyleManager` (no per-app
override — fine for this app class).
✅ Custom CSS is minimal and built on theme variables
(`alpha(@window_fg_color, …)`), so it adapts to light/dark/high-contrast.
✅ Error text uses a `.status-error` CSS class with `@error_color`, so it
follows high-contrast mode.
⚠️ The cairo graphs hardcode Adwaita approximations (`theme.rs`): they follow
light/dark and the accent, but not high-contrast mode (GTK 4 has no
`compute_color`; a documented limitation). The relief series now uses the
HIG palette's Orange 2/4.
✅ The trend graph distinguishes the two series by line style as well as
colour (solid °C, dashed %, with a small corner legend).

### Writing Style

✅ Sentence case for body/tooltip sentences; informal heading style for toast
titles ("Config saved"); no pronouns; no Latin abbreviations.
✅ Trailing periods dropped from single-sentence tooltips.
⚠️ Button labels: the page asks for **header capitalization** on short control
labels ("button labels"), but the app uses sentence case ("Detect fans",
"Save config"). Real GNOME apps are mixed; pick one and be consistent —
currently the app is at least internally consistent.
✅ No ellipses needed (no confirm-then-act dialogs).

### Typography _(quick pass)_

✅ Default label fonts; hierarchy via size/weight.
✅ `section_title()` uses libadwaita's `.title-4` class (the placeholder
headings use `.heading`); the CSS only adds spacing and the rule.

### Navigation _(quick pass)_

✅ Single-window app, no hierarchical navigation to get wrong.

### Scaling & Adaptiveness _(quick pass)_

⚠️ Fixed two-column layout; FlowBoxes wrap tiles at wide widths but there is
no narrow-window/mobile adaptation (columns just scroll). Acceptable for a
desktop hardware tool; worth knowing it's a conscious deviation.
⚠️ Window always opens 1600×900 maximised; the HIG suggests restoring the
previous window size for apps that restore state.

### Accessibility

✅ Accessible names on every entry, dropdown, slider, spin and icon button;
the curve graph carries an accessible description of its keyboard keys.
✅ Labels precede their controls in the widget order.
✅ Keyboard-only curve editing works (the former big gap).
⚠️ Still to test by hand against the HIG checklist: high-contrast mode,
large text, screen reader, on-screen keyboard, and keyboard-only case-view
placement (not yet possible). Error text and the graphs follow light/dark;
high-contrast for the cairo graphs is a known limitation.

## Patterns — Containers

### Windows

✅ Single resizable primary window; content-appropriate default size.
✅ Ctrl+W closes the window.
➖ Secondary windows (About/Preferences) — none; About is a dialog (see
Menus), which the HIG accepts for a short about box.

### Header Bars

✅ `AdwHeaderBar`; actions at the start, menu at the end; flat icon+label
buttons (the style the HIG says works automatically in header bars); every
header control has a tooltip; spinner sits in the bar beside the action it
relates to.
⚠️ At narrow widths the bar fills up and loses drag space — minor for a
1600-px-default app.

### Popovers _(quick pass)_

✅ Standard `MenuButton` popover (GTK-managed: Esc, focus; F10 is
app-wired, see Keyboard above).

### Utility Panes, Boxed Lists, Grid Views, List & Column Views,

### Selection & Edit Modes _(quick pass)_

➖ Not used. Fan/sensor tiles are custom FlowBox cards rather than boxed
lists — fine, but they're custom UI the HIG's testing advice applies to.
The hidden-fans list is a plain `ListBox`.

## Patterns — Navigation

### Browsing, View Switchers, Tabs, Sidebars _(quick pass)_

➖ Single view; none used, none needed.

### Search

✅ The sensor dropdown sets `enable-search` — the right pattern for a long
sensor list.

## Patterns — Controls

### Buttons

✅ Outside header bars, buttons are icon-only or label-only (sensor hide
icon button, "Show" label buttons).
✅ Linked toggle buttons for the three fan modes and for the two-option
slot shape — exactly the HIG's recommendation for short option sets in a
row.
✅ No suggested/destructive styles misused; no double-click/right-click on
buttons (the graph handles are direct manipulation, not buttons).
⚠️ "Make invalid buttons insensitive rather than showing an error": inserting
a curve point too close to a neighbour errors after the click; clamping or
insensitising would be more HIG-ish.

### Menus

✅ `open-menu-symbolic`, at the end of the header bar; no Quit/Close items.
✅ The menu is a `PopoverMenu` from a model and includes **About Biggest
Fan** (`adw::AboutDialog`); the refresh-rate spin lives in a custom row.
✅ The menu button's tooltip and accessible name are "Main Menu".
➖ Keyboard Shortcuts / Help / Preferences items stay out — the app has no
shortcuts window or preferences window to point at.

### Switches, Checkboxes, Radio Buttons _(quick pass)_

➖ None used; linked toggles cover the mode and shape choices.

### Text Fields

✅ Fan-name entry sized to content; changes apply on Return or focus-out.
⚠️ No visible label or placeholder on the name entry (the tooltip and
accessible name carry the hint) — deliberate: each entry sits inside a
titled fan tile, and a per-row label would just repeat the tile's heading.

### Drop-Down Lists

✅ `gtk4::DropDown` throughout (ComboBoxText deprecated); sentence-case
labels; items are short option names.
✅ Shape is now linked toggle buttons, not a two-item drop-down.
➖ No access keys on labels or items — the documented deviation (repeated
fan rows cannot keep them unique per window; see the Keyboard note).

### Sliders

✅ Speed slider: fixed 0–100 range, live feedback (the fan and graphs react),
value shown, and a "Speed" text label beside it.

### Spin Buttons

✅ Exact numbers are meaningful for all spins (rpm factor, floor, diameter,
card size, interval, durations, guard).
✅ All spins right-justify their numbers.
➖ Access keys only on the header/stress spins — the documented deviation.

### Overlaid Controls _(quick pass)_

➖ None. The curve readout is painted on the canvas rather than overlaid —
deliberate (a GTK tooltip would cover the curve).

## Patterns — Feedback

### Notifications _(quick pass)_

➖ None sent. Everything shown is only useful while the window is open, which
is the HIG's own test for "toast, not notification". The daemon logs its own
problems.

### Toasts

✅ `AdwToastOverlay` (bottom-centre, dismissible, transient); short informal
heading titles; used for one-shot results, not ongoing state.
✅ Hide/show and detect toasts carry an **Undo** button that reverts quietly
(no second toast).

### Banners _(quick pass)_

⚠️ Ongoing state (fan notes, stress progress) is inline in the owning tile —
reasonable, though the HIG's banner is the named pattern for window-level
ongoing state. No window-level banner is currently needed.

### Progress Bars _(quick pass)_

✅ Stress runs show a per-stressor progress bar (elapsed/duration as bar
\_text); Detect is short, so its spinner is the right control there.
The bars follow the Spinners page's ">30 s prefer a progress bar" note.

### Spinners

✅ One spinner, in the header bar beside the Detect button, only while the
operation is in flight; Detect button insensitive meanwhile.

### Dialogs _(quick pass)_

✅ None — the app avoids confirmation dialogs entirely, which the HIG prefers.

### Placeholder Pages

✅ Both empty states are compact placeholders: a dim symbolic icon (the
app's fan glyph / a thermometer), a heading and one line of guidance.

### Tooltips

✅ All header-bar controls have tooltips; tooltips for icon-only controls.
✅ Tooltip text is short header-capitalization labels ("Commanded Speed
Percent", "Governing Sensor"), not sentences.
✅ Coverage within a fan row is now even: name entry, mode, speed, sensor,
preset, airflow, shape and the spins all carry tooltips.

## Reference

### Standard Keyboard Shortcuts

✅ Ctrl+S, Ctrl+W, Ctrl+Q.
➖ F1/Ctrl+?/Ctrl+, apply only if Help/shortcuts-window/Preferences exist —
none do; the About dialog covers the standard "About" item.

### Palette

✅ The graphs' fixed colours (`theme.rs`) approximate Adwaita _theme_ colours
(fine — the palette page is for icons/illustrations), and the relief series
now uses palette Orange 2 `#ffa348` (dark) / Orange 4 `#e66100` (light).

### Backgrounds _(quick pass)_

✅ Default window background; tiles use translucent theme-variable tints.

## Improvement list (prioritised)

Done markers: ✅ implemented; ➖ decided against (deviation documented
above); open items keep their old marker.

1. ✅ Add **About Biggest Fan** to the primary menu (`adw::AboutDialog`),
   tooltip/accessible name "Main Menu".
2. ✅ Add **Ctrl+W** to close the window.
3. ✅ Curve editing has a keyboard path: focusable graph (focus ring),
   Left/Right select, Up/Down nudge speed, Shift+arrows nudge temperature,
   Plus/Minus add and remove, Esc clears the selection.
4. ✅ **Shape** choice: linked toggle buttons (the mode segmented control,
   generalised to `Segmented`), not a drop-down.
5. ✅ Tooltips trimmed to short header-case labels; every fan-row control
   now has one.
6. ✅ Trailing periods dropped from single-sentence tooltips.
7. ✅ Hide/show and detect toasts carry an **Undo** button.
8. ✅ Error colour: `.status-error` CSS class with `@error_color`.
   (The cairo graphs' fixed colours stay a documented limitation.)
9. ✅ Placeholders: dim symbolic icon + heading + guidance, both panels.
10. ✅ Fan rename commits on Return/focus-out.
11. ✅ Trend graph: solid °C vs dashed % with a corner legend.
12. ➖ Mnemonics: window-unique labels only; per-row controls use accessible
    names (unique Alt-keys across repeated rows are impossible).
13. ✅ Spins right-justified; "Speed" label beside the slider.
14. ✅ Symbolic app icon + temperature symbolic icon ship in the .deb.
    ⚠️ Full-color icon vs App Icon Preview template: still to check by hand.
15. ⚠️ Accessibility checklist: keyboard-only curve editing now works; high
    contrast, large text and screen-reader passes still to run by hand.
16. ✅ Relief orange → palette Orange 2/4; section headings → `.title-4`.
    ✅ Stress runs >30 s → per-stressor progress bars in the Stress lab.
