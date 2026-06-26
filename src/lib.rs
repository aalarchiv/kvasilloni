// SPDX-License-Identifier: LGPL-3.0-or-later
//! canlib32.dll - a drop-in Kvaser CANlib shim that bridges to a Linux `vcan`
//! via cannelloni (UDP or TCP), with no Kvaser hardware or driver.
//!
//! Implements the 13 CANlib symbols the target Windows app resolves, plus an
//! extended set of CANlib functions for retargeting to other apps (epic
//! kvasilloni-5yp): queue flushing, bus-output control, blocking I/O,
//! `canIoCtl`, acceptance filtering, channel enumeration, and event
//! notifications. Instead of touching hardware, the shim is itself a cannelloni
//! peer talking to a stock `cannelloni -I vcan0 ...` on the Linux side.
//!
//!   Windows app -> canlib32.dll (this) --UDP|TCP--> cannelloni -> vcan -> Linux CAN
//!
//! Config is layered (defaults < `kvasilloni.ini` < environment), read fresh at
//! each `canOpenChannel`; see `config.rs`. Every key has a `KVASILLONI_*` env
//! override (host may be IPv4 or IPv6):
//!   KVASILLONI_HOST            Linux cannelloni IP          (default 127.0.0.1)
//!   KVASILLONI_PORT            remote port to send to       (default 20000)
//!   KVASILLONI_LOCALPORT       UDP bind / TCP server port   (default 20000)
//!   KVASILLONI_PROTO           "udp" | "tcp"                (default "udp")
//!   KVASILLONI_TCPROLE         "client" | "server"          (default "client")
//!   KVASILLONI_LOG             path; if set, append a debug log
//!   KVASILLONI_CHANNELS        canGetNumberOfChannels count (default 1)
//!   KVASILLONI_CONNECT_TIMEOUT TCP client connect/handshake timeout, ms (5000)
//!   KVASILLONI_ACCEPT_TIMEOUT  TCP server accept timeout, ms          (30000)
//!   KVASILLONI_PEER_CHECK      restrict inbound to host/allow (default on)
//!   KVASILLONI_ALLOW           extra peer IPs (comma/space separated)
//!   KVASILLONI_UDP_PORT_FALLBACK  bind ephemeral if localport busy (default off)

mod config;
mod transport;
mod wire;

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::os::raw::{c_char, c_int, c_long, c_uint, c_ulong, c_ushort};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use config::Config;
use transport::Conn;
use wire::Frame;

// ---- CANlib return codes ----
const CAN_OK: c_int = 0;
const CAN_ERR_PARAM: c_int = -1;
const CAN_ERR_NOMSG: c_int = -2;
const CAN_ERR_NOTFOUND: c_int = -3;
const CAN_ERR_INVHANDLE: c_int = -10; // canERR_INVHANDLE: handle not an open channel
const CAN_ERR_NOT_IMPLEMENTED: c_int = -32;

/// `canDRIVER_NORMAL` (canlib.h): default output-control driver type.
const CAN_DRIVER_NORMAL: u32 = 4;

/// `canOPEN_CAN_FD` (canlib.h) open flag: the app opened the channel for CAN FD
/// and so provides a receive buffer large enough for FD payloads (up to 64
/// bytes). Without it the channel is classic and `canRead` never returns more
/// than 8 bytes, protecting an 8-byte caller buffer (kvasilloni-nmt).
const CAN_OPEN_CAN_FD: c_int = 0x0400;

// ---- canReadStatus flag bits (canstat.h) ----
/// `canSTAT_RX_PENDING`: at least one frame is queued for reading.
const CAN_STAT_RX_PENDING: c_ulong = 0x0000_0020;
/// `canSTAT_SW_OVERRUN`: the driver dropped received frames (ring overflow).
const CAN_STAT_SW_OVERRUN: c_ulong = 0x0000_0400;

// ---- canGetChannelData CHANNEL_CAP bits (canlib.h canCHANNEL_CAP_*) ----
/// The shim does classic+FD, extended IDs, on a virtual channel; advertise that
/// so apps that gate CAN FD on the capability mask actually enable it. See
/// kvasilloni-vsd.
const CAN_CHANNEL_CAP_EXTENDED_CAN: u32 = 0x0000_0001;
const CAN_CHANNEL_CAP_VIRTUAL: u32 = 0x0001_0000;
const CAN_CHANNEL_CAP_CAN_FD: u32 = 0x0008_0000;
/// `canWAIT_INFINITE` sentinel for the blocking-I/O timeout arguments (ms).
const CAN_WAIT_INFINITE: c_ulong = 0xFFFF_FFFF;

/// Open channels keyed by the handle `canOpenChannel` returned. A `BTreeMap` so
/// the static is const-initializable without `LazyLock`; the channel count is
/// tiny. Each API call resolves its `hnd` here, so an app that opens several
/// channels (e.g. one per thread) gets isolated connections instead of all
/// sharing - or clobbering - one global. All channels still bridge to the same
/// configured cannelloni endpoint. See kvasilloni-j83.
/// `Arc<Conn>` so a call can clone its channel out of the map and then operate
/// on it *without* holding the global lock - critical for `canWrite`, whose TCP
/// send may block: holding `CONNS` across it would stall every other channel
/// (kvasilloni-lo7). `Conn`'s methods all take `&self` (interior mutability).
static CONNS: Mutex<BTreeMap<c_int, Arc<Conn>>> = Mutex::new(BTreeMap::new());
/// Next handle to hand out. Kvaser handles are non-negative; we start at 1 and
/// never reuse, so a stale handle from a closed channel fails lookup cleanly.
static NEXT_HANDLE: AtomicI32 = AtomicI32::new(1);
/// Log path resolved from config at canOpenChannel (env `KVASILLONI_LOG` still wins).
static LOG_PATH: Mutex<Option<String>> = Mutex::new(None);
/// Last value passed to `canSetBusOutputControl`; returned by the getter.
static DRIVER_TYPE: AtomicU32 = AtomicU32::new(CAN_DRIVER_NORMAL);
/// Memoized channel count for `canGetNumberOfChannels` (read from config once).
static ENUM_CHANNELS: OnceLock<u32> = OnceLock::new();

/// Cached append handle for the log file, keyed by its resolved path, so that
/// high-rate logging (every canWrite/canRead when KVASILLONI_LOG is set) doesn't
/// reopen the file on every call. Reopened only when the path changes.
static LOG_FILE: Mutex<Option<(String, std::fs::File)>> = Mutex::new(None);

fn log(msg: &str) {
    let path = std::env::var("KVASILLONI_LOG")
        .ok()
        .or_else(|| LOG_PATH.lock().ok().and_then(|g| g.clone()));
    let path = match path {
        Some(p) => p,
        None => return,
    };
    log_to(&LOG_FILE, &path, msg);
}

/// Append `msg` to the file at `path`, reusing the cached append handle in
/// `cache` so high-rate logging doesn't reopen the file on every call. The handle
/// is reopened only when `path` differs from the cached one; on open failure the
/// cache is cleared so the next call retries (best-effort, never errors out).
///
/// Split out of `log()` so the cache contract - reuse, path-change reopen, and
/// open-failure fallback - is unit-testable against a caller-supplied cache, with
/// no process-env mutation racing parallel tests (kvasilloni-1gl/-7yl).
fn log_to(cache: &Mutex<Option<(String, std::fs::File)>>, path: &str, msg: &str) {
    use std::io::Write;
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    // (Re)open only when the cache is empty or the configured path changed.
    if !matches!(guard.as_ref(), Some((p, _)) if p == path) {
        match std::fs::OpenOptions::new().create(true).append(true).open(path) {
            Ok(f) => *guard = Some((path.to_string(), f)),
            Err(_) => {
                *guard = None; // best-effort: drop cache, retry on next call
                return;
            }
        }
    }
    if let Some((_, f)) = guard.as_mut() {
        let _ = writeln!(f, "{msg}");
    }
}

/// Read-once memoization for `canGetNumberOfChannels`: initialize `cell` from
/// `load` on first use and return that value forever after, so apps that poll the
/// channel count don't re-hit the disk and the answer can't change mid-run.
/// Extracted so the read-once contract is unit-testable without mutating the
/// process env (kvasilloni-1gl/-7yl).
fn channels_memo(cell: &OnceLock<u32>, load: impl FnOnce() -> u32) -> u32 {
    *cell.get_or_init(load)
}

/// Run an export body, converting any panic into `CAN_ERR_PARAM` so it never
/// unwinds across the FFI boundary.
fn guard<F: FnOnce() -> c_int>(f: F) -> c_int {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(CAN_ERR_PARAM)
}

// ============================== exported API ==============================

#[no_mangle]
pub extern "system" fn canInitializeLibrary() {
    log("canInitializeLibrary");
}

#[no_mangle]
pub extern "system" fn canOpenChannel(channel: c_int, flags: c_int) -> c_int {
    guard(|| {
        let cfg = Config::load();
        if let Ok(mut g) = LOG_PATH.lock() {
            *g = cfg.log.clone();
        }
        log(&format!(
            "canOpenChannel(ch={channel}, flags={flags:#x}) proto={} host={}:{} local={} role={}",
            if cfg.tcp { "tcp" } else { "udp" },
            cfg.host,
            cfg.remote_port,
            cfg.local_port,
            if cfg.tcp_server { "server" } else { "client" },
        ));
        match Conn::connect(&cfg) {
            Ok(c) => {
                let bound = c.local_port();
                if bound != cfg.local_port {
                    log(&format!(
                        "canOpenChannel: local port {} busy; bound ephemeral {bound} (kvasilloni-iai)",
                        cfg.local_port
                    ));
                }
                // Record whether the app opted into CAN FD; gates RX delivery
                // width so an FD frame can't overflow a classic 8-byte canRead
                // buffer (kvasilloni-nmt).
                c.set_fd_capable(flags & CAN_OPEN_CAN_FD != 0);
                let hnd = NEXT_HANDLE.fetch_add(1, Ordering::SeqCst);
                CONNS.lock().unwrap_or_else(|e| e.into_inner()).insert(hnd, Arc::new(c));
                log(&format!("canOpenChannel -> handle {hnd}"));
                hnd // distinct non-negative handle per open channel
            }
            Err(e) => {
                log(&format!("canOpenChannel: connect failed: {e}"));
                if cfg.tcp {
                    log("canOpenChannel: hint: cannelloni -p or -R <peer-ip> skips peer check");
                } else {
                    log(
                        "canOpenChannel: hint: UDP localport may be in use; give each instance a \
                         unique localport (and matching cannelloni -r). Set \
                         KVASILLONI_UDP_PORT_FALLBACK=1 to bind an ephemeral port instead, but a \
                         stock cannelloni then never sends to it (TX-only). See kvasilloni-25q.",
                    );
                }
                CAN_ERR_NOTFOUND
            }
        }
    })
}

#[no_mangle]
pub extern "system" fn canSetBusParams(
    _hnd: c_int,
    freq: c_long,
    _tseg1: c_uint,
    _tseg2: c_uint,
    _sjw: c_uint,
    _no_samp: c_uint,
    _sync_mode: c_uint,
) -> c_int {
    log(&format!("canSetBusParams(freq={freq})"));
    CAN_OK
}

#[no_mangle]
pub extern "system" fn canBusOn(_hnd: c_int) -> c_int {
    log("canBusOn");
    CAN_OK
}

#[no_mangle]
pub extern "system" fn canBusOff(_hnd: c_int) -> c_int {
    log("canBusOff");
    CAN_OK
}

#[no_mangle]
pub extern "system" fn canWrite(
    hnd: c_int,
    id: c_long,
    msg: *mut c_void,
    dlc: c_uint,
    flag: c_uint,
) -> c_int {
    guard(|| write_frame(hnd, id, msg, dlc, flag))
}

/// Encode and send a single frame on channel `hnd`. Shared by `canWrite` and
/// `canWriteWait` (the shim sends synchronously, so there is no separate queued
/// path).
fn write_frame(hnd: c_int, id: c_long, msg: *mut c_void, dlc: c_uint, flag: c_uint) -> c_int {
    {
        let mut f = Frame::default();
        f.can_id = wire::kvaser_to_canid(id as i32, flag as u32);

        // How many data bytes the caller actually supplied, and the on-wire len.
        // CAN FD: up to 64 bytes, len rounded up to a valid FD DLC (pad with 0).
        // Classic CAN: up to 8 bytes.
        let user_bytes = if flag & wire::CAN_MSG_FDF != 0 {
            f.fd = true;
            f.fd_flags = wire::kvaser_to_fd_flags(flag as u32);
            f.len = wire::fd_round_len(dlc.min(64) as u8);
            dlc.min(64) as usize
        } else {
            f.len = dlc.min(8) as u8;
            f.len as usize
        };
        if !msg.is_null() && user_bytes > 0 && flag & wire::CAN_MSG_RTR == 0 {
            let src = unsafe { std::slice::from_raw_parts(msg as *const u8, user_bytes) };
            f.data[..user_bytes].copy_from_slice(src);
        }
        // Clone the channel's Arc out of the map, then DROP the global lock before
        // writing: a TCP send can block, and holding CONNS across it would stall
        // every other channel (kvasilloni-lo7).
        let conn = match CONNS.lock().unwrap_or_else(|e| e.into_inner()).get(&hnd) {
            Some(c) => c.clone(),
            None => return CAN_ERR_INVHANDLE,
        };
        match conn.write(&f) {
            Ok(()) => {
                log(&format!("canWrite id={id:#x} dlc={dlc} flag={flag:#x} -> ok"));
                CAN_OK
            }
            Err(e) => {
                log(&format!("canWrite id={id:#x} -> ERR {e}"));
                CAN_ERR_PARAM
            }
        }
    }
}

#[no_mangle]
pub extern "system" fn canRead(
    hnd: c_int,
    id: *mut c_long,
    msg: *mut c_void,
    dlc: *mut c_uint,
    flag: *mut c_uint,
    time: *mut c_ulong,
) -> c_int {
    guard(|| {
        let conn = match CONNS.lock().unwrap_or_else(|e| e.into_inner()).get(&hnd) {
            Some(c) => c.clone(),
            None => return CAN_ERR_INVHANDLE,
        };
        let fd_capable = conn.fd_capable();
        let frame = conn.read();
        let f = match frame {
            Some(f) => f,
            None => return CAN_ERR_NOMSG,
        };
        unsafe { write_read_outputs(&f, id, msg, dlc, flag, time, fd_capable) };
        CAN_OK
    })
}

/// Marshal a received [`Frame`] into the C out-parameters shared by `canRead`
/// and `canReadWait`. Every pointer is null-checked.
///
/// `fd_capable` is whether the channel was opened with `canOPEN_CAN_FD`. It bounds
/// how many bytes may be written into the caller's `msg` buffer: the Kvaser
/// `canRead` ABI carries no buffer length, so a classic (non-FD) caller sizes
/// `msg` at 8 bytes. A classic channel therefore delivers at most 8 bytes and is
/// reported as a classic frame, so even a 64-byte FD frame on the wire cannot
/// overflow that buffer (kvasilloni-nmt). An FD channel delivers up to 64.
unsafe fn write_read_outputs(
    f: &Frame,
    id: *mut c_long,
    msg: *mut c_void,
    dlc: *mut c_uint,
    flag: *mut c_uint,
    time: *mut c_ulong,
    fd_capable: bool,
) {
    // Only present FD framing (FDF/BRS/ESI, wide payload) when the app opened the
    // channel for it; otherwise the frame is delivered with classic semantics.
    let report_fd = f.fd && fd_capable;
    let (oid, mut oflag) = wire::canid_to_kvaser(f.can_id, report_fd);
    if report_fd {
        oflag |= wire::fd_flags_to_kvaser(f.fd_flags); // BRS/ESI
    }
    // Cap the delivered length to what the caller's buffer can hold for its
    // channel class. Classic decoders already reject non-FD frames over 8 bytes;
    // this also truncates a real FD frame that lands on a classic channel.
    let max_len = if fd_capable { wire::MAX_FRAME_LEN } else { wire::CLASSIC_FRAME_MAX_LEN };
    let n = (f.len as usize).min(max_len);
    if !id.is_null() {
        *id = oid as c_long;
    }
    if !flag.is_null() {
        *flag = oflag as c_uint;
    }
    if !dlc.is_null() {
        *dlc = n as c_uint;
    }
    if !time.is_null() {
        *time = f.rx_time_ms as c_ulong; // receive timestamp in ms (kvasilloni-kha)
    }
    if !msg.is_null() && n > 0 && !f.is_rtr() {
        let dst = std::slice::from_raw_parts_mut(msg as *mut u8, n);
        dst.copy_from_slice(&f.data[..n]);
    }
}

/// Convert a CANlib millisecond timeout (with `canWAIT_INFINITE` sentinel) into
/// a [`Duration`]. "Infinite" maps to ~49 days, effectively unbounded here.
fn timeout_to_duration(timeout: c_ulong) -> Duration {
    if timeout == CAN_WAIT_INFINITE {
        Duration::from_millis(u32::MAX as u64)
    } else {
        Duration::from_millis(timeout as u64)
    }
}

#[no_mangle]
pub extern "system" fn canReadStatus(hnd: c_int, flags: *mut c_ulong) -> c_int {
    guard(|| {
        // Report real RX state: pending frames and a sticky software-overrun bit
        // when the ring has dropped frames (kvasilloni-tlm). Lenient on an unknown
        // handle - report 0 rather than INVHANDLE, matching the prior behavior.
        let mut st: c_ulong = 0;
        let conn = CONNS.lock().unwrap_or_else(|e| e.into_inner()).get(&hnd).cloned();
        if let Some(c) = conn {
            if c.rx_level() > 0 {
                st |= CAN_STAT_RX_PENDING;
            }
            if c.rx_overflowed() {
                st |= CAN_STAT_SW_OVERRUN;
            }
        }
        unsafe {
            if !flags.is_null() {
                *flags = st;
            }
        }
        CAN_OK
    })
}

#[no_mangle]
pub extern "system" fn canReadErrorCounters(
    _hnd: c_int,
    tx_err: *mut c_uint,
    rx_err: *mut c_uint,
    ov_err: *mut c_uint,
) -> c_int {
    unsafe {
        if !tx_err.is_null() {
            *tx_err = 0;
        }
        if !rx_err.is_null() {
            *rx_err = 0;
        }
        if !ov_err.is_null() {
            *ov_err = 0;
        }
    }
    CAN_OK
}

#[no_mangle]
pub extern "system" fn canGetBusStatistics(
    _hnd: c_int,
    stat: *mut c_void,
    bufsiz: usize,
) -> c_int {
    unsafe {
        if !stat.is_null() && bufsiz > 0 {
            std::ptr::write_bytes(stat as *mut u8, 0, bufsiz);
        }
    }
    CAN_OK
}

#[no_mangle]
pub extern "system" fn canGetVersion() -> c_ushort {
    0x0900 // 9.0
}

#[no_mangle]
pub extern "system" fn canGetErrorText(err: c_int, buf: *mut c_char, bufsiz: c_uint) -> c_int {
    let text: &[u8] = match err {
        CAN_OK => b"OK\0",
        CAN_ERR_PARAM => b"Error in parameter\0",
        CAN_ERR_NOMSG => b"No messages available\0",
        CAN_ERR_NOTFOUND => b"Specified device not found\0",
        _ => b"Unknown error\0",
    };
    unsafe {
        if !buf.is_null() && bufsiz > 0 {
            let n = (bufsiz as usize - 1).min(text.len() - 1);
            std::ptr::copy_nonoverlapping(text.as_ptr(), buf as *mut u8, n);
            *buf.add(n) = 0;
        }
    }
    CAN_OK
}

#[no_mangle]
pub extern "system" fn canClose(hnd: c_int) -> c_int {
    guard(|| {
        log(&format!("canClose(hnd={hnd})"));
        // Remove under the lock, then close() outside it: close() joins the RX
        // thread (up to the 500ms UDP read timeout) and we must not hold the
        // global map lock - and thus stall every other channel - while it does.
        let conn = CONNS.lock().unwrap_or_else(|e| e.into_inner()).remove(&hnd);
        if let Some(c) = conn {
            c.close();
        }
        // Closing an unknown/already-closed handle is a benign no-op (lenient so
        // double-close cleanup paths in lower-quality apps don't error).
        CAN_OK
    })
}

// ================== retargeting: extended export coverage ==================
// The functions below are NOT used by the current target app. They exist so the
// shim can stand in for other CANlib apps whose import tables resolve them (see
// AGENTS.md coverage-check procedure). Implemented under epic kvasilloni-5yp.

/// Run `f` with the connection for `hnd`, or return `err` when no such channel
/// is open. The channel's `Arc` is cloned out and the global lock released before
/// `f` runs, so per-channel work never blocks the whole table (kvasilloni-lo7).
fn with_conn<F: FnOnce(&Conn) -> c_int>(hnd: c_int, err: c_int, f: F) -> c_int {
    let conn = match CONNS.lock().unwrap_or_else(|e| e.into_inner()).get(&hnd) {
        Some(c) => c.clone(),
        None => return err,
    };
    f(&conn)
}

/// Write a little-endian u32 into a `bufsize`-bounded C buffer.
fn out_u32(buf: *mut c_void, bufsize: usize, val: u32) -> c_int {
    if buf.is_null() || bufsize < 4 {
        return CAN_ERR_PARAM;
    }
    unsafe { std::ptr::copy_nonoverlapping(val.to_le_bytes().as_ptr(), buf as *mut u8, 4) };
    CAN_OK
}

/// Write a little-endian u64 into a `bufsize`-bounded C buffer.
fn out_u64(buf: *mut c_void, bufsize: usize, val: u64) -> c_int {
    if buf.is_null() || bufsize < 8 {
        return CAN_ERR_PARAM;
    }
    unsafe { std::ptr::copy_nonoverlapping(val.to_le_bytes().as_ptr(), buf as *mut u8, 8) };
    CAN_OK
}

/// Write a NUL-terminated ASCII string into a `bufsize`-bounded C buffer.
fn out_cstr(buf: *mut c_void, bufsize: usize, s: &str) -> c_int {
    if buf.is_null() || bufsize == 0 {
        return CAN_ERR_PARAM;
    }
    let bytes = s.as_bytes();
    let n = bytes.len().min(bufsize - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, n);
        *(buf as *mut u8).add(n) = 0;
    }
    CAN_OK
}

// ---- kvasilloni-qaq: queue flushing ----

#[no_mangle]
pub extern "system" fn canFlushReceiveQueue(hnd: c_int) -> c_int {
    guard(|| {
        log("canFlushReceiveQueue");
        with_conn(hnd, CAN_ERR_INVHANDLE, |c| {
            c.clear_rx();
            CAN_OK
        })
    })
}

#[no_mangle]
pub extern "system" fn canFlushTransmitQueue(_hnd: c_int) -> c_int {
    log("canFlushTransmitQueue");
    CAN_OK // TX is synchronous: nothing is ever queued
}

// ---- kvasilloni-zva: bus output control (driver type) ----

#[no_mangle]
pub extern "system" fn canSetBusOutputControl(_hnd: c_int, drivertype: c_uint) -> c_int {
    log(&format!("canSetBusOutputControl(drivertype={drivertype})"));
    DRIVER_TYPE.store(drivertype as u32, Ordering::SeqCst);
    CAN_OK
}

#[no_mangle]
pub extern "system" fn canGetBusOutputControl(_hnd: c_int, drivertype: *mut c_uint) -> c_int {
    guard(|| {
        unsafe {
            if !drivertype.is_null() {
                *drivertype = DRIVER_TYPE.load(Ordering::SeqCst) as c_uint;
            }
        }
        CAN_OK
    })
}

// ---- kvasilloni-fqe: blocking I/O ----

#[no_mangle]
pub extern "system" fn canReadWait(
    hnd: c_int,
    id: *mut c_long,
    msg: *mut c_void,
    dlc: *mut c_uint,
    flag: *mut c_uint,
    time: *mut c_ulong,
    timeout: c_ulong,
) -> c_int {
    guard(|| {
        // Take a clone of the channel and release CONNS before blocking. Capture
        // the FD-capability up front so a classic caller's buffer stays bounded.
        let conn = match CONNS.lock().unwrap_or_else(|e| e.into_inner()).get(&hnd) {
            Some(c) => c.clone(),
            None => return CAN_ERR_INVHANDLE,
        };
        let fd_capable = conn.fd_capable();
        match conn.rx_shared().pop_wait(timeout_to_duration(timeout)) {
            Some(f) => {
                unsafe { write_read_outputs(&f, id, msg, dlc, flag, time, fd_capable) };
                CAN_OK
            }
            None => CAN_ERR_NOMSG,
        }
    })
}

#[no_mangle]
pub extern "system" fn canReadSync(hnd: c_int, timeout: c_ulong) -> c_int {
    guard(|| {
        let shared = match CONNS.lock().unwrap_or_else(|e| e.into_inner()).get(&hnd) {
            Some(c) => c.rx_shared(),
            None => return CAN_ERR_INVHANDLE,
        };
        if shared.peek_wait(timeout_to_duration(timeout)) {
            CAN_OK
        } else {
            CAN_ERR_NOMSG
        }
    })
}

#[no_mangle]
pub extern "system" fn canWriteWait(
    hnd: c_int,
    id: c_long,
    msg: *mut c_void,
    dlc: c_uint,
    flag: c_uint,
    _timeout: c_ulong,
) -> c_int {
    // We already send synchronously, so the timeout is irrelevant.
    guard(|| write_frame(hnd, id, msg, dlc, flag))
}

#[no_mangle]
pub extern "system" fn canWriteSync(_hnd: c_int, _timeout: c_ulong) -> c_int {
    CAN_OK // no TX queue to drain
}

// ---- kvasilloni-efg: canIoCtl dispatch ----

#[no_mangle]
pub extern "system" fn canIoCtl(
    hnd: c_int,
    func: c_uint,
    buf: *mut c_void,
    buflen: c_uint,
) -> c_int {
    guard(|| {
        log(&format!("canIoCtl(func={func}, buflen={buflen})"));
        let len = buflen as usize;
        match func {
            10 => with_conn(hnd, CAN_ERR_INVHANDLE, |c| {
                c.clear_rx();
                CAN_OK
            }), // canIOCTL_FLUSH_RX_BUFFER
            11 => CAN_OK,                       // canIOCTL_FLUSH_TX_BUFFER (synchronous TX)
            6 | 7 | 13 => CAN_OK,               // SET_TIMER_SCALE / SET_TXACK / SET_TXRQ: accept
            12 => out_u32(buf, len, 1000),      // canIOCTL_GET_TIMER_SCALE (us/tick)
            8 => with_conn(hnd, CAN_ERR_INVHANDLE, |c| out_u32(buf, len, c.rx_level() as u32)), // GET_RX_BUFFER_LEVEL
            9 => out_u32(buf, len, 0),          // canIOCTL_GET_TX_BUFFER_LEVEL
            _ => CAN_ERR_NOT_IMPLEMENTED,
        }
    })
}

// ---- kvasilloni-guu: acceptance filtering ----

#[no_mangle]
pub extern "system" fn canAccept(hnd: c_int, envelope: c_long, flag: c_uint) -> c_int {
    guard(|| {
        log(&format!("canAccept(envelope={envelope:#x}, flag={flag})"));
        with_conn(hnd, CAN_ERR_INVHANDLE, |c| {
            c.set_accept(flag as u32, envelope as u32);
            CAN_OK
        })
    })
}

#[no_mangle]
pub extern "system" fn canObjBufSetFilter(
    _hnd: c_int,
    _idx: c_int,
    _code: c_uint,
    _mask: c_uint,
) -> c_int {
    // Object buffers are a separate (auto-response) mechanism; benign no-op.
    log("canObjBufSetFilter (no-op)");
    CAN_OK
}

// ---- kvasilloni-7hn: channel enumeration ----

#[no_mangle]
pub extern "system" fn canGetNumberOfChannels(channel_count: *mut c_int) -> c_int {
    guard(|| {
        // Channel count is static config; read the INI once and memoize so apps
        // that poll this don't re-hit the disk every call (kvasilloni-7yl).
        // Clamp to i32::MAX so an absurd config value can't wrap to a negative
        // count Kvaser would never return (apps use it as a loop/array bound).
        let n = channels_memo(&ENUM_CHANNELS, || Config::load().channels).min(i32::MAX as u32) as c_int;
        unsafe {
            if !channel_count.is_null() {
                *channel_count = n;
            }
        }
        log(&format!("canGetNumberOfChannels -> {n}"));
        CAN_OK
    })
}

#[no_mangle]
pub extern "system" fn canGetChannelData(
    channel: c_int,
    item: c_int,
    buffer: *mut c_void,
    bufsize: usize,
) -> c_int {
    guard(|| {
        // Synthetic-but-plausible values; see canCHANNELDATA_* in canlib.h.
        match item {
            13 => out_cstr(buffer, bufsize, &format!("kvasilloni vcan{channel}")), // CHANNEL_NAME
            26 => out_cstr(buffer, bufsize, "kvasilloni virtual CAN"), // DEVDESCR_ASCII
            24 => out_cstr(buffer, bufsize, "kvasilloni"),             // MFGNAME_ASCII
            // CHANNEL_CAP: advertise classic+FD, extended IDs, virtual (kvasilloni-vsd)
            1 => out_u32(
                buffer,
                bufsize,
                CAN_CHANNEL_CAP_EXTENDED_CAN | CAN_CHANNEL_CAP_VIRTUAL | CAN_CHANNEL_CAP_CAN_FD,
            ),
            3 => out_u32(buffer, bufsize, 0),                          // CHANNEL_FLAGS
            4 => out_u32(buffer, bufsize, 0),                          // CARD_TYPE
            5 => out_u32(buffer, bufsize, 0),                          // CARD_NUMBER
            6 => out_u32(buffer, bufsize, channel as u32),             // CHAN_NO_ON_CARD
            7 => out_u64(buffer, bufsize, channel as u64 + 1),         // CARD_SERIAL_NO
            _ => CAN_ERR_PARAM,
        }
    })
}

// ---- kvasilloni-ur8: event-driven notifications ----

#[no_mangle]
pub extern "system" fn canSetNotify(
    hnd: c_int,
    callback: *const c_void,
    notify_flags: c_uint,
    tag: *mut c_void,
) -> c_int {
    guard(|| {
        log(&format!(
            "canSetNotify(flags={notify_flags:#x}, cb={})",
            if callback.is_null() { "null" } else { "set" }
        ));
        with_conn(hnd, CAN_ERR_INVHANDLE, |c| {
            c.set_notify(callback as usize, notify_flags as u32, tag as usize);
            CAN_OK
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_output_control_roundtrips() {
        assert_eq!(canSetBusOutputControl(1, 1 /* SILENT */), CAN_OK);
        let mut dt: c_uint = 0;
        assert_eq!(canGetBusOutputControl(1, &mut dt), CAN_OK);
        assert_eq!(dt, 1);
        // restore default so test order can't leak
        canSetBusOutputControl(1, CAN_DRIVER_NORMAL as c_uint);
    }

    #[test]
    fn ioctl_unknown_func_is_not_implemented() {
        assert_eq!(canIoCtl(1, 9999, std::ptr::null_mut(), 0), CAN_ERR_NOT_IMPLEMENTED);
    }

    #[test]
    fn ioctl_get_timer_scale_writes_value() {
        let mut v: u32 = 0;
        let p = &mut v as *mut u32 as *mut c_void;
        assert_eq!(canIoCtl(1, 12 /* GET_TIMER_SCALE */, p, 4), CAN_OK);
        assert_eq!(v, 1000);
    }

    #[test]
    fn number_of_channels_defaults_to_one() {
        // No KVASILLONI_CHANNELS / ini in the test env -> default 1.
        let mut n: c_int = -1;
        assert_eq!(canGetNumberOfChannels(&mut n), CAN_OK);
        assert!(n >= 1);
    }

    #[test]
    fn channel_data_name_and_serial() {
        let mut buf = [0u8; 64];
        let p = buf.as_mut_ptr() as *mut c_void;
        // CHANNEL_NAME (13) -> "kvasilloni vcan2"
        assert_eq!(canGetChannelData(2, 13, p, buf.len()), CAN_OK);
        let name = std::ffi::CStr::from_bytes_until_nul(&buf).unwrap().to_str().unwrap();
        assert_eq!(name, "kvasilloni vcan2");

        // CARD_SERIAL_NO (7) -> channel+1 as little-endian u64
        let mut s = [0u8; 8];
        assert_eq!(canGetChannelData(2, 7, s.as_mut_ptr() as *mut c_void, s.len()), CAN_OK);
        assert_eq!(u64::from_le_bytes(s), 3);

        // unknown item -> CAN_ERR_PARAM
        assert_eq!(canGetChannelData(2, 9999, p, buf.len()), CAN_ERR_PARAM);
    }

    #[test]
    fn ops_on_unknown_handle_report_invhandle() {
        // No channel is open in the unit-test env (no network). Every data/control
        // op on a handle that was never returned must report INVHANDLE - not
        // panic, and not touch a stale shared connection. Regression for the
        // handle-table refactor (kvasilloni-j83); negative handles are never
        // handed out (allocation starts at 1).
        let bad: c_int = -999;
        assert_eq!(
            canRead(
                bad,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ),
            CAN_ERR_INVHANDLE
        );
        assert_eq!(canWrite(bad, 0x123, std::ptr::null_mut(), 0, 0), CAN_ERR_INVHANDLE);
        assert_eq!(
            canReadWait(
                bad,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            ),
            CAN_ERR_INVHANDLE
        );
        assert_eq!(canReadSync(bad, 0), CAN_ERR_INVHANDLE);
        assert_eq!(canFlushReceiveQueue(bad), CAN_ERR_INVHANDLE);
        assert_eq!(canAccept(bad, 0, 0), CAN_ERR_INVHANDLE);
        assert_eq!(
            canSetNotify(bad, std::ptr::null(), 0, std::ptr::null_mut()),
            CAN_ERR_INVHANDLE
        );
        assert_eq!(canIoCtl(bad, 10 /* FLUSH_RX */, std::ptr::null_mut(), 0), CAN_ERR_INVHANDLE);
        // Close of an unknown handle is a deliberately lenient no-op.
        assert_eq!(canClose(bad), CAN_OK);
    }

    // ---- kvasilloni-7yl: log file-handle cache ----

    fn tmp_log_path(tag: &str) -> std::path::PathBuf {
        // Unique per process + tag so parallel tests never collide; no Date/rand.
        std::env::temp_dir().join(format!("kvasilloni-log-{}-{tag}.log", std::process::id()))
    }

    #[test]
    fn log_cache_reopens_on_path_change() {
        let pa = tmp_log_path("pc-a");
        let pb = tmp_log_path("pc-b");
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);
        let cache = Mutex::new(None);

        log_to(&cache, pa.to_str().unwrap(), "to-a-1");
        log_to(&cache, pa.to_str().unwrap(), "to-a-2"); // same path -> cached handle
        log_to(&cache, pb.to_str().unwrap(), "to-b-1"); // path change -> reopen on B

        assert_eq!(std::fs::read_to_string(&pa).unwrap(), "to-a-1\nto-a-2\n");
        assert_eq!(std::fs::read_to_string(&pb).unwrap(), "to-b-1\n");
        // Cache now points at B, proving the path change swapped the handle.
        assert_eq!(cache.lock().unwrap().as_ref().unwrap().0, pb.to_str().unwrap());

        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);
    }

    #[test]
    fn log_cache_reuses_handle_for_same_path() {
        // Proof of reuse (not just a matching path string): after the first write
        // caches the handle, unlink the file. A *retained* handle keeps writing to
        // the now-unlinked inode, so the path does NOT reappear; a reopen would
        // recreate it. (Linux unlink-while-open semantics; host tests run on Linux.)
        let p = tmp_log_path("reuse");
        let _ = std::fs::remove_file(&p);
        let cache = Mutex::new(None);

        log_to(&cache, p.to_str().unwrap(), "first");
        assert!(p.exists(), "first write creates the file");
        std::fs::remove_file(&p).unwrap();
        log_to(&cache, p.to_str().unwrap(), "second"); // reuse -> writes to unlinked inode
        assert!(!p.exists(), "same path reused the cached handle (no reopen recreated it)");

        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn log_cache_falls_back_on_open_failure() {
        // An unopenable path (a directory) must not poison the cache: it is left
        // empty and a later valid path still logs.
        let bad = std::env::temp_dir(); // a directory: cannot open for append
        let good = tmp_log_path("recover");
        let _ = std::fs::remove_file(&good);
        let cache = Mutex::new(None);

        log_to(&cache, bad.to_str().unwrap(), "nope");
        assert!(cache.lock().unwrap().is_none(), "open failure leaves the cache empty");

        log_to(&cache, good.to_str().unwrap(), "recovered");
        assert_eq!(std::fs::read_to_string(&good).unwrap(), "recovered\n");
        let _ = std::fs::remove_file(&good);
    }

    // ---- kvasilloni-7yl: channel-count memoization ----

    #[test]
    fn channels_memo_reads_once() {
        // The count is read once and returned consistently: a fresh cell takes the
        // first loader's value, and a later loader must never run, so a runtime
        // config change cannot alter an already-answered count. Local cell -> no
        // process-env mutation racing other parallel tests.
        let cell = OnceLock::new();
        assert_eq!(channels_memo(&cell, || 3), 3);
        let mut ran_again = false;
        let n = channels_memo(&cell, || {
            ran_again = true;
            7
        });
        assert_eq!(n, 3, "memoized value returned, not re-read");
        assert!(!ran_again, "loader ran twice: count was not memoized");
    }

    // ---- kvasilloni-eoq: concurrency / stress ----

    #[test]
    fn stress_concurrent_open_write_read_close() {
        // Drive the handle table from many threads at once - rapid open/close plus
        // a burst of write/read on each channel - to prove the mutex-serialized
        // table (kvasilloni-j83) and the RX-thread teardown (hc9/cqe) hold under
        // contention: no deadlock, no crash, no leaked handles. UDP opens succeed
        // with no cannelloni (they just bind a socket and spawn the RX loop);
        // writes go nowhere and reads return NOMSG. Bounded modestly because each
        // close joins an RX thread parked on a 500ms read timeout.
        const THREADS: i32 = 8;
        const OUTER: i32 = 6; // open/close cycles per thread
        const INNER: i32 = 40; // write/read ops per open
        let workers: Vec<_> = (0..THREADS)
            .map(|t| {
                std::thread::spawn(move || {
                    for o in 0..OUTER {
                        let h = canOpenChannel(0, 0);
                        assert!(h > 0, "open failed on thread {t}: {h}");
                        canBusOn(h);
                        let mut data = [t as u8, o as u8, 0xAA, 0xBB];
                        let mut id: c_long = 0;
                        let (mut dlc, mut flag): (c_uint, c_uint) = (0, 0);
                        let mut time: c_ulong = 0;
                        for i in 0..INNER {
                            canWrite(h, 0x100 + i as c_long, data.as_mut_ptr() as *mut c_void, 4, 0);
                            // non-blocking read: usually NOMSG, must never hang/crash
                            let _ = canRead(
                                h,
                                &mut id,
                                std::ptr::null_mut(),
                                &mut dlc,
                                &mut flag,
                                &mut time,
                            );
                        }
                        assert_eq!(canClose(h), CAN_OK);
                    }
                })
            })
            .collect();
        for w in workers {
            w.join().expect("a stress worker thread panicked (deadlock/crash)");
        }
        // No leaked handles: every channel opened was removed on close.
        assert!(
            CONNS.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "handle table leaked channels after the stress run"
        );
    }

    // ---- kvasilloni-nmt: RX delivery must not overflow the caller's buffer ----

    #[test]
    fn classic_channel_caps_rx_to_eight_bytes() {
        // An FD frame (64 bytes on the wire) delivered to a channel opened WITHOUT
        // canOPEN_CAN_FD must hand back at most 8 bytes and report classic
        // semantics, so a classic app's 8-byte msg buffer is never overrun.
        let mut f = Frame::default();
        f.can_id = 0x321;
        f.fd = true;
        f.fd_flags = wire::CANFD_BRS;
        f.len = 64;
        for i in 0..64 {
            f.data[i] = i as u8;
        }
        // Over-size the destination so we can prove only 8 bytes were touched.
        let mut buf = [0xEEu8; 64];
        let mut id: c_long = 0;
        let (mut dlc, mut flag): (c_uint, c_uint) = (0, 0);
        let mut time: c_ulong = 0;
        unsafe {
            write_read_outputs(
                &f,
                &mut id,
                buf.as_mut_ptr() as *mut c_void,
                &mut dlc,
                &mut flag,
                &mut time,
                false, // classic channel
            );
        }
        assert_eq!(dlc, 8, "classic channel must report at most 8 bytes");
        assert_eq!(flag & wire::CAN_MSG_FDF, 0, "FD flag must not be set on a classic channel");
        assert_eq!(&buf[..8], &f.data[..8], "first 8 bytes delivered");
        assert!(buf[8..].iter().all(|&b| b == 0xEE), "bytes past 8 must be untouched (no overflow)");
    }

    #[test]
    fn fd_channel_delivers_full_payload() {
        // The same frame on an FD-opened channel delivers all 64 bytes with the
        // FD/BRS flags intact.
        let mut f = Frame::default();
        f.can_id = 0x321;
        f.fd = true;
        f.fd_flags = wire::CANFD_BRS;
        f.len = 64;
        for i in 0..64 {
            f.data[i] = i as u8;
        }
        let mut buf = [0u8; 64];
        let mut id: c_long = 0;
        let (mut dlc, mut flag): (c_uint, c_uint) = (0, 0);
        let mut time: c_ulong = 0;
        unsafe {
            write_read_outputs(
                &f,
                &mut id,
                buf.as_mut_ptr() as *mut c_void,
                &mut dlc,
                &mut flag,
                &mut time,
                true, // FD channel
            );
        }
        assert_eq!(dlc, 64);
        assert_ne!(flag & wire::CAN_MSG_FDF, 0, "FD flag must be reported");
        assert_ne!(flag & wire::CAN_MSG_BRS, 0, "BRS must be reported");
        assert_eq!(&buf[..], &f.data[..]);
    }

    #[test]
    fn out_helpers_bounds_check() {
        let mut small = [0u8; 2];
        let p = small.as_mut_ptr() as *mut c_void;
        assert_eq!(out_u32(p, small.len(), 0xAABBCCDD), CAN_ERR_PARAM); // too small
        assert_eq!(out_u32(std::ptr::null_mut(), 4, 0), CAN_ERR_PARAM); // null
        assert_eq!(out_cstr(p, 0, "x"), CAN_ERR_PARAM); // zero len
    }
}
