// SPDX-License-Identifier: LGPL-3.0-or-later
//! Network transport: makes the shim a cannelloni peer over UDP or TCP.
//!
//! One channel = one [`Conn`]. A background RX thread decodes inbound cannelloni
//! traffic into a bounded ring; `canRead` drains it. `canWrite` encodes a single
//! frame and sends it (one-frame UDP datagram, or a headerless TCP stream write).

use std::collections::VecDeque;
use std::ffi::c_void;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::raw::{c_int, c_long, c_ulong};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::wire::{self, DecodeState, Decoded, Frame};

const RING_CAP: usize = 8192;

/// `canSetNotify` flag: notify on receive (canstat.h canNOTIFY_RX).
pub const NOTIFY_RX: u32 = 0x0001;
/// Event code passed to the notify callback for a received frame (canstat.h).
const EVENT_RX: c_int = 32000; // canEVENT_RX

/// Milliseconds since the first call (a process-wide monotonic epoch). Used to
/// stamp RX frames so `canRead`/`canReadWait`/notify can report a receive time
/// instead of a constant 0. ms matches the timer scale the shim reports from
/// canIOCTL_GET_TIMER_SCALE (1000 us/tick). Wraps after ~49 days on the 32-bit
/// `c_ulong` the app sees, exactly as Kvaser's own timer does. See kvasilloni-kha.
fn now_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

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
    /// Latched when a frame is dropped because the ring was full, so the app can
    /// learn it fell behind via `canReadStatus` (canSTAT_SW_OVERRUN). Cleared on
    /// `clear()` (canFlushReceiveQueue). See kvasilloni-tlm.
    overflow: AtomicBool,
    filters: Mutex<Accept>,
    notify: Mutex<Notify>,
}

impl Shared {
    fn new() -> Arc<Shared> {
        Arc::new(Shared {
            q: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            closed: AtomicBool::new(false),
            overflow: AtomicBool::new(false),
            filters: Mutex::new(Accept::default()),
            notify: Mutex::new(Notify::default()),
        })
    }

    /// Producer side: apply acceptance filtering, enqueue (dropping when full),
    /// wake any blocked reader, then fire the notify callback if armed for RX.
    fn push(&self, mut f: Frame) {
        if !self.filters.lock().unwrap_or_else(|e| e.into_inner()).accepts(&f) {
            return;
        }
        f.rx_time_ms = now_ms(); // receive timestamp (kvasilloni-kha)
        let notify_id = wire::canid_to_kvaser(f.can_id, f.fd).0;
        let notify_time = f.rx_time_ms;
        {
            let mut q = self.q.lock().unwrap_or_else(|e| e.into_inner());
            if q.len() >= RING_CAP {
                self.overflow.store(true, Ordering::SeqCst); // dropped: latch overrun
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
                    time: notify_time as c_ulong,
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

    /// Drop all queued frames (backs `canFlushReceiveQueue`). Also clears the
    /// overrun latch: flushing means the app is resyncing. (kvasilloni-tlm)
    pub fn clear(&self) {
        self.q.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.overflow.store(false, Ordering::SeqCst);
    }

    /// Current number of queued frames (backs canIOCTL_GET_RX_BUFFER_LEVEL).
    pub fn level(&self) -> usize {
        self.q.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Whether at least one frame has been dropped due to a full ring since the
    /// last `clear()` (backs canReadStatus canSTAT_SW_OVERRUN). See kvasilloni-tlm.
    pub fn overflowed(&self) -> bool {
        self.overflow.load(Ordering::SeqCst)
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
    /// RX-thread join handle behind a `Mutex<Option>` so `close()` takes `&self`:
    /// the `Conn` lives behind an `Arc` in the handle table, and `canWrite` clones
    /// that `Arc` to send *without* holding the global map lock (kvasilloni-lo7).
    /// A `&mut self` close would make that sharing impossible.
    handle: Mutex<Option<JoinHandle<()>>>,
    /// Actual bound local port. For UDP this may differ from the configured
    /// `local_port` if that was busy and we fell back to ephemeral (kvasilloni-iai).
    local_port: u16,
    /// Whether the channel was opened for CAN FD (canOPEN_CAN_FD). Gates how wide
    /// RX delivery may be: a classic channel never returns more than 8 data bytes,
    /// so a (legitimately up-to-64-byte) FD frame cannot overflow a classic app's
    /// 8-byte `canRead` buffer. Set by `canOpenChannel` from the open flags; the
    /// Kvaser `canRead` ABI has no buffer-length argument (kvasilloni-nmt).
    fd_capable: AtomicBool,
}

/// The unspecified ("any") address of the same family as `peer` - `0.0.0.0` for
/// IPv4, `::` for IPv6 - so the shim binds the family of the configured host
/// instead of being hard-wired to IPv4. (kvasilloni-c3x)
fn wildcard_for(peer: IpAddr) -> IpAddr {
    match peer {
        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    }
}

/// Bind a UDP socket on `bind`. If the requested port is busy and
/// `allow_fallback` is set, bind an OS-assigned ephemeral port of the same
/// family instead of failing.
///
/// The fallback is OFF by default (kvasilloni-25q): a stock cannelloni replies
/// only to its fixed `-r` port, so an ephemeral source port receives nothing -
/// the channel would be silently TX-only. Opt in via `udp_port_fallback` only if
/// that is acceptable. In unit tests it is always allowed: the handle-table
/// stress test opens many channels on the same default port with no real peer.
fn bind_udp(bind: SocketAddr, allow_fallback: bool) -> std::io::Result<UdpSocket> {
    let allow_fallback = allow_fallback || cfg!(test);
    match UdpSocket::bind(bind) {
        Ok(s) => Ok(s),
        Err(ref e) if allow_fallback && bind.port() != 0 && e.kind() == std::io::ErrorKind::AddrInUse => {
            UdpSocket::bind(SocketAddr::new(bind.ip(), 0))
        }
        Err(e) => Err(e),
    }
}

/// The set of peer IPs allowed to send to us: an explicit `allow` list if given,
/// otherwise just the configured peer. Only consulted when `peer_check` is on.
fn effective_allow(cfg: &Config, peer: IpAddr) -> Vec<IpAddr> {
    if cfg.allow.is_empty() {
        vec![peer]
    } else {
        cfg.allow.clone()
    }
}

/// Whether a datagram/connection from `from` is allowed in. `check` off => all.
fn peer_allowed(from: IpAddr, allow: &[IpAddr], check: bool) -> bool {
    !check || allow.iter().any(|a| *a == from)
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
        // Parse the host as a bare IP (v4 or v6) so an IPv6 literal works without
        // the bracket dance a SocketAddr string parse would need. (kvasilloni-c3x)
        let peer_ip: IpAddr = cfg
            .host
            .trim()
            .parse()
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad host"))?;
        let remote = SocketAddr::new(peer_ip, cfg.remote_port);
        let wildcard = wildcard_for(peer_ip);
        let allow = effective_allow(cfg, peer_ip);
        let peer_check = cfg.peer_check;

        let shared = Shared::new();
        let running = Arc::new(AtomicBool::new(true));
        let negotiated = Arc::new(AtomicBool::new(false));

        if !cfg.tcp {
            // -------------------------------- UDP --------------------------------
            let sock = bind_udp(SocketAddr::new(wildcard, cfg.local_port), cfg.udp_port_fallback)?;
            let local_port = sock.local_addr().map(|a| a.port()).unwrap_or(cfg.local_port);
            let tx_sock = sock.try_clone()?;
            let rx_sock = sock.try_clone()?;
            rx_sock.set_read_timeout(Some(Duration::from_millis(500)))?;
            negotiated.store(true, Ordering::SeqCst);

            let (rxq, run) = (shared.clone(), running.clone());
            let handle = std::thread::spawn(move || udp_rx_loop(rx_sock, rxq, run, allow, peer_check));
            return Ok(Conn {
                tx: Mutex::new(Tx::Udp { sock: tx_sock, remote, seq: 0 }),
                shared,
                running,
                negotiated,
                rx_sock: RxStop::Udp(sock), // original handle, used to stop the loop
                handle: Mutex::new(Some(handle)),
                local_port,
                fd_capable: AtomicBool::new(false),
            });
        }

        // ---------------------------------- TCP ----------------------------------
        let connect_timeout = Duration::from_millis(cfg.connect_timeout_ms as u64);
        let stream = if cfg.tcp_server {
            let listener = TcpListener::bind(SocketAddr::new(wildcard, cfg.local_port))?;
            listener.set_nonblocking(false)?;
            // Block for an allowed client, but not forever.
            let (s, _peer) = accept_with_timeout(
                &listener,
                Duration::from_millis(cfg.accept_timeout_ms as u64),
                &allow,
                peer_check,
            )?;
            s
        } else {
            TcpStream::connect_timeout(&remote, connect_timeout)?
        };
        stream.set_nodelay(true).ok();
        // Bound blocking writes so a stalled peer makes canWrite fail instead of
        // parking forever (kvasilloni-lo7). Reuses the connect timeout budget.
        stream.set_write_timeout(Some(connect_timeout)).ok();

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
            handle: Mutex::new(Some(handle)),
            local_port,
            fd_capable: AtomicBool::new(false),
        })
    }

    /// Record whether this channel was opened for CAN FD (canOPEN_CAN_FD). When
    /// false, RX delivery is capped at 8 data bytes so an FD frame cannot overflow
    /// a classic caller's `canRead` buffer (kvasilloni-nmt).
    pub fn set_fd_capable(&self, yes: bool) {
        self.fd_capable.store(yes, Ordering::SeqCst);
    }

    /// Whether the channel was opened for CAN FD (see `set_fd_capable`).
    pub fn fd_capable(&self) -> bool {
        self.fd_capable.load(Ordering::SeqCst)
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
                if let Err(e) = stream.write_all(&buf) {
                    // A timed-out or partial write leaves the headerless stream
                    // mid-frame; never stream more bytes into a desynced socket.
                    // Tear the connection down so this and future writes fail
                    // fast and the app can reopen, and the RX loop exits too.
                    // (kvasilloni-lo7)
                    self.negotiated.store(false, Ordering::SeqCst);
                    self.running.store(false, Ordering::SeqCst);
                    let _ = stream.shutdown(Shutdown::Both);
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    pub fn read(&self) -> Option<Frame> {
        self.shared.pop()
    }

    /// Whether RX frames have been dropped due to a full ring (canSTAT_SW_OVERRUN).
    pub fn rx_overflowed(&self) -> bool {
        self.shared.overflowed()
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

    pub fn close(&self) {
        self.running.store(false, Ordering::SeqCst);
        self.negotiated.store(false, Ordering::SeqCst);
        self.shared.mark_closed(); // wake blocked canReadWait/canReadSync (kvasilloni-hc9)
        match &self.rx_sock {
            RxStop::Tcp(s) => {
                let _ = s.shutdown(Shutdown::Both);
            }
            RxStop::Udp(_) => { /* the 500ms read timeout lets the loop exit */ }
        }
        // Take the join handle under its lock; only the first close() joins (a
        // later Drop sees None). idempotent across canClose + Drop.
        let h = self.handle.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(h) = h {
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

fn accept_with_timeout(
    l: &TcpListener,
    total: Duration,
    allow: &[IpAddr],
    peer_check: bool,
) -> std::io::Result<(TcpStream, SocketAddr)> {
    l.set_nonblocking(true)?;
    let start = Instant::now();
    loop {
        match l.accept() {
            Ok((s, a)) => {
                // Reject connections from a non-allowed source (cannelloni's -R
                // default; kvasilloni-872) and keep waiting for an allowed one.
                if !peer_allowed(a.ip(), allow, peer_check) {
                    let _ = s.shutdown(Shutdown::Both);
                    if start.elapsed() > total {
                        return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "no allowed client"));
                    }
                    continue;
                }
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

// Test-only fault injection for the RX panic firewall (kvasilloni-uzk/-ehp).
// With the kkt fix in place nothing in the decode/push path panics on its own,
// so the `catch_unwind` firewall would otherwise be untested. When armed, the
// next guarded ingest panics, letting a test prove the firewall contains it.
// Thread-local so parallel tests don't disturb each other; compiled out of every
// non-test build, so the shipped DLL carries none of this.
#[cfg(test)]
thread_local! {
    static INJECT_RX_PANIC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Arm a one-shot panic in the next guarded RX ingest on this thread.
#[cfg(test)]
fn arm_rx_panic() {
    INJECT_RX_PANIC.with(|c| c.set(true));
}

/// Panic exactly once if armed; the only place test fault-injection enters the
/// guarded ingest path. A no-op (and fully inlined away) in release builds.
#[cfg(test)]
#[inline]
fn rx_panic_hook() {
    if INJECT_RX_PANIC.with(|c| c.replace(false)) {
        panic!("injected RX decode panic (kvasilloni-uzk firewall test)");
    }
}
#[cfg(not(test))]
#[inline(always)]
fn rx_panic_hook() {}

/// Decode one inbound UDP datagram and enqueue its frames, with any panic in the
/// untrusted parse/push path contained so a single bad datagram can never kill
/// the RX thread (kvasilloni-uzk). A panic *inside* the app's extern "system"
/// notify callback still aborts at the FFI boundary by Rust's rules - that is the
/// app's contract, not something we can or should resume from. Shared by the loop
/// and the firewall test (kvasilloni-ehp) so the test exercises the real guard.
fn guarded_udp_ingest(buf: &[u8], rx: &Arc<Shared>) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        rx_panic_hook(); // no-op outside tests
        if let Some(frames) = wire::parse_udp(buf) {
            for f in frames {
                rx.push(f);
            }
        }
    }));
}

fn udp_rx_loop(
    sock: UdpSocket,
    rx: Arc<Shared>,
    running: Arc<AtomicBool>,
    allow: Vec<IpAddr>,
    peer_check: bool,
) {
    let mut buf = [0u8; 2048];
    while running.load(Ordering::SeqCst) {
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => {
                // Drop datagrams from a non-allowed source (cannelloni's -R
                // default; kvasilloni-872) before they reach the parser.
                if peer_allowed(from.ip(), &allow, peer_check) {
                    guarded_udp_ingest(&buf[..n], &rx);
                }
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

/// Decode one TCP stream chunk, containing a decoder panic so a malformed stream
/// can't kill the RX thread (kvasilloni-uzk). A panic surfaces as `Decoded::Error`
/// which the loop treats as an unrecoverable stream (a byte stream cannot resync
/// like independent UDP datagrams can), so the connection tears down cleanly
/// instead of crashing. Shared with the firewall test (kvasilloni-ehp).
fn guarded_decode_stream(chunk: &[u8], f: &mut Frame, st: &mut DecodeState) -> Decoded {
    catch_unwind(AssertUnwindSafe(|| {
        rx_panic_hook(); // no-op outside tests
        wire::decode_stream(chunk, f, st)
    }))
    .unwrap_or(Decoded::Error)
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
        let decoded = guarded_decode_stream(&chunk[..need], &mut f, &mut st);
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
        Frame { can_id, len: 1, fd: false, fd_flags: 0, data: [0u8; 64], rx_time_ms: 0 }
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
    fn overflow_latches_on_full_ring_and_clears_on_flush() {
        // kvasilloni-tlm: dropping a frame because the ring is full must latch an
        // overrun the app can see via canReadStatus, and a flush must clear it.
        let s = Shared::new();
        assert!(!s.overflowed());
        for i in 0..(RING_CAP + 10) {
            s.push(frame(0x100 + i as u32));
        }
        assert!(s.overflowed(), "full ring did not latch overrun");
        assert_eq!(s.level(), RING_CAP, "ring grew past its cap");
        s.clear();
        assert!(!s.overflowed(), "flush did not clear the overrun latch");
    }

    #[test]
    fn push_stamps_receive_timestamp() {
        // kvasilloni-kha: an enqueued frame carries a non-default RX timestamp so
        // canRead/canReadWait can report it. (now_ms is monotonic from first use.)
        let s = Shared::new();
        let _ = now_ms(); // ensure the epoch is initialized before we sleep
        std::thread::sleep(Duration::from_millis(2));
        s.push(frame(0x7FF));
        let f = s.pop().expect("frame");
        assert!(f.rx_time_ms > 0, "receive timestamp was not stamped");
    }

    #[test]
    fn peer_allowed_matches_only_listed_ips_unless_disabled() {
        // kvasilloni-872: with the check on, only allow-listed sources pass; with
        // it off, anyone does.
        let a: IpAddr = "127.0.0.1".parse().unwrap();
        let b: IpAddr = "10.0.0.9".parse().unwrap();
        let allow = [a];
        assert!(peer_allowed(a, &allow, true));
        assert!(!peer_allowed(b, &allow, true));
        assert!(peer_allowed(b, &allow, false)); // check disabled => all pass
    }

    #[test]
    fn wildcard_matches_peer_family() {
        // kvasilloni-c3x: we bind the unspecified address of the peer's family.
        assert_eq!(wildcard_for("127.0.0.1".parse().unwrap()), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(wildcard_for("::1".parse().unwrap()), IpAddr::V6(Ipv6Addr::UNSPECIFIED));
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
    fn rx_firewall_contains_injected_panic_udp() {
        // kvasilloni-ehp: with kkt fixed, nothing in the decode/push path panics
        // on its own, so the catch_unwind firewall (uzk) was never exercised.
        // Force a panic in the guarded ingest and prove (a) it does not unwind
        // out of the guard and (b) the RX path keeps delivering afterwards - i.e.
        // a real RX thread would log-and-continue rather than die.
        let s = Shared::new();
        let pkt = wire::build_udp(&frame(0x123), 0);

        arm_rx_panic();
        guarded_udp_ingest(&pkt, &s); // panics inside the guard; must be contained
        assert_eq!(s.level(), 0, "panicking ingest must not enqueue");

        // Firewall held: the next valid datagram is still decoded and delivered.
        guarded_udp_ingest(&pkt, &s);
        assert_eq!(s.pop().map(|f| f.can_id), Some(0x123));
    }

    #[test]
    fn rx_firewall_contains_injected_panic_tcp() {
        // The TCP decoder is the other untrusted-parse surface. A panic there is
        // contained and surfaced as Decoded::Error (the loop then tears down the
        // unresyncable stream) instead of unwinding and killing the RX thread.
        let mut f = Frame::default();
        let mut st = DecodeState::Init;
        arm_rx_panic();
        let d = guarded_decode_stream(&[0u8; 4], &mut f, &mut st);
        assert!(matches!(d, Decoded::Error), "panic must surface as Error, not unwind");
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
