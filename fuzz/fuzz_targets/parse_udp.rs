// SPDX-License-Identifier: LGPL-3.0-or-later
//! Fuzz target: wire::parse_udp over arbitrary bytes (kvasilloni-lw6.2).
//!
//! parse_udp consumes a raw UDP datagram straight off the wire - the source of
//! kkt/nmt/56p. ASAN (cargo-fuzz default) aborts on any out-of-bounds read or UB;
//! libFuzzer's coverage feedback drives it into the truncation / flag-combination /
//! count-boundary edges that the R1 structured property generator may not reach.
#![no_main]

use libfuzzer_sys::fuzz_target;

// The REAL codec (see fuzz/Cargo.toml for why #[path] rather than a dependency).
// Each target uses only part of wire.rs, so the rest reads as dead code here.
#[path = "../../src/wire.rs"]
#[allow(dead_code)]
mod wire;

fuzz_target!(|data: &[u8]| {
    // Must not panic / over-read for ANY input. Any frame it returns is validated by
    // parse_udp's own length-class guards; here we only need it to not crash.
    let _ = wire::parse_udp(data);
});
