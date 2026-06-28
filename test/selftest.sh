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
# Needs: wine, can-utils, cmake+g++ (to build cannelloni), mingw (probe),
#        a cargo-built DLL, and permission to create a vcan link.
# Skips gracefully (exit 0) when a prerequisite is missing.
#
# Env: SELFTEST_VCAN (default vcan1), SELFTEST_ARCH (32 [default] | 64 - which
#      DLL/probe arch to build and run; wine-10 runs both via wow64).
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BUILD="$ROOT/target/selftest"
CANN="$ROOT/refs/cannelloni"
VCAN="${SELFTEST_VCAN:-vcan1}"
# Architecture under test: 32 (default) or 64. wine-10 runs both (wow64), so the
# same harness exercises either DLL. SELFTEST_ARCH=64 covers the x86_64 build's
# runtime behaviour (notify struct, FFI, handle table) - kvasilloni-868.
ARCH="${SELFTEST_ARCH:-32}"
case "$ARCH" in
  32) RUST_TARGET="i686-pc-windows-gnu";   MINGW="i686-w64-mingw32-gcc" ;;
  64) RUST_TARGET="x86_64-pc-windows-gnu"; MINGW="x86_64-w64-mingw32-gcc" ;;
  *)  echo "SELFTEST: bad SELFTEST_ARCH=$ARCH (use 32 or 64)"; exit 2 ;;
esac
DLL="$ROOT/target/$RUST_TARGET/release/canlib32.dll"

skip() { echo "SELFTEST SKIP: $*"; exit 0; }
need() { command -v "$1" >/dev/null 2>&1 || skip "missing tool: $1"; }

SUDO=""
if [ "$(id -u)" != "0" ]; then
  if command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null; then SUDO="sudo"; fi
fi

need wine
need candump
need cansend
need "$MINGW"

# build the DLL if missing
if [ ! -f "$DLL" ]; then
  need cargo
  echo "== building $ARCH-bit DLL ($RUST_TARGET) =="
  ( cd "$ROOT" && cargo build --release --target "$RUST_TARGET" ) >/dev/null 2>&1 \
    || skip "cargo build failed"
fi
[ -f "$DLL" ] || skip "DLL not found: $DLL"

mkdir -p "$BUILD"
# build the probe and place the shim beside it (wine LoadLibrary checks app dir)
"$MINGW" -O2 -Wall "$ROOT/test/canshim_probe.c" -o "$BUILD/canshim_probe.exe" \
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
echo "using isolated bus: $VCAN  (DLL arch: ${ARCH}-bit)"

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
# Assert id AND the exact payload/dlc/flag (kvasilloni-im6.7), not just the id: a
# receive path delivering the WRONG bytes would otherwise still pass.
RXGREP="RX id=0x18FF0102 dlc=4 flag=0x4 data=01020304"

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
# Verify wide-payload FD RECEIVE fully (kvasilloni-im6.7): dlc=16 AND the exact
# 16-byte payload AND the FD+BRS+EXT flag bits (FDF 0x10000 | BRS 0x20000 | EXT
# 0x4 = 0x30004), not merely "some FD flag bit is set".
RXGREP="RX id=0x18FF0105 dlc=16 flag=0x30004 data=00112233445566778899AABBCCDDEEFF"

CANENV="KVASILLONI_PROTO=udp KVASILLONI_HOST=127.0.0.1 KVASILLONI_PORT=20104 KVASILLONI_LOCALPORT=20105" \
  run_case "UDP (CAN FD + BRS)" -I "$VCAN" -R 127.0.0.1 -r 20105 -l 20104

# --- classic-opened channel caps a real FD frame at 8 bytes (kvasilloni-nmt e2e) ---
# A channel opened CLASSIC (flags=0, 8-byte rbuf) must survive a real >8-byte FD
# frame on the wire: deliver at most 8 bytes, report NO FD flag bit, never crash.
# This is exactly the path the write_read_outputs cap protects (kvasilloni-im6.4);
# reverting that cap writes 16 bytes into rbuf[8] and crashes the probe.
echo; echo "===== CASE: classic channel caps a real FD frame at 8 bytes (--classic-rx, UDP) ====="
crx_out="$(mktemp)"
"$CANNBIN" -I "$VCAN" -R 127.0.0.1 -r 20151 -l 20150 >/dev/null 2>&1 & CRXNPID=$!; PIDS+=("$CRXNPID")
sleep 1.0
( cd "$BUILD" && KVASILLONI_PROTO=udp KVASILLONI_HOST=127.0.0.1 KVASILLONI_PORT=20150 KVASILLONI_LOCALPORT=20151 \
    wine ./canshim_probe.exe --classic-rx 0x18FF0160 ext ) > "$crx_out" 2>/dev/null & CRXPBPID=$!; PIDS+=("$CRXPBPID")
sleep 2.0
# A 16-byte BRS CAN FD frame to a CLASSIC-opened channel. (## => FD, nibble 1 = BRS)
cansend "$VCAN" "18FF0160##10102030405060708090A0B0C0D0E0F10" 2>/dev/null
echo "  injected 16-byte BRS FD frame 18FF0160 to a CLASSIC-opened channel"
wait "$CRXPBPID" 2>/dev/null; crx_rc=$?
kill "$CRXNPID" 2>/dev/null
sleep 0.8
# (1) the probe must exit cleanly - a stack smash from an un-capped >8-byte copy
# would crash it (nonzero exit).
if [ "$crx_rc" = 0 ]; then
  echo "  PASS: classic-rx probe exited cleanly (survived a >8-byte FD frame)"
else
  echo "  FAIL: classic-rx probe crashed/exited nonzero ($crx_rc) on a >8-byte FD frame"; sed 's/^/    /' "$crx_out"; FAIL=1
fi
# (2) the delivered frame must be capped to 8 bytes, classic flags (0x4, no FD
# bit), with the first-8 payload - reverting the cap would show dlc=16 / an FD bit.
if grep -qE "RX id=0x18FF0160 dlc=8 flag=0x4 data=0102030405060708" "$crx_out"; then
  echo "  PASS: FD frame capped to 8 bytes with classic flags and correct first-8 payload"
else
  echo "  FAIL: classic channel did not cap the FD frame (expected dlc=8 flag=0x4 data=0102030405060708)"; sed 's/^/    /' "$crx_out"; FAIL=1
fi
rm -f "$crx_out"

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
RXGREP="RX id=0x18FF0106 dlc=4 flag=0x4 data=05060708"  # id + payload (kvasilloni-im6.7)
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
if grep -qE "RX id=0x18FF0201 dlc=1 flag=0x4 data=AA" "$acc_out" && ! grep -qE "RX id=0x18FF0202" "$acc_out"; then
  echo "  PASS: only the accepted id reached canRead, with the right payload"
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

# --- multi-channel (handle table + ephemeral fallback): two channels, one app ---
# One process opens two UDP channels. The 2nd open hits the busy configured port
# and falls back to an ephemeral one (kvasilloni-iai); both must get distinct,
# usable handles (kvasilloni-j83). cannelloni replies to the configured port, so
# the first channel receives the injected frame.
echo; echo "===== CASE: multi-channel (--multi, UDP) ====="
multi_cap="$(mktemp)"; multi_out="$(mktemp)"
candump -L "$VCAN" > "$multi_cap" 2>/dev/null & MCDPID=$!; PIDS+=("$MCDPID")
"$CANNBIN" -I "$VCAN" -R 127.0.0.1 -r 20113 -l 20112 >/dev/null 2>&1 & MCNPID=$!; PIDS+=("$MCNPID")
sleep 1.0
# The two channels share one localport, so the 2nd open needs the ephemeral
# fallback, which is now opt-in (KVASILLONI_UDP_PORT_FALLBACK=1; kvasilloni-25q).
( cd "$BUILD" && KVASILLONI_PROTO=udp KVASILLONI_HOST=127.0.0.1 KVASILLONI_PORT=20112 KVASILLONI_LOCALPORT=20113 \
    KVASILLONI_UDP_PORT_FALLBACK=1 \
    wine ./canshim_probe.exe --multi 0x18EEFF10 0x18EEFF11 ) > "$multi_out" 2>/dev/null & MPBPID=$!; PIDS+=("$MPBPID")
sleep 2.0
cansend "$VCAN" "18FF0113#CD" 2>/dev/null
echo "  injected: 18FF0113 (to first channel)"
wait "$MPBPID" 2>/dev/null
sleep 0.4
kill "$MCNPID" "$MCDPID" 2>/dev/null
sleep 0.8
if grep -qE "MULTI .*distinct=1" "$multi_out"; then
  echo "  PASS: two channels opened with distinct handles"
else
  echo "  FAIL: distinct handles not reported"; sed 's/^/    /' "$multi_out"; FAIL=1
fi
if grep -qiE "18EEFF10" "$multi_cap" && grep -qiE "18EEFF11" "$multi_cap"; then
  echo "  PASS: TX from both channels seen on $VCAN"
else
  echo "  FAIL: both channels' TX not seen on $VCAN"
  echo "  --- candump ---"; sed 's/^/    /' "$multi_cap"
  echo "  --- probe ---"; sed 's/^/    /' "$multi_out"; FAIL=1
fi
if grep -qE "RXa id=0x18FF0113 dlc=1 data=CD" "$multi_out"; then
  echo "  PASS: first channel still receives injected frame (payload verified)"
else
  echo "  FAIL: first channel did not receive injected frame"; sed 's/^/    /' "$multi_out"; FAIL=1
fi
rm -f "$multi_cap" "$multi_out"

# --- close from inside a notify callback (cqe): the callback runs on the RX
# thread and calls canClose; it must not self-join/deadlock, and it must observe
# the right info.rx.id. closed=0 in the output means the close deadlocked.
echo; echo "===== CASE: close from notify callback (--notify-close, UDP) ====="
nc_out="$(mktemp)"
"$CANNBIN" -I "$VCAN" -R 127.0.0.1 -r 20115 -l 20114 >/dev/null 2>&1 & NCNPID=$!; PIDS+=("$NCNPID")
sleep 1.0
( cd "$BUILD" && KVASILLONI_PROTO=udp KVASILLONI_HOST=127.0.0.1 KVASILLONI_PORT=20114 KVASILLONI_LOCALPORT=20115 \
    wine ./canshim_probe.exe --notify-close ) > "$nc_out" 2>/dev/null & NCPBPID=$!; PIDS+=("$NCPBPID")
sleep 2.0
cansend "$VCAN" "18FF0114#7A" 2>/dev/null
echo "  injected: 18FF0114 (callback calls canClose)"
wait "$NCPBPID" 2>/dev/null
kill "$NCNPID" 2>/dev/null
sleep 0.8
if grep -qE "NOTIFYCLOSE id=0x18FF0114 closed=1" "$nc_out"; then
  echo "  PASS: canClose from the notify callback returned (no self-join), id matched"
else
  echo "  FAIL: close-from-callback deadlocked (closed=0) or wrong id"; sed 's/^/    /' "$nc_out"; FAIL=1
fi
rm -f "$nc_out"

# --- RX thread survives malformed input (kkt): a datagram claiming len=100 (which
# would have panicked the pre-fix decoder) must be dropped, and a following valid
# frame must still be delivered - proving the RX thread did not die.
echo; echo "===== CASE: RX survives malformed UDP (kkt e2e) ====="
if command -v python3 >/dev/null 2>&1; then
  mal_out="$(mktemp)"
  "$CANNBIN" -I "$VCAN" -R 127.0.0.1 -r 20117 -l 20116 >/dev/null 2>&1 & MLNPID=$!; PIDS+=("$MLNPID")
  sleep 1.0
  ( cd "$BUILD" && KVASILLONI_PROTO=udp KVASILLONI_HOST=127.0.0.1 KVASILLONI_PORT=20116 KVASILLONI_LOCALPORT=20117 \
      wine ./canshim_probe.exe 0x18EEFF20 ext DE AD ) > "$mal_out" 2>/dev/null & MLPBPID=$!; PIDS+=("$MLPBPID")
  sleep 2.0
  # malformed cannelloni UDP straight to the shim RX port: count=1, classic frame,
  # len=100 (>64) + 100 data bytes (passes the buffer bound, hits the len guard).
  python3 -c "import socket;s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);s.sendto(bytes([2,0,0,0,1,0,0,1,0x23,100])+b'\xAB'*100,('127.0.0.1',20117))" 2>/dev/null
  echo "  injected malformed UDP (len=100) to shim RX port 20117"
  sleep 0.3
  cansend "$VCAN" "18FF0117#0102" 2>/dev/null
  echo "  injected valid 18FF0117"
  wait "$MLPBPID" 2>/dev/null
  kill "$MLNPID" 2>/dev/null
  sleep 0.8
  if grep -qE "RX id=0x18FF0117 dlc=2 flag=0x4 data=0102" "$mal_out"; then
    echo "  PASS: RX thread survived the malformed datagram and delivered the valid frame (payload verified)"
  else
    echo "  FAIL: valid frame not received after malformed input (RX thread may have died)"; sed 's/^/    /' "$mal_out"; FAIL=1
  fi
  rm -f "$mal_out"
else
  echo "  SKIP: python3 not available for raw UDP injection"
fi

# --- peer-IP check (kvasilloni-872): with the allow-list set to a bogus, non-
# loopback IP, cannelloni's 127.0.0.1 datagrams must be DROPPED before canRead;
# disabling the check (KVASILLONI_PEER_CHECK=off) must let the same frame through.
# This isolates the filter as the cause: on => no RX, off => RX.
echo; echo "===== CASE: peer-IP check drops non-allowed source (UDP) ====="
pc_out1="$(mktemp)"; pc_out2="$(mktemp)"
"$CANNBIN" -I "$VCAN" -R 127.0.0.1 -r 20119 -l 20118 >/dev/null 2>&1 & PCNPID=$!; PIDS+=("$PCNPID")
sleep 1.0
( cd "$BUILD" && KVASILLONI_PROTO=udp KVASILLONI_HOST=127.0.0.1 KVASILLONI_PORT=20118 KVASILLONI_LOCALPORT=20119 \
    KVASILLONI_ALLOW=10.255.255.254 \
    wine ./canshim_probe.exe 0x18EEFF50 ext DE AD ) > "$pc_out1" 2>/dev/null & PCPB1=$!; PIDS+=("$PCPB1")
sleep 2.0
cansend "$VCAN" "18FF0150#11" 2>/dev/null
echo "  injected 18FF0150 with allow=10.255.255.254, peer check ON (should drop)"
wait "$PCPB1" 2>/dev/null
sleep 0.5
( cd "$BUILD" && KVASILLONI_PROTO=udp KVASILLONI_HOST=127.0.0.1 KVASILLONI_PORT=20118 KVASILLONI_LOCALPORT=20119 \
    KVASILLONI_ALLOW=10.255.255.254 KVASILLONI_PEER_CHECK=off \
    wine ./canshim_probe.exe 0x18EEFF50 ext DE AD ) > "$pc_out2" 2>/dev/null & PCPB2=$!; PIDS+=("$PCPB2")
sleep 2.0
cansend "$VCAN" "18FF0150#11" 2>/dev/null
echo "  injected 18FF0150 with peer check OFF (should deliver)"
wait "$PCPB2" 2>/dev/null
kill "$PCNPID" 2>/dev/null
sleep 0.8
if ! grep -qE "RX id=0x18FF0150" "$pc_out1" && grep -qE "RX id=0x18FF0150 dlc=1 flag=0x4 data=11" "$pc_out2"; then
  echo "  PASS: non-allowed source dropped with peer check on, delivered (payload verified) with it off"
else
  echo "  FAIL: peer check wrong (on must drop, off must deliver)"
  echo "  --- check on ---"; sed 's/^/    /' "$pc_out1"
  echo "  --- check off ---"; sed 's/^/    /' "$pc_out2"; FAIL=1
fi
rm -f "$pc_out1" "$pc_out2"

# ===================== timeout + role coverage (epic kvasilloni-nzp) =====================
# Helpers to pull the two fields out of the probe's "TIMEDOPEN ms=.. h=.." line.
parse_ms() { sed -n 's/.*TIMEDOPEN ms=\([0-9]*\).*/\1/p' "$1"; }
parse_h()  { sed -n 's/.*TIMEDOPEN .*h=\(-\{0,1\}[0-9]*\).*/\1/p' "$1"; }

# --- connect/handshake timeout fast-fail (kvasilloni-7yl) ---
# A TCP client open against a peer that accepts but never sends the CANNELLONIv1
# banner blocks in the handshake read, which is bounded by connecttimeout. Assert
# the open fails in ~connecttimeout (1.5s), not the 5s default. Needs python3 for
# a silent listener (accept + hold, send nothing).
echo; echo "===== CASE: TCP connect/handshake timeout fast-fail (kvasilloni-7yl) ====="
if command -v python3 >/dev/null 2>&1; then
  ct_out="$(mktemp)"
  python3 - 20140 >/dev/null 2>&1 <<'PY' & SILENT=$!; PIDS+=("$SILENT")
import socket, sys, time
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", int(sys.argv[1]))); s.listen(4)
conns, end = [], time.time() + 20
while time.time() < end:
    s.settimeout(1.0)
    try:
        c, _ = s.accept(); conns.append(c)   # accept but never send the banner
    except socket.timeout:
        pass
PY
  sleep 0.7
  ( cd "$BUILD" && KVASILLONI_PROTO=tcp KVASILLONI_TCPROLE=client KVASILLONI_HOST=127.0.0.1 \
      KVASILLONI_PORT=20140 KVASILLONI_CONNECT_TIMEOUT=1500 \
      timeout 20 wine ./canshim_probe.exe --timed-open ) > "$ct_out" 2>/dev/null
  kill "$SILENT" 2>/dev/null
  ms="$(parse_ms "$ct_out")"; h="$(parse_h "$ct_out")"
  echo "  open: ms=${ms:-?} h=${h:-?} (connecttimeout=1500, default would be 5000)"
  if [ -n "$ms" ] && [ "$ms" -ge 1000 ] && [ "$ms" -le 4000 ] && [ -n "$h" ] && [ "$h" -lt 0 ]; then
    echo "  PASS: client open fast-failed in ~connecttimeout (well under the 5s default)"
  else
    echo "  FAIL: connect timeout not honored"; sed 's/^/    /' "$ct_out"; FAIL=1
  fi
  rm -f "$ct_out"
else
  echo "  SKIP: python3 not available for the silent TCP listener"
fi

# --- TCP server role: shim listens, cannelloni connects as client (kvasilloni-6ot) ---
# The probe must be up and blocked in accept() BEFORE cannelloni dials in, so we
# start it first (unlike run_case). Both data directions are then exercised.
echo; echo "===== CASE: TCP server role (shim listens, cannelloni client) ====="
srv_cap="$(mktemp)"; srv_out="$(mktemp)"
candump -L "$VCAN" > "$srv_cap" 2>/dev/null & SRVCD=$!; PIDS+=("$SRVCD")
( cd "$BUILD" && KVASILLONI_PROTO=tcp KVASILLONI_TCPROLE=server KVASILLONI_LOCALPORT=20142 \
    KVASILLONI_ACCEPT_TIMEOUT=8000 \
    wine ./canshim_probe.exe 0x18EEFF40 ext 5A 5B ) > "$srv_out" 2>/dev/null & SRVPB=$!; PIDS+=("$SRVPB")
sleep 1.5   # let the probe bind its listener and enter accept()
"$CANNBIN" -C c -R 127.0.0.1 -r 20142 -p -I "$VCAN" >/dev/null 2>&1 & SRVCN=$!; PIDS+=("$SRVCN")
sleep 2.0
cansend "$VCAN" "18FF0140#42" 2>/dev/null
echo "  injected: 18FF0140 (to shim via cannelloni client)"
wait "$SRVPB" 2>/dev/null
sleep 0.4
kill "$SRVCN" "$SRVCD" 2>/dev/null
sleep 0.8
if grep -qiE "18EEFF40" "$srv_cap"; then
  echo "  PASS: probe TX (server) reached $VCAN via the cannelloni client"
else
  echo "  FAIL: probe TX not seen on $VCAN"; echo "  --- candump ---"; sed 's/^/    /' "$srv_cap"
  echo "  --- probe ---"; sed 's/^/    /' "$srv_out"; FAIL=1
fi
if grep -qE "RX id=0x18FF0140 dlc=1 flag=0x4 data=42" "$srv_out"; then
  echo "  PASS: injected frame delivered to probe canRead (server RX, payload verified)"
else
  echo "  FAIL: probe did not receive injected frame (server RX)"; sed 's/^/    /' "$srv_out"; FAIL=1
fi
rm -f "$srv_cap" "$srv_out"

# --- TCP server one-to-one: client drop surfaces BUS_OFF (kvasilloni-5gh) ---
# In server mode the shim accepts a single client. When that client (cannelloni)
# drops, a read-only consumer must learn the link died via canReadStatus
# (canSTAT_BUS_OFF), not sit on a silently quiet bus.
echo; echo "===== CASE: TCP server client-drop surfaces BUS_OFF (--server-drop) ====="
sd_out="$(mktemp)"
( cd "$BUILD" && KVASILLONI_PROTO=tcp KVASILLONI_TCPROLE=server KVASILLONI_LOCALPORT=20146 \
    KVASILLONI_ACCEPT_TIMEOUT=8000 \
    timeout 40 wine ./canshim_probe.exe --server-drop ) > "$sd_out" 2>/dev/null & SDPB=$!; PIDS+=("$SDPB")
sleep 1.5   # let the probe bind its listener and enter accept()
"$CANNBIN" -C c -R 127.0.0.1 -r 20146 -p -I "$VCAN" >/dev/null 2>&1 & SDCN=$!; PIDS+=("$SDCN")
sleep 2.0   # let cannelloni connect + handshake
cansend "$VCAN" "18FF0540#77" 2>/dev/null
echo "  injected: 18FF0540 (to shim via cannelloni client)"
sleep 1.5   # let the probe receive it (link up) before we drop the peer
kill "$SDCN" 2>/dev/null   # drop the one client
echo "  killed cannelloni client (peer drop)"
wait "$SDPB" 2>/dev/null
if grep -qE "PROBE: link-up rx=1" "$sd_out" && grep -qE "PROBE: BUSOFF=1" "$sd_out"; then
  echo "  PASS: reader saw BUS_OFF after the client dropped (link-down is observable)"
else
  echo "  FAIL: client drop not surfaced via canReadStatus"; sed 's/^/    /' "$sd_out"; FAIL=1
fi
rm -f "$sd_out"

# --- TCP server accept timeout with no client (kvasilloni-6ot) ---
# A server open with no client must time out in ~accepttimeout and fail, not hang.
echo; echo "===== CASE: TCP server accept timeout (no client) ====="
to_out="$(mktemp)"
( cd "$BUILD" && KVASILLONI_PROTO=tcp KVASILLONI_TCPROLE=server KVASILLONI_LOCALPORT=20144 \
    KVASILLONI_ACCEPT_TIMEOUT=1500 \
    timeout 20 wine ./canshim_probe.exe --timed-open ) > "$to_out" 2>/dev/null
ms="$(parse_ms "$to_out")"; h="$(parse_h "$to_out")"
echo "  open: ms=${ms:-?} h=${h:-?} (accepttimeout=1500)"
if [ -n "$ms" ] && [ "$ms" -ge 1000 ] && [ "$ms" -le 4000 ] && [ -n "$h" ] && [ "$h" -lt 0 ]; then
  echo "  PASS: server open with no client failed in ~accepttimeout"
else
  echo "  FAIL: accept timeout not honored"; sed 's/^/    /' "$to_out"; FAIL=1
fi
rm -f "$to_out"

# --- per-channel RX isolation, two endpoints (kvasilloni-pwx) ---
# Two cannelloni instances on two isolated vcans, one per shim channel. A frame
# injected on $VCAN must reach ONLY channel A's canRead; one on $VCAN2 ONLY
# channel B's - proving the handle table keeps per-channel RX queues isolated.
echo; echo "===== CASE: per-channel RX isolation (--multi-rx, two endpoints) ====="
VCAN2="${SELFTEST_VCAN2:-vcan2}"
pwx_ok=1
if ! ip link show "$VCAN2" >/dev/null 2>&1; then
  $SUDO modprobe vcan 2>/dev/null
  $SUDO ip link add dev "$VCAN2" type vcan 2>/dev/null || pwx_ok=0
fi
$SUDO ip link set "$VCAN2" mtu 72 2>/dev/null || true
$SUDO ip link set up "$VCAN2" 2>/dev/null || pwx_ok=0
if [ "$pwx_ok" = 1 ]; then
  mrx_out="$(mktemp)"
  # endpoint A on $VCAN: listens 20120 (shim TX), sends RX to 20121 (channel A)
  "$CANNBIN" -I "$VCAN"  -R 127.0.0.1 -r 20121 -l 20120 >/dev/null 2>&1 & MRXA=$!; PIDS+=("$MRXA")
  # endpoint B on $VCAN2: listens 20122 (shim TX), sends RX to 20123 (channel B)
  "$CANNBIN" -I "$VCAN2" -R 127.0.0.1 -r 20123 -l 20122 >/dev/null 2>&1 & MRXB=$!; PIDS+=("$MRXB")
  sleep 1.0
  ( cd "$BUILD" && wine ./canshim_probe.exe --multi-rx 20120 20121 20122 20123 ) \
      > "$mrx_out" 2>/dev/null & MRXPB=$!; PIDS+=("$MRXPB")
  sleep 2.0
  cansend "$VCAN"  "18FF01A0#A1" 2>/dev/null   # -> endpoint A -> channel A only
  cansend "$VCAN2" "18FF01B0#B1" 2>/dev/null   # -> endpoint B -> channel B only
  echo "  injected: 18FF01A0 on $VCAN (chan A), 18FF01B0 on $VCAN2 (chan B)"
  wait "$MRXPB" 2>/dev/null
  kill "$MRXA" "$MRXB" 2>/dev/null
  sleep 0.8
  if grep -qE "MULTIRX .*distinct=1" "$mrx_out"; then
    echo "  PASS: two channels opened with distinct handles"
  else
    echo "  FAIL: distinct handles not reported"; sed 's/^/    /' "$mrx_out"; FAIL=1
  fi
  if grep -qE "RXa id=0x18FF01A0 dlc=1 data=A1" "$mrx_out" && ! grep -qE "RXa id=0x18FF01B0" "$mrx_out"; then
    echo "  PASS: channel A received only its own frame, payload verified (no leak from B)"
  else
    echo "  FAIL: channel A RX isolation broken"; sed 's/^/    /' "$mrx_out"; FAIL=1
  fi
  if grep -qE "RXb id=0x18FF01B0 dlc=1 data=B1" "$mrx_out" && ! grep -qE "RXb id=0x18FF01A0" "$mrx_out"; then
    echo "  PASS: channel B received only its own frame, payload verified (no leak from A)"
  else
    echo "  FAIL: channel B RX isolation broken"; sed 's/^/    /' "$mrx_out"; FAIL=1
  fi
  rm -f "$mrx_out"
else
  echo "  SKIP: could not create a second vcan ($VCAN2) for the two-endpoint test"
fi

# --- concurrency / stress, real DLL (kvasilloni-eoq) ---
# Hammer the shim from several wine threads at once (rapid open/close + write/read)
# over UDP with no cannelloni. `timeout` bounds a hang so a deadlock fails the test
# rather than stalling the suite. The host-side `cargo test` proves no handle leak;
# this proves the shipped DLL survives the real Windows threading model. Many
# channels share one localport, so opt into the ephemeral fallback (kvasilloni-25q).
echo; echo "===== CASE: concurrency stress, real DLL (--stress) ====="
st_out="$(mktemp)"
( cd "$BUILD" && KVASILLONI_PROTO=udp KVASILLONI_HOST=127.0.0.1 KVASILLONI_PORT=20130 KVASILLONI_LOCALPORT=20131 \
    KVASILLONI_UDP_PORT_FALLBACK=1 \
    timeout 40 wine ./canshim_probe.exe --stress 4 4 ) > "$st_out" 2>/dev/null
st_rc=$?
if [ "$st_rc" = 124 ]; then
  echo "  FAIL: stress run timed out (possible deadlock)"; sed 's/^/    /' "$st_out"; FAIL=1
elif grep -qE "STRESS .*ok=1" "$st_out"; then
  echo "  PASS: concurrent open/write/read/close from 4 threads completed cleanly"
else
  echo "  FAIL: stress run did not report ok"; sed 's/^/    /' "$st_out"; FAIL=1
fi
rm -f "$st_out"

echo
if [ "$FAIL" = 0 ]; then echo "SELFTEST: PASS"; else echo "SELFTEST: FAIL"; fi
exit "$FAIL"
