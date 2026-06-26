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
            if f.is_rtr() {
                *state = DecodeState::Init;
                f.len = 0;
                return Decoded::Complete;
            }
            if f.len == 0 {
                *state = DecodeState::Init;
                return Decoded::Complete;
            }
            if f.len as usize > CLASSIC_FRAME_MAX_LEN {
                // A non-FD frame over 8 bytes is malformed; rejecting it keeps an
                // over-length "classic" frame from ever reaching a caller's
                // 8-byte canRead buffer (kvasilloni-nmt).
                return Decoded::Error;
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
    // Cap the pre-allocation to what this datagram could physically hold (each
    // frame is >= FRAME_BASE_SIZE on the wire). `count` is attacker-controlled,
    // so trusting it would let a 7-byte spoofed packet force a ~5MB allocation.
    // The per-frame bounds below still validate the real content. (kvasilloni-56p)
    let cap = (count as usize).min(buf.len() / FRAME_BASE_SIZE + 1);
    let mut frames = Vec::with_capacity(cap);
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
    fn parse_udp_huge_count_does_not_over_allocate() {
        // A 7-byte datagram claiming count=65535 must not pre-allocate for 65535
        // frames; it has no frame bodies, so parsing returns None gracefully.
        // (kvasilloni-56p: capacity is bounded by the datagram length.)
        let pkt = vec![0x02, 0x00, 0x00, 0xFF, 0xFF]; // ver, DATA, seq, count=65535
        assert!(parse_udp(&pkt).is_none());
        // A well-formed single-frame packet with the same shape still parses.
        let ok = build_udp(&mk(0x123, &[1, 2, 3]), 0);
        assert_eq!(parse_udp(&ok).unwrap().len(), 1);
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
}
