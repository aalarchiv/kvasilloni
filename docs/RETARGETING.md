# Functions & retargeting

What the shim's `canlib32.dll` exports, and how to point it at a different
application. For setup and everyday use, see the [README](../README.md).

## Core functions

The shim implements the **13 core CANlib functions** the current target app
resolves (matching the reference DLL's export table):

```
canInitializeLibrary  canOpenChannel   canSetBusParams  canBusOn   canBusOff
canWrite              canRead          canReadStatus    canReadErrorCounters
canGetBusStatistics  canGetErrorText  canGetVersion    canClose
```

Classic CAN frames (11-bit and 29-bit IDs, DLC 0-8, RTR) and **CAN FD**
(`canFDMSG_FDF`/`BRS`/`ESI`, payloads up to 64 bytes, DLC auto-rounded to a valid
FD length) work in both directions. To **receive** FD payloads larger than 8
bytes, open the channel with `canOPEN_CAN_FD` (`canOpenChannel(ch,
canOPEN_CAN_FD)`); a channel opened classic caps `canRead` at 8 bytes so an FD
frame on the bus can never overrun a classic 8-byte receive buffer.

## Extended exports

This further set is implemented so the shim can stand in for apps with a wider
import table:

| Function(s) | Behavior |
|---|---|
| `canFlushReceiveQueue` / `canFlushTransmitQueue` | clears the RX ring / no-op (TX is synchronous) |
| `canSetBusOutputControl` / `canGetBusOutputControl` | stores & returns the driver type (default `canDRIVER_NORMAL`) |
| `canReadWait` / `canReadSync` | blocking read / wait-for-available, honoring the ms timeout (`canWAIT_INFINITE` ~ unbounded) |
| `canWriteWait` / `canWriteSync` | send synchronously, ignoring the timeout |
| `canIoCtl` | dispatches `canIOCTL_FLUSH_RX/TX_BUFFER`, `GET/SET_TIMER_SCALE`, `SET_TXACK/TXRQ`, `GET_RX/TX_BUFFER_LEVEL`; unknown funcs return `canERR_NOT_IMPLEMENTED` |
| `canAccept` | real acceptance filtering: drops frames where `(id & mask) != (code & mask)` (separate STD/EXT code+mask; zero mask = accept all) |
| `canObjBufSetFilter` | benign no-op (object buffers are a distinct mechanism) |
| `canGetNumberOfChannels` | returns the configured channel count (`KVASILLONI_CHANNELS` / ini `channels`, default 1) |
| `canGetChannelData` | synthetic values for `CHANNEL_NAME` ("kvasilloni vcan*N*"), `DEVDESCR_ASCII`, `MFGNAME_ASCII`, `CARD_SERIAL_NO`, `CHANNEL_CAP/FLAGS`, `CARD_TYPE/NUMBER`, `CHAN_NO_ON_CARD` |
| `canSetNotify` | event-driven RX callbacks (`canNOTIFY_RX`) |

### `canSetNotify` threading caveat

The registered callback is invoked **on the shim's RX thread**, not the thread
that called `canSetNotify`. Keep it short and non-blocking. Disarming
(`canSetNotify(h, NULL, ...)`) or `canClose` is race-free: once it returns the old
callback is no longer running, so you may then free its context. A
`canReadWait`/`canReadSync` issued *from inside* the callback collapses to a
non-blocking poll (blocking there would stall the RX thread). Only `canNOTIFY_RX`
is delivered.

## Retargeting to a new app

The shim exports only the functions the *current* target app resolves - it is
**not** a general replacement for the full Kvaser `canlib32.dll` (which exports
hundreds). Any symbol an app imports that the shim does not export will fail to
load that app.

**Before pointing kvasilloni at a different application, run the import-coverage
check** - the step-by-step procedure (enumerate the app's canlib imports, diff
against `make verify`, implement any missing symbols, then update the tables
above) lives in
[`AGENTS.md`](https://github.com/aalarchiv/kvasilloni/blob/main/AGENTS.md).
