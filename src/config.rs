// SPDX-License-Identifier: LGPL-3.0-or-later
//! Layered configuration: built-in defaults < `kvasilloni.ini` < environment.
//!
//! The INI file is the Windows-native mechanism (drop it next to the DLL or next
//! to the application's .exe). Environment variables still override it so the
//! selftest and scripted runs keep working.
//!
//! Path note: the INI is auto-discovered relative to the DLL/EXE *module*
//! location, so it works regardless of the host's current working directory.
//! But a value you put in `log=` (or the `KVASILLONI_INI` env var) is opened
//! verbatim: a relative path resolves against the host process's CWD - often
//! `C:\Windows\System32` for a service - not the folder you dropped files in.
//! Always use ABSOLUTE paths for `log=` and `KVASILLONI_INI`.
//!
//! `kvasilloni.ini` (a `[cannelloni]` section header is optional):
//! ```ini
//! [cannelloni]
//! host      = 192.168.1.50   ; IPv4 or IPv6 literal
//! port      = 20000
//! localport = 20000        ; UDP only: must be unique per app running simultaneously
//! proto     = udp        ; udp | tcp
//! tcprole   = client     ; client | server  (tcp only)
//! peercheck = on         ; restrict inbound to host (cannelloni -R); off = accept all (-p)
//! allow     =            ; extra peer IPs, comma/space separated (replaces host default)
//! udpportfallback = off  ; UDP: bind ephemeral if localport busy (TX-only; see below)
//! log       = C:\temp\kvasilloni.log
//! channels  = 1          ; advertised by canGetNumberOfChannels (retargeting)
//! connecttimeout = 5000  ; tcp client connect timeout, ms (also bounds handshake + writes)
//! accepttimeout  = 30000 ; tcp server: how long to wait for a client, ms
//! ```
//!
//! Note: `canOpenChannel` blocks the calling thread during TCP setup for up to
//! `connecttimeout` (client) / `accepttimeout` (server). Open off any UI or
//! watchdog thread, or lower the timeout, if a fast non-blocking open matters.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;

#[derive(Clone)]
pub struct Config {
    pub host: String,
    pub remote_port: u16,
    pub local_port: u16,
    pub tcp: bool,
    pub tcp_server: bool,
    pub log: Option<String>,
    /// Number of channels advertised by `canGetNumberOfChannels` (retargeting).
    pub channels: u32,
    /// TCP client connect timeout, milliseconds (also bounds the handshake read).
    pub connect_timeout_ms: u32,
    /// TCP server accept timeout, milliseconds (how long to wait for a client).
    pub accept_timeout_ms: u32,
    /// UDP only: if the configured `local_port` is busy, bind an OS-assigned
    /// ephemeral port instead of failing the open. OFF by default: a stock
    /// cannelloni replies only to its fixed `-r` port, so an ephemeral bind
    /// receives nothing (TX-only). Opt in only if you knowingly want that.
    /// See kvasilloni-25q / kvasilloni-iai.
    pub udp_port_fallback: bool,
    /// Restrict inbound traffic to the configured peer (cannelloni's `-R`/`-p`).
    /// ON by default: UDP datagrams and TCP-server connections from any source
    /// other than `host` (or an `allow` entry) are dropped. See kvasilloni-872.
    pub peer_check: bool,
    /// Explicit allow-list of peer IPs. Empty => just the configured `host`.
    /// Only consulted when `peer_check` is on.
    pub allow: Vec<IpAddr>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            host: "127.0.0.1".into(),
            remote_port: 20000,
            local_port: 20000,
            tcp: false,
            tcp_server: false,
            log: None,
            channels: 1,
            connect_timeout_ms: 5000,
            accept_timeout_ms: 30000,
            udp_port_fallback: false,
            peer_check: true,
            allow: Vec::new(),
        }
    }
}

impl Config {
    /// Load defaults, then overlay the INI file (if found), then env overrides.
    pub fn load() -> Config {
        let mut cfg = Config::default();
        if let Some(map) = find_and_parse_ini() {
            cfg.apply_map(&map);
        }
        cfg.apply_env();
        cfg
    }

    fn apply_map(&mut self, m: &HashMap<String, String>) {
        if let Some(v) = m.get("host") {
            self.host = v.clone();
        }
        if let Some(v) = m.get("port").and_then(|v| v.parse().ok()) {
            self.remote_port = v;
        }
        if let Some(v) = m.get("localport").and_then(|v| v.parse().ok()) {
            self.local_port = v;
        }
        if let Some(v) = m.get("proto") {
            self.tcp = starts_with_ci(v, b't');
        }
        if let Some(v) = m.get("tcprole") {
            self.tcp_server = starts_with_ci(v, b's');
        }
        if let Some(v) = m.get("log") {
            if !v.is_empty() {
                self.log = Some(v.clone());
            }
        }
        if let Some(v) = m.get("channels").and_then(|v| v.parse().ok()) {
            self.channels = v;
        }
        if let Some(v) = m.get("connecttimeout").and_then(|v| v.parse().ok()) {
            self.connect_timeout_ms = v;
        }
        if let Some(v) = m.get("accepttimeout").and_then(|v| v.parse().ok()) {
            self.accept_timeout_ms = v;
        }
        if let Some(v) = m.get("udpportfallback") {
            self.udp_port_fallback = parse_bool(v);
        }
        if let Some(v) = m.get("peercheck") {
            self.peer_check = parse_bool(v);
        }
        if let Some(v) = m.get("allow") {
            self.allow = parse_ip_list(v);
        }
    }

    fn apply_env(&mut self) {
        let e = |k: &str| std::env::var(k).ok();
        if let Some(v) = e("KVASILLONI_HOST") {
            self.host = v;
        }
        if let Some(v) = e("KVASILLONI_PORT").and_then(|v| v.parse().ok()) {
            self.remote_port = v;
        }
        if let Some(v) = e("KVASILLONI_LOCALPORT").and_then(|v| v.parse().ok()) {
            self.local_port = v;
        }
        if let Some(v) = e("KVASILLONI_PROTO") {
            self.tcp = starts_with_ci(&v, b't');
        }
        if let Some(v) = e("KVASILLONI_TCPROLE") {
            self.tcp_server = starts_with_ci(&v, b's');
        }
        if let Some(v) = e("KVASILLONI_LOG") {
            if !v.is_empty() {
                self.log = Some(v);
            }
        }
        if let Some(v) = e("KVASILLONI_CHANNELS").and_then(|v| v.parse().ok()) {
            self.channels = v;
        }
        if let Some(v) = e("KVASILLONI_CONNECT_TIMEOUT").and_then(|v| v.parse().ok()) {
            self.connect_timeout_ms = v;
        }
        if let Some(v) = e("KVASILLONI_ACCEPT_TIMEOUT").and_then(|v| v.parse().ok()) {
            self.accept_timeout_ms = v;
        }
        if let Some(v) = e("KVASILLONI_UDP_PORT_FALLBACK") {
            self.udp_port_fallback = parse_bool(&v);
        }
        if let Some(v) = e("KVASILLONI_PEER_CHECK") {
            self.peer_check = parse_bool(&v);
        }
        if let Some(v) = e("KVASILLONI_ALLOW") {
            self.allow = parse_ip_list(&v);
        }
    }
}

fn starts_with_ci(s: &str, c: u8) -> bool {
    matches!(s.as_bytes().first(), Some(&b) if b.to_ascii_lowercase() == c)
}

/// Parse a boolean-ish config value. `1/true/yes/on/enable[d]` (any case) =>
/// true; everything else (including empty) => false.
fn parse_bool(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on" | "enable" | "enabled"
    )
}

/// Parse a comma/whitespace-separated list of IP literals (v4 or v6), skipping
/// anything that does not parse. Used for the peer allow-list.
fn parse_ip_list(s: &str) -> Vec<IpAddr> {
    s.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.parse::<IpAddr>().ok())
        .collect()
}

/// Minimal INI parser: `key = value`, `;`/`#` comments, optional `[section]`
/// headers (ignored - all keys are flattened). Keys are lowercased.
fn parse_ini(text: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            // strip inline comments from the value
            let v = v.split(|c| c == ';' || c == '#').next().unwrap_or("").trim();
            map.insert(k.trim().to_ascii_lowercase(), v.to_string());
        }
    }
    map
}

/// Search for `kvasilloni.ini` and parse it. Precedence:
///   1. `KVASILLONI_INI` (explicit path)
///   2. next to this DLL
///   3. next to the host application's .exe
fn find_and_parse_ini() -> Option<HashMap<String, String>> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(p) = std::env::var("KVASILLONI_INI") {
        if !p.is_empty() {
            candidates.push(PathBuf::from(p));
        }
    }
    if let Some(dir) = dll_dir() {
        candidates.push(dir.join("kvasilloni.ini"));
    }
    if let Some(dir) = exe_dir() {
        candidates.push(dir.join("kvasilloni.ini"));
    }
    for c in candidates {
        if let Ok(text) = std::fs::read_to_string(&c) {
            return Some(parse_ini(&text));
        }
    }
    None
}

// ----------------------------- module locations -----------------------------

#[cfg(windows)]
fn exe_dir() -> Option<PathBuf> {
    // null HMODULE => the path of the host process's .exe
    module_path(std::ptr::null_mut()).and_then(|p| p.parent().map(|p| p.to_path_buf()))
}

#[cfg(windows)]
fn dll_dir() -> Option<PathBuf> {
    let h = self_module()?;
    module_path(h).and_then(|p| p.parent().map(|p| p.to_path_buf()))
}

#[cfg(windows)]
mod sys {
    use std::os::raw::c_void;
    pub type HModule = *mut c_void;
    pub const FROM_ADDRESS: u32 = 0x4;
    pub const UNCHANGED_REFCOUNT: u32 = 0x2;
    extern "system" {
        pub fn GetModuleHandleExW(flags: u32, name: *const u16, module: *mut HModule) -> i32;
        pub fn GetModuleFileNameW(module: HModule, filename: *mut u16, size: u32) -> u32;
    }
}

/// HMODULE of this DLL, resolved from the address of a local function.
#[cfg(windows)]
fn self_module() -> Option<sys::HModule> {
    extern "C" fn anchor() {}
    let mut h: sys::HModule = std::ptr::null_mut();
    let ok = unsafe {
        sys::GetModuleHandleExW(
            sys::FROM_ADDRESS | sys::UNCHANGED_REFCOUNT,
            anchor as *const () as *const u16,
            &mut h,
        )
    };
    if ok != 0 && !h.is_null() {
        Some(h)
    } else {
        None
    }
}

#[cfg(windows)]
fn module_path(h: sys::HModule) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    let mut buf = vec![0u16; 32768];
    let n = unsafe { sys::GetModuleFileNameW(h, buf.as_mut_ptr(), buf.len() as u32) };
    if n == 0 || n as usize >= buf.len() {
        return None;
    }
    buf.truncate(n as usize);
    Some(PathBuf::from(std::ffi::OsString::from_wide(&buf)))
}

// Non-Windows (host `cargo test`): no module introspection; rely on KVASILLONI_INI/env.
#[cfg(not(windows))]
fn exe_dir() -> Option<PathBuf> {
    None
}
#[cfg(not(windows))]
fn dll_dir() -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ini_parsing_with_sections_and_comments() {
        let txt = "\
            ; a comment\n\
            [cannelloni]\n\
            host = 192.168.1.50   ; the linux box\n\
            Port=20000\n\
            proto = tcp\n\
            tcprole= server\n\
            # log disabled\n\
            log=\n";
        let m = parse_ini(txt);
        assert_eq!(m.get("host").unwrap(), "192.168.1.50");
        assert_eq!(m.get("port").unwrap(), "20000");
        assert_eq!(m.get("proto").unwrap(), "tcp");
        assert_eq!(m.get("tcprole").unwrap(), "server");
        assert_eq!(m.get("log").unwrap(), "");
    }

    #[test]
    fn map_applies_to_config() {
        let mut cfg = Config::default();
        let mut m = HashMap::new();
        m.insert("host".into(), "10.0.0.1".into());
        m.insert("port".into(), "21000".into());
        m.insert("proto".into(), "tcp".into());
        m.insert("tcprole".into(), "server".into());
        cfg.apply_map(&m);
        assert_eq!(cfg.host, "10.0.0.1");
        assert_eq!(cfg.remote_port, 21000);
        assert!(cfg.tcp);
        assert!(cfg.tcp_server);
    }

    #[test]
    fn timeouts_default_and_parse() {
        let cfg = Config::default();
        assert_eq!(cfg.connect_timeout_ms, 5000);
        assert_eq!(cfg.accept_timeout_ms, 30000);

        let mut cfg = Config::default();
        let mut m = HashMap::new();
        m.insert("connecttimeout".into(), "1500".into());
        m.insert("accepttimeout".into(), "8000".into());
        cfg.apply_map(&m);
        assert_eq!(cfg.connect_timeout_ms, 1500);
        assert_eq!(cfg.accept_timeout_ms, 8000);
    }

    #[test]
    fn peer_and_fallback_defaults_and_parse() {
        // Secure-by-default: peer check on, fallback off, no explicit allow-list.
        let cfg = Config::default();
        assert!(cfg.peer_check);
        assert!(!cfg.udp_port_fallback);
        assert!(cfg.allow.is_empty());

        let mut cfg = Config::default();
        let mut m = HashMap::new();
        m.insert("peercheck".into(), "off".into());
        m.insert("udpportfallback".into(), "1".into());
        m.insert("allow".into(), "127.0.0.1, ::1 10.0.0.5".into());
        cfg.apply_map(&m);
        assert!(!cfg.peer_check);
        assert!(cfg.udp_port_fallback);
        let want: Vec<IpAddr> = ["127.0.0.1", "::1", "10.0.0.5"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(cfg.allow, want);
    }

    #[test]
    fn parse_bool_and_ip_list_are_lenient() {
        assert!(parse_bool("YES") && parse_bool("On") && parse_bool("1"));
        assert!(!parse_bool("0") && !parse_bool("") && !parse_bool("nope"));
        // Invalid entries are skipped, not fatal.
        let ips = parse_ip_list("not-an-ip, 192.168.0.1,,fe80::1");
        let want: Vec<IpAddr> = ["192.168.0.1", "fe80::1"].iter().map(|s| s.parse().unwrap()).collect();
        assert_eq!(ips, want);
    }
}
