#!/usr/bin/env bash
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

# build cannelloni if needed
CANNBIN="$CANN/build/cannelloni"
if [ ! -x "$CANNBIN" ]; then
  need cmake
  echo "== building cannelloni =="
  cmake -S "$CANN" -B "$CANN/build" -DCMAKE_BUILD_TYPE=Release -DSCTP_SUPPORT=OFF >/dev/null 2>&1 \
    && cmake --build "$CANN/build" -j >/dev/null 2>&1 || skip "cannelloni build failed"
fi
[ -x "$CANNBIN" ] || skip "cannelloni binary not found"

# bring up the ISOLATED vcan
if ! ip link show "$VCAN" >/dev/null 2>&1; then
  $SUDO modprobe vcan 2>/dev/null
  $SUDO ip link add dev "$VCAN" type vcan 2>/dev/null || skip "cannot create $VCAN (need root/CAP_NET_ADMIN)"
fi
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
CANENV="CANSHIM_PROTO=udp CANSHIM_HOST=127.0.0.1 CANSHIM_PORT=20100 CANSHIM_LOCALPORT=20101" \
  run_case "UDP (classic)" -I "$VCAN" -R 127.0.0.1 -r 20101 -l 20100

# TCP: cannelloni is server on 20102, shim connects as client.
# -p disables cannelloni's peer-IP check (else it rejects our client's source IP
# since no -R is set on a server). On a real deployment, set -R <win-ip> instead.
CANENV="CANSHIM_PROTO=tcp CANSHIM_TCPROLE=client CANSHIM_HOST=127.0.0.1 CANSHIM_PORT=20102" \
  run_case "TCP (classic)" -C s -p -I "$VCAN" -l 20102

# --- CAN FD with bit-rate switch (over UDP) ---
# probe sends 12 FD bytes with BRS; candump -L marks FD frames with '##'.
PROBE_ARGS="0x18EEFF02 extfdbrs 00 11 22 33 44 55 66 77 88 99 AA BB"
INJECT="18FF0105##100112233445566778899AABBCCDDEEFF"   # ## => FD, flags nibble 1 = BRS
TXGREP="18EEFF02##.*001122334455"
# flag >= 0x10000 (5 hex digits, nonzero lead) => an FD flag bit is set (FDF/BRS/ESI)
RXGREP="RX id=0x18FF0105 .*flag=0x[1-9a-fA-F][0-9a-fA-F]{4}"

CANENV="CANSHIM_PROTO=udp CANSHIM_HOST=127.0.0.1 CANSHIM_PORT=20104 CANSHIM_LOCALPORT=20105" \
  run_case "UDP (CAN FD + BRS)" -I "$VCAN" -R 127.0.0.1 -r 20105 -l 20104

# --- INI-based config (Windows-native): no CANSHIM_* config env at all ---
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

echo
if [ "$FAIL" = 0 ]; then echo "SELFTEST: PASS"; else echo "SELFTEST: FAIL"; fi
exit "$FAIL"
