//! GPU coolers driven through NVML, for NVIDIA cards whose fans are not
//! exposed as hwmon `pwmN` headers (most `GeForce` cards are like this).
//!
//! NVML controls each fan with a 0–100 percent speed setting, so one GPU's
//! fans are driven together as a single logical cooler. It also reports real
//! RPM (`nvmlDeviceGetFanSpeedRPM`, drivers from R525 on), which is the same
//! number `nvidia-settings` shows; drivers without that call report percent
//! only, and the RPM reads simply come back as nothing.

use crate::error::CoreError;
use crate::fans::model::{FanDescriptor, FanId};
use nvml_wrapper::Nvml;
use nvml_wrapper::error::NvmlError;
use std::io::{Error as IoError, ErrorKind};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Slowest speed an NVIDIA fan holds: 30 %.
///
/// On a `GeForce` RTX 4070 SUPER (and other recent cards) the driver
/// clamps every speed write to this minimum, and percentages between 1
/// and 29 do not spin the fans reliably, so a request inside the band
/// below has to pick a side: park the cooler or run it at the floor.
pub const MIN_SPIN_FRACTION: f64 = 0.30;

/// Speed fraction a spinning cooler must fall below before it is parked: 25 %.
///
/// Parking means handing the fans back to the card's own policy — NVML has
/// no "stop" setting, see [`NvidiaFan::stop`]. Parking below the spin point
/// ([`MIN_SPIN_FRACTION`]) leaves the cooler a band to cross, so a curve or
/// a ramp sitting at 25–30 % does not stop and restart the fans each tick.
pub const SPIN_DOWN_FRACTION: f64 = 0.25;

/// How long a cooler we took over from the card keeps the spin floor before
/// the park point is tried again: 90 s.
///
/// Without a hold the mapping alternates every tick in the zone where the
/// card's own policy wants more air than the floor while our curve asks less
/// than the park point: we hand the fans back, the card spends several
/// seconds ramping them up (42-48 % was measured on a 4070 SUPER while a
/// curve was asking for 13 %), the take-over rule takes the cooler back at
/// the floor, and the next tick hands it back again. Holding the floor for a
/// dwell keeps the cooler at the quietest speed it can run instead of
/// letting the card shout, and still asks the card whether it wants air
/// about every minute and a half rather than every second.
pub const PARK_DWELL: Duration = Duration::from_secs(90);

/// The take-over hold behind [`PARK_DWELL`]: while armed, a request below the
/// spin floor keeps the floor instead of handing the fans back.
///
/// Only a take-over arms it — the card was caught running its fans above the
/// floor, so it demonstrably wants air. A curve that simply dips below the
/// floor on its own still parks at once, which keeps a fan-off-when-cool
/// profile quiet as soon as the machine cools. The hold is *not* extended
/// while it runs, so the dwell eventually expires and the cooler can go back
/// to the card's 0 dB mode; a speed at or above the floor, or a park that was
/// honoured, ends it.
#[derive(Clone, Copy, Debug, Default)]
struct SpinPin {
    started: Option<Instant>,
}

impl SpinPin {
    /// Is the hold still in force at `now`?
    #[must_use]
    fn fresh(&self, now: Instant) -> bool {
        self.started
            .is_some_and(|started| now.saturating_duration_since(started) < PARK_DWELL)
    }

    /// Starts a hold; a hold already running keeps its own deadline.
    fn arm(&mut self, now: Instant) {
        self.started.get_or_insert(now);
    }

    /// Ends the hold: the fans went back to the card, or we are commanding a
    /// speed the hardware can run, so the next dip below the floor starts a
    /// fresh dwell.
    const fn release(&mut self) {
        self.started = None;
    }
}

/// Who decides a cooler's fan speed right now.
///
/// NVML reports no fan-control mode, so the only way to know is to remember
/// what this process last wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FanOwnership {
    /// Nothing written yet, so it is unknown whether the fans are under the
    /// card's policy or still in manual mode left behind by an earlier run.
    Unknown,
    /// The card's automatic policy holds the fans. Recent cards park them at
    /// 0 rpm until they warm up.
    Driver,
    /// This process commands the fans, at or above [`MIN_SPIN_FRACTION`].
    Manual,
    /// Parked through a 0 percent write, because this driver has no default
    /// policy call to hand the fans back to. Such a driver may keep the fans
    /// at its minimum speed regardless, which is what [`speed_mismatch_note`]
    /// looks for.
    ///
    /// [`speed_mismatch_note`]: NvidiaFan::speed_note
    ForcedStop,
}

/// How long a change of fan control is given to take effect before the
/// read-back is believed over what we wrote.
///
/// The card parks its fans a couple of seconds after the policy is handed
/// back, so a shorter window would report a park that is already on its way.
const READBACK_GRACE: Duration = Duration::from_secs(5);

/// Smallest speed change worth restarting the read-back grace window. NVML
/// takes whole percents, so anything finer than this cannot be written.
const SPEED_GRANULARITY: f64 = 0.005;

/// Maps a requested speed fraction onto what NVIDIA fans can run.
///
/// `ownership` is the cooler's state *before* this request, which is what
/// keeps the 25–30 % band stable instead of flapping (`ForcedStop` counts as
/// parked, since nothing is being commanded):
///
/// ```text
///                 parked (Driver)      spinning (Manual)
///  request < 25 %      0 %                   0 %
///  25–30 %             0 %                  30 %      <- hold whatever it was doing
///  request >= 30 %   >= 30 %               >= 30 %
/// ```
///
/// `card_above_floor` is true when the card reads its fans above the floor
/// ([`NvidiaFan::read_percent`] > [`MIN_SPIN_FRACTION`]). It adds one
/// take-over on top of the table: a request under 30 % while the card
/// itself has the fans above 30 % commands the floor instead of parking.
/// A Quiet profile, which only crosses the floor at 65 °C, can then quiet
/// a card-driven 40 % down to 30 % rather than leaving the cooler to the
/// card. Under `Manual` the reading is this process's own last command,
/// which is why the rule is gated on the owner; `Unknown` is handed to the
/// card first (its low request parks it) and the rule picks the cooler back
/// up on the next tick. A parked card reads 0 %, so the rule stays off and
/// a fan-off-when-cool profile still switches the fans fully off.
///
/// `pin_fresh` is the hold that follows a take-over ([`PARK_DWELL`]): once we have taken a cooler the card was running, a
/// request under the floor keeps the floor instead of handing the fans back,
/// so the cooler is not parked a second after being taken. The hold outlasts
/// [`PARK_DWELL`] and then the table above applies again, which is how a
/// cooler that no longer needs air reaches the card's 0 dB mode. A mode of
/// _Off_ never reaches this mapping: it parks through [`NvidiaFan::stop`],
/// so switching a cooler off is immediate.
///
/// Examples:
///
/// ```
/// use biggest_fan_core::fans::nvidia::{FanOwnership, effective_speed};
///
/// // Off always parks a cooler the card is not running above the floor.
/// assert_eq!(effective_speed(0.0, FanOwnership::Manual, false, false), 0.0);
/// // Mid-band requests keep the cooler doing what it already was.
/// assert_eq!(effective_speed(0.27, FanOwnership::Manual, false, false), 0.30);
/// assert_eq!(effective_speed(0.27, FanOwnership::Driver, false, false), 0.0);
/// // A stopped cooler only restarts at the floor it can hold.
/// assert_eq!(effective_speed(0.30, FanOwnership::Driver, false, false), 0.30);
/// // The card has its fans at 40 %; a Quiet-style 20 % request takes them
/// // at the floor rather than handing control back.
/// assert_eq!(effective_speed(0.20, FanOwnership::Driver, true, false), 0.30);
/// // And having taken them, the floor holds until the dwell runs out.
/// assert_eq!(effective_speed(0.20, FanOwnership::Manual, false, true), 0.30);
/// ```
#[must_use]
pub fn effective_speed(
    requested: f64,
    ownership: FanOwnership,
    card_above_floor: bool,
    pin_fresh: bool,
) -> f64 {
    // A request that is not a number must never park a cooler: run it full,
    // the same fail-safe direction as `FanCurve::speed_at`.
    if !requested.is_finite() {
        return 1.0;
    }
    if requested >= MIN_SPIN_FRACTION {
        return requested;
    }
    // The take-over rule: the card has the fans above the floor while our
    // request cannot reach that speed, so command the floor rather than
    // leave the cooler (or park it) under a hotter automatic policy. The
    // hold that follows it keeps that floor instead of handing the fans
    // straight back.
    if pin_fresh
        || (card_above_floor
            && matches!(ownership, FanOwnership::Driver | FanOwnership::ForcedStop))
    {
        return MIN_SPIN_FRACTION;
    }
    if requested < SPIN_DOWN_FRACTION {
        0.0
    } else if ownership == FanOwnership::Manual {
        MIN_SPIN_FRACTION
    } else {
        0.0
    }
}

/// A warning when the card's own fan-speed reading contradicts what we
/// command — the check that catches a stop the driver refused.
///
/// * `commanded_effective` is the speed fraction actually written for the
///   cooler (what [`effective_speed`] returned), and `measured_percent` is
///   what the card reports back, as a 0.0–1.0 fraction.
/// * `settled` is false while the last change of control has not had the
///   read-back grace window (five seconds) to take effect, which suppresses
///   the note while the card is still carrying out a park.
///
/// While the card holds the fans (`Driver`) no note is raised: it is free to
/// spin the fans up because it warmed up, and that is not a fault.
///
/// Examples:
///
/// ```
/// use biggest_fan_core::fans::nvidia::{FanOwnership, speed_mismatch_note};
///
/// // A driver without an automatic policy kept the fans running at its floor.
/// let note = speed_mismatch_note(0.0, Some(0.30), FanOwnership::ForcedStop, true)
///     .expect("worth telling the user");
/// assert!(note.contains("30%"));
/// // A cooler we command at 40 % that reports nothing is stalled or unplugged.
/// assert!(
///     speed_mismatch_note(0.4, Some(0.0), FanOwnership::Manual, true)
///         .is_some_and(|note| note.contains("stalled"))
/// );
/// // While the card itself holds the fans, any speed it picks is fine.
/// assert!(speed_mismatch_note(0.0, Some(0.60), FanOwnership::Driver, true).is_none());
/// // A park that has not had time to happen yet is not a contradiction.
/// assert!(speed_mismatch_note(0.0, Some(0.30), FanOwnership::Driver, false).is_none());
/// ```
#[must_use]
pub fn speed_mismatch_note(
    commanded_effective: f64,
    measured_percent: Option<f64>,
    ownership: FanOwnership,
    settled: bool,
) -> Option<String> {
    let measured = measured_percent?;
    if !settled {
        return None;
    }
    match ownership {
        FanOwnership::Manual if measured <= 0.0 => Some(format!(
            "this cooler reports nothing running even though it is commanded to {:.0}% — it may be stalled or unplugged",
            commanded_effective * 100.0
        )),
        FanOwnership::Manual if measured < MIN_SPIN_FRACTION => Some(format!(
            "this cooler reads back at {:.0}% though {:.0}% was commanded — the card may not honour speeds near its {:.0}% floor",
            measured * 100.0,
            commanded_effective * 100.0,
            MIN_SPIN_FRACTION * 100.0
        )),
        FanOwnership::ForcedStop if measured >= MIN_SPIN_FRACTION => Some(format!(
            "this driver has no automatic fan policy, so 0 % does not stop this cooler: it is still at {:.0}% — give it a speed floor instead of turning it off",
            measured * 100.0
        )),
        _ => None,
    }
}

/// Whether an NVML error means "this driver has no default fan policy call"
/// rather than a refusal worth reporting.
const fn policy_unsupported(error: &NvmlError) -> bool {
    matches!(
        error,
        NvmlError::NotSupported | NvmlError::FunctionNotFound | NvmlError::FailedToLoadSymbol(_)
    )
}

/// The fans of one NVIDIA GPU, driven as a single logical cooler.
///
/// Example (a 4070 SUPER reports two fans but behaves as one unit):
///
/// ```no_run
/// use biggest_fan_core::fans::NvidiaFan;
///
/// for mut cooler in NvidiaFan::discover_all().expect("NVML enumeration") {
///     // Half speed until the engine hands control back on shutdown.
///     cooler.apply(0.5).expect("needs root to write fan speeds");
/// }
/// ```
#[derive(Clone)]
pub struct NvidiaFan {
    nvml: Arc<Nvml>,
    /// NVML index of the GPU this cooler belongs to.
    gpu_index: u32,
    /// How many fans NVML reports for this GPU (a 4070 SUPER has two).
    fan_count: u32,
    /// Whether NVML's RPM call answers for this card. It needs a driver from
    /// R525 on and is refused by some cards, so it is probed once here rather
    /// than guessed from the model.
    reports_rpm: bool,
    id: FanId,
    default_name: String,
    /// Who holds the fans: `Unknown` until the first write, because NVML
    /// exposes no fan-control mode to read back.
    ownership: FanOwnership,
    /// Speed fraction the last write actually commanded (0.0 when parked).
    last_effective: f64,
    /// When the commanded speed last changed materially; the read-back is
    /// only trusted after [`READBACK_GRACE`] has passed since then.
    last_speed_change: Instant,
    /// The take-over hold: while armed, a request under the spin floor keeps
    /// the floor instead of handing the fans back to the card (see
    /// [`PARK_DWELL`]).
    pin: SpinPin,
}

impl NvidiaFan {
    /// Discovers one cooler per GPU that reports controllable fans.
    ///
    /// Returns an empty list when NVML is not available (no NVIDIA driver
    /// on this machine), which is normal and not an error.
    ///
    /// # Errors
    ///
    /// [`CoreError::Nvidia`] when NVML starts but fails to enumerate
    /// devices.
    pub fn discover_all() -> Result<Vec<Self>, CoreError> {
        // Failing to even load NVML just means there is nothing NVIDIA
        // here; the same reasoning as `NvidiaSource::try_new`.
        let Ok(nvml) = Nvml::init() else {
            return Ok(Vec::new());
        };
        let nvml = Arc::new(nvml);
        let mut coolers = Vec::new();
        for gpu_index in 0..nvml.device_count()? {
            let Ok(device) = nvml.device_by_index(gpu_index) else {
                continue;
            };
            // NotSupported / missing symbols mean integrated or fanless
            // GPUs; those have no cooler for us to drive.
            let Ok(fan_count) = device.num_fans() else {
                continue;
            };
            if fan_count == 0 {
                continue;
            }
            let model = device.name().unwrap_or_else(|_| format!("GPU {gpu_index}"));
            // Ask once whether the RPM call works here, so what discovery
            // reports matches what this machine can actually read.
            let reports_rpm = device.fan_speed_rpm(0).is_ok();
            coolers.push(Self {
                nvml: Arc::clone(&nvml),
                gpu_index,
                fan_count,
                reports_rpm,
                id: FanId::new(format!("nvidia-gpu{gpu_index}-cooler")),
                default_name: format!("{model} cooler"),
                ownership: FanOwnership::Unknown,
                last_effective: 0.0,
                // Nothing has been written yet, so no reading can contradict
                // us; the clock starts here rather than at the first write.
                last_speed_change: Instant::now(),
                pin: SpinPin::default(),
            });
        }
        Ok(coolers)
    }

    /// Stable id of this cooler, e.g. `nvidia-gpu0-cooler`.
    #[must_use]
    pub const fn id(&self) -> &FanId {
        &self.id
    }

    /// Hardware-derived name, e.g. `NVIDIA GeForce RTX 4070 SUPER cooler`.
    #[must_use]
    pub fn default_name(&self) -> &str {
        &self.default_name
    }

    /// Discovery descriptor. `rpm_supported` is the probed answer for this
    /// card, so an older driver (no RPM call) says `false`.
    #[must_use]
    pub fn descriptor(&self) -> FanDescriptor {
        FanDescriptor {
            id: self.id.clone(),
            default_name: self.default_name.clone(),
            rpm_supported: self.reports_rpm,
        }
    }

    /// Sets every fan of this GPU to `speed_fraction` as a percent.
    ///
    /// Requests that resolve to a stop (see [`effective_speed`]) park the
    /// cooler through [`NvidiaFan::stop`] rather than writing a percent the
    /// card cannot hold. The take-over rule can turn a low request into a
    /// 30 % command when the card itself has the fans above the floor, and
    /// the hold that follows keeps the floor until [`PARK_DWELL`] has run
    /// out, so a cooler the card wants running is not handed back every tick.
    /// A curve dipping under the floor on its own (no card-driven reading
    /// above it) still parks straight away.
    ///
    /// # Errors
    ///
    /// [`CoreError::Nvidia`] when NVML refuses: `NoPermission` means this
    /// process lacks the privileges (fan writes need root), other errors
    /// mean the card disallows software fan control.
    pub fn apply(&mut self, speed_fraction: f64) -> Result<(), CoreError> {
        let now = Instant::now();
        let held = self.pin.fresh(now);
        let above_floor = self.card_above_floor();
        // This tick's request takes the cooler away from a card that is
        // running its fans above the floor: the one event that arms the hold.
        let taken = above_floor
            && matches!(
                self.ownership,
                FanOwnership::Driver | FanOwnership::ForcedStop
            );
        let speed = effective_speed(speed_fraction, self.ownership, above_floor, held);
        if speed <= 0.0 {
            // The park point applies after all: the fans go back to the card
            // and the hold starts over the next time we take them.
            self.pin.release();
            return self.stop();
        }
        if speed_fraction < MIN_SPIN_FRACTION {
            // A request the hardware cannot run: only a take-over arms the
            // hold, so the floor survives the next tick instead of handing
            // the fans straight back.
            if taken {
                self.pin.arm(now);
            }
        } else {
            // Normal control at a speed the hardware can run (a non-numeric
            // request runs the fan full): any hold is over, and a later dip
            // below the floor only gets a dwell of its own if the card is
            // found driving the fans again.
            self.pin.release();
        }
        let percent = speed_percent(speed);
        let mut device = self.nvml.device_by_index(self.gpu_index)?;
        for fan_index in 0..self.fan_count {
            device
                .set_fan_speed(fan_index, percent)
                .map_err(|error| self.permission_as_io(error))?;
        }
        self.record_commanded_speed(speed, FanOwnership::Manual);
        Ok(())
    }

    /// Parks the cooler at 0 rpm by handing the fans back to the card's own
    /// policy.
    ///
    /// Writing 0 percent is *not* a stop: the driver clamps it to its minimum
    /// speed (30 % on a 4070 SUPER), which is what left the fans spinning.
    /// Under its own policy a modern card parks the fans until they warm up,
    /// a couple of seconds after the hand-back — the card times the moment,
    /// which is why a percent read straight after parking can still show the
    /// old speed. Deferring to the card is also the safe answer whenever the
    /// user asks for a speed the hardware cannot hold.
    ///
    /// An already-parked cooler writes nothing, so the control loop never
    /// resets the driver's policy every tick. `apply` picks the fans back up
    /// at 30 % or more on the next request that wants them.
    ///
    /// Parking here also ends the take-over hold, which is why a mode of _Off_
    /// stops the cooler at once even while [`PARK_DWELL`] would otherwise be
    /// holding the floor.
    ///
    /// # Errors
    ///
    /// As for [`NvidiaFan::apply`].
    pub fn stop(&mut self) -> Result<(), CoreError> {
        self.pin.release();
        if matches!(
            self.ownership,
            FanOwnership::Driver | FanOwnership::ForcedStop
        ) {
            return Ok(());
        }
        if let Err(error) = self.write_driver_policy() {
            if !policy_unsupported(&error) {
                return Err(self.permission_as_io(error));
            }
            // A driver with no "back to automatic" call offers 0 percent as
            // its only stop, and may keep its minimum speed regardless; the
            // read-back note tells the user when it does.
            self.write_zero_percent()?;
            self.record_commanded_speed(0.0, FanOwnership::ForcedStop);
        } else {
            self.record_commanded_speed(0.0, FanOwnership::Driver);
        }
        Ok(())
    }

    /// Hands the fans back to the driver's automatic control, the NVML
    /// equivalent of restoring a `pwmN_enable` mode. Drivers without the
    /// "back to automatic" call report it as unsupported and are treated
    /// as already handed back.
    ///
    /// # Errors
    ///
    /// [`CoreError::Nvidia`] when NVML refuses for any other reason.
    pub fn restore_control(&mut self) -> Result<(), CoreError> {
        self.pin.release();
        if let Err(error) = self.write_driver_policy()
            && !policy_unsupported(&error)
        {
            return Err(error.into());
        }
        self.record_commanded_speed(0.0, FanOwnership::Driver);
        Ok(())
    }

    /// Forgets everything this cooler was doing, the state a suspend leaves
    /// behind: the card has had its fans back under its own policy while the
    /// machine slept, so who holds them is unknown again, and a take-over
    /// hold from before the sleep says nothing about what the card wants now.
    ///
    /// The next tick then hands the fans to the card once (clearing any mode
    /// an earlier run left) and takes them back if the card has them running
    /// above the spin floor.
    pub fn reset_state(&mut self) {
        self.ownership = FanOwnership::Unknown;
        self.pin.release();
        self.last_effective = 0.0;
        self.last_speed_change = Instant::now();
    }

    /// The cooler's current fan speed as NVML reports it (0.0–1.0), averaged
    /// over its fans, or `None` when the card reports nothing. This is what
    /// makes a park visible: a `GeForce` cooler under the card's own policy
    /// reads back at 0 once the fans have actually stopped.
    #[must_use]
    pub fn read_percent(&self) -> Option<f64> {
        let device = self.nvml.device_by_index(self.gpu_index).ok()?;
        let mut total = 0_u32;
        let mut readable = 0_u32;
        for fan_index in 0..self.fan_count {
            if let Ok(percent) = device.fan_speed(fan_index) {
                total = total.saturating_add(percent);
                readable += 1;
            }
        }
        if readable == 0 {
            return None;
        }
        Some(f64::from(total) / f64::from(readable) / 100.0)
    }

    /// A warning when this cooler's read-back contradicts `commanded` — the
    /// speed fraction the engine wrote for it — see [`speed_mismatch_note`].
    ///
    /// [`speed_mismatch_note`]: speed_mismatch_note
    #[must_use]
    pub fn speed_note(&self, commanded: f64) -> Option<String> {
        let settled = self.last_speed_change.elapsed() >= READBACK_GRACE;
        speed_mismatch_note(commanded, self.read_percent(), self.ownership, settled)
    }

    /// The speed fraction this cooler can actually run for `requested`,
    /// given who holds the fans now, what the card reads and whether a
    /// take-over hold is running. The same mapping [`NvidiaFan::apply`] writes,
    /// without changing anything — the engine reports it so the UI never
    /// claims a cooler is crawling at 20 % or parked while the take-over rule
    /// has it at the floor.
    #[must_use]
    pub fn effective_speed(&self, requested: f64) -> f64 {
        effective_speed(
            requested,
            self.ownership,
            self.card_above_floor(),
            self.pin.fresh(Instant::now()),
        )
    }

    /// True when the card reads its fans above the spin floor, the hardware
    /// fact the take-over rule in [`effective_speed`] acts on. Under
    /// `Manual` that reading is this process's own last command, which is
    /// why the mapping gates the rule on the owner as well.
    #[must_use]
    fn card_above_floor(&self) -> bool {
        self.read_percent()
            .is_some_and(|percent| percent > MIN_SPIN_FRACTION)
    }

    /// Remembers the speed and owner, restarting the read-back grace window
    /// whenever the commanded speed changes materially (a park, a spin-up or
    /// a ramp step) rather than on every identical re-write.
    fn record_commanded_speed(&mut self, effective: f64, next: FanOwnership) {
        let changed =
            next != self.ownership || (effective - self.last_effective).abs() > SPEED_GRANULARITY;
        if changed {
            self.last_speed_change = Instant::now();
        }
        self.ownership = next;
        self.last_effective = effective;
    }

    /// Asks the driver for its default fan policy on every fan, returning
    /// the first error so callers can tell "no such call" from a refusal.
    fn write_driver_policy(&self) -> Result<(), NvmlError> {
        let mut device = self.nvml.device_by_index(self.gpu_index)?;
        for fan_index in 0..self.fan_count {
            device.set_default_fan_speed(fan_index)?;
        }
        Ok(())
    }

    /// Writes 0 percent to every fan: the only stop offered by drivers with
    /// no default-policy call, and clamped to the driver's minimum by newer
    /// ones, so it stays a fallback rather than the primary path.
    fn write_zero_percent(&self) -> Result<(), CoreError> {
        let mut device = self.nvml.device_by_index(self.gpu_index)?;
        for fan_index in 0..self.fan_count {
            device
                .set_fan_speed(fan_index, 0)
                .map_err(|error| self.permission_as_io(error))?;
        }
        Ok(())
    }

    /// Measured RPM, averaged over the fans that report it, or `None` when
    /// none do. NVML's RPM call needs a driver from R525 on and answers
    /// `NotSupported` for cards it will not report RPM for; both come out as
    /// `None`, leaving [`NvidiaFan::read_percent`] as the only reading.
    ///
    /// This is the same figure `nvidia-settings` prints for a GPU fan — that
    /// program reads the same hardware through the X `NV-CONTROL` extension,
    /// which has no Rust bindings worth taking on for one number.
    #[must_use]
    pub fn read_rpm(&self) -> Option<f64> {
        if !self.reports_rpm {
            return None;
        }
        let device = self.nvml.device_by_index(self.gpu_index).ok()?;
        let mut total = 0_u32;
        let mut readable = 0_u32;
        for fan_index in 0..self.fan_count {
            if let Ok(rpm) = device.fan_speed_rpm(fan_index) {
                total = total.saturating_add(rpm);
                readable += 1;
            }
        }
        average_rpm(total, readable)
    }

    /// Controller-style name used for ordering fans, e.g. `nvidia-gpu0`.
    #[must_use]
    pub fn chip(&self) -> String {
        format!("nvidia-gpu{}", self.gpu_index)
    }

    /// NVML's `NoPermission` becomes a plain permission [`CoreError::Io`],
    /// so callers treat "needs root" the same way they treat a sysfs
    /// permission error, while every other NVML failure stays a
    /// [`CoreError::Nvidia`] semantic refusal.
    fn permission_as_io(&self, error: NvmlError) -> CoreError {
        match error {
            NvmlError::NoPermission => CoreError::Io {
                path: PathBuf::from(format!("NVML GPU {}", self.gpu_index)),
                source: IoError::new(
                    ErrorKind::PermissionDenied,
                    "NVML refused the write; fan control needs root",
                ),
            },
            other => other.into(),
        }
    }
}

impl std::fmt::Debug for NvidiaFan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NvidiaFan")
            .field("gpu_index", &self.gpu_index)
            .field("fan_count", &self.fan_count)
            .field("id", &self.id)
            .field("default_name", &self.default_name)
            .finish_non_exhaustive()
    }
}

/// The mean of `readable` fan readings that add up to `total`, or `None`
/// when nothing was readable. Averaging (rather than the maximum) matches
/// [`NvidiaFan::read_percent`], so a cooler with one dead tachometer still
/// reports a believable speed. A parked cooler reads 0, not `None`.
fn average_rpm(total: u32, readable: u32) -> Option<f64> {
    (readable > 0).then(|| f64::from(total) / f64::from(readable))
}

/// Maps a speed fraction (0.0..=1.0) onto NVML's 0–100 percent scale.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the clamped result is 0..=100"
)]
#[expect(
    clippy::cast_sign_loss,
    reason = "a clamped fraction is never negative"
)]
fn speed_percent(speed_fraction: f64) -> u32 {
    (speed_fraction.clamp(0.0, 1.0) * 100.0).round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVERY_STATE: [FanOwnership; 3] = [
        FanOwnership::Unknown,
        FanOwnership::Driver,
        FanOwnership::Manual,
    ];

    #[test]
    fn mid_band_requests_hold_whatever_the_cooler_was_doing() {
        // Spinning at the floor: a request inside the band keeps it there.
        assert_eq!(
            effective_speed(0.25, FanOwnership::Manual, false, false),
            0.30
        );
        assert_eq!(
            effective_speed(0.29, FanOwnership::Manual, false, false),
            0.30
        );
        // Parked: the same request is not worth starting the fans for.
        assert_eq!(
            effective_speed(0.27, FanOwnership::Driver, false, false),
            0.0
        );
        // A cooler whose state is unknown is treated as parked.
        assert_eq!(
            effective_speed(0.27, FanOwnership::Unknown, false, false),
            0.0
        );
    }

    #[test]
    fn parking_needs_a_clear_request_below_the_spin_down_point() {
        for state in EVERY_STATE {
            assert_eq!(
                effective_speed(0.0, state, false, false),
                0.0,
                "off mode parks"
            );
            assert_eq!(
                effective_speed(0.24, state, false, false),
                0.0,
                "below the band parks"
            );
            assert_eq!(
                effective_speed(-0.5, state, false, false),
                0.0,
                "clamped by the engine"
            );
        }
    }

    #[test]
    fn spinning_up_starts_at_the_reliable_floor() {
        assert_eq!(
            effective_speed(0.30, FanOwnership::Driver, false, false),
            0.30
        );
        assert_eq!(
            effective_speed(0.50, FanOwnership::Unknown, false, false),
            0.50
        );
        assert_eq!(
            effective_speed(1.0, FanOwnership::Driver, false, false),
            1.0
        );
    }

    #[test]
    fn the_band_sits_between_the_park_and_spin_points() {
        // The park point must stay below the spin point, otherwise the
        // mapping above has no band to hold and the fans flap on a ramp.
        let middle = f64::midpoint(SPIN_DOWN_FRACTION, MIN_SPIN_FRACTION);
        assert_eq!(
            effective_speed(middle, FanOwnership::Manual, false, false),
            MIN_SPIN_FRACTION
        );
        assert_eq!(
            effective_speed(middle, FanOwnership::Driver, false, false),
            0.0
        );
    }

    #[test]
    fn mapping_the_mapped_speed_again_changes_nothing() {
        // The engine reports a cooler's speed by re-running the mapping on
        // what `apply` just wrote, so that has to be a fixed point.
        for state in EVERY_STATE {
            for requested in [0.0, 0.1, 0.25, 0.27, 0.29, 0.3, 0.42, 1.0] {
                let effective = effective_speed(requested, state, false, false);
                let state_after = if effective > 0.0 {
                    FanOwnership::Manual
                } else {
                    FanOwnership::Driver
                };
                assert_eq!(
                    effective_speed(effective, state_after, false, false),
                    effective,
                    "requested {requested} from {state:?}"
                );
            }
        }
    }

    #[test]
    fn a_broken_speed_request_runs_the_fan_full() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            for above_floor in [false, true] {
                assert_eq!(
                    effective_speed(value, FanOwnership::Driver, above_floor, false),
                    1.0,
                    "{value} must never park a cooler"
                );
            }
        }
    }

    #[test]
    fn drivers_without_a_default_fan_policy_call_are_recognised() {
        assert!(policy_unsupported(&NvmlError::NotSupported));
        assert!(policy_unsupported(&NvmlError::FunctionNotFound));
        assert!(!policy_unsupported(&NvmlError::NoPermission));
        assert!(!policy_unsupported(&NvmlError::InvalidArg));
    }

    #[test]
    fn a_forced_stop_counts_as_parked_for_the_next_request() {
        // A driver with no automatic policy still gets the band rules: the
        // cooler stays parked until a request clears the spin floor. A 0 %
        // write the driver clamped to exactly its floor reads back at 30 %,
        // which is not *above* the floor, so no take-over either.
        assert_eq!(
            effective_speed(0.27, FanOwnership::ForcedStop, false, false),
            0.0
        );
        assert_eq!(
            effective_speed(0.30, FanOwnership::ForcedStop, false, false),
            0.30
        );
    }

    #[test]
    fn a_cooler_the_card_holds_above_the_floor_is_taken_at_the_floor() {
        // The take-over rule: a Quiet-style request the card cannot match.
        // Taking control at the floor is quieter than leaving the fans to
        // the card's louder policy and safer than parking them under a hot
        // card.
        for state in [FanOwnership::Driver, FanOwnership::ForcedStop] {
            for requested in [0.0, 0.1, 0.24, 0.27, 0.29] {
                assert_eq!(
                    effective_speed(requested, state, true, false),
                    MIN_SPIN_FRACTION,
                    "{requested} from {state:?}"
                );
            }
        }
        // A request that already clears the floor is untouched by the rule.
        assert_eq!(
            effective_speed(0.35, FanOwnership::Driver, true, false),
            0.35
        );
        // A cooler nobody has taken control of yet defers to the card: its
        // first low request parks it (into `Driver`), then the rule picks
        // it back up on the next tick.
        assert_eq!(
            effective_speed(0.20, FanOwnership::Unknown, true, false),
            0.0
        );
        // Under `Manual` the reading is this process's own last command,
        // so with no hold running a drop below the spin-down point parks the
        // cooler.
        assert_eq!(
            effective_speed(0.20, FanOwnership::Manual, true, false),
            0.0
        );
        // The band case is unchanged: it was already holding 30 %.
        assert_eq!(
            effective_speed(0.27, FanOwnership::Manual, true, false),
            0.30
        );
    }

    #[test]
    fn a_cooler_under_the_take_over_hold_keeps_the_floor() {
        // The tick after a take-over: the reading is now our own 30 %
        // command, so `card_above_floor` is off and only the hold keeps the
        // cooler running. Without it this is the case that used to hand the
        // fans back, letting the card ramp them up again.
        for state in [
            FanOwnership::Driver,
            FanOwnership::ForcedStop,
            FanOwnership::Manual,
        ] {
            for requested in [0.0, 0.1, 0.24, 0.27, 0.29] {
                assert_eq!(
                    effective_speed(requested, state, false, true),
                    MIN_SPIN_FRACTION,
                    "{requested} from {state:?}"
                );
            }
        }
        // A request that clears the floor is the mapping's normal case.
        assert_eq!(effective_speed(0.4, FanOwnership::Manual, false, true), 0.4);
        // A broken request still runs the fan full rather than parking.
        assert_eq!(
            effective_speed(f64::NAN, FanOwnership::Manual, false, true),
            1.0
        );
    }

    #[test]
    fn the_take_over_hold_lasts_one_dwell_and_is_not_extended() {
        let start = Instant::now();
        let mut pin = SpinPin::default();
        assert!(!pin.fresh(start), "nothing has been taken over yet");

        pin.arm(start);
        assert!(pin.fresh(start), "the hold starts at once");
        let middle = start.checked_add(PARK_DWELL / 2).expect("half a dwell");
        assert!(pin.fresh(middle), "the hold runs through the dwell");

        // Re-arming while a hold runs must not push its deadline back, or a
        // pinned cooler would never be handed back to the card's 0 dB mode.
        pin.arm(middle);
        let due = start.checked_add(PARK_DWELL).expect("a whole dwell");
        assert!(
            !pin.fresh(due),
            "the park point applies again after the dwell"
        );

        pin.release();
        assert!(!pin.fresh(start), "a park or a real speed ends the hold");
    }

    #[test]
    fn read_back_notes_name_the_contradiction() {
        // A driver without an automatic policy refused the stop.
        let note = speed_mismatch_note(0.0, Some(0.30), FanOwnership::ForcedStop, true)
            .expect("worth telling the user");
        assert!(
            note.contains("30%") && note.contains("speed floor"),
            "{note}"
        );

        // Commanded to run, reporting nothing: stalled or unplugged.
        let note = speed_mismatch_note(0.4, Some(0.0), FanOwnership::Manual, true)
            .expect("worth telling the user");
        assert!(note.contains("stalled"), "{note}");

        // Commanded above the card's floor, reading back below it.
        let note = speed_mismatch_note(0.4, Some(0.20), FanOwnership::Manual, true)
            .expect("worth telling the user");
        assert!(note.contains("40%"), "{note}");
    }

    #[test]
    fn read_back_stays_quiet_when_there_is_nothing_to_report() {
        // The card holds the fans: whatever speed it chooses is its business.
        assert_eq!(
            speed_mismatch_note(0.0, Some(0.80), FanOwnership::Driver, true),
            None
        );
        // Nothing written yet (a read-only GUI mirror), so nothing contradicts.
        assert_eq!(
            speed_mismatch_note(0.5, Some(0.0), FanOwnership::Unknown, true),
            None
        );
        // No reading to compare against.
        assert_eq!(
            speed_mismatch_note(0.4, None, FanOwnership::Manual, true),
            None
        );
        // A change that has not had the grace window to take effect.
        assert_eq!(
            speed_mismatch_note(0.0, Some(0.30), FanOwnership::ForcedStop, false),
            None
        );
        // Agreed readings need no note.
        assert_eq!(
            speed_mismatch_note(0.4, Some(0.40), FanOwnership::Manual, true),
            None
        );
        assert_eq!(
            speed_mismatch_note(0.0, Some(0.0), FanOwnership::Driver, true),
            None
        );
    }

    #[test]
    fn the_write_path_never_commands_a_crawl() {
        // Mapping plus percent conversion: 27 % of a spinning cooler stays
        // at 30 %, 24 % becomes a park (no percent written at all).
        assert_eq!(
            speed_percent(effective_speed(0.27, FanOwnership::Manual, false, false)),
            30
        );
        assert_eq!(
            speed_percent(effective_speed(0.24, FanOwnership::Manual, false, false)),
            0
        );
        assert_eq!(
            speed_percent(effective_speed(0.27, FanOwnership::Driver, false, false)),
            0
        );
        assert_eq!(
            speed_percent(effective_speed(0.3, FanOwnership::Driver, false, false)),
            30
        );
        // The take-over rule writes the floor, never the crawl it replaced.
        assert_eq!(
            speed_percent(effective_speed(0.2, FanOwnership::Driver, true, false)),
            30
        );
        // ... and so does the hold that follows it.
        assert_eq!(
            speed_percent(effective_speed(0.2, FanOwnership::Manual, false, true)),
            30
        );
    }

    #[test]
    fn speed_percent_maps_fractions_to_nvml_scale() {
        assert_eq!(speed_percent(0.0), 0);
        assert_eq!(speed_percent(0.5), 50);
        assert_eq!(speed_percent(0.999), 100);
        assert_eq!(speed_percent(1.0), 100);
        // Out-of-range fractions clamp like everywhere else in core.
        assert_eq!(speed_percent(-0.5), 0);
        assert_eq!(speed_percent(1.5), 100);
        // A non-finite fraction clamps to a usable bound, never NaN.
        assert_eq!(speed_percent(f64::NAN), 0);
        assert_eq!(speed_percent(f64::INFINITY), 100);
    }

    #[test]
    fn rpm_averages_the_fans_that_reported() {
        // A two-fan cooler spins both at 1200 rpm.
        assert_eq!(average_rpm(2400, 2), Some(1200.0));
        // One tachometer reporting on a two-fan card is still a reading.
        assert_eq!(average_rpm(900, 1), Some(900.0));
        // Parked fans report 0 rpm rather than nothing.
        assert_eq!(average_rpm(0, 2), Some(0.0));
        // A driver without the RPM call, or a card that refuses it.
        assert_eq!(average_rpm(0, 0), None);
    }
}
