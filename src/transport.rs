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

// While this thread is inside a notify callback, a re-entrant `canSetNotify`
// (the callback re-arming or disarming itself) skips the drain in `set_notify`
// instead of deadlocking on `cb_active`, which the same thread already holds.
// The new callback then takes effect on the next event. See kvasilloni-4vc.
thread_local! {
    static IN_NOTIFY_CB: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the current thread is executing inside a notify callback - i.e. on the
/// RX (producer) thread, mid-callback. A blocking read issued from there would
/// park the only producer, so `canReadWait`/`canReadSync` fall back to a
/// non-blocking poll instead of stalling (kvasilloni-2qh).
pub fn in_notify_callback() -> bool {
    IN_NOTIFY_CB.with(|f| f.get())
}

/// RAII marker for "this thread is currently executing a notify callback".
/// Resets the flag even if the callback unwinds.
struct CbScope;
impl CbScope {
    fn enter() -> Self {
        IN_NOTIFY_CB.with(|f| f.set(true));
        CbScope
    }
}
impl Drop for CbScope {
    fn drop(&mut self) {
        IN_NOTIFY_CB.with(|f| f.set(false));
    }
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
    /// Held by the RX thread across reading *and* invoking the notify callback.
    /// `set_notify` (canSetNotify disarm/replace from another thread) drains this
    /// after swapping the stored callback, so once it returns the old callback is
    /// no longer running and the app may free its context without a use-after-free
    /// (kvasilloni-4vc).
    cb_active: Mutex<()>,
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
            cb_active: Mutex::new(()),
        })
    }

    /// Producer side: apply acceptance filtering, enqueue (dropping when full),
    /// wake any blocked reader, then fire the notify callback if armed for RX.
    /// `pub` so tests can inject RX frames without a live peer (kvasilloni-im6.6);
    /// in production only the RX loops call it.
    pub fn push(&self, mut f: Frame) {
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
        // Hold cb_active across BOTH the read of the callback and its invocation.
        // A concurrent canSetNotify disarm/replace drains cb_active *after*
        // swapping the stored callback, so either it waits for this call to finish
        // or we observe the new (e.g. disarmed) value here - never calling a stale
        // callback after canSetNotify returned and the app freed its context
        // (kvasilloni-4vc). cb_active is taken before the notify lock; set_notify
        // releases the notify lock before draining, so the two never deadlock.
        let _active = self.cb_active.lock().unwrap_or_else(|e| e.into_inner());
        let n = *self.notify.lock().unwrap_or_else(|e| e.into_inner());
        if n.cb != 0 && n.flags & NOTIFY_RX != 0 {
            let _scope = CbScope::enter(); // re-entrant canSetNotify skips the drain
            // SAFETY: cb is the function pointer the app passed to canSetNotify;
            // the app contracts to keep it valid until canClose / canSetNotify(NULL),
            // and the cb_active drain makes that disarm boundary race-free.
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
    ///
    /// After swapping the stored callback, drain `cb_active` so that once this
    /// returns the *previous* callback is no longer executing - the app may then
    /// free the callback's context without a use-after-free (kvasilloni-4vc). The
    /// drain is skipped when called from within a callback on the RX thread (the
    /// callback re-arming itself): that thread already holds `cb_active`, so
    /// draining would self-deadlock; the new callback applies to the next event.
    pub fn set_notify(&self, cb: usize, flags: u32, tag: usize) {
        *self.notify.lock().unwrap_or_else(|e| e.into_inner()) = Notify { cb, flags, tag };
        if !IN_NOTIFY_CB.with(|f| f.get()) {
            let _drain = self.cb_active.lock().unwrap_or_else(|e| e.into_inner());
        }
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
        // kvasilloni-kha: an enqueued frame is stamped with the receive time.
        // Bracket the push between two now_ms() reads and assert the stamp lands in
        // [t0, t1] - not merely >0, which a hard-coded constant would also satisfy
        // (kvasilloni-im6.2c). The 2ms sleep guarantees t0 > 0 so an un-stamped
        // (rx_time_ms == 0) frame is actually rejected by the lower bound.
        let s = Shared::new();
        let _ = now_ms(); // initialize the monotonic epoch
        std::thread::sleep(Duration::from_millis(2)); // let it advance so t0 > 0
        let t0 = now_ms();
        s.push(frame(0x7FF));
        let t1 = now_ms();
        let f = s.pop().expect("frame");
        assert!(
            t0 <= f.rx_time_ms && f.rx_time_ms <= t1,
            "stamp {} not within the push window [{t0}, {t1}]",
            f.rx_time_ms
        );
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

    // Each notify test owns a private callback + statics so nothing mutable is
    // shared across tests. `cargo test` runs them on parallel threads; a shared
    // counter (as before) let one test's store(0)/increments bleed into another's
    // assert - a latent flake that passed only by luck (kvasilloni-im6.1).
    static FIRES_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static FIRES_LAST_ID: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

    extern "system" fn cb_fires(d: *mut NotifyData) {
        unsafe {
            FIRES_COUNT.fetch_add(1, Ordering::SeqCst);
            FIRES_LAST_ID.store((*d).id as i64, Ordering::SeqCst);
        }
    }

    static SILENT_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    extern "system" fn cb_silent(_d: *mut NotifyData) {
        SILENT_COUNT.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn notify_fires_for_each_rx_when_armed() {
        let s = Shared::new();
        s.set_notify(cb_fires as *const () as usize, NOTIFY_RX, 0);
        s.push(frame(0x111));
        s.push(frame(0x222));
        assert_eq!(FIRES_COUNT.load(Ordering::SeqCst), 2);
        assert_eq!(FIRES_LAST_ID.load(Ordering::SeqCst), 0x222);
        // Disarm: no further callbacks.
        s.set_notify(0, 0, 0);
        s.push(frame(0x333));
        assert_eq!(FIRES_COUNT.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn notify_silent_when_flag_not_set() {
        let s = Shared::new();
        s.set_notify(cb_silent as *const () as usize, 0 /* no NOTIFY_RX */, 0);
        s.push(frame(0x111));
        assert_eq!(SILENT_COUNT.load(Ordering::SeqCst), 0);
    }

    // A callback that disarms ITSELF by calling set_notify(0,0,0) on its own Shared,
    // reached via a raw pointer the test parks here. Fired through a real push() so
    // CbScope is genuinely entered (the old test just poked IN_NOTIFY_CB directly,
    // never exercising the real re-entrancy path). kvasilloni-4vc / -im6.2b.
    static REENTRANT_SHARED: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    static REENTRANT_FIRES: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    extern "system" fn cb_disarms_self(_d: *mut NotifyData) {
        REENTRANT_FIRES.fetch_add(1, Ordering::SeqCst);
        let p = REENTRANT_SHARED.load(Ordering::SeqCst);
        if p != 0 {
            // SAFETY: the test parks a pointer to its live Shared here and clears it
            // before that Shared is dropped, so this deref is valid for the call.
            let s = unsafe { &*(p as *const Shared) };
            s.set_notify(0, 0, 0); // re-entrant disarm: runs while push holds cb_active
        }
    }

    #[test]
    fn set_notify_reentrant_does_not_deadlock() {
        // kvasilloni-4vc: a callback that re-arms/disarms itself runs set_notify on
        // the RX thread while push() already holds cb_active. set_notify must skip
        // the cb_active drain (IN_NOTIFY_CB is set by CbScope) instead of joining
        // the lock against itself. Drive it through a REAL push so CbScope is
        // actually entered, then prove (a) push returned - reaching the asserts
        // means no hang - and (b) the disarm took effect.
        let s = Shared::new();
        REENTRANT_FIRES.store(0, Ordering::SeqCst);
        REENTRANT_SHARED.store(&*s as *const Shared as usize, Ordering::SeqCst);
        s.set_notify(cb_disarms_self as *const () as usize, NOTIFY_RX, 0);

        s.push(frame(0x123)); // fires cb_disarms_self inside CbScope; must return
        assert_eq!(REENTRANT_FIRES.load(Ordering::SeqCst), 1, "callback should fire once");
        assert_eq!(
            s.notify.lock().unwrap_or_else(|e| e.into_inner()).cb,
            0,
            "the callback's self-disarm did not take effect"
        );

        // Disarmed: a further push must not fire it again.
        s.push(frame(0x124));
        assert_eq!(REENTRANT_FIRES.load(Ordering::SeqCst), 1, "disarm did not stop callbacks");
        REENTRANT_SHARED.store(0, Ordering::SeqCst); // drop the dangling pointer
    }

    static SLOW_CB_STARTED: AtomicBool = AtomicBool::new(false);
    static SLOW_CB_FINISHED: AtomicBool = AtomicBool::new(false);

    extern "system" fn slow_cb(_d: *mut NotifyData) {
        SLOW_CB_STARTED.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(100));
        SLOW_CB_FINISHED.store(true, Ordering::SeqCst);
    }

    #[test]
    fn set_notify_disarm_drains_inflight_callback() {
        // kvasilloni-4vc: disarming from another thread must not return while the
        // previous callback is still executing - otherwise the app could free the
        // callback's context mid-call (use-after-free). Prove the disarm blocks
        // until the in-flight callback completes.
        let s = Shared::new();
        SLOW_CB_STARTED.store(false, Ordering::SeqCst);
        SLOW_CB_FINISHED.store(false, Ordering::SeqCst);
        s.set_notify(slow_cb as *const () as usize, NOTIFY_RX, 0);

        let s2 = s.clone();
        let t = std::thread::spawn(move || s2.push(frame(0x123))); // runs slow_cb here
        while !SLOW_CB_STARTED.load(Ordering::SeqCst) {
            std::thread::yield_now(); // wait until the callback is in flight
        }
        s.set_notify(0, 0, 0); // disarm from this thread: must wait for the drain
        assert!(
            SLOW_CB_FINISHED.load(Ordering::SeqCst),
            "disarm returned while the callback was still running (UAF window)"
        );
        t.join().unwrap();
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
        // The ABI that actually ships is the Windows one, and it is locked at
        // COMPILE TIME by the `#[cfg(windows)] const _: () = { ... }` block next to
        // the NotifyData definition (transport.rs, just below the struct): it
        // asserts offset_of!(NotifyData, id) / (NotifyData, time) and size_of for
        // BOTH the 32- and 64-bit Windows targets. That const-assert - not this
        // test - is the real guard; a bad field edit fails the Windows cross-build,
        // not merely this host test. On the host build c_long is 64-bit, so those
        // exact Windows offsets do not apply; here we assert only the
        // platform-independent invariant that `time` immediately follows `id` by
        // one c_long, matching canNotifyData.info.rx. (kvasilloni-im6.2e)
        use std::mem::offset_of;
        assert!(offset_of!(NotifyData, time) > offset_of!(NotifyData, id));
        assert_eq!(
            offset_of!(NotifyData, time) - offset_of!(NotifyData, id),
            std::mem::size_of::<c_long>()
        );
    }

    // ===================== kvasilloni-im6.5: fast network-layer tests =====================
    // These drive the real UDP/TCP paths over loopback 127.0.0.1 sockets - no vcan,
    // cannelloni, or wine. They give `cargo test` coverage of code that previously
    // only ran in the slow e2e selftest. cfg!(test) force-enables the UDP ephemeral
    // fallback, and 127.0.0.0/8 is all loopback (so 127.0.0.2 is a usable 2nd source
    // IP for the peer filter).

    fn loopback_v4() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))
    }

    /// A connected loopback TCP pair: (client side, server-accepted side).
    fn loopback_tcp_pair() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server, _) = l.accept().unwrap();
        (client, server)
    }

    /// Grab an OS-assigned TCP port, then release it so a Conn can rebind it. Small
    /// TOCTOU window, fine for a loopback test.
    fn free_tcp_port() -> u16 {
        TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    /// A loopback TCP Config. `server` picks the role; peer_check is off so the
    /// loopback peer is always allowed; timeouts are short to keep the test snappy.
    fn tcp_cfg(server: bool, remote: u16, local: u16) -> Config {
        Config {
            host: "127.0.0.1".into(),
            remote_port: remote,
            local_port: local,
            tcp: true,
            tcp_server: server,
            peer_check: false,
            connect_timeout_ms: 1000,
            accept_timeout_ms: 5000,
            ..Config::default()
        }
    }

    #[test]
    fn udp_rx_loop_filters_peer_and_enqueues_allowed() {
        // The UDP RX loop must enqueue a datagram from an allowed source and DROP
        // one from any other (cannelloni's -R default; kvasilloni-872). Two distinct
        // loopback source IPs make the filter observable with no real peer.
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let server_addr = server.local_addr().unwrap();

        let allowed = UdpSocket::bind("127.0.0.1:0").unwrap();
        let denied = UdpSocket::bind("127.0.0.2:0").unwrap(); // 127.0.0.2: loopback, 2nd IP

        let rx = Shared::new();
        let running = Arc::new(AtomicBool::new(true));
        let allow = vec![loopback_v4()]; // 127.0.0.1 only
        let (rxq, run) = (rx.clone(), running.clone());
        let h = std::thread::spawn(move || udp_rx_loop(server, rxq, run, allow, true));

        denied.send_to(&wire::build_udp(&frame(0x222), 0), server_addr).unwrap();
        allowed.send_to(&wire::build_udp(&frame(0x123), 0), server_addr).unwrap();

        let f = rx.pop_wait(Duration::from_secs(2)).expect("allowed frame must be enqueued");
        assert_eq!(f.can_id, 0x123, "wrong frame delivered");
        std::thread::sleep(Duration::from_millis(80)); // let the denied datagram (not) land
        assert!(rx.pop().is_none(), "a datagram from a disallowed source was enqueued");

        running.store(false, Ordering::SeqCst);
        h.join().unwrap();
    }

    #[test]
    fn tcp_rx_loop_reassembles_dribbled_frame_then_exits_on_error() {
        // A frame written one byte at a time must still reassemble (read_exact
        // coalesces across the dribbled writes), and a stream that decodes to
        // Decoded::Error must make the loop exit ON ITS OWN - no running=false.
        let (mut peer, conn_side) = loopback_tcp_pair();
        let rx = Shared::new();
        let running = Arc::new(AtomicBool::new(true));
        let (rxq, run) = (rx.clone(), running.clone());
        let h = std::thread::spawn(move || tcp_rx_loop(conn_side, rxq, run));

        let mut enc = Vec::new();
        wire::encode_frame(&mut enc, &frame(0x456));
        for b in &enc {
            peer.write_all(&[*b]).unwrap();
            peer.flush().unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
        let f = rx.pop_wait(Duration::from_secs(2)).expect("dribbled frame must reassemble");
        assert_eq!(f.can_id, 0x456);

        // A classic frame claiming len=9 (> 8) decodes to Decoded::Error: the loop
        // tears down the unresyncable stream and returns. join() proves it exited.
        peer.write_all(&[0x00, 0x00, 0x01, 0x00, 9]).unwrap();
        peer.flush().unwrap();
        h.join().unwrap();
        assert!(running.load(Ordering::SeqCst), "loop must exit on Error, not via running=false");
    }

    #[test]
    fn conn_tcp_exchanges_frame_both_directions() {
        // A server-role and a client-role Conn over loopback complete the handshake
        // and pass one frame each way end-to-end - the real connect/encode/decode
        // path, fast.
        let port = free_tcp_port();
        let server_cfg = tcp_cfg(true, port, port);
        let client_cfg = tcp_cfg(false, port, 0);

        let sh = std::thread::spawn(move || Conn::connect(&server_cfg).expect("server connect"));
        std::thread::sleep(Duration::from_millis(150)); // let the server bind + listen
        let client = Conn::connect(&client_cfg).expect("client connect");
        let server = sh.join().unwrap();

        client.write(&frame(0x321)).expect("client write");
        let got = server.rx_shared().pop_wait(Duration::from_secs(2)).expect("server received");
        assert_eq!(got.can_id, 0x321);

        server.write(&frame(0x654)).expect("server write");
        let got = client.rx_shared().pop_wait(Duration::from_secs(2)).expect("client received");
        assert_eq!(got.can_id, 0x654);

        client.close();
        server.close();
    }

    #[test]
    fn conn_connect_rejects_non_ip_host() {
        let mut cfg = tcp_cfg(false, 20000, 0);
        cfg.host = "not-an-ip".into();
        // Conn isn't Debug, so unwrap_err() won't compile; pull the error via .err().
        let err = Conn::connect(&cfg).err().expect("a non-IP host must error");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn conn_tcp_write_error_tears_down_connection() {
        // No coverage anywhere today: a failed TCP write must clear is_ready and
        // shut the socket down, so the app reopens instead of streaming more bytes
        // into a desynced (mid-frame) connection (kvasilloni-lo7).
        let port = free_tcp_port();
        let server_cfg = tcp_cfg(true, port, port);
        let client_cfg = tcp_cfg(false, port, 0);
        let sh = std::thread::spawn(move || Conn::connect(&server_cfg).expect("server connect"));
        std::thread::sleep(Duration::from_millis(150));
        let client = Conn::connect(&client_cfg).expect("client connect");
        let server = sh.join().unwrap();
        assert!(client.is_ready(), "client should be negotiated after handshake");

        // Drop the peer entirely so the client's writes hit a reset/broken pipe.
        server.close();
        drop(server);

        let mut errored = false;
        for _ in 0..2000 {
            if client.write(&frame(0x111)).is_err() {
                errored = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(errored, "write to a dropped peer never errored");
        assert!(!client.is_ready(), "a failed TCP write must clear negotiated/is_ready");
    }

    #[test]
    fn handshake_rejects_wrong_banner() {
        // The peer reads our CANNELLONIv1 then replies with 12 WRONG bytes: the
        // handshake must reject it as InvalidData, not accept a non-cannelloni peer.
        let (peer, conn_side) = loopback_tcp_pair();
        let ph = std::thread::spawn(move || {
            let mut p = peer;
            let mut buf = [0u8; 12];
            let _ = p.read_exact(&mut buf); // consume our banner
            let _ = p.write_all(b"NOTCANNELLON"); // 12 bytes, wrong
        });
        let err = handshake(&conn_side, Duration::from_millis(500)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        let _ = ph.join();
    }

    #[test]
    fn accept_with_timeout_returns_allowed_peer() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let ch = std::thread::spawn(move || {
            let _c = TcpStream::connect(addr).unwrap();
            std::thread::sleep(Duration::from_millis(200)); // hold it open past accept
        });
        let allow = vec![loopback_v4()];
        let (s, peer) =
            accept_with_timeout(&l, Duration::from_secs(2), &allow, true).expect("accept allowed");
        assert_eq!(peer.ip(), loopback_v4());
        drop(s);
        ch.join().unwrap();
    }

    #[test]
    fn accept_with_timeout_rejects_disallowed_then_times_out() {
        // A connection from a non-allowed source is shut down and the wait
        // continues; with no allowed client it ends in TimedOut. Without the
        // peer-IP gate (kvasilloni-872) the loopback connect would instead be
        // RETURNED (Ok), so the TimedOut assertion is attributable to the filter.
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let got_closed = Arc::new(AtomicBool::new(false));
        let gc = got_closed.clone();
        let ch = std::thread::spawn(move || {
            let mut c = TcpStream::connect(addr).unwrap();
            let mut b = [0u8; 1];
            match c.read(&mut b) {
                Ok(0) | Err(_) => gc.store(true, Ordering::SeqCst), // server shut us down
                Ok(_) => {}
            }
        });
        let allow = vec![IpAddr::V4(Ipv4Addr::new(10, 255, 255, 254))]; // loopback NOT allowed
        let err = accept_with_timeout(&l, Duration::from_millis(700), &allow, true).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        ch.join().unwrap();
        assert!(got_closed.load(Ordering::SeqCst), "disallowed client was not shut down");
    }

    #[test]
    fn accept_with_timeout_times_out_without_client() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let allow = vec![loopback_v4()];
        let err = accept_with_timeout(&l, Duration::from_millis(300), &allow, true).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn bind_udp_falls_back_to_ephemeral_when_busy() {
        // kvasilloni-iai: a busy local port with fallback on must yield a different,
        // non-zero ephemeral port rather than failing the bind.
        let occupied = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = occupied.local_addr().unwrap();
        let s = bind_udp(addr, true).expect("fallback bind must succeed");
        let got = s.local_addr().unwrap();
        assert_ne!(got.port(), 0, "fallback bound port 0");
        assert_ne!(got.port(), addr.port(), "did not fall back off the busy port");
    }

    // ===================== race-detection tests (kvasilloni-lw6.3) =====================
    //
    // METHOD: ThreadSanitizer. The whole transport module - the existing concurrency
    // tests (disarm-drains-inflight 4vc, reentrant-no-deadlock 4vc, mark_closed-wakes
    // hc9, write-teardown lo7) plus the three below - runs race-clean under:
    //
    //   rustup component add rust-src --toolchain nightly   # once
    //   RUSTFLAGS="-Zsanitizer=thread" cargo +nightly test -Z build-std \
    //       --target x86_64-unknown-linux-gnu --lib transport::
    //
    // (`-Z build-std` rebuilds std with TSan instrumentation so its Mutex/Condvar are
    // seen too.) loom was rejected: it would require swapping std sync primitives for
    // loom's behind cfg(loom) in production code and cannot model the real
    // JoinHandle/thread::current().id() close() self-join logic (cqe).
    //
    // The three scenarios the epic calls out:
    //   (a) 4vc - push() firing the callback while another thread disarms: the
    //       cb_active drain must order the app-context access -> cb_active_drain_*.
    //   (b) cqe - a callback that calls close() ON the RX thread: detach, never
    //       self-join -> close_from_notify_callback_detaches_*.
    //   (c) hc9 - readers blocked in pop_wait/peek_wait woken by mark_closed: covered
    //       by mark_closed_wakes_* above and stressed under contention below.
    //
    // VERIFIED that TSan (the chosen method) detects a removed drain: deleting the
    // `cb_active` drain in set_notify makes cb_active_drain_orders_app_context_access
    // report a data race under TSan (and the existing logical assertion in
    // set_notify_disarm_drains_inflight_callback fails even without TSan).
    //
    // shared_concurrent_stress_is_race_clean also stands alone as the documented
    // heavier std-thread stress regime for hosts without a TSan-capable nightly: it
    // is probabilistic there, a proof only under TSan.

    /// `*mut u64` standing in for the app's callback context; the callback READS it
    /// on the RX thread, the disarming thread WRITES (frees) it after the drain.
    static CTX_PTR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static CTX_CB_ENTERED: AtomicBool = AtomicBool::new(false);

    extern "system" fn cb_reads_context(_d: *mut NotifyData) {
        let p = CTX_PTR.load(Ordering::SeqCst) as *const u64;
        if !p.is_null() {
            CTX_CB_ENTERED.store(true, Ordering::SeqCst);
            // Widen the window so a disarm that does NOT drain reliably overlaps this
            // non-atomic read (which is what TSan would then flag).
            std::thread::sleep(Duration::from_millis(60));
            // SAFETY: the test keeps the pointee alive until it has observed the drain.
            let _v = unsafe { std::ptr::read_volatile(p) };
        }
    }

    #[test]
    fn cb_active_drain_orders_app_context_access() {
        // kvasilloni-4vc, TSan target. push() holds cb_active across reading AND
        // invoking the callback; set_notify drains cb_active after swapping the
        // callback. So a disarm from another thread happens-AFTER any in-flight
        // callback's access to the app context. Here the callback does a NON-atomic
        // read of a heap "context"; once the disarm returns we do a NON-atomic write
        // (modelling the app freeing it). With the drain, write happens-after read:
        // race-free. Drop the drain and TSan reports a data race on the context.
        let s = Shared::new();
        let mut ctx = Box::new(0xAAAA_u64);
        let ptr = &mut *ctx as *mut u64;
        CTX_PTR.store(ptr as usize, Ordering::SeqCst);
        CTX_CB_ENTERED.store(false, Ordering::SeqCst);
        s.set_notify(cb_reads_context as *const () as usize, NOTIFY_RX, 0);

        let s2 = s.clone();
        let t = std::thread::spawn(move || s2.push(frame(0x123))); // callback reads ctx here
        while !CTX_CB_ENTERED.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        s.set_notify(0, 0, 0); // disarm: blocks on the drain until the read is done
        // Safe now (post-drain): write the "context". Races with the read iff the
        // drain was removed.
        unsafe { std::ptr::write_volatile(ptr, 0xBBBB_u64) };
        t.join().unwrap();
        CTX_PTR.store(0, Ordering::SeqCst);
        assert_eq!(*ctx, 0xBBBB);
    }

    static CQE_CONN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static CQE_CLOSED: AtomicBool = AtomicBool::new(false);

    extern "system" fn cb_closes_conn(_d: *mut NotifyData) {
        let p = CQE_CONN.load(Ordering::SeqCst);
        if p != 0 {
            // SAFETY: the test keeps the Conn alive (and pins it) until it observes
            // CQE_CLOSED, then clears this pointer.
            let c = unsafe { &*(p as *const Conn) };
            c.close(); // runs ON the RX thread -> must detach, not self-join (cqe)
            CQE_CLOSED.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn close_from_notify_callback_detaches_not_self_joins() {
        // kvasilloni-cqe: a canSetNotify callback fires on the RX thread; if the app
        // calls canClose from it, Conn::close() runs on the RX thread and must DETACH
        // it (h.thread().id() == current id) rather than join, which would deadlock
        // the thread on itself. Drive the real path: a loopback datagram makes the RX
        // thread fire the callback, which closes the Conn from within. Proof = close()
        // completing (CQE_CLOSED) well within the timeout instead of hanging.
        let conn = Conn::connect(&udp_cfg(0, 0)).expect("udp connect");
        let port = conn.local_port();
        CQE_CLOSED.store(false, Ordering::SeqCst);
        CQE_CONN.store(&conn as *const Conn as usize, Ordering::SeqCst);
        conn.set_notify(cb_closes_conn as *const () as usize, NOTIFY_RX, 0);

        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        // A couple of datagrams in case the first races the loop's first recv.
        for _ in 0..3 {
            let _ = sender.send_to(&wire::build_udp(&frame(0x123), 0), ("127.0.0.1", port));
            std::thread::sleep(Duration::from_millis(10));
        }

        let start = Instant::now();
        while !CQE_CLOSED.load(Ordering::SeqCst) {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "close() from the RX-thread callback never completed (self-join deadlock?)"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        CQE_CONN.store(0, Ordering::SeqCst); // unpark before conn drops
        std::thread::sleep(Duration::from_millis(50)); // let the detached RX thread unwind
        // conn's Drop calls close() again; the handle was already taken -> no double join.
    }

    extern "system" fn cb_noop(_d: *mut NotifyData) {}

    #[test]
    fn shared_concurrent_stress_is_race_clean() {
        // High-contention stand-in: many producers (push) + a notify churner
        // (arm/disarm, exercising the cb_active drain against concurrent push, 4vc)
        // + blocking readers (pop_wait/peek_wait woken by mark_closed, hc9). Bounded
        // so a plain `cargo test` run stays fast; run under TSan for a race proof,
        // where the dense interleavings are what the sanitizer inspects.
        let s = Shared::new();
        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();

        for p in 0..4u32 {
            let s = s.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..1000u32 {
                    s.push(frame(0x100 + (p << 8) + (i & 0xFF)));
                }
            }));
        }
        // Notify churner: repeatedly arm/disarm so set_notify's drain races push()'s
        // cb_active + callback invocation. cb_noop touches no shared state, so it
        // cannot bleed into the other notify tests (cf. kvasilloni-im6.1).
        {
            let s = s.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..1000 {
                    s.set_notify(cb_noop as *const () as usize, NOTIFY_RX, 0);
                    s.set_notify(0, 0, 0);
                }
            }));
        }
        // Blocking readers + peekers; mark_closed (and `stop`) release them.
        for _ in 0..3 {
            let (s, stop) = (s.clone(), stop.clone());
            handles.push(std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let _ = s.pop_wait(Duration::from_millis(20));
                }
            }));
        }
        for _ in 0..2 {
            let (s, stop) = (s.clone(), stop.clone());
            handles.push(std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let _ = s.peek_wait(Duration::from_millis(20));
                }
            }));
        }

        std::thread::sleep(Duration::from_millis(50));
        s.mark_closed(); // hc9: wake every currently-blocked reader at once
        stop.store(true, Ordering::SeqCst);
        for h in handles {
            h.join().unwrap();
        }
    }

    /// A loopback UDP Config (local ephemeral via cfg!(test) fallback). peer_check
    /// off so a loopback sender is always allowed.
    fn udp_cfg(remote: u16, local: u16) -> Config {
        Config {
            host: "127.0.0.1".into(),
            remote_port: remote,
            local_port: local,
            tcp: false,
            peer_check: false,
            ..Config::default()
        }
    }

    // ===================== error-path leak audit (kvasilloni-lw6.4) =====================
    // im6.5 covered the TCP write-error teardown; these cover the PARTIAL-OPEN failure
    // paths - a connect that succeeds then fails the handshake, and a bind that fails -
    // proving they strand no OS fd and no RX thread. The stress test checks the CONNS
    // map; this checks the real OS resources via /proc/self. Linux-only (the host /
    // selftest platform); the shipped DLL is unaffected.

    #[cfg(target_os = "linux")]
    fn count_fds() -> usize {
        std::fs::read_dir("/proc/self/fd").map(|d| d.count()).unwrap_or(0)
    }

    #[cfg(target_os = "linux")]
    fn count_threads() -> usize {
        std::fs::read_dir("/proc/self/task").map(|d| d.count()).unwrap_or(0)
    }

    // /proc/self/fd and /proc/self/task are process-GLOBAL, so other tests running
    // in parallel add (and remove) fds/threads during the measurement window. The
    // discriminator: a genuine leak grows by ~1 per failed open (so ~N total), while
    // that parallel-test noise is bounded and independent of N. So we only flag
    // GROWTH (a thread/fd count can legitimately drop as a neighbour test finishes)
    // and tolerate up to N/4 - far above realistic neighbour noise for this suite,
    // far below the ~N a real per-iteration leak would produce.
    #[cfg(target_os = "linux")]
    fn assert_no_leak(label: &str, before: (usize, usize), after: (usize, usize), n: usize) {
        let tol = n / 4;
        assert!(
            after.0 <= before.0 + tol,
            "{label}: fd leak {} -> {} over {n} failed opens (tol {tol})",
            before.0,
            after.0
        );
        assert!(
            after.1 <= before.1 + tol,
            "{label}: thread leak {} -> {} over {n} failed opens (tol {tol})",
            before.1,
            after.1
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn failed_tcp_handshake_opens_leak_no_fd_or_thread() {
        // A loopback server accepts then sends a WRONG 12-byte banner, so the client
        // Conn::connect fails the handshake AFTER the socket is connected - the
        // partial-open path. Many such failed opens must not grow the process fd or
        // thread count: the connected socket (and handshake's clone) must be dropped,
        // and the RX thread must never be spawned (handshake runs before the spawn).
        const N: usize = 100;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = std::thread::spawn(move || {
            for _ in 0..N {
                if let Ok((mut s, _)) = listener.accept() {
                    let mut buf = [0u8; 12];
                    let _ = s.read_exact(&mut buf); // consume the client's banner
                    let _ = s.write_all(b"NOPENOPENOPE"); // 12 bytes, wrong
                }
            }
        });

        let cfg = tcp_cfg(false, addr.port(), 0);
        // Warm up once: the first connect lazily initialises thread-locals / the
        // now_ms epoch / etc., which would otherwise look like a leak at the baseline.
        assert!(Conn::connect(&cfg).is_err(), "wrong-banner handshake must fail");
        std::thread::sleep(Duration::from_millis(30));

        let before = (count_fds(), count_threads());
        for _ in 0..(N - 1) {
            assert!(Conn::connect(&cfg).is_err(), "wrong-banner handshake must fail");
        }
        std::thread::sleep(Duration::from_millis(60)); // let any transient fds close
        let after = (count_fds(), count_threads());
        srv.join().unwrap();

        assert_no_leak("handshake-fail", before, after, N - 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn failed_bind_opens_leak_no_fd_or_thread() {
        // Occupy a port, then attempt a SERVER-role Conn::connect bound to it: the
        // listener bind fails before anything is spawned. Many such failures must not
        // leak a listener fd or a thread.
        const N: usize = 100;
        let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = occupied.local_addr().unwrap().port();
        let cfg = tcp_cfg(true, port, port); // server role, bind on the busy port

        assert!(Conn::connect(&cfg).is_err(), "binding a busy port must fail"); // warm up
        std::thread::sleep(Duration::from_millis(20));

        let before = (count_fds(), count_threads());
        for _ in 0..N {
            let err = Conn::connect(&cfg).err().expect("binding a busy port must fail");
            assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
        }
        std::thread::sleep(Duration::from_millis(20));
        let after = (count_fds(), count_threads());

        assert_no_leak("bind-fail", before, after, N);
    }
}
