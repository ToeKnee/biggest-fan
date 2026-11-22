//! D-Bus bridge between the (privileged) daemon and the user's GUI.
//!
//! The daemon owns the fan headers; the GUI talks to it over the system
//! bus (`net.ynottony.ten.biggestfan`, policy in `packaging/dbus/`), so
//! the GUI needs no root. Method payloads are JSON strings: modes and ids
//! cross the bus in exactly the serde shape the config file uses, and
//! every method answers with an envelope — `{"ok": true, ...}` or
//! `{"ok": false, "error": "..."}` — so service failures surface in the
//! GUI status line rather than as D-Bus errors.
//!
//! The daemon's engine stays single-threaded: interface methods only
//! queue [`FanRequest`]s, which [`drain_commands`] applies on the
//! daemon's own thread between ticks.

use biggest_fan_core as core;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::time::Duration;

/// Bus name the daemon owns (granting it needs the shipped policy file).
pub const BUS_NAME: &str = "net.ynottony.ten.biggestfan";
/// Object path the daemon serves on; the interface name
/// (`net.ynottony.ten.biggestfan.Control1`) appears in the `interface`
/// and `proxy` macros below and in the bus policy file.
pub const OBJECT_PATH: &str = "/net/ynottony/ten/biggestfan";

/// A change the GUI asks the running service to apply to its engine.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum FanRequest {
    /// Set one fan's mode; `mode_json` is a [`core::FanMode`] in
    /// JSON, the same shape the config file writes.
    SetFanMode {
        /// Fan id.
        fan: String,
        /// The mode, as JSON.
        mode_json: String,
    },
    /// Rename a fan for display.
    SetName {
        /// Fan id.
        fan: String,
        /// New display name; empty restores the hardware default.
        name: String,
    },
    /// Corrects a tachometer that reports double (or half).
    SetRpmFactor {
        /// Fan id.
        fan: String,
        /// Measured RPM is multiplied by this.
        factor: f64,
    },
    /// Sets a fan's speed floor: the smallest fraction the engine may command
    /// in fixed or auto mode, for something that must keep turning (an AIO
    /// pump). _Off_ mode still stops the fan.
    SetMinSpeed {
        /// Fan id.
        fan: String,
        /// Floor as a fraction of full speed (0.0 = no floor).
        speed: f64,
    },
    /// Spin every header up and park the ones with nothing connected.
    Detect,
    /// Take over a discovered header no managed fan drives yet (or revive
    /// a parked one) as a managed fan, and save the config.
    AddFan {
        /// Header id to take over.
        fan: String,
    },
    /// Ask for the full config text the service runs with.
    GetConfig,
    /// Replace the service's config with the GUI's edited TOML text and
    /// save it.
    Save {
        /// Complete config file text.
        toml_text: String,
    },
    /// Change the control-loop refresh period.
    SetInterval {
        /// Poll period in milliseconds.
        ms: u64,
    },
}

/// One queued request plus the channel its envelope answer comes back on.
pub struct Command {
    /// The change to apply.
    pub request: FanRequest,
    /// Where to send the envelope answer.
    pub reply: Sender<String>,
}

/// JSON envelope every interface method answers with.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Reply {
    /// Whether the request succeeded.
    pub ok: bool,
    /// Failure text, when `ok` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Fan ids disabled by `Detect`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ids: Option<Vec<String>>,
    /// Config text, from `GetConfig`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<String>,
}

fn render(reply: &Reply) -> String {
    serde_json::to_string(reply).unwrap_or_else(|_| {
        r#"{"ok":false,"error":"service reply failed to serialise"}"#.to_string()
    })
}

fn parse_envelope(text: &str) -> Reply {
    serde_json::from_str(text).unwrap_or_else(|_| Reply {
        ok: false,
        error: Some("service reply was unreadable".to_string()),
        ..Reply::default()
    })
}

fn envelope_ok() -> String {
    render(&Reply {
        ok: true,
        ..Reply::default()
    })
}

fn envelope_err(message: &str) -> String {
    render(&Reply {
        ok: false,
        error: Some(message.to_string()),
        ..Reply::default()
    })
}

fn envelope_ids(ids: Vec<String>) -> String {
    render(&Reply {
        ok: true,
        ids: Some(ids),
        ..Reply::default()
    })
}

fn envelope_config(config: String) -> String {
    render(&Reply {
        ok: true,
        config: Some(config),
        ..Reply::default()
    })
}

/// Why the GUI could not join a running service. Displayed verbatim, so
/// each variant is a complete human-readable sentence.
#[derive(Debug)]
pub enum ConnectError {
    /// The system bus itself is unreachable.
    Bus(String),
    /// No process owns the bus name: the daemon (or service) is not running.
    NotRunning,
    /// Something owns the name, but it never answered the first call.
    Unresponsive,
    /// The service answered, but not with a usable config.
    Service(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bus(error) => write!(f, "cannot reach the system bus: {error}"),
            Self::NotRunning => write!(f, "no Biggest Fan service owns {BUS_NAME}"),
            Self::Unresponsive => write!(f, "{BUS_NAME} is registered but is not answering"),
            Self::Service(error) => {
                write!(f, "the Biggest Fan service answered badly: {error}")
            }
        }
    }
}

/// The object published on the system bus. Its methods queue requests
/// for the daemon's main thread; none of them touch the engine directly.
pub struct ControlService {
    commands: Sender<Command>,
}

impl ControlService {
    /// Wraps the daemon's request queue.
    #[must_use]
    pub const fn new(commands: Sender<Command>) -> Self {
        Self { commands }
    }

    fn dispatch(&self, request: FanRequest) -> String {
        let (reply_tx, reply_rx) = mpsc::channel();
        if self
            .commands
            .send(Command {
                request,
                reply: reply_tx,
            })
            .is_err()
        {
            return envelope_err("the daemon is stopping");
        }
        reply_rx
            .recv()
            .unwrap_or_else(|_| envelope_err("the daemon is stopping"))
    }
}

#[zbus::interface(name = "net.ynottony.ten.biggestfan.Control1")]
impl ControlService {
    /// Sets one fan's mode; `mode_json` uses the config file's
    /// serde shape for a mode.
    fn set_fan_mode(&self, fan_id: &str, mode_json: &str) -> String {
        self.dispatch(FanRequest::SetFanMode {
            fan: fan_id.to_string(),
            mode_json: mode_json.to_string(),
        })
    }

    /// Renames a fan; an empty name restores the hardware default.
    fn set_fan_name(&self, fan_id: &str, name: &str) -> String {
        self.dispatch(FanRequest::SetName {
            fan: fan_id.to_string(),
            name: name.to_string(),
        })
    }

    /// Corrects a fan's tachometer reading by a multiplication factor.
    fn set_fan_rpm_factor(&self, fan_id: &str, factor: f64) -> String {
        self.dispatch(FanRequest::SetRpmFactor {
            fan: fan_id.to_string(),
            factor,
        })
    }

    /// Sets a fan's speed floor (0.0 clears it).
    fn set_fan_min_speed(&self, fan_id: &str, speed: f64) -> String {
        self.dispatch(FanRequest::SetMinSpeed {
            fan: fan_id.to_string(),
            speed,
        })
    }

    /// Spins every header up and parks the ones that do not spin.
    fn detect_fans(&self) -> String {
        self.dispatch(FanRequest::Detect)
    }

    /// Takes over an unclaimed header (or revives a parked one) as a
    /// managed fan.
    fn add_fan(&self, fan_id: &str) -> String {
        self.dispatch(FanRequest::AddFan {
            fan: fan_id.to_string(),
        })
    }

    /// The complete config the service currently runs with.
    fn get_config(&self) -> String {
        self.dispatch(FanRequest::GetConfig)
    }

    /// Applies and saves a whole config (the GUI's edited copy).
    fn save_config(&self, toml_text: &str) -> String {
        self.dispatch(FanRequest::Save {
            toml_text: toml_text.to_string(),
        })
    }

    /// Changes the control-loop refresh period, in milliseconds.
    fn set_poll_interval_ms(&self, interval_ms: u64) -> String {
        self.dispatch(FanRequest::SetInterval { ms: interval_ms })
    }
}

/// Runs one request against the daemon's engine and answers with the
/// reply envelope. Called on the daemon's own thread.
#[must_use]
pub fn apply_request(engine: &mut core::Engine, request: FanRequest) -> String {
    match request {
        FanRequest::SetFanMode { fan, mode_json } => {
            match serde_json::from_str::<core::FanMode>(&mode_json) {
                Ok(mode) => match engine.set_fan_mode(&core::FanId::new(fan), &mode) {
                    Ok(()) => envelope_ok(),
                    Err(error) => envelope_err(&error.to_string()),
                },
                Err(error) => envelope_err(&format!("invalid mode: {error}")),
            }
        }
        FanRequest::SetName { fan, name } => {
            match engine.set_fan_name(&core::FanId::new(fan), &name) {
                Ok(()) => envelope_ok(),
                Err(error) => envelope_err(&error.to_string()),
            }
        }
        FanRequest::SetRpmFactor { fan, factor } => {
            match engine.set_fan_rpm_factor(&core::FanId::new(fan), factor) {
                Ok(()) => envelope_ok(),
                Err(error) => envelope_err(&error.to_string()),
            }
        }
        FanRequest::SetMinSpeed { fan, speed } => {
            match engine.set_fan_min_speed(&core::FanId::new(fan), speed) {
                Ok(()) => envelope_ok(),
                Err(error) => envelope_err(&error.to_string()),
            }
        }
        FanRequest::Detect => {
            match engine.detect_unconnected_fans(Duration::from_millis(900)) {
                Ok(hidden) => {
                    let ids: Vec<String> = hidden.into_iter().map(|id| id.to_string()).collect();
                    // Keep the parked set across daemon restarts.
                    match engine.save_config() {
                        Ok(()) => envelope_ids(ids),
                        Err(error) => envelope_err(&error.to_string()),
                    }
                }
                Err(error) => envelope_err(&error.to_string()),
            }
        }
        FanRequest::AddFan { fan } => {
            // The added fan must survive restarts, so save like `Detect` does.
            match engine
                .add_fan(&core::FanId::new(fan))
                .and_then(|()| engine.save_config())
            {
                Ok(()) => envelope_ok(),
                Err(error) => envelope_err(&error.to_string()),
            }
        }
        FanRequest::GetConfig => match core::config::to_text(&engine.current_config()) {
            Ok(text) => envelope_config(text),
            Err(error) => envelope_err(&error.to_string()),
        },
        FanRequest::Save { toml_text } => {
            match core::config::from_text(&toml_text)
                .and_then(|config| engine.apply_config(config))
                .and_then(|()| engine.save_config())
            {
                Ok(()) => envelope_ok(),
                Err(error) => envelope_err(&error.to_string()),
            }
        }
        FanRequest::SetInterval { ms } => {
            engine.set_poll_interval_ms(ms);
            envelope_ok()
        }
    }
}

/// Registers the interface on the system bus from a helper thread.
///
/// Lets the GUI reach the daemon without root. A missing system bus (or a
/// denied bus policy) is logged; the daemon controls fans regardless.
pub fn spawn_server(commands: Sender<Command>, stopping: Arc<AtomicBool>) {
    let spawned = std::thread::Builder::new()
        .name("biggest-fan-dbus".to_string())
        .spawn(move || {
            zbus::block_on(async {
                let served = zbus::connection::Builder::system()
                    .name(BUS_NAME)
                    .serve_at(OBJECT_PATH, ControlService::new(commands))
                    .build()
                    .await;
                match served {
                    Ok(connection) => {
                        // Stay inside `block_on` so its runtime keeps
                        // driving the connection; with the
                        // multi-threaded runtime enabled, method calls
                        // are served while this thread waits.
                        while !stopping.load(Ordering::Relaxed) {
                            std::thread::sleep(Duration::from_millis(250));
                        }
                        drop(connection);
                    }
                    Err(error) => eprintln!(
                        "biggest-fan: could not serve {BUS_NAME} on the system bus: {error} — the GUI needs this D-Bus interface to control fans"
                    ),
                }
            });
        });
    if spawned.is_err() {
        eprintln!("biggest-fan: could not start the D-Bus service thread");
    }
}

/// Applies queued GUI requests to the daemon's engine; call between
/// ticks (and inside sleep quanta) on the daemon's main thread.
pub fn drain_commands(engine: &mut core::Engine, commands: &Receiver<Command>) {
    while let Ok(command) = commands.try_recv() {
        let envelope = apply_request(engine, command.request);
        command.reply.send(envelope).ok();
    }
}

/// What the service tells the GUI about, to show in the status line.
#[derive(Debug)]
pub enum Event {
    /// A human-readable confirmation or error from the service.
    Status(String),
    /// Fans the service disabled during `Detect`; the GUI parks its own
    /// mirrors of these ids.
    Detected(Vec<String>),
    /// The service's full config text, from a periodic `GetConfig` poll:
    /// the service owns the live config, so the GUI's mirror follows it.
    Config(String),
}

/// Handle for talking to a running service; cheap to clone.
#[derive(Clone)]
pub struct Client {
    commands: Sender<FanRequest>,
}

impl Client {
    /// Queues a request for the service; problems come back as events.
    pub fn send(&self, request: FanRequest) {
        self.commands.send(request).ok();
    }

    /// Connects to a running Biggest Fan service on the system bus.
    ///
    /// Returns the client handle, its event stream, and the service's
    /// full config text.
    ///
    /// # Errors
    ///
    /// [`ConnectError`] explaining why the GUI cannot work without the
    /// service: no system bus, no service owning the bus name, a service
    /// that never answers, or one that answers badly.
    pub fn connect() -> Result<(Self, Receiver<Event>, String), ConnectError> {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || client_worker(command_rx, event_tx, ready_tx));
        let config = match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(config)) => config,
            Ok(Err(problem)) => return Err(problem),
            Err(mpsc::RecvTimeoutError::Timeout) => return Err(ConnectError::Unresponsive),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(ConnectError::Service(
                    "the connection thread stopped before the service answered".to_string(),
                ));
            }
        };
        Ok((
            Self {
                commands: command_tx,
            },
            event_rx,
            config,
        ))
    }
}

#[zbus::proxy(
    interface = "net.ynottony.ten.biggestfan.Control1",
    default_service = "net.ynottony.ten.biggestfan",
    default_path = "/net/ynottony/ten/biggestfan"
)]
trait Control {
    async fn set_fan_mode(&self, fan_id: &str, mode_json: &str) -> zbus::Result<String>;
    async fn set_fan_name(&self, fan_id: &str, name: &str) -> zbus::Result<String>;
    async fn set_fan_rpm_factor(&self, fan_id: &str, factor: f64) -> zbus::Result<String>;
    async fn set_fan_min_speed(&self, fan_id: &str, speed: f64) -> zbus::Result<String>;
    async fn detect_fans(&self) -> zbus::Result<String>;
    async fn add_fan(&self, fan_id: &str) -> zbus::Result<String>;
    async fn get_config(&self) -> zbus::Result<String>;
    async fn save_config(&self, toml_text: &str) -> zbus::Result<String>;
    async fn set_poll_interval_ms(&self, interval_ms: u64) -> zbus::Result<String>;
}

/// Owns the GUI's bus connection and calls: every queued request becomes
/// one service call, and the answer becomes an [`Event`].
async fn call_service(proxy: &ControlProxy<'_>, request: &FanRequest) -> zbus::Result<String> {
    match request {
        FanRequest::SetFanMode { fan, mode_json } => proxy.set_fan_mode(fan, mode_json).await,
        FanRequest::SetName { fan, name } => proxy.set_fan_name(fan, name).await,
        FanRequest::SetRpmFactor { fan, factor } => proxy.set_fan_rpm_factor(fan, *factor).await,
        FanRequest::SetMinSpeed { fan, speed } => proxy.set_fan_min_speed(fan, *speed).await,
        FanRequest::Detect => proxy.detect_fans().await,
        FanRequest::AddFan { fan } => proxy.add_fan(fan).await,
        FanRequest::GetConfig => proxy.get_config().await,
        FanRequest::Save { toml_text } => proxy.save_config(toml_text).await,
        FanRequest::SetInterval { ms } => proxy.set_poll_interval_ms(*ms).await,
    }
}

fn client_worker(
    commands: Receiver<FanRequest>,
    events: Sender<Event>,
    ready: SyncSender<Result<String, ConnectError>>,
) {
    zbus::block_on(async move {
        let Ok(connection) = zbus::Connection::system().await else {
            ready
                .send(Err(ConnectError::Bus(
                    "is the message bus running?".to_string(),
                )))
                .ok();
            return;
        };
        let Ok(fdo) = zbus::fdo::DBusProxy::new(&connection).await else {
            ready
                .send(Err(ConnectError::Bus(
                    "cannot talk to the bus daemon".to_string(),
                )))
                .ok();
            return;
        };
        let Ok(service_name) = zbus::names::BusName::try_from(BUS_NAME) else {
            ready
                .send(Err(ConnectError::Service(format!(
                    "{BUS_NAME} is not a valid bus name"
                ))))
                .ok();
            return;
        };
        if !fdo.name_has_owner(service_name).await.unwrap_or(false) {
            ready.send(Err(ConnectError::NotRunning)).ok();
            return;
        }
        let Ok(proxy) = ControlProxy::new(&connection).await else {
            ready
                .send(Err(ConnectError::Service(
                    "cannot reach the control interface".to_string(),
                )))
                .ok();
            return;
        };
        // Hand the service's config to the caller before serving requests.
        let Ok(envelope) = proxy.get_config().await else {
            ready
                .send(Err(ConnectError::Service(
                    "GetConfig did not answer".to_string(),
                )))
                .ok();
            return;
        };
        let reply = parse_envelope(&envelope);
        let (true, Some(config)) = (reply.ok, reply.config) else {
            ready
                .send(Err(ConnectError::Service(
                    reply
                        .error
                        .unwrap_or_else(|| "GetConfig failed".to_string()),
                )))
                .ok();
            return;
        };
        if ready.send(Ok(config)).is_err() {
            return; // the GUI gave up waiting
        }
        while let Ok(request) = commands.recv() {
            let was_save = matches!(request, FanRequest::Save { .. });
            let envelope = call_service(&proxy, &request).await;
            match envelope {
                Err(error) => {
                    events
                        .send(Event::Status(format!("service unreachable: {error}")))
                        .ok();
                }
                Ok(envelope) => {
                    let reply = parse_envelope(&envelope);
                    if reply.ok {
                        if let Some(ids) = reply.ids {
                            events.send(Event::Detected(ids)).ok();
                        } else if let Some(config) = reply.config {
                            events.send(Event::Config(config)).ok();
                        } else if was_save {
                            events
                                .send(Event::Status(
                                    "config applied and saved by the service".to_string(),
                                ))
                                .ok();
                        }
                    } else if let Some(error) = reply.error {
                        events.send(Event::Status(format!("service: {error}"))).ok();
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fan_mode_json_round_trips_like_the_config_file() {
        let mode = core::FanMode::Auto {
            sensor: core::SensorId::new("k10temp-t1"),
            curve: core::FanCurve::new(vec![core::CurvePoint {
                temp_c: 40.0,
                speed: 0.2,
            }])
            .expect("curve"),
        };
        let text = serde_json::to_string(&mode).expect("serialise");
        assert_eq!(
            serde_json::from_str::<core::FanMode>(&text).expect("parse"),
            mode
        );
    }

    #[test]
    fn the_fan_requests_round_trip_through_json() {
        // Anything that logs, queues or replays a request keeps the same
        // shape, including the speed floor an AIO pump needs.
        let requests = [
            FanRequest::SetMinSpeed {
                fan: "nct6795-p4".to_string(),
                speed: 0.4,
            },
            FanRequest::SetRpmFactor {
                fan: "nct6795-p1".to_string(),
                factor: 0.5,
            },
            FanRequest::SetInterval { ms: 250 },
            FanRequest::AddFan {
                fan: "arctic_fan-p3".to_string(),
            },
        ];
        for request in requests {
            let text = serde_json::to_string(&request).expect("serialise");
            assert_eq!(
                serde_json::from_str::<FanRequest>(&text).expect("parse"),
                request,
                "{text}"
            );
        }
    }

    #[test]
    fn envelopes_carry_ids_config_and_errors() {
        let ids = parse_envelope(&envelope_ids(vec!["it87-p4".to_string()]));
        assert!(ids.ok);
        assert_eq!(ids.ids.as_deref(), Some(&["it87-p4".to_string()][..]));

        let config = parse_envelope(&envelope_config("poll_interval_ms = 500\n".to_string()));
        assert_eq!(config.config.as_deref(), Some("poll_interval_ms = 500\n"));

        let failure = parse_envelope(&envelope_err("nope"));
        assert!(!failure.ok);
        assert_eq!(failure.error.as_deref(), Some("nope"));

        let garbage = parse_envelope("not json");
        assert!(!garbage.ok);
        assert!(garbage.error.is_some());
    }
}
