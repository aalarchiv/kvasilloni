# kvasilloni

A drop-in replacement for Kvaser's **`canlib32.dll`** that bridges a Windows CAN
application to a **Linux `vcan`** over the network using
[cannelloni](https://github.com/mguentner/cannelloni) — no Kvaser hardware, no
kernel driver.

The intended use case: CAN software running in a **Windows 11 VM** (that drives
the Kvaser CANlib API) needs to talk to CAN software bound to a **virtual CAN bus
on a separate Linux host**.

```
 Windows 11 VM                                       Linux host
┌───────────────────────────┐                      ┌───────────────────────────┐
│ CAN application           │      cannelloni      │ cannelloni -I vcan0 ...   │
│  canlib32.dll  (SHIM)     ┼────── UDP/TCP ───────┼ vcan0  <->  your CAN code │
└───────────────────────────┘  (LAN / host link)   └───────────────────────────┘
```

Instead of touching hardware, the shim **is itself a cannelloni peer**: it speaks
cannelloni's wire format directly to a stock `cannelloni` process on the Linux
side. Nothing else is needed on Linux beyond `cannelloni` and a `vcan`.

## Quickstart

```bash
# === BUILD (dev host) ===
rustup target add i686-pc-windows-gnu x86_64-pc-windows-gnu
make                       # -> target/{i686,x86_64}-pc-windows-gnu/release/canlib32.dll
```

```bash
# === LINUX HOST (UDP) ===  <win-ip> = Windows VM address
sudo modprobe vcan
sudo ip link add dev vcan0 type vcan
sudo ip link set up vcan0
cannelloni -I vcan0 -R <win-ip> -r 20000 -l 20000
```

```ini
; === WINDOWS VM ===  put canlib32.dll next to the app .exe, plus kvasilloni.ini:
[cannelloni]
host      = <linux-ip>
port      = 20000
localport = 20000
proto     = udp
```

```bat
:: launch the app (Interface Type = Kvaser). Optional one-off override + log:
set KVASILLONI_HOST=<linux-ip>
set KVASILLONI_LOG=C:\temp\kvasilloni.log
your-can-app.exe
```

```bash
# === TCP instead of UDP ===
# Linux:   cannelloni -C s -R <win-ip> -I vcan0 -l 20000
# Windows: kvasilloni.ini -> proto = tcp   (tcprole = client)
```

```bash
# === VERIFY (dev host; needs wine, can-utils, cmake) ===
make verify                # all exports present + undecorated
make test                  # wire-codec + ring/notify/export unit tests
make selftest              # full loopback over vcan1: classic UDP/TCP, CAN FD, INI,
                           #   enumeration, acceptance filtering, notify callbacks
```

## Scope

The shim implements the **13 core CANlib functions** the target application
resolves (matching the reference DLL's export table):

```
canInitializeLibrary  canOpenChannel   canSetBusParams  canBusOn   canBusOff
canWrite              canRead          canReadStatus    canReadErrorCounters
canGetBusStatistics  canGetErrorText  canGetVersion    canClose
```

Classic CAN frames (11-bit and 29-bit IDs, DLC 0–8, RTR) and **CAN FD**
(`canFDMSG_FDF`/`BRS`/`ESI`, payloads up to 64 bytes, DLC auto-rounded to a valid
FD length) are supported in both directions.

### Extended exports (for retargeting to other apps)

A further set of CANlib functions is implemented so the shim can stand in for
apps with a wider import table (run the coverage check in `AGENTS.md` before
targeting a new app):

| Function(s) | Behavior |
|---|---|
| `canFlushReceiveQueue` / `canFlushTransmitQueue` | clears the RX ring / no-op (TX is synchronous) |
| `canSetBusOutputControl` / `canGetBusOutputControl` | stores & returns the driver type (default `canDRIVER_NORMAL`) |
| `canReadWait` / `canReadSync` | blocking read / wait-for-available, honoring the ms timeout (`canWAIT_INFINITE` ≈ unbounded) |
| `canWriteWait` / `canWriteSync` | send synchronously, ignoring the timeout |
| `canIoCtl` | dispatches `canIOCTL_FLUSH_RX/TX_BUFFER`, `GET/SET_TIMER_SCALE`, `SET_TXACK/TXRQ`, `GET_RX/TX_BUFFER_LEVEL`; unknown funcs return `canERR_NOT_IMPLEMENTED` |
| `canAccept` | real acceptance filtering: drops frames where `(id & mask) != (code & mask)` (separate STD/EXT code+mask; zero mask = accept all) |
| `canObjBufSetFilter` | benign no-op (object buffers are a distinct mechanism) |
| `canGetNumberOfChannels` | returns the configured channel count (`KVASILLONI_CHANNELS` / ini `channels`, default 1) |
| `canGetChannelData` | synthetic values for `CHANNEL_NAME` ("kvasilloni vcan*N*"), `DEVDESCR_ASCII`, `MFGNAME_ASCII`, `CARD_SERIAL_NO`, `CHANNEL_CAP/FLAGS`, `CARD_TYPE/NUMBER`, `CHAN_NO_ON_CARD` |
| `canSetNotify` | event-driven RX callbacks (`canNOTIFY_RX`) |

**`canSetNotify` threading caveat:** the registered callback is invoked **on the
shim's RX thread**, not the thread that called `canSetNotify`. Keep it short and
non-blocking, and ensure the function pointer stays valid until you disarm it
(`canSetNotify(h, NULL, …)`) or `canClose`. Only `canNOTIFY_RX` is delivered.

## Build

Requires the Rust toolchain plus the mingw-w64 linkers
(`i686-w64-mingw32-gcc`, `x86_64-w64-mingw32-gcc`).

```bash
rustup target add i686-pc-windows-gnu x86_64-pc-windows-gnu

make            # -> target/{i686,x86_64}-pc-windows-gnu/release/canlib32.dll
make verify     # confirm all exports are present and undecorated (32-bit)
make test       # host unit tests for the wire codec (golden vectors + round-trips)
```

Build a single bitness with `make dll32` or `make dll64`. Pick the DLL that
matches your Windows application's bitness (most legacy Kvaser apps are 32-bit).

## Deploy (Windows side)

1. Drop the matching `canlib32.dll` **next to the application's executable**
   (Windows resolves `LoadLibrary("canlib32.dll")` from the app directory first;
   back up the genuine Kvaser DLL if one is present).
2. Copy `kvasilloni.ini.example` to **`kvasilloni.ini`**, edit it to point at the
   Linux host, and place it next to the DLL (or next to the .exe).
3. Start the app and select **Interface Type = Kvaser** (or however it opens a
   Kvaser channel).

### Configuration

The shim is configured by an **INI file** — the Windows-native mechanism. It looks
for `kvasilloni.ini` next to the DLL, then next to the application's .exe
(`KVASILLONI_INI` may give an explicit path). See `kvasilloni.ini.example`:

```ini
[cannelloni]
host      = 192.168.1.50   ; Linux host running cannelloni
port      = 20000          ; remote port the shim sends to (cannelloni's -l)
localport = 20000          ; local UDP bind / TCP server port (cannelloni's -r)
proto     = udp            ; udp | tcp
tcprole   = client         ; client | server  (tcp only)
; log     = C:\temp\kvasilloni.log
```

Every setting can also be overridden by an **environment variable**, which takes
precedence over the INI (handy for scripting/CI). Precedence is
**defaults → INI → environment**.

| Variable            | INI key     | Default     | Meaning                              |
|---------------------|-------------|-------------|--------------------------------------|
| `KVASILLONI_HOST`      | `host`      | `127.0.0.1` | Linux host running cannelloni        |
| `KVASILLONI_PORT`      | `port`      | `20000`     | Remote port the shim sends to        |
| `KVASILLONI_LOCALPORT` | `localport` | `20000`     | Local UDP bind / TCP server port     |
| `KVASILLONI_PROTO`     | `proto`     | `udp`       | `udp` or `tcp`                       |
| `KVASILLONI_TCPROLE`   | `tcprole`   | `client`    | `client` or `server` (TCP only)      |
| `KVASILLONI_LOG`       | `log`       | (unset)     | If set, append a debug log here      |
| `KVASILLONI_INI`       | —           | (auto)      | Explicit path to the INI file        |

## Linux side (cannelloni)

Bring up a virtual CAN bus and run cannelloni so its `vcan` mirrors the shim:

```bash
sudo modprobe vcan
sudo ip link add dev vcan0 type vcan
sudo ip link set up vcan0
```

**UDP** (default; symmetric — each side binds its local port and sends to the
remote). Replace `<win-ip>` with the Windows VM's address:

```bash
cannelloni -I vcan0 -R <win-ip> -r <KVASILLONI_LOCALPORT> -l <KVASILLONI_PORT>
# matching shim env: KVASILLONI_PROTO=udp KVASILLONI_HOST=<linux-ip>
#                    KVASILLONI_PORT=<l-port> KVASILLONI_LOCALPORT=<r-port>
```

**TCP** with the shim as client (recommended TCP setup):

```bash
cannelloni -C s -R <win-ip> -I vcan0 -l <KVASILLONI_PORT>
# matching shim env: KVASILLONI_PROTO=tcp KVASILLONI_TCPROLE=client
#                    KVASILLONI_HOST=<linux-ip> KVASILLONI_PORT=<port>
```

> cannelloni's TCP/UDP server checks the peer IP against `-R` by default. Set
> `-R <win-ip>` (as above) or pass `-p` to disable the check, or the connection
> is rejected.

UDP is cannelloni's native/default mode and is simplest. TCP gives reliable,
ordered delivery (better when packet loss would corrupt multi-frame NMEA 2000
transport-protocol messages).

## Verify end-to-end

`make selftest` runs a full loopback on this Linux host using an **isolated
`vcan1`** (so it never disturbs anything on `vcan0`). It builds cannelloni,
launches a small probe under **wine** that loads the shim, and asserts that frames
cross **both directions** for four scenarios:

```
make selftest
# CASE: UDP (classic)        PASS (TX + RX)
# CASE: TCP (classic)        PASS (TX + RX)
# CASE: UDP (CAN FD + BRS)   PASS (TX + RX)
# CASE: UDP (INI config, no env)  PASS (TX + RX)
# SELFTEST: PASS
```

(Needs `wine`, `can-utils`, `cmake`/`g++`, and permission to create a vcan link.)

## How it works

- **`src/wire.rs`** — the cannelloni codec: per-frame `encode`/`decode`, the
  UDP packet builder/parser, the TCP streaming decoder state machine, and the
  Kvaser↔SocketCAN ID/flag translation. An independent implementation of the
  cannelloni wire protocol, unit-tested against golden byte vectors.
- **`src/transport.rs`** — UDP and TCP (client/server) transports with a
  background RX thread feeding a bounded ring; `canWrite` sends one frame,
  `canRead` drains the ring (`canERR_NOMSG` when empty).
- **`src/config.rs`** — layered config (defaults → `kvasilloni.ini` → env). Finds
  the INI next to the DLL or the .exe via `GetModuleFileNameW`.
- **`src/lib.rs`** — the `extern "system"` exports (13 core + the extended
  retargeting set). Each wraps its body in `catch_unwind` so a stray panic
  becomes a CANlib error code, never an unwind across the FFI boundary.

### Wire format (cannelloni, for reference)

Per frame: `can_id` (4 bytes, big-endian, SocketCAN flag bits in the top —
`EFF 0x80000000`, `RTR 0x40000000`) · `len` (1 byte; `0x80` ⇒ CAN-FD) · `flags`
(1 byte, only if CAN-FD) · `data[len]` (omitted for RTR).
**UDP** prefixes a 5-byte header `{ version=2, op=DATA(0), seq, count(BE u16) }`
and packs `count` frames. **TCP** opens with both peers exchanging the ASCII
string `CANNELLONIv1`, then streams frames back-to-back with no packet header.

## License

Licensed under the **GNU Lesser General Public License v3.0 or later**
(LGPL-3.0-or-later). See [`COPYING.LESSER`](COPYING.LESSER) (which extends the
GPLv3 in [`COPYING`](COPYING)).

The shim contains no Kvaser or cannelloni source: it is an independent
implementation that emulates the CANlib export interface and speaks the
cannelloni wire protocol over the network to interoperate with a stock,
separately-running `cannelloni` process. LGPL was chosen because the DLL is
designed to be loaded by other applications (including proprietary ones) as a
drop-in replacement — the library itself stays open and user-replaceable, while
the programs that load it are unaffected.
