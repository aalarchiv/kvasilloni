# kvasilloni

A drop-in replacement for Kvaser's **`canlib32.dll`** that bridges a Windows CAN
application to a **Linux `vcan`** over the network using
[cannelloni](https://github.com/mguentner/cannelloni) — no Kvaser hardware, no
kernel driver.

The intended use case: CAN software running in a **Windows 11 VM** (that drives
the Kvaser CANlib API) needs to talk to CAN software bound to a **virtual CAN bus
on a separate Linux host**.

```
 Windows 11 VM                                    Linux host
┌──────────────────────────┐                    ┌───────────────────────────┐
│ CAN application           │   cannelloni       │ cannelloni -I vcan0 ...    │
│  └ canlib32.dll  (SHIM) ──┼── UDP or TCP ──────┼─▶ vcan0 ◀─▶ your CAN code  │
└──────────────────────────┘   (LAN / host link) └───────────────────────────┘
```

Instead of touching hardware, the shim **is itself a cannelloni peer**: it speaks
cannelloni's wire format directly to a stock `cannelloni` process on the Linux
side. Nothing else is needed on Linux beyond `cannelloni` and a `vcan`.

## Scope

The shim implements exactly the **13 CANlib functions** the target application
resolves (matching the reference DLL's export table):

```
canInitializeLibrary  canOpenChannel   canSetBusParams  canBusOn   canBusOff
canWrite              canRead          canReadStatus    canReadErrorCounters
canGetBusStatistics  canGetErrorText  canGetVersion    canClose
```

Classic CAN frames (11-bit and 29-bit IDs, DLC 0–8, RTR) and **CAN FD**
(`canFDMSG_FDF`/`BRS`/`ESI`, payloads up to 64 bytes, DLC auto-rounded to a valid
FD length) are supported in both directions.

## Build

Requires the Rust toolchain plus the mingw-w64 linkers
(`i686-w64-mingw32-gcc`, `x86_64-w64-mingw32-gcc`).

```bash
rustup target add i686-pc-windows-gnu x86_64-pc-windows-gnu

make            # -> target/{i686,x86_64}-pc-windows-gnu/release/canlib32.dll
make verify     # confirm all 13 exports are present and undecorated (32-bit)
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
(`CANSHIM_INI` may give an explicit path). See `kvasilloni.ini.example`:

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
| `CANSHIM_HOST`      | `host`      | `127.0.0.1` | Linux host running cannelloni        |
| `CANSHIM_PORT`      | `port`      | `20000`     | Remote port the shim sends to        |
| `CANSHIM_LOCALPORT` | `localport` | `20000`     | Local UDP bind / TCP server port     |
| `CANSHIM_PROTO`     | `proto`     | `udp`       | `udp` or `tcp`                       |
| `CANSHIM_TCPROLE`   | `tcprole`   | `client`    | `client` or `server` (TCP only)      |
| `CANSHIM_LOG`       | `log`       | (unset)     | If set, append a debug log here      |
| `CANSHIM_INI`       | —           | (auto)      | Explicit path to the INI file        |

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
cannelloni -I vcan0 -R <win-ip> -r <CANSHIM_LOCALPORT> -l <CANSHIM_PORT>
# matching shim env: CANSHIM_PROTO=udp CANSHIM_HOST=<linux-ip>
#                    CANSHIM_PORT=<l-port> CANSHIM_LOCALPORT=<r-port>
```

**TCP** with the shim as client (recommended TCP setup):

```bash
cannelloni -C s -R <win-ip> -I vcan0 -l <CANSHIM_PORT>
# matching shim env: CANSHIM_PROTO=tcp CANSHIM_TCPROLE=client
#                    CANSHIM_HOST=<linux-ip> CANSHIM_PORT=<port>
```

> cannelloni's TCP/UDP server checks the peer IP against `-R` by default. Set
> `-R <win-ip>` (as above) or pass `-p` to disable the check, or the connection
> is rejected.

UDP is cannelloni's native/default mode and is simplest. TCP gives reliable,
ordered delivery (better when packet loss would corrupt multi-frame NMEA 2000
transport-protocol messages).

## Verify end-to-end

`make selftest` runs a full loopback on this Linux host using an **isolated
`vcan1`** (so it never disturbs anything on `vcan0`). It builds cannelloni, runs
it for both UDP and TCP, launches a small probe under **wine** that loads the
shim, and asserts that frames cross **both directions** over **both transports**:

```
make selftest
# ... CASE: UDP / CASE: TCP ...
# SELFTEST: PASS
```

(Needs `wine`, `can-utils`, `cmake`/`g++`, and permission to create a vcan link.)

## How it works

- **`src/wire.rs`** — the cannelloni codec: per-frame `encode`/`decode`, the
  UDP packet builder/parser, the TCP streaming decoder state machine, and the
  Kvaser↔SocketCAN ID/flag translation. Mirrors `refs/cannelloni`
  (`parser.cpp`, `decoder.cpp`) byte-for-byte. Unit-tested against golden vectors.
- **`src/transport.rs`** — UDP and TCP (client/server) transports with a
  background RX thread feeding a bounded ring; `canWrite` sends one frame,
  `canRead` drains the ring (`canERR_NOMSG` when empty).
- **`src/config.rs`** — layered config (defaults → `kvasilloni.ini` → env). Finds
  the INI next to the DLL or the .exe via `GetModuleFileNameW`.
- **`src/lib.rs`** — the 13 `extern "system"` exports. Each wraps its body in
  `catch_unwind` so a stray panic becomes a CANlib error code, never an unwind
  across the FFI boundary.

### Wire format (cannelloni, for reference)

Per frame: `can_id` (4 bytes, big-endian, SocketCAN flag bits in the top —
`EFF 0x80000000`, `RTR 0x40000000`) · `len` (1 byte; `0x80` ⇒ CAN-FD) · `flags`
(1 byte, only if CAN-FD) · `data[len]` (omitted for RTR).
**UDP** prefixes a 5-byte header `{ version=2, op=DATA(0), seq, count(BE u16) }`
and packs `count` frames. **TCP** opens with both peers exchanging the ASCII
string `CANNELLONIv1`, then streams frames back-to-back with no packet header.

## Reference C prototype

`reference/c-prototype/` holds the original C implementation of the same shim. Its
`make test` cross-validates the wire codec against cannelloni's *own*
`parser.cpp`/`decoder.cpp` compiled natively — a useful independent oracle of the
wire format. The Rust crate is the maintained deliverable.
