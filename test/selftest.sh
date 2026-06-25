#!/usr/bin/env bash
# SPDX-License-Identifier: LGPL-3.0-or-later
# End-to-end selftest for the Rust canlib32 cannelloni shim, on a single host:
#
#   canshim_probe.exe (wine) --[canlib32.dll shim]--> cannelloni <--> vcan1 <--> candump/cansend
#
# Uses an ISOLATED vcan1 so it never disturbs czone research running on vcan0.
# Verifies BOTH directions over BOTH transports (UDP, TCP):
#   * probe canWrite   -> appears on vcan1 (candump)
#   * cansend on vcan1 -> delivered to probe canRead
#
# Needs: wine, can-utils, cmake+g++ (to build cannelloni), i686 mingw (probe),
#        a cargo-built 32-bit DLL, and permission to create a vcan link.
# Skips gracefully (exit 0) when a prerequisite is missing.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BUILD="$ROOT/target/selftest"
CANN="$ROOT/refs/cannelloni"
VCAN="${SELFTEST_VCAN:-vcan1}"
DLL="$ROOT/target/i686-pc-windows-gnu/release/canlib32.dll"

skip() { echo "SELFTEST SKIP: $*"; exit 0; }
need() { command -v "$1" >/dev/null 2>&1 || skip "missing tool: $1"; }

SUDO=""
if [ "$(id -u)" != "0" ]; then
  if command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null; then SUDO="sudo"; fi
fi

need wine
need candump
need cansend
need i686-w64-mingw32-gcc

# build the DLL if missing
if [ ! -f "$DLL" ]; then
  need cargo
  echo "== building 32-bit DLL =="
  ( cd "$ROOT" && cargo build --release --target i686-pc-windows-gnu ) >/dev/null 2>&1 \
    || skip "cargo build failed"
fi
[ -f "$DLL" ] || skip "DLL not found: $DLL"

mkdir -p "$BUILD"
# build the probe and place the shim beside it (wine LoadLibrary checks app dir)
i686-w64-mingw32-gcc -O2 -Wall "$ROOT/test/canshim_probe.c" -o "$BUILD/canshim_probe.exe" \
  || skip "probe build failed"
cp -f "$DLL" "$BUILD/canlib32.dll"

# Locate cannelloni: prefer one already installed on PATH; otherwise build from
# a local source checkout at $CANN (override with CANNELLONI_SRC) if present.
CANN="${CANNELLONI_SRC:-$CANN}"
if command -v cannelloni >/dev/null 2>&1; then
  CANNBIN="$(command -v cannelloni)"
else
  CANNBIN="$CANN/build/cannelloni"
  if [ ! -x "$CANNBIN" ]; then
    [ -d "$CANN" ] || skip "cannelloni not on PATH and no source at $CANN (set CANNELLONI_SRC)"
    need cmake
    echo "== building cannelloni from $CANN =="
    cmake -S "$CANN" -B "$CANN/build" -DCMAKE_BUILD_TYPE=Release -DSCTP_SUPPORT=OFF >/dev/null 2>&1 \
      && cmake --build "$CANN/build" -j >/dev/null 2>&1 || skip "cannelloni build failed"
  fi
fi
[ -x "$CANNBIN" ] || skip "cannelloni binary not found"

# bring up the ISOLATED vcan
if ! ip link show "$VCAN" >/dev/null 2>&1; then
  $SUDO modprobe vcan 2>/dev/null
  $SUDO ip link add dev "$VCAN" type vcan 2>/dev/null || skip "cannot create $VCAN (need root/CAP_NET_ADMIN)"
fi
# CAN FD frames need an FD-capable MTU (CANFD_MTU=72); set it while the link is
# down (a freshly added vcan is down) so the FD case can inject via cansend.
$SUDO ip link set "$VCAN" mtu 72 2>/dev/null || true
$SUDO ip link set up "$VCAN" 2>/dev/null || skip "cannot bring up $VCAN"
echo "using isolated bus: $VCAN"

FAIL=0
PIDS=()
cleanup() { for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null; done; }
trap cleanup EXIT

run_case() {  # $1=label  $2..=cannelloni args ; CANENV provides shim env
  local label="$1"; shift
  local cap probe_out
  cap="$(mktemp)"; probe_out="$(mktemp)"
  echo; echo "===== CASE: $label ====="

  candump -L "$VCAN" > "$cap" 2>/dev/null & local CDPID=$!; PIDS+=("$CDPID")
  echo "  cannelloni $*"
  "$CANNBIN" "$@" >/dev/null 2>&1 & local CNPID=$!; PIDS+=("$CNPID")
  sleep 1.0

  # PROBE_ARGS, INJECT, TXGREP, RXGREP are set by the caller per scenario.
  ( cd "$BUILD" && eval "$CANENV" wine ./canshim_probe.exe $PROBE_ARGS ) \
      > "$probe_out" 2>/dev/null & local PBPID=$!; PIDS+=("$PBPID")

  sleep 2.0
  cansend "$VCAN" "$INJECT" 2>/dev/null
  echo "  injected: cansend $VCAN $INJECT"

  wait "$PBPID" 2>/dev/null
  sleep 0.4
  kill "$CNPID" "$CDPID" 2>/dev/null
  sleep 0.8   # let sockets release before the next case rebinds

  if grep -qiE "$TXGREP" "$cap"; then
    echo "  PASS: probe TX seen on $VCAN ($TXGREP)"
  else
    echo "  FAIL: probe TX not seen on $VCAN ($TXGREP)"; echo "  --- candump ---"; sed 's/^/    /' "$cap"
    echo "  --- probe ---"; sed 's/^/    /' "$probe_out"; FAIL=1
  fi
  if grep -qiE "$RXGREP" "$probe_out"; then
    echo "  PASS: injected frame delivered to probe canRead ($RXGREP)"
  else
    echo "  FAIL: probe did not receive injected frame ($RXGREP)"; echo "  --- probe ---"; sed 's/^/    /' "$probe_out"; FAIL=1
  fi
  rm -f "$cap" "$probe_out"
}

# --- classic CAN, extended ID, over both transports ---
PROBE_ARGS="0x18EEFF00 ext DE AD BE EF"
INJECT="18FF0102#01020304"
TXGREP="18EEFF00#?.*DEADBEEF|18EEFF00#DEADBEEF"
RXGREP="RX id=0x18FF0102"

# UDP: cannelloni listens 20100, sends to probe at 20101
CANENV="KVASILLONI_PROTO=udp KVASILLONI_HOST=127.0.0.1 KVASILLONI_PORT=20100 KVASILLONI_LOCALPORT=20101" \
  run_case "UDP (classic)" -I "$VCAN" -R 127.0.0.1 -r 20101 -l 20100

# TCP: cannelloni is server on 20102, shim connects as client.
# -p disables cannelloni's peer-IP check (else it rejects our client's source IP
# since no -R is set on a server). On a real deployment, set -R <win-ip> instead.
CANENV="KVASILLONI_PROTO=tcp KVASILLONI_TCPROLE=client KVASILLONI_HOST=127.0.0.1 KVASILLONI_PORT=20102" \
  run_case "TCP (classic)" -C s -p -I "$VCAN" -l 20102

# --- CAN FD with bit-rate switch (over UDP) ---
# probe sends 12 FD bytes with BRS; candump -L marks FD frames with '##'.
PROBE_ARGS="0x18EEFF02 extfdbrs 00 11 22 33 44 55 66 77 88 99 AA BB"
INJECT="18FF0105##100112233445566778899AABBCCDDEEFF"   # ## => FD, flags nibble 1 = BRS
TXGREP="18EEFF02##.*001122334455"
# flag >= 0x10000 (5 hex digits, nonzero lead) => an FD flag bit is set (FDF/BRS/ESI)
RXGREP="RX id=0x18FF0105 .*flag=0x[1-9a-fA-F][0-9a-fA-F]{4}"

CANENV="KVASILLONI_PROTO=udp KVASILLONI_HOST=127.0.0.1 KVASILLONI_PORT=20104 KVASILLONI_LOCALPORT=20105" \
  run_case "UDP (CAN FD + BRS)" -I "$VCAN" -R 127.0.0.1 -r 20105 -l 20104

# --- INI-based config (Windows-native): no KVASILLONI_* config env at all ---
# The shim must auto-discover kvasilloni.ini next to the DLL (ports 20106/20107).
cat > "$BUILD/kvasilloni.ini" <<EOF
[cannelloni]
host      = 127.0.0.1
port      = 20106
localport = 20107
proto     = udp
EOF
PROBE_ARGS="0x18EEFF03 ext CA FE"
INJECT="18FF0106#05060708"
TXGREP="18EEFF03#?.*CAFE"
RXGREP="RX id=0x18FF0106"
CANENV="" \
  run_case "UDP (INI config, no env)" -I "$VCAN" -R 127.0.0.1 -r 20107 -l 20106
rm -f "$BUILD/kvasilloni.ini"

# ===================== extended exports (epic kvasilloni-5yp) =====================

# --- channel enumeration (canGetNumberOfChannels / canGetChannelData) ---
# No cannelloni needed; the shim answers from config. KVASILLONI_CHANNELS=2.
echo; echo "===== CASE: channel enumeration (--enum) ====="
enum_out="$(mktemp)"
( cd "$BUILD" && KVASILLONI_CHANNELS=2 wine ./canshim_probe.exe --enum ) > "$enum_out" 2>/dev/null
if grep -qE "ENUM count=2 st=0" "$enum_out" && grep -qE 'ENUM name="kvasilloni vcan0" st=0' "$enum_out"; then
  echo "  PASS: enumerated 2 channels and read channel name"
else
  echo "  FAIL: enumeration output unexpected"; sed 's/^/    /' "$enum_out"; FAIL=1
fi
rm -f "$enum_out"

# --- acceptance filtering (canAccept): inject two ids, only one accepted ---
echo; echo "===== CASE: acceptance filter (--accept, UDP) ====="
acc_out="$(mktemp)"
"$CANNBIN" -I "$VCAN" -R 127.0.0.1 -r 20109 -l 20108 >/dev/null 2>&1 & ACNPID=$!; PIDS+=("$ACNPID")
sleep 1.0
( cd "$BUILD" && KVASILLONI_PROTO=udp KVASILLONI_HOST=127.0.0.1 KVASILLONI_PORT=20108 KVASILLONI_LOCALPORT=20109 \
    wine ./canshim_probe.exe --accept 0x18FF0201 ext ) > "$acc_out" 2>/dev/null & ACPBPID=$!; PIDS+=("$ACPBPID")
sleep 2.0
cansend "$VCAN" "18FF0201#AA" 2>/dev/null   # accepted
cansend "$VCAN" "18FF0202#BB" 2>/dev/null   # rejected
echo "  injected: 18FF0201 (accept) and 18FF0202 (reject)"
wait "$ACPBPID" 2>/dev/null
kill "$ACNPID" 2>/dev/null
sleep 0.8
if grep -qE "RX id=0x18FF0201" "$acc_out" && ! grep -qE "RX id=0x18FF0202" "$acc_out"; then
  echo "  PASS: only the accepted id reached canRead"
else
  echo "  FAIL: acceptance filtering wrong"; sed 's/^/    /' "$acc_out"; FAIL=1
fi
rm -f "$acc_out"

# --- notifications (canSetNotify): N injected frames => N callbacks ---
echo; echo "===== CASE: notify callback (--notify, UDP) ====="
not_out="$(mktemp)"
"$CANNBIN" -I "$VCAN" -R 127.0.0.1 -r 20111 -l 20110 >/dev/null 2>&1 & NTNPID=$!; PIDS+=("$NTNPID")
sleep 1.0
( cd "$BUILD" && KVASILLONI_PROTO=udp KVASILLONI_HOST=127.0.0.1 KVASILLONI_PORT=20110 KVASILLONI_LOCALPORT=20111 \
    wine ./canshim_probe.exe --notify ) > "$not_out" 2>/dev/null & NTPBPID=$!; PIDS+=("$NTPBPID")
sleep 2.0
for n in 1 2 3; do cansend "$VCAN" "18FF030$n#0$n" 2>/dev/null; sleep 0.2; done
echo "  injected: 3 frames"
wait "$NTPBPID" 2>/dev/null
kill "$NTNPID" 2>/dev/null
sleep 0.8
if grep -qE "NOTIFY count=3" "$not_out"; then
  echo "  PASS: received 3 notify callbacks for 3 frames"
else
  echo "  FAIL: notify count wrong"; sed 's/^/    /' "$not_out"; FAIL=1
fi
rm -f "$not_out"

echo
if [ "$FAIL" = 0 ]; then echo "SELFTEST: PASS"; else echo "SELFTEST: FAIL"; fi
exit "$FAIL"
