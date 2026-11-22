//! Watching for the machine coming back from sleep.
//!
//! A resume leaves the hardware in a state the control loop's memory does
//! not match: hwmon directories get re-numbered when a controller re-binds,
//! the board's `pwmN_enable` files are usually back in hardware-automatic
//! mode, and a graphics card has spent the sleep running its own fan policy.
//! The daemon's answer is
//! [`biggest_fan_core::Engine::resync_hardware`] (wired up in
//! `daemon_main`); this module is only the detector, which listens for
//! logind's `PrepareForSleep` signal — `systemd-logind` emits it before
//! suspending and again once the machine is back.

use futures_util::stream::StreamExt;
use std::sync::mpsc::Sender;

/// `systemd-logind`'s sleep interface.
#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait LoginManager {
    /// One boolean: `true` before the machine sleeps, `false` once it has
    /// finished waking.
    #[zbus(signal)]
    fn prepare_for_sleep(&self, sleeping: bool) -> zbus::Result<()>;
}

/// Sends `()` on `wake` every time the machine finishes waking from suspend
/// or hibernate.
///
/// Like the D-Bus service thread, this runs on its own thread with its own
/// `zbus::block_on` runtime, and stays alive for the rest of the process —
/// the signal is the only thing it waits for. A machine without logind, or a
/// system bus this process cannot reach, is reported once and gives up: the
/// daemon keeps controlling fans, it simply will not notice a resume.
pub fn spawn_resume_watcher(wake: Sender<()>) {
    let spawned = std::thread::Builder::new()
        .name("biggest-fan-power".to_string())
        .spawn(move || {
            zbus::block_on(async {
                let connection = match zbus::Connection::system().await {
                    Ok(connection) => connection,
                    Err(error) => {
                        eprintln!(
                            "biggest-fan-daemon: cannot reach the system bus: {error} — fan \
                             settings will not be re-asserted after a suspend"
                        );
                        return;
                    }
                };
                let manager = match LoginManagerProxy::new(&connection).await {
                    Ok(manager) => manager,
                    Err(error) => {
                        eprintln!(
                            "biggest-fan-daemon: no logind on the system bus ({error}) — fan \
                             settings will not be re-asserted after a suspend"
                        );
                        return;
                    }
                };
                let mut sleep_signals = match manager.receive_prepare_for_sleep().await {
                    Ok(signals) => signals,
                    Err(error) => {
                        eprintln!(
                            "biggest-fan-daemon: cannot watch logind's sleep signal: {error}"
                        );
                        return;
                    }
                };
                while let Some(signal) = sleep_signals.next().await {
                    match signal.args() {
                        Ok(args) if *args.sleeping() => {}
                        Ok(_) => {
                            // The waking half of the signal: the hardware is
                            // back and needs telling who drives the fans.
                            if wake.send(()).is_err() {
                                // The control loop is gone; nothing left to
                                // wake.
                                return;
                            }
                        }
                        Err(error) => eprintln!(
                            "biggest-fan-daemon: PrepareForSleep signal could not be read: {error}"
                        ),
                    }
                }
            });
        });
    if spawned.is_err() {
        eprintln!("biggest-fan-daemon: could not start the resume watcher thread");
    }
}
