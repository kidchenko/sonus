//! Minimal file logging to `%APPDATA%\Sonus\sonus.log` (or
//! `~/Library/Application Support/Sonus/sonus.log`).
//!
//! Under `windows_subsystem = "windows"` there is no console, so `eprintln!`
//! goes nowhere. Without a log, "the slider does nothing" is unanswerable: the
//! audio API refused, the app's session belongs to another user, the tap was
//! denied a permission, or the app simply is not running. All of those look
//! the same from the outside.
//!
//! Hand-rolled rather than pulling in `log` + a backend, for the same reason
//! as the rest of the family: this needs about sixty lines and the binary is a
//! stated constraint.
//!
//! Volume is low enough to leave on permanently: startup, device changes and
//! failures. Set `SONUS_LOG=debug` for every session found and every level
//! set, which is the thing you want the day an app's audio goes missing.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

static SINK: Mutex<Option<File>> = Mutex::new(None);
static DEBUG: AtomicBool = AtomicBool::new(false);

/// Rotate at 1 MB. A tray app can run for months, and an unbounded log on
/// someone else's disk is not our call to make.
const MAX_BYTES: u64 = 1_000_000;

pub fn path() -> Option<PathBuf> {
    crate::config::Config::dir().map(|d| d.join("sonus.log"))
}

pub fn debug_enabled() -> bool {
    DEBUG.load(Ordering::Relaxed)
}

pub fn init() {
    DEBUG.store(
        std::env::var("SONUS_LOG")
            .map(|v| v.eq_ignore_ascii_case("debug"))
            .unwrap_or(false),
        Ordering::Relaxed,
    );

    let Some(p) = path() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Keep one previous generation, so a crash loop cannot erase the run that
    // actually explains it.
    if std::fs::metadata(&p)
        .map(|m| m.len() > MAX_BYTES)
        .unwrap_or(false)
    {
        let _ = std::fs::rename(&p, p.with_extension("log.1"));
    }
    if let Ok(f) = OpenOptions::new().create(true).append(true).open(&p) {
        if let Ok(mut sink) = SINK.lock() {
            *sink = Some(f);
        }
    }
}

/// Local wall-clock time, to the millisecond. Local rather than UTC because the
/// log is read by a human next to a clock on the same wall.
#[cfg(windows)]
fn stamp() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}

#[cfg(target_os = "macos")]
fn stamp() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs() as libc::time_t;
    let millis = dur.subsec_millis();
    // localtime_r turns the UTC time_t into the process's local calendar time,
    // honouring the current zone and DST without pulling in a date crate.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::localtime_r(&secs, &mut tm);
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        millis
    )
}

pub fn write(level: &str, msg: &str) {
    // Formatted up front so the line reaches the file in a single write; the
    // UI thread and the instance listener both log, and interleaved fragments
    // are worst exactly when the log matters.
    let line = format!("{} {:<5} {}\n", stamp(), level, msg);
    let Ok(mut sink) = SINK.lock() else { return };
    if let Some(f) = sink.as_mut() {
        // A failed write is ignored on purpose: logging must never be the
        // reason the app misbehaves.
        let _ = f.write_all(line.as_bytes());
        let _ = f.flush();
    }
}

macro_rules! linfo  { ($($a:tt)*) => { $crate::log::write("INFO",  &format!($($a)*)) } }
macro_rules! lwarn  { ($($a:tt)*) => { $crate::log::write("WARN",  &format!($($a)*)) } }
macro_rules! lerror { ($($a:tt)*) => { $crate::log::write("ERROR", &format!($($a)*)) } }
/// Only emitted when `SONUS_LOG=debug`; the argument expression is not even
/// evaluated otherwise.
macro_rules! ldebug {
    ($($a:tt)*) => {
        if $crate::log::debug_enabled() {
            $crate::log::write("DEBUG", &format!($($a)*))
        }
    };
}

pub(crate) use {ldebug, lerror, linfo, lwarn};

/// Open the log in whatever handles .log (Notepad by default). Exposed on the
/// tray menu because the tray is the only surface this app has.
pub fn reveal() {
    if let Some(p) = path() {
        crate::util::shell_open(&p.to_string_lossy());
    }
}
