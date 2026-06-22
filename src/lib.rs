//! canlib32.dll — a drop-in Kvaser CANlib shim that bridges to a Linux `vcan`
//! via cannelloni (UDP or TCP), with no Kvaser hardware or driver.
//!
//! Implements exactly the 13 symbols the target Windows app resolves (see the
//! reference DLL export table in `refs/canlib32.dll`). Instead of touching
//! hardware, the shim is itself a cannelloni peer talking to a stock
//! `cannelloni -I vcan0 ...` on the Linux side.
//!
//!   Windows app -> canlib32.dll (this) --UDP|TCP--> cannelloni -> vcan -> Linux CAN
//!
//! Config (environment, read at canOpenChannel):
//!   CANSHIM_HOST      Linux cannelloni IP        (default 127.0.0.1)
//!   CANSHIM_PORT      remote port to send to     (default 20000)
//!   CANSHIM_LOCALPORT UDP bind / TCP server port (default 20000)
//!   CANSHIM_PROTO     "udp" | "tcp"              (default "udp")
//!   CANSHIM_TCPROLE   "client" | "server"        (default "client")
//!   CANSHIM_LOG       path; if set, append a debug log

mod config;
mod transport;
mod wire;

use std::ffi::c_void;
use std::os::raw::{c_char, c_int, c_long, c_uint, c_ulong, c_ushort};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Mutex;

use config::Config;
use transport::Conn;
use wire::Frame;

// ---- CANlib return codes (refs/kvaser_canlib/canstat.h) ----
const CAN_OK: c_int = 0;
const CAN_ERR_PARAM: c_int = -1;
const CAN_ERR_NOMSG: c_int = -2;
const CAN_ERR_NOTFOUND: c_int = -3;

/// Single channel; the target app opens exactly one.
static CONN: Mutex<Option<Conn>> = Mutex::new(None);
/// Log path resolved from config at canOpenChannel (env `CANSHIM_LOG` still wins).
static LOG_PATH: Mutex<Option<String>> = Mutex::new(None);

fn log(msg: &str) {
    let path = std::env::var("CANSHIM_LOG")
        .ok()
        .or_else(|| LOG_PATH.lock().ok().and_then(|g| g.clone()));
    if let Some(path) = path {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{msg}");
        }
    }
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
                *CONN.lock().unwrap_or_else(|e| e.into_inner()) = Some(c);
                1 // fixed non-negative handle
            }
            Err(e) => {
                log(&format!("canOpenChannel: connect failed: {e}"));
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
    _hnd: c_int,
    id: c_long,
    msg: *mut c_void,
    dlc: c_uint,
    flag: c_uint,
) -> c_int {
    guard(|| {
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
        let guard = CONN.lock().unwrap_or_else(|e| e.into_inner());
        let r = match guard.as_ref() {
            Some(c) => c.write(&f),
            None => return CAN_ERR_PARAM,
        };
        match r {
            Ok(()) => {
                log(&format!("canWrite id={id:#x} dlc={dlc} flag={flag:#x} -> ok"));
                CAN_OK
            }
            Err(e) => {
                log(&format!("canWrite id={id:#x} -> ERR {e}"));
                CAN_ERR_PARAM
            }
        }
    })
}

#[no_mangle]
pub extern "system" fn canRead(
    _hnd: c_int,
    id: *mut c_long,
    msg: *mut c_void,
    dlc: *mut c_uint,
    flag: *mut c_uint,
    time: *mut c_ulong,
) -> c_int {
    guard(|| {
        let frame = {
            let g = CONN.lock().unwrap_or_else(|e| e.into_inner());
            match g.as_ref() {
                Some(c) => c.read(),
                None => return CAN_ERR_PARAM,
            }
        };
        let f = match frame {
            Some(f) => f,
            None => return CAN_ERR_NOMSG,
        };
        let (oid, mut oflag) = wire::canid_to_kvaser(f.can_id, f.fd);
        if f.fd {
            oflag |= wire::fd_flags_to_kvaser(f.fd_flags); // BRS/ESI
        }
        unsafe {
            if !id.is_null() {
                *id = oid as c_long;
            }
            if !flag.is_null() {
                *flag = oflag as c_uint;
            }
            if !dlc.is_null() {
                *dlc = f.len as c_uint;
            }
            if !time.is_null() {
                *time = 0;
            }
            if !msg.is_null() && f.len > 0 && !f.is_rtr() {
                let dst = std::slice::from_raw_parts_mut(msg as *mut u8, f.len as usize);
                dst.copy_from_slice(&f.data[..f.len as usize]);
            }
        }
        CAN_OK
    })
}

#[no_mangle]
pub extern "system" fn canReadStatus(_hnd: c_int, flags: *mut c_ulong) -> c_int {
    unsafe {
        if !flags.is_null() {
            *flags = 0;
        }
    }
    CAN_OK
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
pub extern "system" fn canClose(_hnd: c_int) -> c_int {
    guard(|| {
        log("canClose");
        if let Some(mut c) = CONN.lock().unwrap_or_else(|e| e.into_inner()).take() {
            c.close();
        }
        CAN_OK
    })
}
