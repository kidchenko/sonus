//! Persisted settings, stored as JSON in `%APPDATA%\Sonus\config.json` or
//! `~/Library/Application Support/Sonus/config.json`.
//!
//! There is no settings window: the panel is the app, the tray menu carries
//! the switches, and "Edit settings file…" carries the rest.

use crate::log::{lerror, linfo};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    /// Percentage points per notch of the mouse wheel over a slider.
    pub wheel_step: u8,
    /// List Windows' own "System sounds" session. It is the one that plays the
    /// notification chimes, so muting it here is a quick way to a quiet
    /// afternoon; off hides it for anyone who never wants to see it.
    pub show_system_sounds: bool,
    /// Put an app back where you left it when it next makes a sound.
    ///
    /// Windows does this itself, per app, so this matters on macOS: Core Audio
    /// has no notion of an app's volume, and without it every app would start
    /// at full volume again.
    pub remember: bool,
    /// The levels being remembered, by app (bundle id where there is one).
    /// Written by the app; editing it by hand works, but there is little reason
    /// to.
    pub levels: BTreeMap<String, Level>,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Level {
    /// 0.0 to 1.0.
    pub volume: f32,
    #[serde(default)]
    pub muted: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            wheel_step: 2,
            show_system_sounds: true,
            remember: true,
            levels: BTreeMap::new(),
        }
    }
}

impl Config {
    /// `%APPDATA%\Sonus` on Windows, `~/Library/Application Support/Sonus` on
    /// macOS: the per-user, per-app spot each platform expects.
    #[cfg(windows)]
    pub fn dir() -> Option<PathBuf> {
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("Sonus"))
    }

    #[cfg(target_os = "macos")]
    pub fn dir() -> Option<PathBuf> {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support/Sonus"))
    }

    pub fn path() -> Option<PathBuf> {
        Self::dir().map(|d| d.join("config.json"))
    }

    /// Last-modified time of the config file, or `None` if it is not there.
    /// Used to notice a hand edit without parsing the file on every tick.
    pub fn modified() -> Option<std::time::SystemTime> {
        std::fs::metadata(Self::path()?).ok()?.modified().ok()
    }

    /// Read and parse, without logging or falling back to defaults.
    ///
    /// `None` covers both "not there" and "could not be parsed". A caller
    /// reacting to a change on disk wants to leave the running config alone in
    /// either case: an editor caught mid-save must not reset anyone's
    /// settings.
    pub fn reload() -> Option<Self> {
        let text = std::fs::read_to_string(Self::path()?).ok()?;
        Self::parse(&text).ok()
    }

    fn parse(text: &str) -> Result<Self, serde_json::Error> {
        // Strip a UTF-8 BOM. serde_json treats one as a syntax error, and
        // anyone who edits this file in Notepad or writes it from PowerShell
        // will have one.
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        serde_json::from_str::<Config>(text).map(Config::sanitised)
    }

    /// Never fails: a missing, unreadable, or corrupt file yields defaults.
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            linfo!("no config at {}; using defaults", path.display());
            return Self::default();
        };
        match Self::parse(&text) {
            Ok(c) => {
                linfo!(
                    "config loaded: wheelStep={} systemSounds={} remember={} levels={}",
                    c.wheel_step,
                    c.show_system_sounds,
                    c.remember,
                    c.levels.len()
                );
                c
            }
            Err(e) => {
                lerror!(
                    "config at {} is unreadable ({e}); falling back to defaults",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// Write via a temp file + rename so an interrupted save can never leave a
    /// half-written config behind.
    pub fn save(&self) -> std::io::Result<()> {
        let (Some(dir), Some(path)) = (Self::dir(), Self::path()) else {
            return Err(std::io::Error::other("no config directory in environment"));
        };
        std::fs::create_dir_all(&dir)?;
        let json = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)
    }

    /// Clamp anything a hand-edited config could set to a hostile value.
    pub fn sanitised(mut self) -> Self {
        self.wheel_step = self.wheel_step.clamp(1, 25);
        self.levels
            .retain(|k, l| !k.is_empty() && l.volume.is_finite());
        for l in self.levels.values_mut() {
            l.volume = l.volume.clamp(0.0, 1.0);
        }
        self
    }

    /// The level to put an app back to, if it has one worth restoring: an app
    /// at full volume and unmuted needs nothing done to it.
    pub fn level_for(&self, key: &str) -> Option<Level> {
        if !self.remember {
            return None;
        }
        self.levels
            .get(key)
            .copied()
            .filter(|l| l.muted || l.volume < 0.999)
    }

    /// Record an app's level. Returns whether anything changed, so the caller
    /// only writes the file when it has to. Full volume and unmuted is stored
    /// as no entry at all, so the file holds only the apps you have touched.
    pub fn remember_level(&mut self, key: &str, level: Level) -> bool {
        if !self.remember || key.is_empty() {
            return false;
        }
        let neutral = !level.muted && level.volume >= 0.999;
        if neutral {
            return self.levels.remove(key).is_some();
        }
        let level = Level {
            volume: (level.volume * 100.0).round() / 100.0,
            muted: level.muted,
        };
        if self.levels.get(key) == Some(&level) {
            return false;
        }
        self.levels.insert(key.to_string(), level);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hand_edit_cannot_set_hostile_values() {
        let c = Config::parse(
            r#"{"wheelStep": 0, "levels": {"a": {"volume": 7.0}, "": {"volume": 0.5}}}"#,
        )
        .unwrap();
        assert_eq!(c.wheel_step, 1);
        assert_eq!(c.levels.len(), 1, "an empty key is dropped");
        assert_eq!(c.levels["a"].volume, 1.0);
    }

    #[test]
    fn a_bom_is_not_a_syntax_error() {
        assert!(Config::parse("\u{feff}{}").is_ok());
    }

    #[test]
    fn full_volume_is_not_stored() {
        let mut c = Config::default();
        assert!(c.remember_level(
            "com.spotify.client",
            Level {
                volume: 0.4,
                muted: false
            }
        ));
        assert!(c.level_for("com.spotify.client").is_some());
        assert!(c.remember_level(
            "com.spotify.client",
            Level {
                volume: 1.0,
                muted: false
            }
        ));
        assert!(c.levels.is_empty(), "back to neutral removes the entry");
        assert!(c.level_for("com.spotify.client").is_none());
    }

    #[test]
    fn an_unchanged_level_is_not_a_change() {
        let mut c = Config::default();
        let l = Level {
            volume: 0.333,
            muted: true,
        };
        assert!(c.remember_level("x", l));
        assert!(!c.remember_level("x", l), "same level twice is no write");
    }

    #[test]
    fn remembering_can_be_turned_off() {
        let mut c = Config {
            remember: false,
            ..Default::default()
        };
        assert!(!c.remember_level(
            "x",
            Level {
                volume: 0.2,
                muted: false
            }
        ));
        assert!(c.level_for("x").is_none());
    }
}
