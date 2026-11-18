# Biggest Fan

A smart fan controller app.

Read temperatures from lm-sensor and nvidia graphics cards and control fans speed.

Map sensors names to human readable names ("CPU temperature" instead of "k10temp Tcctl", "CPU CCD 1 temperature" instead of "k10temp Tccd1" etc.)
Map fans to a graphical representation of a modern PC case.
Different fans should be able to be controlled be different sensors.
Fans can be to be turned off, run at a constant speed or automatically respond to sensors.
Handles hysteresis to as not to ramp up and down the fans too quickly.
Has adjustable update time (default to 1 second refresh)

## Development

### Requirements

- Rust toolchain supporting edition 2024
- Linux (the core reads and writes hwmon files under `/sys/class/hwmon`)
- GTK 4 development packages, e.g. on Debian/Ubuntu:
  `sudo apt install libgtk-4-dev pkg-config`
- For NVIDIA GPU temperatures: the system NVIDIA driver provides
  `libnvidia-ml.so` (loaded at runtime via NVML). Pass `--no-nvidia` on
  machines without an NVIDIA GPU.

### Workspace layout

| Path                | Crate              | What it is                                                                                                                                                                                                      |
| ------------------- | ------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/core`       | `biggest-fan-core` | Library: sensor discovery (hwmon + NVML), human naming, fan/PWM control, config (TOML), case layout, and the control-loop `Engine` with hysteresis.                                                             |
| `crates/app`        | `biggest-fan`      | Library plus two binaries: `biggest-fan` (GTK 4 GUI — case view + fan controls — and the `scan`, `init-config`, `detect-fans` one-shot commands) and `biggest-fan-daemon` (headless control loop, for systemd). |
| `packaging/systemd` | —                  | The `biggest-fan.service` unit installed by the .deb.                                                                                                                                                           |

### Common commands

```sh
cargo build
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all                        # rustfmt, edition 2024
cargo doc -p biggest-fan-core --no-deps
sh tools/dbus-e2e.sh                   # live D-Bus smoke check: private bus, --dry-run
```

`tools/dbus-e2e.sh` proves the daemon↔GUI plumbing end to end: it starts a
throwaway private bus, checks the daemon answers `busctl` calls, that the
GUI exits with the service-required error when no service is running, that
it joins a running service and shows a window (GUI checks skipped without
a display), and that the daemon keeps running when the bus is broken.

## Usage

Global flags (both binaries): `--config PATH` (default
`~/.config/biggest-fan/config.toml`), `--dry-run` (compute speeds but never
write to the fan headers), `--no-nvidia`.

```sh
biggest-fan scan          # list discovered sensors and fan headers
biggest-fan init-config   # write a starter config from live hardware (--force to replace)
biggest-fan detect-fans   # spin headers up briefly, disable ones with nothing connected
biggest-fan gui           # case view + fan controls (also the default)
biggest-fan-daemon        # headless control loop, for systemd
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
back to the defaults:

```toml
poll_interval_ms = 1000   # control-loop refresh period (floors at 100 ms)
fail_safe_speed = 1.0     # speed for auto fans whose sensor disappears

[hysteresis]
temp_band_c = 2.0         # how far a sensor may fall before speed drops
max_step_per_tick = 0.1   # max speed change per tick (1.0 disables)

[sensor_names]            # human readable overrides, keyed by sensor id
"k10temp-t3" = "CPU CCD 1 temperature"

[[fans]]
id = "nct6795-p1"         # leader header id from `biggest-fan scan`
name = "Front 120mm intake"
members = ["nct6795-p4"]  # extra headers driven as one fan (an AIO's fans…)
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
# Balanced (the default) or Performance (ramps early); a hand-made curve
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
and speeds ramp by at most `max_step_per_tick` per tick (failsafes bypass
this). If an auto fan's sensor stops reporting, the fan goes to
`fail_safe_speed`.

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
- Fan headers are listed by controller name, then header number.
- `init-config` auto-matches each header to a likely sensor: `CPU_FAN`-
  style headers follow CPU temperatures, GPU/aux headers the graphics card
  (NVIDIA coolers follow their GPU's NVML temperature), chassis-named
  headers a case sensor where one exists.

### Unconnected and uncontrollable headers

`biggest-fan detect-fans` (or the GUI's _Detect fans_ button) spins every
managed header up briefly and reads tachometers: headers whose fan never
spins are assumed to have nothing plugged in, are removed from control and
recorded as `hidden = true` in the config. Set `hidden = false` by hand —
or pick the header in the GUI's _Add fan_ dropdown — to bring one back.
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
hysteresis and grouping; `init-config` binds one to the GPU's NVML
temperature automatically. On the case view a cooler is drawn as a card —
a rectangle whose size the fan row's shape/width/height controls set
(`rect = [width, height]` in the config) — not a circle, since it spans
the whole graphics card.

Differences from sysfs headers:

- NVML commands fans in percent and reports no tachometer: the status shows
  the commanded percent and `-` for rpm. It does read the card's fan speed
  back, so a cooler's real speed shows too — “no tach, header at 0 %” in a
  GUI row, `hdr 0%` in the daemon's line — which is how a park becomes
  visible.
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
  crosses the band never stops and restarts the fans. Handing the policy
  back parks the fans a couple of seconds later — the card, not us, decides
  the moment (confirmed on a 4070 SUPER). Writing 0 percent is
  deliberately not the park path — the driver clamps it to its minimum (30 %
  on a 4070 SUPER), which is why setting _Off_ used to leave the fans
  spinning. The percent shown in the GUI and case view is the effective one
  (a 20 % curve point displays as 0 % and the fans are parked), while a
  fixed slider keeps showing the speed you set.
- Fan _writes_ need root (the daemon has it; `scan`/`init-config` only
  read NVML, which works unprivileged). A refused write with the
  privilege meaning surfaces as the same permission hint as sysfs.
- `restore_control` hands the fans back to the driver's automatic control
  via NVML, so the shutdown hand-back covers GPU coolers too; parking an
  _Off_ GPU fan is the same call. Only drivers without that
  call get a 0 percent write instead, and those drivers may keep the fans
  at their minimum speed regardless.
- Detection cannot verify GPU fans (no tachometer), so `detect-fans`
  never hides a cooler.
- While the GPU's NVML throttle report flags a thermal slowdown, fans
  following that GPU's temperature carry a “GPU is thermal throttling”
  note — the check is the same clocks-event-reasons bitmask `nvidia-smi`
  prints. AMD CPUs have no equivalent hwmon signal (their throttle status
  lives in MSRs), so no CPU throttle note exists.

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
name, tach correction, speed floor, grouping, detection, refresh-rate and
save changes to it over D-Bus. Each fan row can rename the fan inline, pick
mode, sensor and a preset curve (Quiet, Balanced, Performance), and set the
"rpm reading ×" tach correction and the "never below %" speed floor under
_extra settings_ (some tachometers report exactly double: 0.5 halves the
reading; the floor keeps an AIO pump turning however quiet its curve is). The window opens at
1600×900 maximised; fans you have not placed yet are laid out on a grid
inside the case drawing, and saved positions outside the case are
clamped back in at startup. The header shows a "via the running service
(root)" badge and the window title says where changes go. If it cannot
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

## Packaging (deb)

```sh
cargo install cargo-deb
cargo deb -p biggest-fan            # release build, output in target/debian/
sudo dpkg -i target/debian/biggest-fan_*_amd64.deb
```

The .deb ships both binaries, the systemd unit, the D-Bus policy file and
this README, with the GPL text in its copyright file. Its maintainer
scripts (`maintainer-scripts` in `[package.metadata.deb]`, files under
`packaging/deb-scripts/`) enable and start the service on install and
stop and disable it on removal. Packaging metadata lives in
`[package.metadata.deb]` in `crates/app/Cargo.toml`.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
