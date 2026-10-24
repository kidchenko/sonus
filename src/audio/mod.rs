//! The audio side: the output device, the apps playing through it, and the
//! levels of both.
//!
//! This module is the shape every caller sees; the platform submodules do the
//! talking. Windows has had per-app volume in the OS since Vista, so there it
//! is a thin layer over WASAPI's audio sessions. macOS has no such thing, so
//! there Sonus builds it: each app whose level you change is captured with a
//! Core Audio process tap, which silences the original, and played back again
//! at the level you chose. Linux's sound server (PulseAudio, or PipeWire
//! speaking its protocol) has per-app volume as Windows does.
//!
//! Everything here runs on the UI thread. The calls are short (a property read,
//! a COM call, or reading what the sound server has already sent), and the
//! panel wants its answers before it paints.

#[cfg(windows)]
mod win;
#[cfg(windows)]
use win as imp;

#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
use mac as imp;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as imp;

use crate::config::Level;

/// The output device everything below plays through.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Output {
    pub name: String,
    /// 0.0 to 1.0.
    pub volume: f32,
    pub muted: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DeviceInfo {
    /// Opaque to everyone but the backend: an endpoint id on Windows, a
    /// device UID on macOS, a sink name on Linux.
    pub id: String,
    pub name: String,
    pub default: bool,
}

/// One app, which may be several audio sessions or processes underneath:
/// Chrome plays from a helper process per tab, and Teams opens a session per
/// call. They are one row and one slider, because that is how people think of
/// them.
#[derive(Clone, Debug, PartialEq)]
pub struct AppAudio {
    /// Stable for as long as the app runs, and across runs: the executable's
    /// path on Windows, the bundle id (or path) on macOS, the executable's
    /// name on Linux. What a level is remembered under.
    pub key: String,
    pub name: String,
    /// Where the icon comes from: the .exe, or the .app bundle.
    pub icon: Option<String>,
    pub volume: f32,
    pub muted: bool,
    /// Playing right now, as opposed to merely holding a session open.
    pub active: bool,
    /// Windows' "System sounds": the chimes, rather than an app.
    pub system: bool,
    /// What it is playing, with the buttons to steer it. Filled in by
    /// [`crate::media`] for the panel; `None` from the audio side.
    pub media: Option<crate::media::Media>,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub output: Option<Output>,
    pub devices: Vec<DeviceInfo>,
    pub apps: Vec<AppAudio>,
    /// Something the user should know about the audio side as a whole, shown
    /// in place of the app list: a permission to grant, or an OS too old.
    pub problem: Option<Problem>,
}

// Only macOS raises these; the panel draws them everywhere.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Clone, Debug, PartialEq)]
pub enum Problem {
    /// macOS: the "System Audio Recording" permission is off, so app volumes
    /// cannot be changed. Device volume still works.
    NeedsPermission,
    /// macOS older than 14.2: no process taps.
    Unsupported,
}

/// Live levels for the meters, read many times a second while the panel is
/// open. Each is the peak since the last read, 0.0 to 1.0.
#[derive(Clone, Debug, Default)]
pub struct Peaks {
    pub output: Option<f32>,
    pub apps: Vec<(String, f32)>,
}

pub struct Mixer(imp::Mixer);

impl Mixer {
    pub fn new() -> Result<Self, String> {
        imp::Mixer::new().map(Self)
    }

    /// Everything, read fresh: the device, its apps and their levels.
    pub fn snapshot(&mut self) -> Snapshot {
        let mut s = self.0.snapshot();
        s.apps.sort_by(|a, b| {
            a.system
                .cmp(&b.system)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then_with(|| a.key.cmp(&b.key))
        });
        s
    }

    pub fn peaks(&mut self) -> Peaks {
        self.0.peaks()
    }

    pub fn set_app_volume(&mut self, key: &str, volume: f32) {
        self.0.set_app_volume(key, volume.clamp(0.0, 1.0));
    }

    pub fn set_app_muted(&mut self, key: &str, muted: bool) {
        self.0.set_app_muted(key, muted);
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.0.set_volume(volume.clamp(0.0, 1.0));
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.0.set_muted(muted);
    }

    pub fn set_default_device(&mut self, id: &str) {
        self.0.set_default_device(id);
    }

    /// Housekeeping on a slow tick, whether or not the panel is open. macOS
    /// uses it to put remembered levels back on apps that have just started,
    /// and to follow a change of output device; Linux to close the meters
    /// once the panel has, and to reconnect to a restarted sound server;
    /// Windows needs nothing.
    pub fn tick(&mut self, remembered: &dyn Fn(&str) -> Option<Level>) {
        self.0.tick(remembered);
    }

    /// Whether this platform needs Sonus to remember levels. Windows keeps an
    /// app's volume itself, between runs; macOS forgets it with the tap.
    pub fn needs_memory() -> bool {
        cfg!(target_os = "macos")
    }
}
