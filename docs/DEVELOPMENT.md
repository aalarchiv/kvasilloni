# Building, testing & internals

For *using* a prebuilt DLL, see the [README](../README.md). This doc is for
building from source, running the test suites, and the shim's internals.

## Build

Requires the Rust toolchain plus the mingw-w64 linkers
(`i686-w64-mingw32-gcc`, `x86_64-w64-mingw32-gcc`).

```bash
rustup target add i686-pc-windows-gnu x86_64-pc-windows-gnu

make            # -> target/{i686,x86_64}-pc-windows-gnu/release/canlib32.dll
make verify     # confirm all exports are present and undecorated (32-bit)
make test       # host unit + property tests (wire codec, transport, config)
```

Build a single bitness with `make dll32` or `make dll64`; pick the DLL that
matches your Windows application's bitness (most legacy Kvaser apps are 32-bit).

Pushing a `v*` tag builds both arches in CI and publishes a GitHub release with
per-arch zips and `SHA256SUMS.txt` (see `.github/workflows/release.yml`).

## Verify end-to-end

`make selftest` runs a full loopback on this Linux host using an **isolated
`vcan1`** (so it never disturbs anything on `vcan0`). It builds cannelloni,
launches a small probe under **wine** that loads the shim, and asserts that frames
cross **both directions**, then exercises the robustness and retargeting paths.
The cases:

- classic UDP and classic TCP (TX and RX)
- CAN FD with BRS over UDP
- INI-only config (no environment variables)
- a classic channel capping an oversized FD frame at 8 bytes
- channel enumeration, acceptance filtering, and notify callbacks
- closing a channel from inside a notify callback
- RX surviving a malformed UDP datagram
- the peer-IP check (drops a non-allowed source; accepts it with the check off)
- the TCP connect/handshake and server-accept timeouts
- the TCP server role (shim listens, cannelloni connects)
- multiple channels: distinct handles, independent TX, and per-channel RX
  isolation across two endpoints (no cross-leak)
- a multi-threaded concurrency stress run against the real DLL

It prints `SELFTEST: PASS` when every case passes. The two-endpoint isolation case
also brings up an isolated `vcan2`. Run the same suite against the 64-bit DLL with
`make selftest64` (wine wow64).

(Needs `wine`, `can-utils`, `cmake`/`g++`, and permission to create a vcan link.)

## Source layout

- **`src/wire.rs`** - the cannelloni codec: per-frame `encode`/`decode`, the
  UDP packet builder/parser, the TCP streaming decoder state machine, and the
  Kvaser<->SocketCAN ID/flag translation. An independent implementation of the
  cannelloni wire protocol, unit-tested against golden byte vectors.
- **`src/transport.rs`** - UDP and TCP (client/server) transports with a
  background RX thread feeding a bounded ring; `canWrite` sends one frame,
  `canRead` drains the ring (`canERR_NOMSG` when empty).
- **`src/config.rs`** - layered config (defaults -> `kvasilloni.ini` -> env). Finds
  the INI next to the DLL or the .exe via `GetModuleFileNameW`.
- **`src/lib.rs`** - the `extern "system"` exports (13 core + the extended
  retargeting set). Each wraps its body in `catch_unwind` so a stray panic
  becomes a CANlib error code, never an unwind across the FFI boundary.

## Wire format (cannelloni, for reference)

Per frame: `can_id` (4 bytes, big-endian, SocketCAN flag bits in the top -
`EFF 0x80000000`, `RTR 0x40000000`) | `len` (1 byte; `0x80` => CAN-FD) | `flags`
(1 byte, only if CAN-FD) | `data[len]` (omitted for RTR).
**UDP** prefixes a 5-byte header `{ version=2, op=DATA(0), seq, count(BE u16) }`
and packs `count` frames. **TCP** opens with both peers exchanging the ASCII
string `CANNELLONIv1`, then streams frames back-to-back with no packet header.

## Robustness checks (optional; need a nightly toolchain)

```bash
make race       # race-detect the transport concurrency under ThreadSanitizer
make fuzz       # coverage-guided fuzz the untrusted wire parsers under ASAN
                #   (cargo install cargo-fuzz first; FUZZ_SECS=N sets the budget)
```

`make race` rebuilds std with TSan instrumentation (`-Z build-std`) and treats any
data race as a hard failure; `make fuzz` exercises `parse_udp` and `decode_stream`
against arbitrary bytes, seeded from `fuzz/corpus/<target>/seed_*`. Both back the
`cargo test` property suites and are documented further in `src/transport.rs` and
`fuzz/`.
