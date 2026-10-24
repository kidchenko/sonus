//! Linux: MPRIS, the D-Bus interface every Linux player and browser offers,
//! the same one the desktop's own media controls use.
//!
//! Each player owns a bus name under `org.mpris.MediaPlayer2.`. The bus says
//! which process owns it, and the process's executable is what a row's key
//! is made of, so that is how a player finds its row. A browser can have a
//! player per window or tab; the one playing wins.
//!
//! D-Bus calls wait on the player, which may be busy, so none of it runs on
//! the UI thread: a worker owns the bus, the panel hands it jobs and takes
//! whatever it last read, at most a refresh old.

use super::{Art, Command, Media, ART_EDGE};
use crate::audio::AppAudio;
use crate::log::{ldebug, lwarn};

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use gtk::gdk_pixbuf::{InterpType, Pixbuf};
use gtk::gio::{self, prelude::*};
use gtk::glib::{self, Variant, VariantDict};

const PREFIX: &str = "org.mpris.MediaPlayer2.";
const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
/// Milliseconds a player gets to answer before it is skipped for this read.
const TIMEOUT: i32 = 500;

enum Job {
    /// Read the players of these apps, by key.
    Read(Vec<String>),
    Command(String, Command),
}

/// What the worker last read, by app key, and when.
#[derive(Default)]
struct Reading {
    media: HashMap<String, Media>,
    at: Option<Instant>,
}

type Latest = Arc<Mutex<Reading>>;

pub struct Players {
    jobs: Option<Sender<Job>>,
    latest: Latest,
    /// A read is under way. While it is, refreshes do not queue more: a
    /// player that is slow to answer would otherwise build a backlog.
    reading: Arc<AtomicBool>,
}

impl Players {
    pub fn new() -> Self {
        let latest: Latest = Arc::default();
        let reading = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let spawned = {
            let latest = latest.clone();
            let reading = reading.clone();
            std::thread::Builder::new()
                .name("sonus-players".into())
                .spawn(move || Worker::default().run(rx, latest, reading))
        };
        let jobs = match spawned {
            Ok(_) => Some(tx),
            Err(e) => {
                lwarn!("no player controls: could not start their thread: {e}");
                None
            }
        };
        Self {
            jobs,
            latest,
            reading,
        }
    }

    /// Show what the worker last read, and ask it for a fresh reading for
    /// next time. Never waits.
    pub fn fill(&mut self, apps: &mut [AppAudio]) {
        if let Ok(latest) = self.latest.lock() {
            // A playing track has moved on since it was read.
            let since = latest.at.map_or(0.0, |at| at.elapsed().as_secs_f32());
            for app in apps.iter_mut().filter(|a| !a.system) {
                if let Some(media) = latest.media.get(&app.key) {
                    let mut media = media.clone();
                    if let (true, Some(at), Some(length)) =
                        (media.playing, media.position, media.duration)
                    {
                        media.position = Some((at + since).min(length));
                    }
                    app.media = Some(media);
                }
            }
        }
        if self.reading.swap(true, Ordering::Relaxed) {
            return;
        }
        let keys = apps
            .iter()
            .filter(|a| !a.system)
            .map(|a| a.key.clone())
            .collect();
        if let Some(jobs) = &self.jobs {
            if jobs.send(Job::Read(keys)).is_err() {
                self.reading.store(false, Ordering::Relaxed);
            }
        }
    }

    pub fn send(&mut self, key: &str, command: Command) {
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send(Job::Command(key.to_owned(), command));
        }
    }
}

/// One player as last read, for its commands.
struct Found {
    bus_name: String,
    track: Option<String>,
    position: Option<f32>,
}

/// The worker's side: the bus and the players found on it, which never leave
/// its thread.
#[derive(Default)]
struct Worker {
    bus: Option<gio::DBusConnection>,
    tried: bool,
    /// The player behind each row that has controls, for the buttons.
    players: HashMap<String, Found>,
    /// Each player's current cover, and the URL it came from. Fetched again
    /// only when that changes, and kept even when fetching failed, so a cover
    /// that cannot be had is not asked for every second.
    covers: HashMap<String, (String, Option<Art>)>,
}

impl Worker {
    fn run(mut self, jobs: Receiver<Job>, latest: Latest, reading: Arc<AtomicBool>) {
        for job in jobs {
            match job {
                Job::Read(keys) => {
                    let media = self.read(&keys);
                    if let Ok(mut latest) = latest.lock() {
                        latest.media = media;
                        latest.at = Some(Instant::now());
                    }
                    reading.store(false, Ordering::Relaxed);
                }
                Job::Command(key, command) => self.command(&key, command),
            }
        }
    }

    /// The session bus, connected on first use.
    fn bus(&mut self) -> Option<gio::DBusConnection> {
        if !self.tried {
            self.tried = true;
            match gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) {
                Ok(bus) => self.bus = Some(bus),
                Err(e) => lwarn!("no session bus, so no player controls: {e}"),
            }
        }
        self.bus.clone()
    }

    /// The best player for each app asked about, and what it is playing.
    fn read(&mut self, keys: &[String]) -> HashMap<String, Media> {
        self.players.clear();
        let mut found = HashMap::new();
        let Some(bus) = self.bus() else {
            return found;
        };
        let names: Vec<String> = call(
            &bus,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "ListNames",
            None,
        )
        .and_then(|r| r.child_value(0).get::<Vec<String>>())
        .unwrap_or_default();

        // The best player found for each key: (playing, player, media).
        let mut best: HashMap<String, (bool, Found, Media, Option<String>)> = HashMap::new();
        for name in names.into_iter().filter(|n| n.starts_with(PREFIX)) {
            let exe = owner_exe(&bus, &name);
            let Some(key) = keys
                .iter()
                .find(|k| player_belongs(&name[PREFIX.len()..], exe.as_deref(), k))
            else {
                ldebug!("player {name} ({exe:?}) matches no app");
                continue;
            };
            let Some((media, found_player, art_url)) = read_player(&bus, &name) else {
                continue;
            };
            let better = best
                .get(key)
                .is_none_or(|(playing, ..)| media.playing && !playing);
            if better {
                best.insert(key.clone(), (media.playing, found_player, media, art_url));
            }
        }
        for (key, (_, player, mut media, art_url)) in best {
            media.art = art_url.and_then(|url| self.cover(&key, &url));
            self.players.insert(key.clone(), player);
            found.insert(key, media);
        }
        found
    }

    /// The cover at `url`, decoded once per track.
    fn cover(&mut self, key: &str, url: &str) -> Option<Art> {
        if let Some((seen, art)) = self.covers.get(key) {
            if seen == url {
                return art.clone();
            }
        }
        let art = fetch(url)
            .and_then(|bytes| decode(&bytes, ART_EDGE))
            .map(|rgba| Art::new(url, rgba, ART_EDGE));
        ldebug!(
            "{key}: cover {}",
            if art.is_some() {
                "loaded"
            } else {
                "unavailable"
            }
        );
        self.covers
            .insert(key.to_string(), (url.to_string(), art.clone()));
        art
    }

    fn command(&mut self, key: &str, command: Command) {
        let (Some(bus), Some(player)) = (self.bus.clone(), self.players.get(key)) else {
            return;
        };
        let (method, args) = match command {
            Command::PlayPause => ("PlayPause", None),
            Command::Next => ("Next", None),
            Command::Previous => ("Previous", None),
            Command::Seek(s) => {
                let to = (s.max(0.0) as f64 * 1e6) as i64;
                match player
                    .track
                    .as_deref()
                    .and_then(|t| glib::variant::ObjectPath::try_from(t.to_string()).ok())
                {
                    Some(track) => ("SetPosition", Some((track, to).to_variant())),
                    // No track id to name: move by the difference instead.
                    None => {
                        let from = (player.position.unwrap_or(0.0) as f64 * 1e6) as i64;
                        ("Seek", Some((to - from,).to_variant()))
                    }
                }
            }
        };
        match call(&bus, &player.bus_name, PATH, PLAYER, method, args) {
            Some(_) => ldebug!("{key}: {command:?}"),
            None => lwarn!("{key}: {command:?} failed"),
        }
    }
}

fn call(
    bus: &gio::DBusConnection,
    name: &str,
    path: &str,
    iface: &str,
    method: &str,
    args: Option<Variant>,
) -> Option<Variant> {
    bus.call_sync(
        Some(name),
        path,
        iface,
        method,
        args.as_ref(),
        None,
        gio::DBusCallFlags::NONE,
        TIMEOUT,
        gio::Cancellable::NONE,
    )
    .map_err(|e| ldebug!("{name} {method}: {e}"))
    .ok()
}

/// The file name of the executable behind a bus name: `spotify`, `firefox`,
/// `chrome`.
fn owner_exe(bus: &gio::DBusConnection, name: &str) -> Option<String> {
    let pid = call(
        bus,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "GetConnectionUnixProcessID",
        Some((name,).to_variant()),
    )?
    .child_value(0)
    .get::<u32>()?;
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    exe.file_name().map(|f| f.to_string_lossy().to_string())
}

/// Whether the player on bus name `org.mpris.MediaPlayer2.<suffix>`, run by
/// the executable `exe`, belongs to the row whose key is `key`. Keys are the
/// executable's name, as the sound server reported it, or else the app's
/// name.
///
/// The bus name comes in two shapes: the player's name alone (`spotify`,
/// `vlc`), or with an instance after a dot (`firefox.instance_1_42`,
/// `chromium.instance2345`).
pub(crate) fn player_belongs(suffix: &str, exe: Option<&str>, key: &str) -> bool {
    let key = key.to_lowercase();
    if let Some(exe) = exe {
        let exe = exe.to_lowercase();
        // Firefox runs as `firefox-bin` on some distributions, Chrome as
        // `chrome` while the stream says `google-chrome`.
        if exe == key
            || exe.strip_suffix("-bin") == Some(key.as_str())
            || key.ends_with(&format!("-{exe}"))
        {
            return true;
        }
    }
    let name = suffix.split('.').next().unwrap_or(suffix).to_lowercase();
    name == key
}

/// What a player is playing, and how to steer it.
fn read_player(bus: &gio::DBusConnection, name: &str) -> Option<(Media, Found, Option<String>)> {
    let reply = call(
        bus,
        name,
        PATH,
        "org.freedesktop.DBus.Properties",
        "GetAll",
        Some((PLAYER,).to_variant()),
    )?;
    let props = VariantDict::new(Some(&reply.child_value(0)));
    let get_bool = |k: &str| {
        props
            .lookup_value(k, None)
            .and_then(|v| v.get::<bool>())
            .unwrap_or(false)
    };
    let status = props
        .lookup_value("PlaybackStatus", None)
        .and_then(|v| v.get::<String>())
        .unwrap_or_default();
    let meta = props
        .lookup_value("Metadata", None)
        .map(|m| VariantDict::new(Some(&m)));
    let text = |k: &str| {
        meta.as_ref()
            .and_then(|m| m.lookup_value(k, None))
            .and_then(|v| v.get::<String>())
            .filter(|s| !s.trim().is_empty())
    };
    let title = text("xesam:title").unwrap_or_default();
    let artist = meta
        .as_ref()
        .and_then(|m| m.lookup_value("xesam:artist", None))
        .and_then(|v| {
            v.get::<Vec<String>>()
                .or_else(|| v.get::<String>().map(|s| vec![s]))
        })
        .map(|a| a.join(", "))
        .unwrap_or_default();
    let track = meta
        .as_ref()
        .and_then(|m| m.lookup_value("mpris:trackid", None))
        .and_then(|v| {
            v.get::<glib::variant::ObjectPath>()
                .map(|p| p.as_str().to_string())
                .or_else(|| v.get::<String>())
        });
    // Microseconds; players disagree on the integer type.
    let micros = |v: Variant| {
        v.get::<i64>()
            .or_else(|| v.get::<u64>().map(|n| n as i64))
            .or_else(|| v.get::<i32>().map(i64::from))
            .or_else(|| v.get::<f64>().map(|n| n as i64))
    };
    let duration = meta
        .as_ref()
        .and_then(|m| m.lookup_value("mpris:length", None))
        .and_then(micros)
        .filter(|&n| n > 0)
        .map(|n| n as f32 / 1e6);
    let position = props
        .lookup_value("Position", None)
        .and_then(micros)
        .map(|n| (n.max(0) as f32 / 1e6).min(duration.unwrap_or(f32::MAX)));

    let control = get_bool("CanControl");
    let can_play_pause = control && (get_bool("CanPlay") || get_bool("CanPause"));
    let can_previous = control && get_bool("CanGoPrevious");
    let can_next = control && get_bool("CanGoNext");
    if title.is_empty() && !can_play_pause && !can_previous && !can_next {
        return None;
    }
    let media = Media {
        title,
        artist,
        playing: status == "Playing",
        can_play_pause,
        can_previous,
        can_next,
        position: duration.and(position),
        duration,
        can_seek: control && duration.is_some() && get_bool("CanSeek"),
        art: None,
    };
    let found = Found {
        bus_name: name.to_string(),
        track,
        position,
    };
    Some((media, found, text("mpris:artUrl")))
}

/// A cover's bytes. `file://` is read directly; anything else (Spotify's are
/// `https://`) through gio, which reaches the web where the desktop has GVfs,
/// and else through curl.
fn fetch(url: &str) -> Option<Vec<u8>> {
    if !(url.starts_with("file://") || url.starts_with("https://")) {
        return None;
    }
    let file = gio::File::for_uri(url);
    if let Ok((bytes, _)) = file.load_contents(gio::Cancellable::NONE) {
        return Some(bytes.to_vec());
    }
    if url.starts_with("https://") {
        let out = std::process::Command::new("curl")
            .args(["-sfL", "--max-time", "5", "--max-filesize", "16777216", url])
            .output()
            .ok()?;
        if out.status.success() && !out.stdout.is_empty() {
            return Some(out.stdout);
        }
    }
    None
}

/// An image file as an `edge` square of straight-alpha RGBA: its middle
/// square, scaled. A video's thumbnail is wide; a cover is square already.
fn decode(bytes: &[u8], edge: u32) -> Option<Vec<u8>> {
    let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from(bytes));
    let image = Pixbuf::from_stream(&stream, gio::Cancellable::NONE).ok()?;
    let side = image.width().min(image.height());
    if side <= 0 {
        return None;
    }
    let square = image.new_subpixbuf(
        (image.width() - side) / 2,
        (image.height() - side) / 2,
        side,
        side,
    );
    let scaled = square
        .scale_simple(edge as i32, edge as i32, InterpType::Bilinear)?
        .add_alpha(false, 0, 0, 0)
        .ok()?;
    let stride = scaled.rowstride() as usize;
    let bytes = scaled.read_pixel_bytes();
    let mut rgba = Vec::with_capacity((edge * edge * 4) as usize);
    for row in 0..edge as usize {
        rgba.extend_from_slice(&bytes[row * stride..row * stride + edge as usize * 4]);
    }
    Some(rgba)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn players_find_their_app() {
        assert!(player_belongs("spotify", Some("spotify"), "spotify"));
        assert!(player_belongs("spotify", None, "spotify"));
        assert!(player_belongs(
            "firefox.instance_1_42",
            Some("firefox-bin"),
            "firefox"
        ));
        assert!(player_belongs(
            "chromium.instance2345",
            Some("chrome"),
            "google-chrome"
        ));
        assert!(player_belongs("vlc", Some("vlc"), "vlc"));
        assert!(!player_belongs("spotify", Some("spotify"), "firefox"));
        assert!(!player_belongs(
            "chromium.instance2345",
            Some("chrome"),
            "firefox"
        ));
    }

    /// Against a player on a real session bus, as
    /// `org.mpris.MediaPlayer2.spotify`: packaging/linux/test-live.sh.
    #[test]
    #[ignore]
    fn mpris_players_are_read_and_steered() {
        let app = |key: &str| AppAudio {
            key: key.into(),
            name: key.into(),
            icon: None,
            volume: 1.0,
            muted: false,
            active: true,
            system: false,
            media: None,
        };
        let mut players = Players::new();
        let mut apps = vec![app("spotify"), app("firefox")];
        let read = |players: &mut Players, apps: &mut Vec<AppAudio>| {
            for a in apps.iter_mut() {
                a.media = None;
            }
            players.fill(apps);
            std::thread::sleep(std::time::Duration::from_millis(400));
            players.fill(apps);
        };
        read(&mut players, &mut apps);
        let media = apps[0].media.clone().expect("spotify has a player");
        eprintln!("SEEN {media:?}");
        assert_eq!(media.title, "Relax, Take It Easy");
        assert_eq!(media.artist, "Mika");
        assert!(media.playing && media.can_next && !media.can_previous && media.can_seek);
        assert_eq!(media.duration, Some(240.0));
        assert!(media.position.is_some_and(|p| (83.0..84.0).contains(&p)));
        let art = media.art.expect("the cover");
        assert_eq!(art.edge, ART_EDGE);
        let centre = ((ART_EDGE / 2 * ART_EDGE + ART_EDGE / 2) * 4) as usize;
        assert_eq!(&art.rgba[centre..centre + 3], &[200, 40, 90]);
        assert!(apps[1].media.is_none(), "firefox has no player");

        players.send("spotify", Command::PlayPause);
        players.send("spotify", Command::Seek(30.0));
        std::thread::sleep(std::time::Duration::from_millis(200));
        read(&mut players, &mut apps);
        let media = apps[0].media.clone().unwrap();
        assert!(!media.playing, "paused");
        assert_eq!(media.position, Some(30.0));
    }

    #[test]
    fn covers_decode_cropped_square() {
        // A 4x2 PNG, left half red and right half blue: the middle square
        // keeps a column of each.
        let mut png = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png, 4, 2);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            let px: Vec<u8> = (0..8)
                .flat_map(|i| {
                    if i % 4 < 2 {
                        [255, 0, 0, 255]
                    } else {
                        [0, 0, 255, 255]
                    }
                })
                .collect();
            w.write_image_data(&px).unwrap();
        }
        let rgba = decode(&png, 2).unwrap();
        assert_eq!(rgba.len(), 16);
        assert_eq!(&rgba[0..4], &[255, 0, 0, 255], "left is red");
        assert_eq!(&rgba[4..8], &[0, 0, 255, 255], "right is blue");
    }
}
