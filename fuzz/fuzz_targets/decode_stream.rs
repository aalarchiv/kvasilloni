// SPDX-License-Identifier: LGPL-3.0-or-later
//! Fuzz target: the wire::decode_stream state machine over arbitrary bytes
//! (kvasilloni-lw6.2).
//!
//! decode_stream is the headerless-TCP untrusted parse surface. This drives it
//! exactly as transport::tcp_rx_loop does - feeding precisely the requested number
//! of bytes each step - decoding as many back-to-back frames as the buffer holds.
//! ASAN aborts on any over-read; libFuzzer explores the per-state length / flag
//! boundaries.
#![no_main]

use libfuzzer_sys::fuzz_target;

// Each target uses only part of wire.rs, so the rest reads as dead code here.
#[path = "../../src/wire.rs"]
#[allow(dead_code)]
mod wire;
use wire::{decode_stream, DecodeState, Decoded, Frame};

fuzz_target!(|data: &[u8]| {
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
            if off + need > data.len() {
                return; // not enough bytes left to satisfy the next read
            }
            let chunk = &data[off..off + need];
            off += need;
            match decode_stream(chunk, &mut f, &mut st) {
                Decoded::Need(n) => need = n,
                Decoded::Complete => break, // st is back at Init; start the next frame
                Decoded::Error => return,
            }
        }
    }
});
