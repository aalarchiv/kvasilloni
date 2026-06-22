#!/usr/bin/env bash
# End-to-end selftest for the canlib32 cannelloni shim, on a single Linux host:
#
#   canshim_probe.exe (wine) --[canlib32.dll shim]--> cannelloni <--> vcan0 <--> candump/cansend
#
# Verifies BOTH directions over BOTH transports (UDP, TCP):
#   * probe canWrite  -> appears on vcan0 (candump)
#   * cansend on vcan0 -> delivered to probe canRead
#
# Needs: wine, can-utils (candump/cansend), cmake+g++ (to build cannelloni),
#        and permission to create a vcan link (root or CAP_NET_ADMIN).
# Skips gracefully (exit 0) when a prerequisite is missing.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BUILD="$ROOT/build"
CANN="$ROOT/refs/cannelloni"
VCAN=vcan0
DLL="$BUILD/canlib32.dll"
PROBE="$BUILD/canshim_probe.exe"

skip() { echo "SELFTEST SKIP: $*"; exit 0; }
need() { command -v "$1" >/dev/null 2>&1 || skip "missing tool: $1"; }

# ---- privilege helper for ip link ----
SUDO=""
if [ "$(id -u)" != "0" ]; then
  if command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null; then SUDO="sudo"; fi
fi

need wine
need candump
need cansend
[ -f "$DLL" ]   || skip "build the DLL first (make all)"
[ -f "$PROBE" ] || skip "build the probe first (make probe)"

# ---- build cannelloni if needed ----
CANNBIN="$CANN/build/cannelloni"
if [ ! -x "$CANNBIN" ]; then
  need cmake
  echo "== building cannelloni =="
  cmake -S "$CANN" -B "$CANN/build" -DCMAKE_BUILD_TYPE=Release -DSCTP_SUPPORT=OFF >/dev/null 2>&1 \
    && cmake --build "$CANN/build" -j >/dev/null 2>&1 || skip "cannelloni build failed"
fi
[ -x "$CANNBIN" ] || skip "cannelloni binary not found after build"

# ---- bring up vcan0 ----
if ! ip link show "$VCAN" >/dev/null 2>&1; then
  $SUDO modprobe vcan 2>/dev/null
  $SUDO ip link add dev "$VCAN" type vcan 2>/dev/null || skip "cannot create $VCAN (need root/CAP_NET_ADMIN)"
fi
$SUDO ip link set up "$VCAN" 2>/dev/null || skip "cannot bring up $VCAN"

# put the shim next to the probe so wine's LoadLibrary finds it in the app dir
cp -f "$DLL" "$BUILD/canlib32.dll"

FAIL=0
CLEAN_PIDS=()
cleanup() { for p in "${CLEAN_PIDS[@]:-}"; do kill "$p" 2>/dev/null; done; }
trap cleanup EXIT

run_case() {  # $1=label  $2..=cannelloni args ; sets env via CANENV
  local label="$1"; shift
  local cap log probe_out
  cap="$(mktemp)"; log="$(mktemp)"; probe_out="$(mktemp)"
  echo
  echo "===== CASE: $label ====="

  # capture everything on the bus
  candump -L "$VCAN" > "$cap" 2>/dev/null &
  local CDPID=$!; CLEAN_PIDS+=("$CDPID")

  # start cannelloni
  echo "  cannelloni $*"
  "$CANNBIN" "$@" > "$log" 2>&1 &
  local CNPID=$!; CLEAN_PIDS+=("$CNPID")
  sleep 1.0

  # launch probe under wine (shim picks up CANSHIM_* from env)
  ( cd "$BUILD" && eval "$CANENV" wine ./canshim_probe.exe 0x18EEFF00 ext DE AD BE EF ) \
      > "$probe_out" 2>/dev/null &
  local PBPID=$!; CLEAN_PIDS+=("$PBPID")

  # inject an inbound frame mid-flight
  sleep 2.0
  cansend "$VCAN" 18FF0102#01020304 2>/dev/null
  echo "  injected: cansend $VCAN 18FF0102#01020304"

  wait "$PBPID" 2>/dev/null
  sleep 0.5
  kill "$CNPID" "$CDPID" 2>/dev/null

  # --- assertions ---
  # 1. probe's TX frame must appear on the bus
  if grep -qiE "18EEFF00#?.*DEADBEEF|18EEFF00#DEADBEEF" "$cap"; then
    echo "  PASS: probe TX (0x18EEFF00 DEADBEEF) seen on $VCAN"
  else
    echo "  FAIL: probe TX not seen on $VCAN"; echo "  --- candump ---"; sed 's/^/    /' "$cap"; FAIL=1
  fi
  # 2. injected frame must be delivered to probe canRead
  if grep -qiE "RX id=0x18FF0102" "$probe_out"; then
    echo "  PASS: injected frame delivered to probe canRead"
  else
    echo "  FAIL: probe did not receive injected frame"; echo "  --- probe ---"; sed 's/^/    /' "$probe_out"; FAIL=1
  fi
  rm -f "$cap" "$log" "$probe_out"
}

# UDP: cannelloni listens 20000, sends to probe at 20001
CANENV="CANSHIM_PROTO=udp CANSHIM_HOST=127.0.0.1 CANSHIM_PORT=20000 CANSHIM_LOCALPORT=20001" \
  run_case "UDP" -I "$VCAN" -R 127.0.0.1 -r 20001 -l 20000

# TCP: cannelloni is server on 20002, shim connects as client
CANENV="CANSHIM_PROTO=tcp CANSHIM_TCPROLE=client CANSHIM_HOST=127.0.0.1 CANSHIM_PORT=20002" \
  run_case "TCP" -C s -I "$VCAN" -l 20002

echo
if [ "$FAIL" = 0 ]; then echo "SELFTEST: PASS"; else echo "SELFTEST: FAIL"; fi
exit "$FAIL"
