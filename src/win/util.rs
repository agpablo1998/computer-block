//! Small helpers shared by the Windows modules.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use windows::core::PCWSTR;

/// Null-terminated UTF-16 copy of `s`. Keep the Vec alive while the PCWSTR
/// is in use.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn pcwstr(v: &[u16]) -> PCWSTR {
    PCWSTR(v.as_ptr())
}

/// UTF-16 buffer (possibly null-terminated) to String.
pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(buf.get(..end).unwrap_or_default())
}

/// `%LOCALAPPDATA%\ComputerBlock`.
pub fn data_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("ComputerBlock"))
}

pub fn installed_exe() -> Option<PathBuf> {
    data_dir().map(|d| d.join("computer-block.exe"))
}

const LOG_LIMIT: u64 = 256 * 1024;

/// Append one line to `log.txt`. Never fails loudly: logging must not be
/// able to break enforcement.
pub fn log(msg: &str) {
    let Some(dir) = data_dir() else { return };
    let path = dir.join("log.txt");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_LIMIT) {
        let _ = std::fs::rename(&path, dir.join("log.old.txt"));
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mut line = String::new();
    let _ = writeln!(line, "{secs} {msg}");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}
