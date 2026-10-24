//! Now playing: the track an app is playing, and the buttons that steer it.
//!
//! Windows has this for every app, in the OS: the media sessions behind the
//! controls in the volume flyout (`GlobalSystemMediaTransportControls`). Each
//! says which app it belongs to and which buttons it supports right now.
//!
//! macOS has no per-app equivalent anyone may use. MediaRemote, which drives
//! Control Center's player, is private and closed to apps since 15.4. What is
//! public is AppleScript, and the two players people use most answer it:
//! Spotify and Music. Those two get controls; other apps keep their slider.
//!
//! Linux has it for every app again: MPRIS, the D-Bus interface players and
//! browsers offer the desktop's own media controls.
//!
//! Read only while the panel is open, on the refresh tick.

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

use crate::audio::AppAudio;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq)]
pub struct Media {
    pub title: String,
    pub artist: String,
    pub playing: bool,
    pub can_play_pause: bool,
    pub can_previous: bool,
    pub can_next: bool,
    /// Seconds into the track, as of now. `None` if the app does not say.
    pub position: Option<f32>,
    /// The track's length in seconds. `None` for a stream, or if unknown.
    pub duration: Option<f32>,
    pub can_seek: bool,
    /// The cover, when the player has one.
    pub art: Option<Art>,
}

/// Cover art, decoded once per track: a square of straight-alpha RGBA with
/// its corners already rounded, so either painter draws it like an icon.
#[derive(Clone)]
pub struct Art {
    /// The same cover always gets the same id: what painters cache by.
    pub id: u64,
    pub edge: u32,
    pub rgba: Arc<Vec<u8>>,
}

impl PartialEq for Art {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl std::fmt::Debug for Art {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Art({:x}, {}px)", self.id, self.edge)
    }
}

/// Pixels a cover is decoded at: its 40 points on a 3x screen.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
pub(crate) const ART_EDGE: u32 = 120;

impl Art {
    /// `rgba` is `edge` square, already cropped and scaled; `key` names the
    /// cover (its URL, or the track), for the id.
    pub(crate) fn new(key: &str, mut rgba: Vec<u8>, edge: u32) -> Self {
        use std::hash::{Hash, Hasher};
        round_corners(&mut rgba, edge, edge as f32 * 0.15);
        let mut h = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut h);
        Self {
            id: h.finish(),
            edge,
            rgba: Arc::new(rgba),
        }
    }
}

/// Fade the corners outside a circle of `radius` px, anti-aliased.
fn round_corners(rgba: &mut [u8], edge: u32, radius: f32) {
    let e = edge as f32;
    for y in 0..edge {
        for x in 0..edge {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let cx = px.clamp(radius, e - radius);
            let cy = py.clamp(radius, e - radius);
            let d = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
            let cover = (radius - d + 0.5).clamp(0.0, 1.0);
            if cover < 1.0 {
                let a = &mut rgba[((y * edge + x) * 4 + 3) as usize];
                *a = (*a as f32 * cover).round() as u8;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Command {
    PlayPause,
    Previous,
    Next,
    /// To this many seconds into the track.
    Seek(f32),
}

pub struct Players(imp::Players);

impl Players {
    pub fn new() -> Self {
        Self(imp::Players::new())
    }

    /// Fill in `media` on every app that has a player to show.
    pub fn fill(&mut self, apps: &mut [AppAudio]) {
        self.0.fill(apps);
    }

    pub fn send(&mut self, key: &str, command: Command) {
        self.0.send(key, command);
    }
}

/// Whether a Windows media session, by its app id, belongs to the app whose
/// key (its lowercased .exe path) is `exe`. App ids come in three shapes:
///
/// - the .exe's file name, for a plain desktop app: `Spotify.exe`;
/// - `Family_publisher!App`, for a packaged one, whose .exe lives in a folder
///   named after the family: `SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify`;
/// - a name the app set for itself, often its .exe's, sometimes with a
///   profile after a dot: `Chrome`, `MSEdge`, `Chrome.ABC123`.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn session_belongs(app_id: &str, exe: &str) -> bool {
    let app_id = app_id.to_lowercase();
    let exe = exe.to_lowercase();
    let file = exe.rsplit(['\\', '/']).next().unwrap_or(&exe);
    let stem = file.strip_suffix(".exe").unwrap_or(file);
    if app_id == file || app_id == stem {
        return true;
    }
    if let Some((family, _)) = app_id.split_once('!') {
        return match family.rsplit_once('_') {
            Some((name, publisher)) => {
                exe.contains(&format!("\\windowsapps\\{name}_"))
                    && exe.contains(&format!("__{publisher}\\"))
            }
            None => false,
        };
    }
    app_id.split('.').next() == Some(stem)
}

/// 83 seconds as "1:23", and an hour or more as "1:02:03".
pub fn clock(seconds: f32) -> String {
    let s = seconds.max(0.0) as u32;
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_find_their_app() {
        let spotify = r"c:\users\me\appdata\roaming\spotify\spotify.exe";
        assert!(session_belongs("Spotify.exe", spotify));
        assert!(session_belongs("Spotify", spotify));
        assert!(!session_belongs("Chrome", spotify));

        let store = r"c:\program files\windowsapps\spotifyab.spotifymusic_1.2.3.0_x64__zpdnekdrzrea0\spotify.exe";
        assert!(session_belongs(
            "SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify",
            store
        ));
        assert!(!session_belongs(
            "SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify",
            spotify
        ));

        let chrome = r"c:\program files\google\chrome\application\chrome.exe";
        assert!(session_belongs("Chrome", chrome));
        assert!(session_belongs("Chrome.QJ3K2", chrome));
        let edge = r"c:\program files (x86)\microsoft\edge\application\msedge.exe";
        assert!(session_belongs("MSEdge", edge));
        assert!(!session_belongs("MSEdge", chrome));
    }

    #[test]
    fn covers_get_round_corners_and_a_stable_id() {
        let edge = 20;
        let a = Art::new("x", vec![255; (edge * edge * 4) as usize], edge);
        assert_eq!(a.rgba[3], 0, "the corner is clear");
        let centre = ((edge / 2 * edge + edge / 2) * 4 + 3) as usize;
        assert_eq!(a.rgba[centre], 255, "the middle is not");
        assert_eq!(a, Art::new("x", vec![0; (edge * edge * 4) as usize], edge));
        assert_ne!(
            a,
            Art::new("y", vec![255; (edge * edge * 4) as usize], edge)
        );
    }

    #[test]
    fn clocks_read_like_a_player() {
        assert_eq!(clock(0.0), "0:00");
        assert_eq!(clock(83.9), "1:23");
        assert_eq!(clock(3723.0), "1:02:03");
        assert_eq!(clock(-4.0), "0:00");
    }
}
