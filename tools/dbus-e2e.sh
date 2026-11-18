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
"$DAEMON" --config "$SMOKE/e2e-daemon-config.toml" --dry-run --no-nvidia \
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

timeout 5 busctl --no-pager call "$BUS_NAME" "$OBJ_PATH" "$IFACE" \
    SetPollIntervalMs t 250 || fail "SetPollIntervalMs failed"
sleep 1
if timeout 5 busctl --no-pager call "$BUS_NAME" "$OBJ_PATH" "$IFACE" GetConfig \
        | grep -q 'poll_interval_ms[^,]*250'; then
    pass "SetPollIntervalMs took effect"
else
    fail "SetPollIntervalMs did not take effect"
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
    "$DAEMON" --config "$SMOKE/e2e-daemon-config.toml" --dry-run --no-nvidia \
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
else
    echo "no display present — skipping the GUI scenarios"
fi

# --- scenario 4: daemon degrades gracefully when the bus is broken --------
DBUS_SYSTEM_BUS_ADDRESS=unix:path=/nonexistent-bus-socket \
    timeout 4 "$DAEMON" --config "$SMOKE/e2e-daemon-config.toml" --dry-run --no-nvidia \
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
