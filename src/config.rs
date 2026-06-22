//! Layered configuration: built-in defaults < `kvasilloni.ini` < environment.
//!
//! The INI file is the Windows-native mechanism (drop it next to the DLL or next
//! to the application's .exe). Environment variables still override it so the
//! selftest and scripted runs keep working.
//!
//! `kvasilloni.ini` (a `[cannelloni]` section header is optional):
//! ```ini
//! [cannelloni]
//! host      = 192.168.1.50
//! port      = 20000
//! localport = 20000
//! proto     = udp        ; udp | tcp
//! tcprole   = client     ; client | server  (tcp only)
//! log       = C:\temp\kvasilloni.log
//! channels  = 1          ; advertised by canGetNumberOfChannels (retargeting)
//! ```

use std::collections::HashMap;
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
    }
}

fn starts_with_ci(s: &str, c: u8) -> bool {
    matches!(s.as_bytes().first(), Some(&b) if b.to_ascii_lowercase() == c)
}

/// Minimal INI parser: `key = value`, `;`/`#` comments, optional `[section]`
/// headers (ignored — all keys are flattened). Keys are lowercased.
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
}
