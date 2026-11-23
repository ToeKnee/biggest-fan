# Biggest Fan

A smart fan controller app.

Read temperatures from lm-sensor and nvidia graphics cards and control fans speed.

Map sensors names to human readable names ("CPU temperature" instead of "k10temp Tcctl", "CPU CCD 1 temperature" instead of "k10temp Tccd1" etc.)
Map fans to a graphical representation of a modern PC case.
Different fans should be able to be controlled be different sensors.
Fans can be to be turned off, run at a constant speed or automatically respond to sensors.
Handles hysteresis so fans do not pump up and down: speed goes up like a rocket and down like a feather.
Has adjustable update time (default to 1 second refresh)

## Development

### Requirements

- Rust toolchain supporting edition 2024
- Linux (the core reads and writes hwmon files under `/sys/class/hwmon`)
- GTK 4 development packages, e.g. on Debian/Ubuntu:
  `sudo apt install libgtk-4-dev pkg-config`
- For NVIDIA GPU temperatures: the system NVIDIA driver provides
  `libnvidia-ml.so` (loaded at runtime via NVML). Both binaries always
  attempt NVML: without NVIDIA management no NVIDIA sensors or coolers
  simply show up, so no configuration is needed on non-NVIDIA machines.

### Workspace layout

| Path                | Crate                    | What it is                                                                                                                                                                                                                      |
| ------------------- | ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/core`       | `biggest-fan-core`       | Library: sensor discovery (hwmon + NVML), human naming, fan/PWM control, config (TOML), case layout, and the control-loop `Engine` with hysteresis.                                                                             |
| `crates/app`        | `biggest-fan`            | Library plus two binaries: `biggest-fan` (GTK 4 GUI — case view + fan controls — and the `scan`, `init-config`, `detect-fans` one-shot commands) and `biggest-fan-daemon` (headless control loop, for systemd).                 |
| `crates/stress-lab` | `biggest-fan-stress-lab` | Library plus developer CLI `stress-lab`: hand-written CPU (AVX-512) and GPU (Vulkan compute) stressors and a CSV temp/fan/PWM monitor. The library also drives the GUI's Stress lab panel; the CLI binary stays developer-only. |
| `packaging/desktop` | —                        | The `net.ynottony.ten.BiggestFan.desktop` launcher installed by the .deb.                                                                                                                                                       |
| `packaging/icons`   | —                        | The app icon (scalable SVG source + generated PNG sizes) installed into the hicolor theme by the .deb.                                                                                                                          |
| `packaging/systemd` | —                        | The `biggest-fan.service` unit installed by the .deb.                                                                                                                                                                           |

### Common commands

```sh
cargo build
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all                        # rustfmt, edition 2024
cargo doc -p biggest-fan-core --no-deps
cargo build -p biggest-fan-stress-lab --release  # stressors: always release — debug is ~100x slower
./target/release/stress-lab --help
sh tools/dbus-e2e.sh                   # live D-Bus smoke check: private bus, --dry-run
sh tools/render-icons.sh               # regenerate PNG icon sizes from the SVG (needs inkscape)
```

`tools/dbus-e2e.sh` proves the daemon↔GUI plumbing end to end: it starts a
throwaway private bus, checks the daemon answers `busctl` calls, that the
GUI exits with the service-required error when no service is running, that
it joins a running service and shows a window (GUI checks skipped without
a display), and that the daemon keeps running when the bus is broken.

## Usage

Global flags (both binaries): `--config PATH` (default
`~/.config/biggest-fan/config.toml`) and `--dry-run` (compute speeds but
never write to the fan headers). NVIDIA handling needs no flag: a machine
without an NVIDIA GPU just discovers no NVIDIA sensors or coolers. The
daemon's fan speed table can also quiet down the journal: `quiet = true`
in the config, or its `--quiet` flag (either one) — the table then prints
once at startup, proof the loop is running, and after that only for fans
carrying a warning.

```sh
biggest-fan scan          # list discovered sensors and fan headers
biggest-fan init-config   # write a starter config from live hardware (--force to replace)
biggest-fan detect-fans   # spin headers up briefly, disable ones with nothing connected
biggest-fan gui           # case view + fan controls (also the default)
biggest-fan-daemon        # headless control loop, for systemd (quiet log: --quiet or config quiet = true)
```

Writing to `pwm` sysfs files requires root, so the daemon must run as
root: the `biggest-fan.service` service (installed, enabled and started by
the .deb — see Packaging) or `sudo biggest-fan-daemon` in a terminal. Use
`--dry-run` to try the control loop safely first; no header is touched.

The GUI never writes headers itself: it **requires** the running daemon,
joins it over D-Bus and forwards every change to it (see "The GUI without
root" below). Without a reachable service it prints which part is missing
and exits. The relationship is one-way: the daemon controls the fans on
its own whether the GUI is open or not — opening, closing or crashing
the GUI changes nothing. `scan`, `init-config` and `detect-fans` each
build their own `Engine` and read `/sys/class/hwmon` directly; root is
needed for _writes_ only there.

## Configuration

TOML; a missing file is auto-generated from the live hardware on first run
(review it before trusting it). All fields shown here are optional and fall
back to the defaults. Keys an older version used but this one no longer has
(for example a `[nudge]` section) are accepted and ignored, then dropped
the next time the config is saved:

```toml
poll_interval_ms = 1000   # control-loop refresh period (floors at 100 ms)
fail_safe_speed = 1.0     # speed for auto fans whose sensor disappears
hidden_sensors = ["nvme-2-t1"]  # temperatures to hide from the GUI list
quiet = false             # daemon: print the fan table at startup + for
                          # warnings only (--quiet does the same thing)

[hysteresis]
temp_band_c = 2.0           # how far a sensor may fall before speed drops
max_step_per_tick = 0.1     # feather: max speed drop per tick (0 or 1.0 = none)
max_step_up_per_tick = 0.5  # rocket: max speed rise per tick (0 or 1.0 = at once)

[sensor_names]            # human readable overrides, keyed by sensor id
"k10temp-t3" = "CPU CCD 1 temperature"

[[fans]]
id = "nct6795-p1"         # the header this fan drives, from `biggest-fan scan`
name = "Front 120mm intake"
hidden = false            # `detect-fans` sets true when nothing is connected
rpm_factor = 1.0          # tach correction: shown rpm = reported × this
                          # (0.5 halves a doubled tachometer)
min_speed = 0.0           # never command less than this; 0.4 keeps an AIO
                          # pump turning whatever its curve asks for
mode = { type = "auto", sensor = "k10temp-t1", curve = [
    { temp_c = 40.0, speed = 0.2 },
    { temp_c = 80.0, speed = 1.0 },
] }
# other modes: { type = "off" }, { type = "fixed", speed = 0.35 }
# the GUI's curve dropdown can replace `curve` with a preset instead:
# Quiet (stopped while cool, ramps hard only near the temperature limit),
# Balanced (the default) or Performance (ramps early); a curve that is not
# exactly one of the three — hand-written here, or dragged on the graph —
# shows as "Custom"

[fans.slot]               # where to draw the fan on the case view
x = 40.0
y = 380.0
diameter = 70.0
airflow = "intake"        # "intake", "exhaust" or "internal"
                          # a whole rectangular object (graphics-card
                          # cooler) is drawn instead of a circle when:
# rect = [220.0, 80.0]    # width, height in canvas units
```

Hysteresis works two ways: a sensor must fall `temp_band_c` below the
temperature that last raised an auto fan's speed before that fan slows down,
and the ramp is asymmetric — up like a rocket, down like a feather. Raising
speed is the safe direction, so it runs at `max_step_up_per_tick` (0.5 gets
to full speed within two ticks of a heat spike, without slamming from idle
in one step); the way back down crawls at `max_step_per_tick`, so fans go
quiet slowly instead of pumping up and down. Once a drop is allowed, the fan
keeps taking feather steps every tick until it reaches the curve, even if
the sensor sits still; the hold band only re-anchors when the drop lands.
Setting either to 0 or 1.0
removes that limit (failsafes bypass both). If an auto fan's sensor stops
reporting, the fan goes to `fail_safe_speed`.

Changing a fan's profile — swapping the curve preset, re-binding the sensor,
or coming back to _auto_ from a fixed speed — continues the ramp from the
speed the fan is already at instead of restarting it at 0. Only _off_ resets
the baseline: a fan you deliberately stopped really does start its next ramp
there. Taking over follows the same rule: at daemon startup (and when a fan
is added), the engine first reads the duty each header currently holds and
starts its ramp there — a fan the BIOS had spinning is never dipped because
Biggest Fan began managing it.

A fan's `min_speed` floor outranks all of that: in fixed or auto mode the
engine can never take it lower, and it jumps straight to the floor instead of
ramping up through speeds it is not allowed to run at. Only _off_ stops a
floored fan, because that is a choice you made. On an NVIDIA cooler a floor
under the park point cannot start the fans, so it parks the cooler instead
(see _NVIDIA GPU coolers_).

### Names, ordering and matching

- Chip names are prettified for display only (`nct6799` → “NCT 6799”,
  `arctic_fan` → “Arctic Fan”); configuration ids always stay raw.
- Dead temperature inputs are skipped at discovery: unreadable files,
  and inputs reading exactly `0` — unconnected placeholder sensors on many
  super-IO chips (a real in-case temperature is never exactly 0 °C).
- Sensors are listed most useful first: CPU, then GPU, then
  motherboard/drive temperatures, then case/chassis temperatures.
- Useless temperatures can be hidden from the GUI's sensor list (per-row
  hide button, or `hidden_sensors` in the config). Hiding is display-only:
  hidden sensors are still read every tick, and a fan bound to a hidden
  temperature keeps working exactly as before — the hidden rows come back
  under the list's "Show N hidden temperatures" line (it appears only
  while something is hidden) if a hide was a mistake.
- Fan headers are listed by controller name, then header number.
- `init-config` auto-matches each header to a likely sensor: `CPU_FAN`-
  style headers follow CPU temperatures, GPU/aux headers the graphics card
  (NVIDIA coolers follow their GPU's NVML temperature), chassis-named
  headers a case sensor where one exists.

### Unconnected and uncontrollable headers

`biggest-fan detect-fans` (or the GUI's _Detect fans_ button) spins every
managed header up briefly and reads tachometers: headers whose fan never
spins are assumed to have nothing plugged in, are removed from control and
recorded as `hidden = true` in the config. Hidden fans wait in the GUI's
Fans panel behind a "Show N hidden fans" line (it appears only while some
are hidden) — pressing _Show_ on a listed fan brings it back under
management with its name and settings — or set `hidden = false` by hand
in the config.
Headers without a tachometer cannot be judged and are left
alone. Parked headers get their duty written to zero — not left at the
detection spin-up speed — before their enable mode is handed back to the
hardware.

Some headers cannot be driven by software at all: pins wired as thermal
sensors, and drivers whose `store` method rejects writes. Writing their
`pwmN` files fails with "Permission denied" no matter who tries. Biggest
Fan recognises this (the register opens for writing, yet the driver still
rejects the write), shows a note once, then disables the fan mid-loop the
same way as an unconnected header (`hidden = true`; the GUI hides the row
straight away, and the daemon re-checks each run).

Manual-only controllers — such as the ARCTIC Fan Controller (mainline
hwmon driver, device name `arctic_fan`) — have no `pwmN_enable` file at
all: the hardware never changes speed on its own. Biggest Fan drives them
by writing `pwmN` duty values directly; it never tries to create the
enable file, since sysfs rejects new files and a create attempt would
surface as a misleading "Permission denied".

A plain permission error — the register cannot even be opened — is _not_
auto-disabled: that usually means you are not root. Run the daemon as
root (or install the service below and use the GUI as a normal user), and
the hint the daemon logs will say so.

## NVIDIA GPU coolers

GeForce cards usually expose no hwmon `pwm` files at all, so their fans
are driven through NVML instead (`crates/core/src/fans/nvidia.rs`). Each
GPU that reports fans appears as one logical cooler — id
`nvidia-gpuN-cooler`, named e.g. “NVIDIA GeForce RTX 4070 SUPER cooler” —
whose _all_ fans move together. Coolers are
managed exactly like sysfs fans: off / fixed / auto with curves,
hysteresis; `init-config` binds one to the GPU's NVML
temperature automatically. On the case view a cooler is drawn as a card —
a rectangle whose size the fan row's shape/width/height controls set
(`rect = [width, height]` in the config) — not a circle, since it spans
the whole graphics card.

Differences from sysfs headers:

- NVML commands fans in percent, and reads them back as both percent and
  real RPM (`nvmlDeviceGetFanSpeedRPM`, driver R525 or newer) — the same
  figure `nvidia-settings` shows, which gets it from the X `NV-CONTROL`
  extension rather than anything we would want to bind. So a GPU cooler
  shows rpm like any other fan, and a park shows as 0 rpm.
- Older drivers have no RPM call, and then the status falls back to the
  card's percent read-back — “no tach, header at 0 %” in a GUI row, `hdr 0%`
  in the daemon's line — which is still how a park becomes visible there.
- Commanded and reported speeds are compared, and a disagreement raises a fan
  note once five seconds have passed since the last speed change (the card
  parks its fans a couple of seconds after the policy is handed back, and a
  shorter window would report a park already on its way). The notes cover: a
  driver without an automatic policy that kept the fans running after a stop,
  a cooler reporting nothing while it is commanded to run (stalled or
  unplugged), and a card refusing a speed below its own floor. While the card
  holds the fans, whatever speed it picks is its own business and no note is
  raised.
- NVIDIA coolers have a spin floor and a park point rather than a speed
  range: they hold nothing reliably under 30 %, and NVML offers no stop
  setting. A request under 25 % hands the fans back to the card's own
  policy (`SPIN_DOWN_FRACTION` and `MIN_SPIN_FRACTION` in
  `fans/nvidia.rs`), where a modern card parks them at 0 rpm; a request of
  30 % or more takes them back under our control. A request in the 25–30 %
  band keeps whatever the cooler was last doing, so a curve or a ramp that
  crosses the band never stops and restarts the fans. On top of that sits a
  take-over rule: while the card itself has the fans reading above 30 %
  (its automatic policy warmed up) and our request asks under the floor, we
  take the cooler at 30 % instead of parking — a Quiet profile (which only
  reaching 30 % at 65 °C) can keep a hot card down at the floor rather than
  leaving it to the card's louder policy. A parked card reads 0 %, so a
  fan-off-when-cool profile still switches the fans fully off, and a cooler
  the card has parked is never disturbed. Having taken a cooler over, we _keep_
  it at the floor: for `PARK_DWELL` (90 s) a request under the floor is
  answered with 30 % rather than a hand-back, because the card's automatic
  policy spends several seconds ramping its fans far above what we wanted (it
  reached 42–48 % on a 4070 SUPER while a curve was asking for 13 %). Only a
  take-over arms that hold: a curve dipping under the floor while the card has
  its fans parked still parks at once, so a fan-off-when-cool profile goes
  quiet as soon as the machine cools. When the
  dwell runs out the park point is tried again, so a cooler that no longer
  needs air still gets handed back to the card's 0 dB mode — once every dwell
  instead of once every tick, which is what used to make the fan flutter and
  the reported percent alternate 0 %/30 %. Switching a fan _Off_ skips the
  dwell and parks immediately. Handing the policy
  back parks the fans a couple of seconds later — the card, not us, decides
  the moment (confirmed on a 4070 SUPER). Writing 0 percent is
  deliberately not the park path — the driver clamps it to its minimum (30 %
  on a 4070 SUPER), which is why setting _Off_ used to leave the fans
  spinning. The percent shown in the GUI and case view is the effective one
  (a 20 % curve point displays as 0 % with the fans parked, or as 30 % while a
  take-over hold is running), while a
  fixed slider keeps showing the speed you set.
- Fan _writes_ need root (the daemon has it; `scan`/`init-config` only
  read NVML, which works unprivileged). A refused write with the
  privilege meaning surfaces as the same permission hint as sysfs.
- `restore_control` hands the fans back to the driver's automatic control
  via NVML, so the shutdown hand-back covers GPU coolers too; parking an
  _Off_ GPU fan is the same call. Only drivers without that
  call get a 0 percent write instead, and those drivers may keep the fans
  at their minimum speed regardless.
- Detection does not judge a GPU cooler by its RPM (a driver without NVML's
  RPM call would look like a fan that never spun), so `detect-fans`
  never hides a cooler.
- While the GPU's NVML throttle report flags a thermal slowdown, fans
  following that GPU's temperature carry a “GPU is thermal throttling”
  note — the check is the same clocks-event-reasons bitmask `nvidia-smi`
  prints. AMD CPUs have no equivalent hwmon signal (their throttle status
  lives in MSRs), so no CPU throttle note exists.
- The GPU's _own cooler_ does more than note it. A Quiet curve runs
  Balanced and a Balanced curve runs Performance while the card throttles
  — runtime relief: the graph draws it over the saved one in orange with
  a "Keep this curve" button, and it clears when the card stops
  throttling. Any other curve (custom, or Performance) is nudged instead:
  the two curve points around the temperature the fan is running at go up
  +5 % (a warmer point that would be left quieter than its neighbour is
  pulled up too, colder points are never touched), saved into the config
  at once, and while the card keeps throttling, at most one further nudge
  every 20 seconds — a real wait to see whether the last step was enough
  — until the curve cannot rise at that temperature any more. Other fans
  tracking the GPU keep following their own curves untouched.

## systemd service

`packaging/systemd/biggest-fan.service` runs the daemon as root against
`/etc/biggest-fan/config.toml` (auto-created on first start). Installing
the .deb enables and (re)starts the service for you (the `postinst` script
in `packaging/deb-scripts/`); removing the package stops and disables it
(`prerm`). To manage it by hand:

```sh
sudo systemctl enable --now biggest-fan.service
`journalctl -u biggest-fan.service   # logs per-tick fan speeds
```

The unit runs `biggest-fan-daemon`; the GUI binary never runs as a
service.

Fans are handed back to hardware-automatic control when the daemon
stops: each motherboard header's `pwmN_enable` mode is recorded at
startup and restored, and GPU coolers go back to the NVIDIA driver
(SIGINT/SIGTERM — the .deb's `prerm` stops the daemon cleanly, so removal
hands the fans back too). Only stop the service when another controller
(BIOS/`fancontrol`) takes over.

## The GUI without root (D-Bus)

`biggest-fan gui` needs no root but **requires** the running service: it
joins the service on the system bus, mirrors the _service's_ config
(which is the live one), and forwards mode, sensor and curve preset, fan
name, tach correction, speed floor, detection, refresh-rate and
save changes to it over D-Bus. The window is laid out as four sections
under ruled headings — **Case** (the drawing and its legend) on the left,
**Fans**, **Sensors** and **Stress lab** down the right — each section's
content indented under its heading. Each fan row can rename the fan inline, pick
mode, sensor and a preset curve (Quiet, Balanced, Performance). Every Auto
fan's curve is drawn as a small graph — speed against sensor temperature —
with a dot at the speed and temperature the fan is actually at right now
(off-curve dots show a ramp or hold band at work). Every row uses the same
20–100 °C domain, so the curves compare straight down the list. The graph
is where the curve is edited: **hover a point** to print its exact
"45 % at 60 °C" beside it, **drag a point** to move it (it snaps to whole
degrees and whole percents and can never cross its neighbours),
**double-click** an empty spot to add a point, **right-click a point** to
delete one (two points is the floor — pick a preset for a simpler curve).
A drag is only committed to the fan and the service when you let go, so a
half-dragged curve never drives anything; the dropdown then reads
"Custom", and picking a preset replaces what you drew. While runtime GPU
throttle relief lifts a preset cooler above its saved curve, the graph
draws the lifted curve over the saved one in orange and offers a "Keep
this curve" button to make it the fan's real curve (saved-curve nudges
need no offer — they arrive as the fan's curve). Beside it
every fan also charts its last 5 minutes — speed % (blue) against the
same 0–100 scale as governing sensor °C (yellow) — so changes over time
stay visible after the numbers have scrolled past in the status line.
The **Sensors** readout has a hide button on every row: hidden
temperatures leave the list behind a "Show N hidden temperatures" line
(kept in `hidden_sensors` when the config is saved), but stay read — a
fan bound to a hidden temperature keeps working and charts normally.
The **Fans** panel treats hidden fans the same way: fans parked by
Detect (or a driver that refuses control) collapse behind a
"Show N hidden fans" line, and a parked fan's _Show_ button takes it
back under management with its name and settings. Both lines appear
only while something is hidden.
Each row
can also set the "rpm reading ×" tach correction and the "never below %"
speed floor under _extra settings_ (some tachometers report exactly double:
0.5 halves the reading; the floor keeps an AIO pump turning however quiet
its curve is). The window opens at
1600×900 maximised; fans you have not placed yet are laid out on a grid
inside the case drawing, and while every fan still sits on that grid the
canvas shows a "drag a shape to move it" hint near its bottom edge — it
vanishes for good once any fan gets a saved position. Saved positions
outside the case are
clamped back in at startup — always keeping a small margin from the case
walls, so no fan shape or its label touches the outline. Each fan row
sits in its own subtle tile; the tiles flow one per line normally, or two
side by side when the panel is wide enough (a 4K screen). If it cannot
join — no system bus, no service owning the bus name, or a service that
never answers — it prints which of those it is plus the fix (`sudo
systemctl enable --now biggest-fan.service`, or `sudo
biggest-fan-daemon`) and exits; it never falls back to touching the
headers itself (that fallback silently produced two processes fighting
over the same fan headers).

The bridge:

- Bus name `net.ynottony.ten.biggestfan`, object
  `/net/ynottony/ten/biggestfan`, interface
  `net.ynottony.ten.biggestfan.Control1`; payloads are JSON (fan modes,
  ids) and TOML text (whole configs), so `busctl` can drive it too. Every
  method answers `{"ok": true, ...}` or `{"ok": false, "error": "..."}`.
- The daemon's engine stays single-threaded: bus methods only queue
  requests, which the daemon applies between ticks (`dbus::drain_commands`
  in `crates/app/src/dbus.rs`).
- The .deb installs `/etc/dbus-1/system.d/net.ynottony.ten.biggestfan.conf`
  so root may own the name and any local user may call it. On machines
  with untrusted local users, tighten that file (the comments explain
  how) — it is the only thing letting ordinary users change fan
  behaviour.

## Suspend and resume

A wake-up leaves the hardware in a state the control loop's memory does
not match: hwmon directories get re-numbered when a controller re-binds
(every stored path is then stale), the board's `pwmN_enable` files are
usually back in hardware-automatic mode, and a graphics card has spent
the sleep running its own fan policy. So after a resume the daemon
re-reads the hardware and re-asserts every fan
(`Engine::resync_hardware`): headers are discovered afresh and re-bound
to their fan by id (ids are chip-name based, so renumbering cannot
break the config), each ramp restarts from the duty the firmware left —
one feather step from what the header actually reads rather than from
the pre-sleep value — NVIDIA coolers forget who drives them and take
over again on the next tick, and display names are refreshed.

Detection is logind's `PrepareForSleep` signal on the system bus
(`crates/app/src/power.rs`, which runs on its own `zbus::block_on`
thread like the D-Bus service does); the wake message is handled between
ticks, on the engine's only thread, exactly like a GUI request. The
journal gets one line per resume whatever `--quiet` says:

```
resume: re-bound 8 fan header(s) and re-asserted every fan
```

plus a line naming any configured header that is not back yet (its fan
keeps the old handle, so the config is not silently dropped and the
failed write keeps being reported until the driver returns) and any
header the machine now exposes that the config has never heard of — a
fan parked as hidden (nothing plugged in, or the driver refuses
software control) is deliberately undriven and never listed. A machine
without logind, or a system bus the daemon cannot reach, is reported
once at startup and simply never notices a resume: fan control carries
on.

## Stress lab

`crates/stress-lab` exists to answer "which fan actually cools what?" with
real heat instead of guesses — deliberately without stress-ng or CUDA.

The GUI carries a **Stress lab** panel at the bottom of the right column:
_toggle_ **CPU load** / **GPU load** to run the stressors as in-process
threads (they share the library with the CLI below), pick how many
seconds to run and the guard temperature, and watch the case view react.
Finished runs report their throughput in the status bar at the bottom of
the window (the same one the fan notes use; a fan's own note outranks
the lab's progress), the
guard aborts the load if k10temp or the GPU gets too hot, and closing
the window always stops the load. Run the GUI itself in release mode
(`cargo run --release -p biggest-fan`) — a debug build runs the
stressors ~100x too slowly to heat anything.

A fan-row change is live at once, and it only survives a restart when the
config is saved — an experiment is self-correcting that way: leave the
config alone and restarting the daemon puts the saved curves back in
charge.

The panel used to offer a full automatic curve search (soak the machine,
then walk each fan down until a temperature rose), and the daemon used to
trim fans towards quieter speeds by itself during normal use. Both are
gone: neither measured reliably enough to be trusted with the fans
unattended. The stressors stay — what they teach you belongs in the
curves you write.

For scripted experiments the CLI does the same things:

```sh
cargo build -p biggest-fan-stress-lab --release   # release: debug stressors are ~100x too slow to heat anything
./target/release/stress-lab cpu --secs 30            # AVX-512 FMA + integer churn across all cores
./target/release/stress-lab gpu --secs 30            # Vulkan compute on the real GPU (refuses llvmpipe)
./target/release/stress-lab monitor log.csv --secs 300 --every-secs 2 --phase-file /tmp/phase
```

The GPU stressor defaults to ~2M shader threads: on an RTX 4070 SUPER
that pins the card near its 220 W power limit (~13 TFLOP/s f32). Much
smaller thread counts still _show_ 100 % utilisation in `nvidia-smi`
while the card sits under 100 W, so watch watts, not utilisation, when
retuning `--threads`.

Each stressor runs a thermal guard (k10temp `Tctl` + NVML GPU temp) and
bails out with exit 3 if `--max-temp-c` is crossed, so experiments cannot
cook the machine. `monitor` appends one CSV row per tick for every hwmon
temp/fan/PWM plus NVML's GPU columns; `--phase-file` lets an outside
script stamp the current experiment phase into each row.

Fan phases are switched live through the daemon's D-Bus API (transient
modes only — the saved config is untouched):

```sh
busctl --system call net.ynottony.ten.biggestfan /net/ynottony/ten/biggestfan \
  net.ynottony.ten.biggestfan.Control1 SetFanMode ss "arctic_fan-p1" '{"type":"fixed","speed":1.0}'
```

Send each fan's original mode back afterwards (the saved config answers
"what were the originals?": `GetConfig` returns the config TOML). Full
one-off harnesses live in `target/lab/` (kept out of git, since cargo may
clear `target/` at any time): `experiment.sh` runs the CPU+GPU fan-phase matrix,
`experiment-gpu.sh` the GPU-only one, and `analyze.py` prints the
per-phase min/median/max table. They snapshot every visible fan's mode
first and restore it on exit, even on Ctrl+C.

First findings (CPU+GPU rig, run 1): with an AIO, k10temp Tctl plateaus
near 80 °C under AVX-512 load no matter what the case fans do — front
intakes off, exhaust off or everything at full speed all land within a
degree or two — so the cooler, not the airflow, is the CPU limit and the
default curve already matches reality up there.

## Packaging (deb)

```sh
cargo install cargo-deb
cargo deb -p biggest-fan            # release build, output in target/debian/
sudo dpkg -i target/debian/biggest-fan_*_amd64.deb
```

The .deb ships both binaries, the systemd unit, the D-Bus policy file,
the GUI launcher (`packaging/desktop/`) and app icons
(`packaging/icons/`: SVG + PNG sizes 16–512, regenerated from the SVG
with `sh tools/render-icons.sh`), and this README, with the GPL text in
its copyright file. Its maintainer scripts (`maintainer-scripts` in
`[package.metadata.deb]`, files under `packaging/deb-scripts/`) enable
and start the service and refresh the hicolor icon cache on install, and
stop/disable the service and refresh the cache again on removal. The
.desktop is named after the GTK app id so the desktop environment matches
it to the running window. Packaging metadata lives in
`[package.metadata.deb]` in `crates/app/Cargo.toml`.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
