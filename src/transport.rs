// SPDX-License-Identifier: LGPL-3.0-or-later
//! Network transport: makes the shim a cannelloni peer over UDP or TCP.
//!
//! One channel = one [`Conn`]. A background RX thread decodes inbound cannelloni
//! traffic into a bounded ring; `canRead` drains it. `canWrite` encodes a single
//! frame and sends it (one-frame UDP datagram, or a headerless TCP stream write).

use std::collections::VecDeque;
use std::ffi::c_void;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::raw::{c_int, c_long, c_ulong};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::config::Config;
use crate::wire::{self, DecodeState, Decoded, Frame};

const RING_CAP: usize = 8192;

/// `canSetNotify` flag: notify on receive (canstat.h canNOTIFY_RX).
pub const NOTIFY_RX: u32 = 0x0001;
/// Event code passed to the notify callback for a received frame (canstat.h).
const EVENT_RX: c_int = 32000; // canEVENT_RX

/// Mirrors Kvaser's `canNotifyData` (canlib.h) for a receive event. Verified
/// against the real header (Windows + Linux): the struct is
/// `{ void* tag; int eventType; union { ...; struct { long id; unsigned long time; } rx; ... } info; }`.
/// For an RX event the C app reads `info.rx.id` / `info.rx.time`; the union's
/// `rx` member is just those two fields, so `id` / `time` here land exactly on
/// them. On *both* Windows targets `c_long` / `c_ulong` are 32-bit, so the
/// offsets and total size match (16 bytes on 32-bit, 24 on 64-bit). The
/// `#[cfg(windows)]` assertion below locks that so a future field edit cannot
/// silently break the ABI. (On the Linux host build `c_long` is 64-bit, hence
/// the gate - the ABI only matters for the shipped Windows DLL.)
#[repr(C)]
struct NotifyData {
    tag: *mut c_void,
    event_type: c_int,
    id: c_long,
    time: c_ulong,
}

#[cfg(windows)]
const _: () = {
    use std::mem::{offset_of, size_of};
    // canNotifyData.info.rx = { long id; unsigned long time; }, 32-bit on Windows.
    assert!(offset_of!(NotifyData, id) == size_of::<*const ()>() + 4);
    assert!(offset_of!(NotifyData, time) == size_of::<*const ()>() + 8);
    #[cfg(target_pointer_width = "32")]
    assert!(size_of::<NotifyData>() == 16);
    #[cfg(target_pointer_width = "64")]
    assert!(size_of::<NotifyData>() == 24);
};

/// Acceptance filter state (set via `canAccept`). A frame passes when
/// `(id & mask) == (code & mask)` for its id class; a zero mask accepts all,
/// which is the default so filtering is opt-in.
#[derive(Clone, Copy, Default)]
struct Accept {
    code_std: u32,
    mask_std: u32,
    code_ext: u32,
    mask_ext: u32,
}

impl Accept {
    fn accepts(&self, f: &Frame) -> bool {
        let ext = f.can_id & wire::CAN_EFF_FLAG != 0;
        let (id, code, mask) = if ext {
            (f.can_id & wire::CAN_EFF_MASK, self.code_ext, self.mask_ext)
        } else {
            (f.can_id & wire::CAN_SFF_MASK, self.code_std, self.mask_std)
        };
        (id & mask) == (code & mask)
    }
}

/// Registered notify callback. Stored as a `usize` so the struct stays `Send`;
/// the C function pointer is transmuted back only on the RX thread that calls it.
#[derive(Clone, Copy, Default)]
struct Notify {
    cb: usize,
    flags: u32,
    tag: usize,
}

/// State shared between the RX thread (producer) and the API (consumer): the
/// bounded ring plus a condvar for blocking reads, acceptance filters, and the
/// optional notify callback.
pub struct Shared {
    q: Mutex<VecDeque<Frame>>,
    cv: Condvar,
    /// Set by `close()` so blocked readers (`canReadWait`/`canReadSync`) wake and
    /// return instead of parking until their timeout (kvasilloni-hc9).
    closed: AtomicBool,
    filters: Mutex<Accept>,
    notify: Mutex<Notify>,
}

impl Shared {
    fn new() -> Arc<Shared> {
        Arc::new(Shared {
            q: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            closed: AtomicBool::new(false),
            filters: Mutex::new(Accept::default()),
            notify: Mutex::new(Notify::default()),
        })
    }

    /// Producer side: apply acceptance filtering, enqueue (dropping when full),
    /// wake any blocked reader, then fire the notify callback if armed for RX.
    fn push(&self, f: Frame) {
        if !self.filters.lock().unwrap_or_else(|e| e.into_inner()).accepts(&f) {
            return;
        }
        let notify_id = wire::canid_to_kvaser(f.can_id, f.fd).0;
        {
            let mut q = self.q.lock().unwrap_or_else(|e| e.into_inner());
            if q.len() >= RING_CAP {
                return; // ring full -> drop, no wakeup
            }
            q.push_back(f);
        }
        self.cv.notify_one();
        let n = *self.notify.lock().unwrap_or_else(|e| e.into_inner());
        if n.cb != 0 && n.flags & NOTIFY_RX != 0 {
            // SAFETY: cb is the function pointer the app passed to canSetNotify;
            // the app contracts to keep it valid until canClose / canSetNotify(NULL).
            unsafe {
                let cb: extern "system" fn(*mut NotifyData) = std::mem::transmute(n.cb);
                let mut d = NotifyData {
                    tag: n.tag as *mut c_void,
                    event_type: EVENT_RX,
                    id: notify_id as c_long,
                    time: 0,
                };
                cb(&mut d);
            }
        }
    }

    /// Non-blocking pop (backs `canRead`).
    pub fn pop(&self) -> Option<Frame> {
        self.q.lock().unwrap_or_else(|e| e.into_inner()).pop_front()
    }

    /// Block up to `timeout` for a frame, then pop it (backs `canReadWait`).
    /// Returns early (with whatever is queued, usually `None`) once `close()`
    /// marks the connection closed, so teardown never strands a reader.
    pub fn pop_wait(&self, timeout: Duration) -> Option<Frame> {
        let q = self.q.lock().unwrap_or_else(|e| e.into_inner());
        let (mut q, _) = self
            .cv
            .wait_timeout_while(q, timeout, |q| {
                q.is_empty() && !self.closed.load(Ordering::SeqCst)
            })
            .unwrap_or_else(|e| e.into_inner());
        q.pop_front()
    }

    /// Block up to `timeout` until a frame is available, without removing it
    /// (backs `canReadSync`). Returns true if one is available. Also returns
    /// (false, if empty) promptly once the connection is closed.
    pub fn peek_wait(&self, timeout: Duration) -> bool {
        let q = self.q.lock().unwrap_or_else(|e| e.into_inner());
        let (q, _) = self
            .cv
            .wait_timeout_while(q, timeout, |q| {
                q.is_empty() && !self.closed.load(Ordering::SeqCst)
            })
            .unwrap_or_else(|e| e.into_inner());
        !q.is_empty()
    }

    /// Mark the connection closed and wake every blocked reader. The flag is set
    /// under the `q` lock so a reader cannot park between checking the predicate
    /// and the wakeup (no lost-wakeup race). See kvasilloni-hc9.
    pub fn mark_closed(&self) {
        {
            let _g = self.q.lock().unwrap_or_else(|e| e.into_inner());
            self.closed.store(true, Ordering::SeqCst);
        }
        self.cv.notify_all();
    }

    /// Drop all queued frames (backs `canFlushReceiveQueue`).
    pub fn clear(&self) {
        self.q.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Current number of queued frames (backs canIOCTL_GET_RX_BUFFER_LEVEL).
    pub fn level(&self) -> usize {
        self.q.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Apply one `canAccept` directive (canFILTER_SET_{CODE,MASK}_{STD,EXT}).
    pub fn set_accept(&self, flag: u32, envelope: u32) {
        let mut a = self.filters.lock().unwrap_or_else(|e| e.into_inner());
        match flag {
            3 => a.code_std = envelope, // canFILTER_SET_CODE_STD
            4 => a.mask_std = envelope, // canFILTER_SET_MASK_STD
            5 => a.code_ext = envelope, // canFILTER_SET_CODE_EXT
            6 => a.mask_ext = envelope, // canFILTER_SET_MASK_EXT
            _ => {}
        }
    }

    /// Arm/replace the notify callback (backs `canSetNotify`; cb==0 disarms).
    pub fn set_notify(&self, cb: usize, flags: u32, tag: usize) {
        *self.notify.lock().unwrap_or_else(|e| e.into_inner()) = Notify { cb, flags, tag };
    }
}

enum Tx {
    Udp { sock: UdpSocket, remote: SocketAddr, seq: u8 },
    Tcp { stream: TcpStream },
}

pub struct Conn {
    tx: Mutex<Tx>,
    shared: Arc<Shared>,
    running: Arc<AtomicBool>,
    negotiated: Arc<AtomicBool>,
    rx_sock: RxStop,
    handle: Option<JoinHandle<()>>,
    /// Actual bound local port. For UDP this may differ from the configured
    /// `local_port` if that was busy and we fell back to ephemeral (kvasilloni-iai).
    local_port: u16,
}

/// Bind a UDP socket on `local_port`, falling back to an OS-assigned ephemeral
/// port if the requested one is already in use (e.g. a second instance of the
/// same app). The cannelloni peer learns our address from the datagrams we send
/// it (or is told via `-R`/`-p`), so a different source port still works.
fn bind_udp(local_port: u16) -> std::io::Result<UdpSocket> {
    match UdpSocket::bind(("0.0.0.0", local_port)) {
        Ok(s) => Ok(s),
        Err(ref e) if local_port != 0 && e.kind() == std::io::ErrorKind::AddrInUse => {
            UdpSocket::bind(("0.0.0.0", 0))
        }
        Err(e) => Err(e),
    }
}

/// Handle used by `close` to unblock the RX thread. The UDP variant is never
/// read - it just keeps the original bound socket alive for the connection's
/// lifetime; the RX loop stops via its read timeout. TCP is shut down explicitly.
enum RxStop {
    Udp(#[allow(dead_code)] UdpSocket),
    Tcp(TcpStream),
}

impl Conn {
    pub fn connect(cfg: &Config) -> std::io::Result<Conn> {
        let remote: SocketAddr = format!("{}:{}", cfg.host, cfg.remote_port)
            .parse()
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad host"))?;

        let shared = Shared::new();
        let running = Arc::new(AtomicBool::new(true));
        let negotiated = Arc::new(AtomicBool::new(false));

        if !cfg.tcp {
            // -------------------------------- UDP --------------------------------
            let sock = bind_udp(cfg.local_port)?;
            let local_port = sock.local_addr().map(|a| a.port()).unwrap_or(cfg.local_port);
            let tx_sock = sock.try_clone()?;
            let rx_sock = sock.try_clone()?;
            rx_sock.set_read_timeout(Some(Duration::from_millis(500)))?;
            negotiated.store(true, Ordering::SeqCst);

            let (rxq, run) = (shared.clone(), running.clone());
            let handle = std::thread::spawn(move || udp_rx_loop(rx_sock, rxq, run));
            return Ok(Conn {
                tx: Mutex::new(Tx::Udp { sock: tx_sock, remote, seq: 0 }),
                shared,
                running,
                negotiated,
                rx_sock: RxStop::Udp(sock), // original handle, used to stop the loop
                handle: Some(handle),
                local_port,
            });
        }

        // ---------------------------------- TCP ----------------------------------
        let connect_timeout = Duration::from_millis(cfg.connect_timeout_ms as u64);
        let stream = if cfg.tcp_server {
            let listener = TcpListener::bind(("0.0.0.0", cfg.local_port))?;
            listener.set_nonblocking(false)?;
            // Block for a client, but not forever.
            let (s, _peer) =
                accept_with_timeout(&listener, Duration::from_millis(cfg.accept_timeout_ms as u64))?;
            s
        } else {
            TcpStream::connect_timeout(&remote, connect_timeout)?
        };
        stream.set_nodelay(true).ok();

        // Symmetric handshake: send + expect "CANNELLONIv1", bounded by the
        // connect timeout so a peer that connects but never sends the banner
        // can't hang the open.
        handshake(&stream, connect_timeout)?;
        negotiated.store(true, Ordering::SeqCst);

        let local_port = stream.local_addr().map(|a| a.port()).unwrap_or(cfg.local_port);
        let rx_stream = stream.try_clone()?;
        let tx_stream = stream.try_clone()?;
        let stop_stream = stream;

        let (rxq, run) = (shared.clone(), running.clone());
        let handle = std::thread::spawn(move || tcp_rx_loop(rx_stream, rxq, run));

        Ok(Conn {
            tx: Mutex::new(Tx::Tcp { stream: tx_stream }),
            shared,
            running,
            negotiated,
            rx_sock: RxStop::Tcp(stop_stream),
            handle: Some(handle),
            local_port,
        })
    }

    /// Actual bound local port (UDP may differ from the configured one after an
    /// ephemeral fallback; kvasilloni-iai). Used by `canOpenChannel` logging.
    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    pub fn is_ready(&self) -> bool {
        self.negotiated.load(Ordering::SeqCst)
    }

    pub fn write(&self, f: &Frame) -> std::io::Result<()> {
        if !self.is_ready() {
            return Err(std::io::Error::new(std::io::ErrorKind::NotConnected, "not negotiated"));
        }
        let mut tx = self.tx.lock().map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::Other, "tx lock poisoned")
        })?;
        match &mut *tx {
            Tx::Udp { sock, remote, seq } => {
                let pkt = wire::build_udp(f, *seq);
                *seq = seq.wrapping_add(1);
                let n = sock.send_to(&pkt, *remote)?;
                if n != pkt.len() {
                    return Err(std::io::Error::new(std::io::ErrorKind::Other, "short udp send"));
                }
            }
            Tx::Tcp { stream } => {
                let mut buf = Vec::with_capacity(16);
                wire::encode_frame(&mut buf, f);
                stream.write_all(&buf)?;
            }
        }
        Ok(())
    }

    pub fn read(&self) -> Option<Frame> {
        self.shared.pop()
    }

    /// Clone of the shared RX state, so a blocking read (`canReadWait` /
    /// `canReadSync`) can wait on the condvar *without* holding the global
    /// `CONN` lock - otherwise `canClose` and concurrent calls would stall.
    pub fn rx_shared(&self) -> Arc<Shared> {
        self.shared.clone()
    }

    /// Drop all queued RX frames (`canFlushReceiveQueue`).
    pub fn clear_rx(&self) {
        self.shared.clear();
    }

    /// Number of queued RX frames (canIOCTL_GET_RX_BUFFER_LEVEL).
    pub fn rx_level(&self) -> usize {
        self.shared.level()
    }

    /// Apply one `canAccept` filter directive.
    pub fn set_accept(&self, flag: u32, envelope: u32) {
        self.shared.set_accept(flag, envelope);
    }

    /// Arm/replace the `canSetNotify` callback (cb==0 disarms).
    pub fn set_notify(&self, cb: usize, flags: u32, tag: usize) {
        self.shared.set_notify(cb, flags, tag);
    }

    pub fn close(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        self.negotiated.store(false, Ordering::SeqCst);
        self.shared.mark_closed(); // wake blocked canReadWait/canReadSync (kvasilloni-hc9)
        match &self.rx_sock {
            RxStop::Tcp(s) => {
                let _ = s.shutdown(Shutdown::Both);
            }
            RxStop::Udp(_) => { /* the 500ms read timeout lets the loop exit */ }
        }
        if let Some(h) = self.handle.take() {
            // If close() runs ON the RX thread itself - e.g. an app's
            // canSetNotify callback (which fires on the RX thread) calls
            // canClose - then joining would deadlock the thread on itself.
            // Detach instead: running=false plus the socket shutdown make the
            // loop exit once the callback returns. See kvasilloni-cqe.
            if h.thread().id() == std::thread::current().id() {
                // running on the RX thread: detach, do not join
            } else {
                let _ = h.join();
            }
        }
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.close();
    }
}

fn handshake(stream: &TcpStream, timeout: Duration) -> std::io::Result<()> {
    let mut s = stream.try_clone()?;
    s.set_read_timeout(Some(timeout))?;
    s.write_all(wire::CONNECT_V1)?;
    let mut buf = [0u8; 12];
    s.read_exact(&mut buf)?;
    if buf != wire::CONNECT_V1 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "bad handshake"));
    }
    s.set_read_timeout(None)?;
    Ok(())
}

fn accept_with_timeout(l: &TcpListener, total: Duration) -> std::io::Result<(TcpStream, SocketAddr)> {
    l.set_nonblocking(true)?;
    let start = std::time::Instant::now();
    loop {
        match l.accept() {
            Ok((s, a)) => {
                s.set_nonblocking(false)?;
                return Ok((s, a));
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if start.elapsed() > total {
                    return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "no client"));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e),
        }
    }
}

fn udp_rx_loop(sock: UdpSocket, rx: Arc<Shared>, running: Arc<AtomicBool>) {
    let mut buf = [0u8; 2048];
    while running.load(Ordering::SeqCst) {
        match sock.recv_from(&mut buf) {
            Ok((n, _from)) => {
                // Contain any panic in the decode/enqueue path so a single bad
                // datagram can never kill the RX thread (kvasilloni-uzk). A panic
                // *inside* the app's extern "system" notify callback aborts at the
                // FFI boundary by Rust's rules - that is the app's contract, not
                // something we can or should resume from.
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    if let Some(frames) = wire::parse_udp(&buf[..n]) {
                        for f in frames {
                            rx.push(f);
                        }
                    }
                }));
            }
            Err(ref e)
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
            {
                continue;
            }
            Err(_) => break,
        }
    }
}

fn tcp_rx_loop(mut stream: TcpStream, rx: Arc<Shared>, running: Arc<AtomicBool>) {
    let mut f = Frame::default();
    let mut st = DecodeState::Init;
    // prime: Init -> asks for the CAN_ID size
    let mut need = match wire::decode_stream(&[], &mut f, &mut st) {
        Decoded::Need(n) => n,
        _ => return,
    };
    let mut chunk = [0u8; 80];
    while running.load(Ordering::SeqCst) {
        if need == 0 || need > chunk.len() {
            break;
        }
        if stream.read_exact(&mut chunk[..need]).is_err() {
            break; // peer closed or socket shut down by close()
        }
        // Contain a decoder panic so a malformed stream can't kill RX
        // (kvasilloni-uzk); decode_stream is the untrusted-parse surface.
        let decoded = catch_unwind(AssertUnwindSafe(|| wire::decode_stream(&chunk[..need], &mut f, &mut st)))
            .unwrap_or(Decoded::Error);
        match decoded {
            Decoded::Need(n) => need = n,
            Decoded::Complete => {
                rx.push(f);
                f = Frame::default();
                st = DecodeState::Init;
                need = match wire::decode_stream(&[], &mut f, &mut st) {
                    Decoded::Need(n) => n,
                    _ => break,
                };
            }
            Decoded::Error => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(can_id: u32) -> Frame {
        Frame { can_id, len: 1, fd: false, fd_flags: 0, data: [0u8; 64] }
    }

    #[test]
    fn push_pop_and_clear() {
        let s = Shared::new();
        s.push(frame(0x100));
        s.push(frame(0x200));
        assert_eq!(s.level(), 2);
        assert_eq!(s.pop().map(|f| f.can_id), Some(0x100));
        s.clear();
        assert_eq!(s.level(), 0);
        assert!(s.pop().is_none());
    }

    #[test]
    fn accept_filter_drops_nonmatching() {
        let s = Shared::new();
        // Only accept standard ids equal to 0x123 (exact match mask).
        s.set_accept(3, 0x123); // SET_CODE_STD
        s.set_accept(4, wire::CAN_SFF_MASK); // SET_MASK_STD
        s.push(frame(0x123));
        s.push(frame(0x124));
        assert_eq!(s.level(), 1);
        assert_eq!(s.pop().map(|f| f.can_id), Some(0x123));
    }

    #[test]
    fn ext_filter_independent_of_std() {
        let s = Shared::new();
        s.set_accept(5, 0x18EE_FF00); // SET_CODE_EXT
        s.set_accept(6, wire::CAN_EFF_MASK); // SET_MASK_EXT
        // Standard frames still pass (std mask defaults to 0 = accept all).
        s.push(frame(0x123));
        // Extended frame matching the code passes; non-matching is dropped.
        s.push(frame(0x18EE_FF00 | wire::CAN_EFF_FLAG));
        s.push(frame(0x18EE_FF01 | wire::CAN_EFF_FLAG));
        assert_eq!(s.level(), 2);
    }

    #[test]
    fn pop_wait_times_out_when_empty() {
        let s = Shared::new();
        assert!(s.pop_wait(Duration::from_millis(20)).is_none());
    }

    #[test]
    fn pop_wait_returns_pushed_frame() {
        let s = Shared::new();
        let s2 = s.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            s2.push(frame(0x321));
        });
        let got = s.pop_wait(Duration::from_secs(2));
        t.join().unwrap();
        assert_eq!(got.map(|f| f.can_id), Some(0x321));
    }

    #[test]
    fn peek_wait_leaves_frame_queued() {
        let s = Shared::new();
        s.push(frame(0x55));
        assert!(s.peek_wait(Duration::from_millis(10)));
        assert_eq!(s.level(), 1); // not consumed
    }

    #[test]
    fn mark_closed_wakes_blocked_pop_wait() {
        // Regression for kvasilloni-hc9: a reader blocked with a long timeout
        // must return promptly once the connection is marked closed, not park
        // for the full duration.
        let s = Shared::new();
        let s2 = s.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            s2.mark_closed();
        });
        let start = std::time::Instant::now();
        let got = s.pop_wait(Duration::from_secs(30)); // would hang ~30s without the fix
        t.join().unwrap();
        assert!(got.is_none());
        assert!(start.elapsed() < Duration::from_secs(5), "pop_wait did not wake on close");
    }

    #[test]
    fn mark_closed_wakes_blocked_peek_wait() {
        let s = Shared::new();
        let s2 = s.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            s2.mark_closed();
        });
        let start = std::time::Instant::now();
        let ready = s.peek_wait(Duration::from_secs(30));
        t.join().unwrap();
        assert!(!ready);
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    static NOTIFY_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static NOTIFY_LAST_ID: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

    extern "system" fn test_cb(d: *mut NotifyData) {
        unsafe {
            NOTIFY_COUNT.fetch_add(1, Ordering::SeqCst);
            NOTIFY_LAST_ID.store((*d).id as i64, Ordering::SeqCst);
        }
    }

    #[test]
    fn notify_fires_for_each_rx_when_armed() {
        let s = Shared::new();
        NOTIFY_COUNT.store(0, Ordering::SeqCst);
        s.set_notify(test_cb as *const () as usize, NOTIFY_RX, 0);
        s.push(frame(0x111));
        s.push(frame(0x222));
        assert_eq!(NOTIFY_COUNT.load(Ordering::SeqCst), 2);
        assert_eq!(NOTIFY_LAST_ID.load(Ordering::SeqCst), 0x222);
        // Disarm: no further callbacks.
        s.set_notify(0, 0, 0);
        s.push(frame(0x333));
        assert_eq!(NOTIFY_COUNT.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn notify_silent_when_flag_not_set() {
        let s = Shared::new();
        NOTIFY_COUNT.store(0, Ordering::SeqCst);
        s.set_notify(test_cb as *const () as usize, 0 /* no NOTIFY_RX */, 0);
        s.push(frame(0x111));
        assert_eq!(NOTIFY_COUNT.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn notify_data_field_order() {
        // The strict Windows offsets/size are checked at compile time by the
        // #[cfg(windows)] const assertion next to NotifyData. Here, on the host
        // build, assert the platform-independent invariant: `time` immediately
        // follows `id` by one `c_long`, matching canNotifyData.info.rx layout.
        use std::mem::offset_of;
        assert!(offset_of!(NotifyData, time) > offset_of!(NotifyData, id));
        assert_eq!(
            offset_of!(NotifyData, time) - offset_of!(NotifyData, id),
            std::mem::size_of::<c_long>()
        );
    }
}
