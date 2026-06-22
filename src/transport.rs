//! Network transport: makes the shim a cannelloni peer over UDP or TCP.
//!
//! One channel = one [`Conn`]. A background RX thread decodes inbound cannelloni
//! traffic into a bounded ring; `canRead` drains it. `canWrite` encodes a single
//! frame and sends it (one-frame UDP datagram, or a headerless TCP stream write).

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::config::Config;
use crate::wire::{self, DecodeState, Decoded, Frame};

const RING_CAP: usize = 8192;

enum Tx {
    Udp { sock: UdpSocket, remote: SocketAddr, seq: u8 },
    Tcp { stream: TcpStream },
}

pub struct Conn {
    tx: Mutex<Tx>,
    rx: Arc<Mutex<VecDeque<Frame>>>,
    running: Arc<AtomicBool>,
    negotiated: Arc<AtomicBool>,
    rx_sock: RxStop,
    handle: Option<JoinHandle<()>>,
}

/// Handle used by `close` to unblock the RX thread. The UDP variant is never
/// read — it just keeps the original bound socket alive for the connection's
/// lifetime; the RX loop stops via its read timeout. TCP is shut down explicitly.
enum RxStop {
    Udp(#[allow(dead_code)] UdpSocket),
    Tcp(TcpStream),
}

fn ring_push(rx: &Mutex<VecDeque<Frame>>, f: Frame) {
    if let Ok(mut q) = rx.lock() {
        if q.len() < RING_CAP {
            q.push_back(f);
        } // else: ring full -> drop
    }
}

impl Conn {
    pub fn connect(cfg: &Config) -> std::io::Result<Conn> {
        let remote: SocketAddr = format!("{}:{}", cfg.host, cfg.remote_port)
            .parse()
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad host"))?;

        let rx: Arc<Mutex<VecDeque<Frame>>> = Arc::new(Mutex::new(VecDeque::new()));
        let running = Arc::new(AtomicBool::new(true));
        let negotiated = Arc::new(AtomicBool::new(false));

        if !cfg.tcp {
            // -------------------------------- UDP --------------------------------
            let sock = UdpSocket::bind(("0.0.0.0", cfg.local_port))?;
            let tx_sock = sock.try_clone()?;
            let rx_sock = sock.try_clone()?;
            rx_sock.set_read_timeout(Some(Duration::from_millis(500)))?;
            negotiated.store(true, Ordering::SeqCst);

            let (rxq, run) = (rx.clone(), running.clone());
            let handle = std::thread::spawn(move || udp_rx_loop(rx_sock, rxq, run));
            return Ok(Conn {
                tx: Mutex::new(Tx::Udp { sock: tx_sock, remote, seq: 0 }),
                rx,
                running,
                negotiated,
                rx_sock: RxStop::Udp(sock), // original handle, used to stop the loop
                handle: Some(handle),
            });
        }

        // ---------------------------------- TCP ----------------------------------
        let stream = if cfg.tcp_server {
            let listener = TcpListener::bind(("0.0.0.0", cfg.local_port))?;
            listener.set_nonblocking(false)?;
            // Block for a client, but not forever.
            let (s, _peer) = accept_with_timeout(&listener, Duration::from_secs(30))?;
            s
        } else {
            TcpStream::connect_timeout(&remote, Duration::from_secs(10))?
        };
        stream.set_nodelay(true).ok();

        // Symmetric handshake: send + expect "CANNELLONIv1".
        handshake(&stream)?;
        negotiated.store(true, Ordering::SeqCst);

        let rx_stream = stream.try_clone()?;
        let tx_stream = stream.try_clone()?;
        let stop_stream = stream;

        let (rxq, run) = (rx.clone(), running.clone());
        let handle = std::thread::spawn(move || tcp_rx_loop(rx_stream, rxq, run));

        Ok(Conn {
            tx: Mutex::new(Tx::Tcp { stream: tx_stream }),
            rx,
            running,
            negotiated,
            rx_sock: RxStop::Tcp(stop_stream),
            handle: Some(handle),
        })
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
        self.rx.lock().ok().and_then(|mut q| q.pop_front())
    }

    pub fn close(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        self.negotiated.store(false, Ordering::SeqCst);
        match &self.rx_sock {
            RxStop::Tcp(s) => {
                let _ = s.shutdown(Shutdown::Both);
            }
            RxStop::Udp(_) => { /* the 500ms read timeout lets the loop exit */ }
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.close();
    }
}

fn handshake(stream: &TcpStream) -> std::io::Result<()> {
    let mut s = stream.try_clone()?;
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
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

fn udp_rx_loop(sock: UdpSocket, rx: Arc<Mutex<VecDeque<Frame>>>, running: Arc<AtomicBool>) {
    let mut buf = [0u8; 2048];
    while running.load(Ordering::SeqCst) {
        match sock.recv_from(&mut buf) {
            Ok((n, _from)) => {
                if let Some(frames) = wire::parse_udp(&buf[..n]) {
                    for f in frames {
                        ring_push(&rx, f);
                    }
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

fn tcp_rx_loop(mut stream: TcpStream, rx: Arc<Mutex<VecDeque<Frame>>>, running: Arc<AtomicBool>) {
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
        match wire::decode_stream(&chunk[..need], &mut f, &mut st) {
            Decoded::Need(n) => need = n,
            Decoded::Complete => {
                ring_push(&rx, f);
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
