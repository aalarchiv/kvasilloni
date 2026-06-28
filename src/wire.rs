// SPDX-License-Identifier: LGPL-3.0-or-later
//! cannelloni wire-format codec + Kvaser <-> SocketCAN id translation.
//!
//! An independent implementation of the cannelloni wire protocol - UDP
//! (packet-framed) and TCP (headerless streaming) - so the shim interoperates
//! as a cannelloni peer. It reproduces only the on-the-wire byte layout needed
//! for that interop and is verified against golden byte vectors in the tests
//! below.

// ---- cannelloni protocol constants ----
pub const FRAME_VERSION: u8 = 2; // CANNELLONI_FRAME_VERSION
pub const OP_DATA: u8 = 0; // op_codes::DATA
pub const DATA_PACKET_BASE_SIZE: usize = 5; // version+op+seq+count(2)
pub const FRAME_BASE_SIZE: usize = 5; // can_id(4)+len(1)
pub const CANFD_FRAME: u8 = 0x80; // high bit of len => CAN FD
pub const CONNECT_V1: &[u8] = b"CANNELLONIv1"; // TCP handshake banner

/// Maximum data bytes a [`Frame`] can hold (CAN FD payload). The wire length
/// field is 7 bits (0..=127), so a malformed/hostile peer can encode more than
/// this; both decoders MUST reject anything larger rather than index past
/// `Frame.data`. See kvasilloni-kkt.
pub const MAX_FRAME_LEN: usize = 64;

/// Maximum data bytes a *classic* (non-FD) CAN frame can carry. A frame without
/// the FD bit set MUST NOT exceed this: a real classic frame physically tops out
/// at 8 bytes, and `canRead` callers that did not open the channel for CAN FD
/// size their receive buffer accordingly (the Kvaser `canRead` ABI carries no
/// buffer-length argument). Both decoders reject a non-FD frame claiming more, so
/// an over-length "classic" frame can never reach a caller's 8-byte buffer.
/// See kvasilloni-nmt.
pub const CLASSIC_FRAME_MAX_LEN: usize = 8;

// ---- SocketCAN can_id flag bits (linux/can.h) ----
pub const CAN_EFF_FLAG: u32 = 0x8000_0000;
pub const CAN_RTR_FLAG: u32 = 0x4000_0000;
pub const CAN_EFF_MASK: u32 = 0x1FFF_FFFF;
pub const CAN_SFF_MASK: u32 = 0x0000_07FF;

// ---- Kvaser canMSG_* / canFDMSG_* message flags ----
pub const CAN_MSG_RTR: u32 = 0x0001;
pub const CAN_MSG_STD: u32 = 0x0002;
pub const CAN_MSG_EXT: u32 = 0x0004;
pub const CAN_MSG_FDF: u32 = 0x0001_0000; // CAN FD frame
pub const CAN_MSG_BRS: u32 = 0x0002_0000; // CAN FD bit-rate switch
pub const CAN_MSG_ESI: u32 = 0x0004_0000; // CAN FD error state indicator

// ---- SocketCAN canfd_frame.flags bits (linux/can.h), carried in fd_flags ----
pub const CANFD_BRS: u8 = 0x01;
pub const CANFD_ESI: u8 = 0x02;

/// Round a byte count up to a valid CAN FD data length
/// (0..8, 12, 16, 20, 24, 32, 48, 64). cannelloni/SocketCAN require a valid DLC.
pub fn fd_round_len(n: u8) -> u8 {
    match n {
        0..=8 => n,
        9..=12 => 12,
        13..=16 => 16,
        17..=20 => 20,
        21..=24 => 24,
        25..=32 => 32,
        33..=48 => 48,
        _ => 64,
    }
}

/// Map Kvaser canFDMSG_* flags to the cannelloni/SocketCAN `fd_flags` byte.
pub fn kvaser_to_fd_flags(kvaser_flag: u32) -> u8 {
    let mut f = 0;
    if kvaser_flag & CAN_MSG_BRS != 0 {
        f |= CANFD_BRS;
    }
    if kvaser_flag & CAN_MSG_ESI != 0 {
        f |= CANFD_ESI;
    }
    f
}

/// Map a cannelloni/SocketCAN `fd_flags` byte to Kvaser canFDMSG_* flags.
pub fn fd_flags_to_kvaser(fd_flags: u8) -> u32 {
    let mut f = 0;
    if fd_flags & CANFD_BRS != 0 {
        f |= CAN_MSG_BRS;
    }
    if fd_flags & CANFD_ESI != 0 {
        f |= CAN_MSG_ESI;
    }
    f
}

/// A decoded CAN frame in SocketCAN terms (`can_id` carries the flag bits).
#[derive(Clone, Copy, Debug)]
pub struct Frame {
    pub can_id: u32,
    pub len: u8,
    pub fd: bool,
    pub fd_flags: u8,
    pub data: [u8; 64],
    /// Monotonic receive timestamp in milliseconds, stamped when the RX thread
    /// enqueues the frame (0 on TX / before stamping). NOT part of the wire
    /// format - the codec neither writes nor reads it. Reported to the app by
    /// `canRead`/`canReadWait`/notify in Kvaser timer units. See kvasilloni-kha.
    pub rx_time_ms: u64,
}

impl Default for Frame {
    fn default() -> Self {
        Frame { can_id: 0, len: 0, fd: false, fd_flags: 0, data: [0; 64], rx_time_ms: 0 }
    }
}

impl Frame {
    pub fn is_rtr(&self) -> bool {
        self.can_id & CAN_RTR_FLAG != 0
    }
}

// ===================== Kvaser <-> SocketCAN translation =====================

/// Build a SocketCAN can_id from a Kvaser id + canMSG_* flags.
pub fn kvaser_to_canid(id: i32, kvaser_flag: u32) -> u32 {
    let mut can_id = if kvaser_flag & CAN_MSG_EXT != 0 {
        (id as u32 & CAN_EFF_MASK) | CAN_EFF_FLAG
    } else {
        id as u32 & CAN_SFF_MASK
    };
    if kvaser_flag & CAN_MSG_RTR != 0 {
        can_id |= CAN_RTR_FLAG;
    }
    can_id
}

/// Split a SocketCAN can_id into a Kvaser id and canMSG_* flags.
pub fn canid_to_kvaser(can_id: u32, fd: bool) -> (i32, u32) {
    let (id, mut flag) = if can_id & CAN_EFF_FLAG != 0 {
        ((can_id & CAN_EFF_MASK) as i32, CAN_MSG_EXT)
    } else {
        ((can_id & CAN_SFF_MASK) as i32, CAN_MSG_STD)
    };
    if can_id & CAN_RTR_FLAG != 0 {
        flag |= CAN_MSG_RTR;
    }
    if fd {
        flag |= CAN_MSG_FDF;
    }
    (id, flag)
}

// ============================ per-frame codec ==============================

/// Encode one frame in cannelloni wire format. Appends to `out`.
pub fn encode_frame(out: &mut Vec<u8>, f: &Frame) {
    let mut len = f.len & 0x7F;
    if f.fd {
        len |= CANFD_FRAME;
    }
    out.extend_from_slice(&f.can_id.to_be_bytes());
    out.push(len);
    if f.fd {
        out.push(f.fd_flags);
    }
    if !f.is_rtr() {
        let dlen = (f.len & 0x7F) as usize;
        out.extend_from_slice(&f.data[..dlen]);
    }
}

/// Streaming TCP decoder for the headerless cannelloni frame stream.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecodeState {
    Init,
    CanId,
    Len,
    Flags,
    Data,
}

/// Result of feeding the next chunk to the streaming decoder.
pub enum Decoded {
    /// Need this many more bytes before the next call.
    Need(usize),
    /// A frame is complete.
    Complete,
    /// Protocol error.
    Error,
}

/// Drive the streaming decoder: start in `Init`, call with an empty slice to
/// learn the first read size, then feed precisely the requested number of bytes
/// each step.
pub fn decode_stream(data: &[u8], f: &mut Frame, state: &mut DecodeState) -> Decoded {
    match *state {
        DecodeState::Init => {
            *f = Frame::default();
            *state = DecodeState::CanId;
            Decoded::Need(4)
        }
        DecodeState::CanId => {
            if data.len() != 4 {
                return Decoded::Error;
            }
            f.can_id = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
            *state = DecodeState::Len;
            Decoded::Need(1)
        }
        DecodeState::Len => {
            if data.len() != 1 {
                return Decoded::Error;
            }
            let raw = data[0];
            if raw & CANFD_FRAME != 0 {
                f.fd = true;
                f.len = raw & !CANFD_FRAME;
                if f.len as usize > MAX_FRAME_LEN {
                    return Decoded::Error; // would overrun Frame.data (kvasilloni-kkt)
                }
                *state = DecodeState::Flags;
                return Decoded::Need(1);
            }
            f.fd = false;
            f.len = raw;
            // A non-FD frame (RTR included) is at most 8 bytes; rejecting an
            // over-length one keeps a bogus "classic" frame from ever reaching a
            // caller's 8-byte canRead buffer (kvasilloni-nmt).
            if f.len as usize > CLASSIC_FRAME_MAX_LEN {
                return Decoded::Error;
            }
            if f.is_rtr() {
                // RTR carries a DLC but no data bytes. Keep the DLC so it matches
                // what parse_udp reports over UDP - both transports now agree
                // instead of TCP zeroing it (kvasilloni-f1b).
                *state = DecodeState::Init;
                return Decoded::Complete;
            }
            if f.len == 0 {
                *state = DecodeState::Init;
                return Decoded::Complete;
            }
            *state = DecodeState::Data;
            Decoded::Need(f.len as usize)
        }
        DecodeState::Flags => {
            if data.len() != 1 {
                return Decoded::Error;
            }
            f.fd_flags = data[0];
            if f.is_rtr() || f.len == 0 {
                *state = DecodeState::Init;
                return Decoded::Complete;
            }
            *state = DecodeState::Data;
            Decoded::Need(f.len as usize)
        }
        DecodeState::Data => {
            let n = f.len as usize;
            // `n > Frame.data` can only happen if the length guards above were
            // bypassed; keep the copy site itself unconditionally in-bounds.
            if data.len() != n || n > f.data.len() {
                return Decoded::Error;
            }
            f.data[..n].copy_from_slice(&data[..n]);
            *state = DecodeState::Init;
            Decoded::Complete
        }
    }
}

// ============================ UDP packet codec =============================

/// Build a one-frame cannelloni UDP datagram (version/op=DATA/seq/count=1 + frame).
pub fn build_udp(f: &Frame, seq_no: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(DATA_PACKET_BASE_SIZE + 16);
    out.push(FRAME_VERSION);
    out.push(OP_DATA);
    out.push(seq_no);
    out.extend_from_slice(&1u16.to_be_bytes()); // count = 1, big-endian
    encode_frame(&mut out, f);
    out
}

/// Upper bound on how many frames to pre-allocate for a datagram of `buf_len`
/// bytes that claims `count` frames. `count` is attacker-controlled, so it is
/// clamped to what the datagram could physically hold (each frame is at least
/// `FRAME_BASE_SIZE` on the wire); trusting it would let a 7-byte spoofed packet
/// force a ~5MB allocation. The per-frame bounds in `parse_udp` still validate the
/// real content. Split out so the clamp is unit-testable on its own (kvasilloni-56p).
fn udp_prealloc_cap(count: u16, buf_len: usize) -> usize {
    (count as usize).min(buf_len / FRAME_BASE_SIZE + 1)
}

/// Parse a cannelloni UDP datagram, returning the contained frames.
/// Returns `None` on a malformed/truncated packet.
pub fn parse_udp(buf: &[u8]) -> Option<Vec<Frame>> {
    if buf.len() < DATA_PACKET_BASE_SIZE {
        return None;
    }
    if buf[0] != FRAME_VERSION || buf[1] != OP_DATA {
        return None;
    }
    let count = u16::from_be_bytes([buf[3], buf[4]]);
    let mut frames = Vec::with_capacity(udp_prealloc_cap(count, buf.len()));
    let mut p = DATA_PACKET_BASE_SIZE;
    for _ in 0..count {
        if p + FRAME_BASE_SIZE > buf.len() {
            return None;
        }
        let mut f = Frame::default();
        f.can_id = u32::from_be_bytes([buf[p], buf[p + 1], buf[p + 2], buf[p + 3]]);
        p += 4;
        let raw = buf[p];
        p += 1;
        if raw & CANFD_FRAME != 0 {
            f.fd = true;
            f.len = raw & !CANFD_FRAME;
            if p + 1 > buf.len() {
                return None;
            }
            f.fd_flags = buf[p];
            p += 1;
        } else {
            f.len = raw;
        }
        let dlen = (f.len & 0x7F) as usize;
        // FD reaches 64 bytes; a non-FD frame must not exceed 8 (a real classic
        // frame's max, and the size a non-FD canRead caller's buffer assumes).
        // Reject anything larger rather than over-read or deliver it (kvasilloni-nmt/-kkt).
        let limit = if f.fd { MAX_FRAME_LEN } else { CLASSIC_FRAME_MAX_LEN };
        if dlen > limit {
            return None;
        }
        if !f.is_rtr() {
            if p + dlen > buf.len() {
                return None;
            }
            f.data[..dlen].copy_from_slice(&buf[p..p + dlen]);
            p += dlen;
        }
        frames.push(f);
    }
    Some(frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(can_id: u32, data: &[u8]) -> Frame {
        let mut f = Frame::default();
        f.can_id = can_id;
        f.len = data.len() as u8;
        f.data[..data.len()].copy_from_slice(data);
        f
    }

    /// Build a multi-frame cannelloni UDP datagram: header {ver, DATA, seq, count}
    /// followed by `frames` back-to-back via encode_frame. `count` is set
    /// independently of `frames.len()` so a test can forge an over-claimed
    /// (truncated) batch. The real `build_udp` only ever emits count=1, so this is
    /// the only way to drive parse_udp's per-frame loop with N>1 (kvasilloni-im6.3).
    fn build_udp_batch(frames: &[Frame], seq_no: u8, count: u16) -> Vec<u8> {
        let mut out = vec![FRAME_VERSION, OP_DATA, seq_no];
        out.extend_from_slice(&count.to_be_bytes());
        for f in frames {
            encode_frame(&mut out, f);
        }
        out
    }

    #[test]
    fn golden_udp_std() {
        // STD id=0x123 dlc=2 data=AA BB, seq=7:
        // header: 02 00 07 00 01 | frame: 00 00 01 23 02 AA BB
        let f = mk(0x123, &[0xAA, 0xBB]);
        let pkt = build_udp(&f, 7);
        assert_eq!(pkt, vec![0x02, 0x00, 0x07, 0x00, 0x01, 0x00, 0x00, 0x01, 0x23, 0x02, 0xAA, 0xBB]);
    }

    #[test]
    fn golden_udp_ext() {
        // EXT id=0x1ABCDEF8 (EFF flag set) dlc=1 data=FF, seq=0:
        // can_id on wire = 0x9ABCDEF8 (big-endian), len=01
        let f = mk(0x1ABCDEF8 | CAN_EFF_FLAG, &[0xFF]);
        let pkt = build_udp(&f, 0);
        assert_eq!(
            pkt,
            vec![0x02, 0x00, 0x00, 0x00, 0x01, 0x9A, 0xBC, 0xDE, 0xF8, 0x01, 0xFF]
        );
    }

    #[test]
    fn udp_roundtrip_all() {
        let cases = [
            mk(0x123, &[1, 2, 3, 4, 5, 6, 7, 8]),
            mk(0x000, &[]),
            mk(0x7FF, &[0xAA, 0xBB, 0xCC]),
            mk(0x1ABCDEF8 | CAN_EFF_FLAG, &[9, 9, 9, 9, 9, 9, 9, 9]),
        ];
        for c in cases {
            let pkt = build_udp(&c, 1);
            let got = parse_udp(&pkt).expect("parse");
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].can_id, c.can_id);
            assert_eq!(got[0].len, c.len);
            assert_eq!(&got[0].data[..c.len as usize], &c.data[..c.len as usize]);
        }
    }

    #[test]
    fn parse_udp_decodes_multi_frame_batch() {
        // kvasilloni-im6.3: stock cannelloni batches several frames into one UDP
        // datagram under load. parse_udp's per-frame loop must advance the offset
        // correctly across a MIX of classic / FD / RTR frames - an off-by-one in
        // the offset arithmetic would mis-decode frame 2 onward.
        let classic = mk(0x123, &[0x11, 0x22, 0x33]); // STD, 3 data bytes
        let mut fd = Frame::default();
        fd.can_id = 0x200;
        fd.fd = true;
        fd.fd_flags = CANFD_BRS;
        fd.len = 16;
        for i in 0..16 {
            fd.data[i] = (0xA0 + i) as u8;
        }
        let mut rtr = Frame::default();
        rtr.can_id = 0x321 | CAN_RTR_FLAG;
        rtr.len = 8; // RTR carries a DLC but no data bytes

        let pkt = build_udp_batch(&[classic, fd, rtr], 9, 3);
        let got = parse_udp(&pkt).expect("batched datagram must parse");
        assert_eq!(got.len(), 3, "all three batched frames must decode");

        // frame 0: classic, decoded at the base offset
        assert_eq!(got[0].can_id, 0x123);
        assert!(!got[0].fd);
        assert_eq!(got[0].len, 3);
        assert_eq!(&got[0].data[..3], &[0x11, 0x22, 0x33]);
        // frame 1: FD + BRS, 16 bytes - only correct if frame 0 advanced p exactly
        assert_eq!(got[1].can_id, 0x200);
        assert!(got[1].fd);
        assert_eq!(got[1].fd_flags, CANFD_BRS);
        assert_eq!(got[1].len, 16);
        assert_eq!(&got[1].data[..16], &fd.data[..16]);
        // frame 2: RTR - DLC kept (kvasilloni-f1b), no data; correct only if the
        // FD frame's extra flags byte + 16 data bytes were all consumed.
        assert_eq!(got[2].can_id, 0x321 | CAN_RTR_FLAG);
        assert!(got[2].is_rtr());
        assert_eq!(got[2].len, 8);
    }

    #[test]
    fn parse_udp_truncated_batch_returns_none() {
        // Header claims 3 frames but only 2 bodies are present: parse_udp's
        // per-frame bounds check must return None (no panic, no over-read past the
        // datagram) rather than fabricate a third frame (kvasilloni-im6.3).
        let a = mk(0x100, &[1, 2]);
        let b = mk(0x101, &[3, 4]);
        let pkt = build_udp_batch(&[a, b], 0, 3); // count=3, only 2 bodies
        assert!(parse_udp(&pkt).is_none(), "an over-claimed count must yield None");

        // The same two frames with a truthful count=2 still parse cleanly.
        let ok = build_udp_batch(&[a, b], 0, 2);
        assert_eq!(parse_udp(&ok).unwrap().len(), 2);
    }

    /// Drive the streaming decoder the way the TCP RX loop does and confirm it
    /// reconstructs what encode_frame produced.
    fn stream_roundtrip(c: &Frame) -> Frame {
        let mut enc = Vec::new();
        encode_frame(&mut enc, c);
        let mut out = Frame::default();
        let mut st = DecodeState::Init;
        let mut off = 0usize;
        let mut need = match decode_stream(&[], &mut out, &mut st) {
            Decoded::Need(n) => n,
            _ => panic!("init"),
        };
        loop {
            let chunk = &enc[off..off + need];
            match decode_stream(chunk, &mut out, &mut st) {
                Decoded::Need(n) => {
                    off += need;
                    need = n;
                }
                Decoded::Complete => break,
                Decoded::Error => panic!("decode error"),
            }
        }
        out
    }

    #[test]
    fn tcp_stream_roundtrip() {
        for c in [
            mk(0x123, &[1, 2, 3, 4, 5, 6, 7, 8]),
            mk(0x000, &[]),
            mk(0x1ABCDEF8 | CAN_EFF_FLAG, &[0xDE, 0xAD]),
        ] {
            let got = stream_roundtrip(&c);
            assert_eq!(got.can_id, c.can_id);
            assert_eq!(got.len, c.len);
            assert_eq!(&got.data[..c.len as usize], &c.data[..c.len as usize]);
        }
    }

    #[test]
    fn fd_round_len_valid_dlcs() {
        assert_eq!(fd_round_len(0), 0);
        assert_eq!(fd_round_len(8), 8);
        assert_eq!(fd_round_len(9), 12);
        assert_eq!(fd_round_len(13), 16);
        assert_eq!(fd_round_len(33), 48);
        assert_eq!(fd_round_len(64), 64);
        assert_eq!(fd_round_len(200), 64);
    }

    #[test]
    fn golden_udp_fd() {
        // FD STD id=0x200, BRS, 12 data bytes 00..0B, seq=0:
        // header 02 00 00 00 01 | id 00 00 02 00 | len 0x8C (12|FD) | flags 01 | data
        let mut f = Frame::default();
        f.can_id = 0x200;
        f.fd = true;
        f.fd_flags = CANFD_BRS;
        f.len = 12;
        for i in 0..12 {
            f.data[i] = i as u8;
        }
        let pkt = build_udp(&f, 0);
        let mut expect = vec![0x02, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x02, 0x00, 0x8C, 0x01];
        expect.extend_from_slice(&(0u8..12).collect::<Vec<u8>>());
        assert_eq!(pkt, expect);

        // and it round-trips back through both decoders
        let got = parse_udp(&pkt).expect("parse")[0];
        assert!(got.fd && got.fd_flags == CANFD_BRS && got.len == 12);
        assert_eq!(&got.data[..12], &expect[11..23]);
    }

    #[test]
    fn fd_stream_roundtrip_with_flags() {
        let mut f = Frame::default();
        f.can_id = 0x1ABCDEF8 | CAN_EFF_FLAG;
        f.fd = true;
        f.fd_flags = CANFD_BRS | CANFD_ESI;
        f.len = 16;
        for i in 0..16 {
            f.data[i] = (0xA0 + i) as u8;
        }
        let got = stream_roundtrip(&f);
        assert!(got.fd);
        assert_eq!(got.fd_flags, CANFD_BRS | CANFD_ESI);
        assert_eq!(got.len, 16);
        assert_eq!(&got.data[..16], &f.data[..16]);
    }

    #[test]
    fn fd_flag_translation() {
        assert_eq!(kvaser_to_fd_flags(CAN_MSG_BRS | CAN_MSG_ESI), CANFD_BRS | CANFD_ESI);
        assert_eq!(fd_flags_to_kvaser(CANFD_BRS), CAN_MSG_BRS);
        assert_eq!(fd_flags_to_kvaser(CANFD_ESI), CAN_MSG_ESI);
    }

    #[test]
    fn parse_udp_rejects_overlong_len_without_panic() {
        // Classic frame claiming 100 data bytes (> 64) must be rejected as None,
        // not panic indexing Frame.data. Regression for kvasilloni-kkt.
        let mut pkt = vec![0x02, 0x00, 0x00, 0x00, 0x01]; // ver, DATA, seq, count=1
        pkt.extend_from_slice(&[0x00, 0x00, 0x01, 0x23]); // can_id 0x123
        pkt.push(100); // len = 100, no FD bit
        pkt.extend(std::iter::repeat(0xAB).take(100));
        assert!(parse_udp(&pkt).is_none());

        // FD frame claiming 100 bytes (len byte 0x80|100) is likewise rejected.
        let mut fd = vec![0x02, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x02, 0x00];
        fd.push(CANFD_FRAME | 100); // FD + len 100
        fd.push(0x00); // fd_flags
        fd.extend(std::iter::repeat(0xCD).take(100));
        assert!(parse_udp(&fd).is_none());

        // A valid 8-byte frame in the same shape still parses (guard isn't too tight).
        let ok = build_udp(&mk(0x123, &[1, 2, 3, 4, 5, 6, 7, 8]), 0);
        assert_eq!(parse_udp(&ok).unwrap()[0].len, 8);
    }

    #[test]
    fn decode_stream_rejects_overlong_len_without_panic() {
        // Classic len=70 (>64): Len state must return Error, never reach a Data
        // copy that overruns Frame.data. Regression for kvasilloni-kkt.
        let mut f = Frame::default();
        let mut st = DecodeState::Init;
        let _ = decode_stream(&[], &mut f, &mut st); // -> Need(4)
        let _ = decode_stream(&[0, 0, 1, 0x23], &mut f, &mut st); // -> Need(1)
        assert!(matches!(decode_stream(&[70], &mut f, &mut st), Decoded::Error));

        // FD len=70 (>64): rejected at the Len state, before requesting flags.
        let mut f2 = Frame::default();
        let mut st2 = DecodeState::Init;
        let _ = decode_stream(&[], &mut f2, &mut st2);
        let _ = decode_stream(&[0, 0, 1, 0x23], &mut f2, &mut st2);
        assert!(matches!(decode_stream(&[CANFD_FRAME | 70], &mut f2, &mut st2), Decoded::Error));
    }

    #[test]
    fn parse_udp_rejects_overlong_classic_keeps_valid_fd() {
        // kvasilloni-nmt: a non-FD frame can carry at most 8 bytes. One claiming
        // 9..64 (FD bit clear) must be rejected so it can never be delivered into
        // a classic caller's 8-byte canRead buffer.
        let mut pkt = vec![0x02, 0x00, 0x00, 0x00, 0x01]; // ver, DATA, seq, count=1
        pkt.extend_from_slice(&[0x00, 0x00, 0x01, 0x23]); // can_id 0x123
        pkt.push(9); // len = 9, no FD bit -> illegal for classic
        pkt.extend(std::iter::repeat(0xAB).take(9));
        assert!(parse_udp(&pkt).is_none(), "over-length classic frame must be rejected");

        // Exactly 8 (classic max) still parses.
        let ok8 = build_udp(&mk(0x123, &[1, 2, 3, 4, 5, 6, 7, 8]), 0);
        assert_eq!(parse_udp(&ok8).unwrap()[0].len, 8);

        // A real FD frame with 16 bytes (FD bit set) is still accepted - the limit
        // is per frame class, not a blanket 8.
        let mut fd = Frame::default();
        fd.can_id = 0x200;
        fd.fd = true;
        fd.len = 16;
        for i in 0..16 {
            fd.data[i] = i as u8;
        }
        let pktfd = build_udp(&fd, 0);
        let got = parse_udp(&pktfd).expect("valid FD frame must parse");
        assert!(got[0].fd && got[0].len == 16);
    }

    #[test]
    fn classic_rtr_dlc_consistent_across_transports() {
        // kvasilloni-f1b: a classic RTR frame carries a DLC but no data. Both the
        // UDP parser and the TCP stream decoder must report the same DLC (TCP used
        // to zero it).
        let mut f = Frame::default();
        f.can_id = 0x123 | CAN_RTR_FLAG;
        f.len = 8;
        let udp = parse_udp(&build_udp(&f, 0)).expect("parse")[0];
        let tcp = stream_roundtrip(&f);
        assert!(udp.is_rtr() && tcp.is_rtr(), "RTR flag lost");
        assert_eq!(udp.len, 8, "UDP dropped the RTR DLC");
        assert_eq!(tcp.len, 8, "TCP dropped the RTR DLC");
    }

    #[test]
    fn decode_stream_rejects_overlong_classic() {
        // kvasilloni-nmt: the TCP decoder must reject a non-FD frame over 8 bytes
        // at the Len state, before requesting a Data read that could be delivered
        // to a classic 8-byte buffer.
        let mut f = Frame::default();
        let mut st = DecodeState::Init;
        let _ = decode_stream(&[], &mut f, &mut st); // -> Need(4)
        let _ = decode_stream(&[0, 0, 1, 0x23], &mut f, &mut st); // -> Need(1)
        assert!(matches!(decode_stream(&[9], &mut f, &mut st), Decoded::Error));

        // len == 8 (classic max) is fine and asks for 8 data bytes.
        let mut f2 = Frame::default();
        let mut st2 = DecodeState::Init;
        let _ = decode_stream(&[], &mut f2, &mut st2);
        let _ = decode_stream(&[0, 0, 1, 0x23], &mut f2, &mut st2);
        assert!(matches!(decode_stream(&[8], &mut f2, &mut st2), Decoded::Need(8)));

        // An FD frame of 16 bytes is still accepted (FD bit set).
        let mut f3 = Frame::default();
        let mut st3 = DecodeState::Init;
        let _ = decode_stream(&[], &mut f3, &mut st3);
        let _ = decode_stream(&[0, 0, 1, 0x23], &mut f3, &mut st3);
        assert!(matches!(decode_stream(&[CANFD_FRAME | 16], &mut f3, &mut st3), Decoded::Need(1)));
    }

    #[test]
    fn parse_udp_huge_count_no_bodies_is_graceful_none() {
        // A datagram claiming count=65535 but carrying NO frame bodies parses to
        // None gracefully - the per-frame bounds check trips on the first missing
        // body - and never panics. (Renamed from the misleading
        // *_does_not_over_allocate: this None comes from the per-frame bounds, not
        // the allocation cap; deleting the cap line still passes here. The cap's
        // own guarantee is covered by udp_prealloc_cap_clamps_attacker_count.
        // kvasilloni-im6.2a / -56p.)
        let pkt = vec![0x02, 0x00, 0x00, 0xFF, 0xFF]; // ver, DATA, seq, count=65535
        assert!(parse_udp(&pkt).is_none());
        // A well-formed single-frame packet with the same shape still parses.
        let ok = build_udp(&mk(0x123, &[1, 2, 3]), 0);
        assert_eq!(parse_udp(&ok).unwrap().len(), 1);
    }

    #[test]
    fn udp_prealloc_cap_clamps_attacker_count() {
        // kvasilloni-56p: the pre-allocation hint must be clamped to what the
        // datagram could physically hold, never the attacker-controlled count. This
        // is the test actually attributable to the cap line - reverting the clamp to
        // a bare `count as usize` fails the first assertion (65535 != 2).
        assert_eq!(udp_prealloc_cap(65535, 7), 7 / FRAME_BASE_SIZE + 1); // 2, not 65535
        // A truthful count (datagram big enough to hold them) passes through.
        assert_eq!(udp_prealloc_cap(3, DATA_PACKET_BASE_SIZE + 3 * FRAME_BASE_SIZE), 3);
        // Never exceeds the physical bound for any count.
        assert!(udp_prealloc_cap(u16::MAX, 100) <= 100 / FRAME_BASE_SIZE + 1);
    }

    #[test]
    fn kvaser_translation_roundtrip() {
        let cid = kvaser_to_canid(0x1ABCDEF8, CAN_MSG_EXT);
        assert_eq!(cid, 0x1ABCDEF8 | CAN_EFF_FLAG);
        let (id, fl) = canid_to_kvaser(cid, false);
        assert_eq!(id, 0x1ABCDEF8);
        assert!(fl & CAN_MSG_EXT != 0);

        let cid = kvaser_to_canid(0x123, CAN_MSG_STD);
        assert_eq!(cid, 0x123);
        let (id, fl) = canid_to_kvaser(cid, false);
        assert_eq!(id, 0x123);
        assert!(fl & CAN_MSG_STD != 0);
    }

    // ======================= property tests (kvasilloni-lw6.1) =======================
    //
    // APPROACH: `proptest` (dev-dependency only - never linked into the shipped DLL).
    // The point tests above pin the known malformed cases from kkt/nmt/56p/f1b; these
    // properties prove the invariants hold across the whole input space:
    //   1. round-trip identity for arbitrary VALID frames (classic / FD / RTR, every
    //      id and every in-class length) through BOTH codecs;
    //   2. never-panic / no-OOB for ARBITRARY bytes fed to parse_udp and to the
    //      decode_stream state machine (catch_unwind asserts no unwind; the debug
    //      bounds checks active under `cargo test` assert no out-of-bounds index);
    //   3. the length-class invariant - no frame with len>8 (classic) or len>64 (FD)
    //      ever ESCAPES a decoder;
    //   4. the udp_prealloc_cap clamp bound.
    // Reverting either length guard in wire.rs is caught here: a dropped FD guard makes
    // a 65..127-byte frame overrun Frame.data -> property 2 catches the panic; a dropped
    // classic guard lets a 9..64-byte non-FD frame escape -> property 3 catches it.
    use proptest::prelude::*;

    /// Strategy for an arbitrary VALID frame: any can_id (RTR bit forced per `rtr`
    /// so RTR and non-RTR are both covered), classic or FD, a length within that
    /// class's limit, valid fd_flags (FD only), and matching random data.
    fn arb_frame() -> impl Strategy<Value = Frame> {
        (any::<u32>(), any::<bool>(), any::<bool>(), any::<u8>(), prop::collection::vec(any::<u8>(), 0..=64))
            .prop_map(|(id, fd, rtr, raw_flags, mut data)| {
                let max = if fd { MAX_FRAME_LEN } else { CLASSIC_FRAME_MAX_LEN };
                data.truncate(max);
                let mut can_id = id;
                if rtr {
                    can_id |= CAN_RTR_FLAG;
                } else {
                    can_id &= !CAN_RTR_FLAG;
                }
                let mut f = Frame::default();
                f.can_id = can_id;
                f.fd = fd;
                // fd_flags only travel on FD frames; a classic frame has no flags byte.
                f.fd_flags = if fd { raw_flags & (CANFD_BRS | CANFD_ESI) } else { 0 };
                f.len = data.len() as u8;
                f.data[..data.len()].copy_from_slice(&data);
                f
            })
    }

    /// Frames are equal after a round-trip when id/fd/len/fd_flags match and, for a
    /// non-RTR frame, the data does too. RTR carries a DLC but no data bytes, so a
    /// decoded RTR frame's data is all-zero by construction - excluded from compare.
    fn frame_eq(a: &Frame, b: &Frame) -> bool {
        a.can_id == b.can_id
            && a.fd == b.fd
            && a.len == b.len
            && a.fd_flags == b.fd_flags
            && (a.is_rtr() || a.data[..a.len as usize] == b.data[..b.len as usize])
    }

    /// Drive the streaming decoder over `buf` exactly as the TCP RX loop does -
    /// feeding precisely the requested number of bytes each step - decoding as many
    /// back-to-back frames as the buffer holds. Returns the decoded frames; stops on
    /// the first Error or when the buffer cannot satisfy the next read. Never reads
    /// past `buf`. Used to fuzz the state machine with arbitrary bytes.
    fn drive_decode_stream(buf: &[u8]) -> Vec<Frame> {
        let mut out = Vec::new();
        let mut f = Frame::default();
        let mut st = DecodeState::Init;
        let mut off = 0usize;
        loop {
            // (re)start a frame: Init consumes nothing and asks for the 4 id bytes.
            let mut need = match decode_stream(&[], &mut f, &mut st) {
                Decoded::Need(n) => n,
                _ => break,
            };
            loop {
                if off + need > buf.len() {
                    return out; // not enough bytes left to satisfy the next read
                }
                let chunk = &buf[off..off + need];
                off += need;
                match decode_stream(chunk, &mut f, &mut st) {
                    Decoded::Need(n) => need = n,
                    Decoded::Complete => {
                        out.push(f);
                        break; // st is back at Init; outer loop starts the next frame
                    }
                    Decoded::Error => return out,
                }
            }
        }
        out
    }

    /// Inputs for the parse_udp never-panic property. Two gates make naive random
    /// bytes useless here: (a) the `ver==2 && op==DATA` header is 1/65536, and (b)
    /// parse_udp is all-or-nothing - an attacker-huge `count` exhausts the buffer and
    /// returns None, discarding (hence hiding from the length-class check) any frame
    /// that escaped mid-loop. So we weight toward a VALID header with a SMALL count
    /// that the body can satisfy (datagram parses to Some, so escaped frames are
    /// actually inspected), keep a huge-count variant (over-claim / mid-loop overrun
    /// path), and keep fully-arbitrary bytes (header-rejection / truncation).
    fn arb_udp_input() -> impl Strategy<Value = Vec<u8>> {
        fn hdr(seq: u8, count: u16, body: Vec<u8>) -> Vec<u8> {
            let mut v = vec![FRAME_VERSION, OP_DATA, seq];
            v.extend_from_slice(&count.to_be_bytes());
            v.extend_from_slice(&body);
            v
        }
        prop_oneof![
            4 => (any::<u8>(), 1u16..=6, prop::collection::vec(any::<u8>(), 0..=600))
                .prop_map(|(s, c, b)| hdr(s, c, b)),
            2 => (any::<u8>(), any::<u16>(), prop::collection::vec(any::<u8>(), 0..=600))
                .prop_map(|(s, c, b)| hdr(s, c, b)),
            1 => prop::collection::vec(any::<u8>(), 0..=2048),
        ]
    }

    fn assert_len_class(f: &Frame) -> Result<(), TestCaseError> {
        if f.fd {
            prop_assert!(f.len as usize <= MAX_FRAME_LEN, "FD frame len {} > {}", f.len, MAX_FRAME_LEN);
        } else {
            prop_assert!(
                f.len as usize <= CLASSIC_FRAME_MAX_LEN,
                "classic frame len {} > {}",
                f.len,
                CLASSIC_FRAME_MAX_LEN
            );
        }
        Ok(())
    }

    proptest! {
        /// Property 1a: encode_frame -> decode_stream is identity for any valid frame.
        #[test]
        fn prop_tcp_roundtrip(f in arb_frame()) {
            let got = stream_roundtrip(&f);
            prop_assert!(frame_eq(&f, &got), "tcp round-trip mismatch: {:?} != {:?}", f, got);
            assert_len_class(&got)?;
        }

        /// Property 1b: build_udp -> parse_udp is identity for any valid frame.
        #[test]
        fn prop_udp_roundtrip(f in arb_frame(), seq in any::<u8>()) {
            let pkt = build_udp(&f, seq);
            let got = parse_udp(&pkt).expect("a valid frame must parse");
            prop_assert_eq!(got.len(), 1);
            prop_assert!(frame_eq(&f, &got[0]), "udp round-trip mismatch: {:?} != {:?}", f, got[0]);
            assert_len_class(&got[0])?;
        }

        /// Property 2a + 3: parse_udp never panics / never over-reads on ARBITRARY
        /// bytes, and every frame it returns obeys the length-class limit.
        #[test]
        fn prop_parse_udp_never_panics(buf in arb_udp_input()) {
            let res = std::panic::catch_unwind(|| parse_udp(&buf));
            prop_assert!(res.is_ok(), "parse_udp panicked on {:?}", buf);
            if let Ok(Some(frames)) = res {
                for fr in &frames {
                    assert_len_class(fr)?;
                }
            }
        }

        /// Property 2b + 3: the decode_stream state machine never panics / never
        /// over-reads on ARBITRARY bytes, and every frame it yields obeys the limit.
        #[test]
        fn prop_decode_stream_never_panics(buf in prop::collection::vec(any::<u8>(), 0..=2048)) {
            let res = std::panic::catch_unwind(|| drive_decode_stream(&buf));
            prop_assert!(res.is_ok(), "decode_stream panicked on {:?}", buf);
            if let Ok(frames) = res {
                for fr in &frames {
                    assert_len_class(fr)?;
                }
            }
        }

        /// Property 4: the pre-allocation hint is always clamped to what the datagram
        /// could physically hold, never the attacker-controlled count (kvasilloni-56p).
        #[test]
        fn prop_udp_prealloc_cap_bounded(count in any::<u16>(), buf_len in 0usize..=1_000_000) {
            let cap = udp_prealloc_cap(count, buf_len);
            prop_assert!(cap <= buf_len / FRAME_BASE_SIZE + 1);
            prop_assert!(cap <= count as usize);
        }
    }
}
