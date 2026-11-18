#!/bin/sh
# End-to-end smoke check for the daemon/GUI D-Bus plumbing. Not part of
# `cargo test`: it needs dbus tools (dbus-daemon, busctl) and, for the GUI
# scenarios, a display.
#
#   sh tools/dbus-e2e.sh
#
# Everything is throwaway: a private bus (never the real system bus),
# --dry-run (no sysfs writes), and scratch dirs under target/smoke.
set -eu
cd "$(dirname "$0")/.."

BIN=target/debug/biggest-fan
DAEMON=target/debug/biggest-fan-daemon
SMOKE=target/smoke
[ -x "$BIN" ] && [ -x "$DAEMON" ] || cargo build
mkdir -p "$SMOKE"

BUS_NAME=net.ynottony.ten.biggestfan
IFACE=$BUS_NAME.Control1
OBJ_PATH=/net/ynottony/ten/biggestfan
SOCK=$PWD/$SMOKE/e2e-bus-socket
PIDFILE=$PWD/$SMOKE/e2e-bus.pid
FAILED=0

fail() { echo "FAIL: $*"; FAILED=$((FAILED + 1)); }
pass() { echo "pass: $*"; }

cleanup() {
    [ -n "${DAEMON_PID:-}" ] && kill "$DAEMON_PID" 2>/dev/null || true
    [ -f "$PIDFILE" ] && kill "$(cat "$PIDFILE")" 2>/dev/null || true
    rm -f "$SOCK" "$SOCK.lock" "$PIDFILE"
}
trap cleanup EXIT

# --- a private bus, exported as the "system" bus for this test only -------
SOCK=$SOCK PIDFILE=$PIDFILE sh -c '
cat > target/smoke/e2e-bus.conf <<EOF
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path=$SOCK</listen>
  <pidfile>$PIDFILE</pidfile>
  <policy context="default">
    <allow own="*"/>
    <allow send_destination="*"/>
    <allow receive_sender="*"/>
  </policy>
</busconfig>
EOF
dbus-daemon --config-file=target/smoke/e2e-bus.conf --fork --print-address=3 3>target/smoke/e2e-bus-address.txt'
DBUS_SYSTEM_BUS_ADDRESS=$(cat "$SMOKE/e2e-bus-address.txt")
export DBUS_SYSTEM_BUS_ADDRESS
echo "private bus at: $DBUS_SYSTEM_BUS_ADDRESS"

# --- scenario 1: daemon answers method calls on the bus -------------------
"$DAEMON" --config "$SMOKE/e2e-daemon-config.toml" --dry-run \
    >"$SMOKE/e2e-daemon.log" 2>&1 &
DAEMON_PID=$!
i=0
until busctl --no-pager list 2>/dev/null | grep -q "$BUS_NAME" || [ $i -ge 50 ]; do
    sleep 0.2
    i=$((i + 1))
done
if busctl --no-pager list | grep -q "$BUS_NAME"; then
    pass "daemon owns $BUS_NAME"
else
    fail "daemon never owned $BUS_NAME (see $SMOKE/e2e-daemon.log)"
fi

if timeout 5 busctl --no-pager call "$BUS_NAME" "$OBJ_PATH" "$IFACE" GetConfig \
        | grep -q "poll_interval_ms"; then
    pass "GetConfig answers"
else
    fail "GetConfig did not answer"
fi

# AddFan must answer with an envelope (not a D-Bus error) even for a
# header this machine does not have; a fresh default config claims every
# discovered header, so the machine-independent path is the rejection.
# (busctl escapes the envelope's quotes, so match across the backslash.)
if timeout 5 busctl --no-pager call "$BUS_NAME" "$OBJ_PATH" "$IFACE" AddFan \
        s "biggest-fan-e2e-does-not-exist-p1" | grep -q 'ok[^,]*false'; then
    pass "AddFan rejects a header the machine does not have"
else
    fail "AddFan did not reject an unknown header"
fi

timeout 5 busctl --no-pager call "$BUS_NAME" "$OBJ_PATH" "$IFACE" \
    SetPollIntervalMs t 250 || fail "SetPollIntervalMs failed"
sleep 1
if timeout 5 busctl --no-pager call "$BUS_NAME" "$OBJ_PATH" "$IFACE" GetConfig \
        | grep -q 'poll_interval_ms[^,]*250'; then
    pass "SetPollIntervalMs took effect"
else
    fail "SetPollIntervalMs did not take effect"
fi

# SetFanMinSpeed (an AIO pump's speed floor) must reach the engine and answer
# with an envelope. The fan named here exists on no machine, so the
# machine-independent path is the engine's rejection — proof the method is on
# the bus rather than a D-Bus "unknown method" error.
if timeout 5 busctl --no-pager call "$BUS_NAME" "$OBJ_PATH" "$IFACE" SetFanMinSpeed \
        sd "biggest-fan-e2e-does-not-exist-p1" 0.4 | grep -q 'ok[^,]*false'; then
    pass "SetFanMinSpeed answers an envelope for a fan the machine does not have"
else
    fail "SetFanMinSpeed did not answer an envelope"
fi

# --- scenario 2: GUI without a service exits fast with the right error ----
if [ -n "${WAYLAND_DISPLAY:-}${DISPLAY:-}" ]; then
    kill -INT "$DAEMON_PID"
    i=0
    while kill -0 "$DAEMON_PID" 2>/dev/null && [ $i -lt 50 ]; do sleep 0.2; i=$((i + 1)); done
    if kill -0 "$DAEMON_PID" 2>/dev/null; then
        fail "daemon did not exit after SIGINT"
        kill -9 "$DAEMON_PID" 2>/dev/null || true
    else
        pass "daemon exits cleanly on SIGINT"
    fi
    DAEMON_PID=

    if XDG_CONFIG_HOME=$PWD/$SMOKE/e2e-xdg timeout 20 "$BIN" gui >"$SMOKE/e2e-gui-noservice.log" 2>&1; then
        gui_status=0
    else
        gui_status=$?
    fi
    if [ $gui_status -ne 0 ] && grep -q "needs the Biggest Fan service" "$SMOKE/e2e-gui-noservice.log"; then
        pass "GUI without service exits with the service-required error"
    else
        fail "GUI without service: status $gui_status, see $SMOKE/e2e-gui-noservice.log"
    fi

    # --- scenario 3: GUI joins the running service and stays alive -------
    "$DAEMON" --config "$SMOKE/e2e-daemon-config.toml" --dry-run \
        >"$SMOKE/e2e-daemon2.log" 2>&1 &
    DAEMON_PID=$!
    sleep 2
    XDG_CONFIG_HOME=$PWD/$SMOKE/e2e-xdg "$BIN" gui >"$SMOKE/e2e-gui.log" 2>&1 &
    GUI_PID=$!
    sleep 6
    if kill -0 "$GUI_PID" 2>/dev/null; then
        pass "GUI with service stays alive (window shown)"
        kill -TERM "$GUI_PID" 2>/dev/null || true
    else
        fail "GUI with service died early, see $SMOKE/e2e-gui.log"
    fi

    # --- scenario 3b: the IMPORTANT contract — closing the GUI must not
    # disturb the daemon: it stays up, keeps ticking and keeps answering.
    i=0
    while kill -0 "$GUI_PID" 2>/dev/null && [ $i -lt 50 ]; do sleep 0.2; i=$((i + 1)); done
    GUI_PID=
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        fail "daemon died when the GUI exited (see $SMOKE/e2e-daemon2.log)"
    else
        lines_before=$(wc -l < "$SMOKE/e2e-daemon2.log")
        sleep 2
        lines_after=$(wc -l < "$SMOKE/e2e-daemon2.log")
        if [ "$lines_after" -gt "$lines_before" ] &&
           timeout 5 busctl --no-pager call "$BUS_NAME" "$OBJ_PATH" "$IFACE" GetConfig \
               | grep -q poll_interval_ms; then
            pass "daemon keeps ticking and answering once the GUI is gone"
        else
            fail "daemon stopped controlling fans after the GUI exited"
        fi
    fi
else
    echo "no display present — skipping the GUI scenarios"
fi

# --- scenario 4: daemon degrades gracefully when the bus is broken --------
DBUS_SYSTEM_BUS_ADDRESS=unix:path=/nonexistent-bus-socket \
    timeout 4 "$DAEMON" --config "$SMOKE/e2e-daemon-config.toml" --dry-run \
    >"$SMOKE/e2e-daemon-nobus.log" 2>&1 &
NOBUS_PID=$!
sleep 3
if kill -0 "$NOBUS_PID" 2>/dev/null; then
    kill -TERM "$NOBUS_PID" 2>/dev/null || true
    if grep -q "could not serve" "$SMOKE/e2e-daemon-nobus.log"; then
        pass "daemon without a usable bus logs the problem and keeps ticking"
    else
        fail "daemon kept running but never logged the bus failure"
    fi
else
    fail "daemon died when the bus was broken (see $SMOKE/e2e-daemon-nobus.log)"
fi

echo
if [ $FAILED -eq 0 ]; then
    echo "ALL CHECKS PASSED"
else
    echo "$FAILED CHECK(S) FAILED"
    exit 1
fi
