//! macOS: Spotify and Music, by AppleScript.
//!
//! Scripting another app needs the Automation permission, one app at a time
//! ("Sonus wants to control Spotify"). macOS asks for it on the first script
//! sent, and that script waits for the answer, as every script waits on the
//! player. So none of it runs on the UI thread. A worker thread owns the
//! scripts; the panel hands it jobs and takes whatever it last read, at most
//! a refresh old.
//!
//! The permission is not looked up beforehand: on macOS 26,
//! `AEDeterminePermissionToAutomateTarget` was seen to block for good
//! without ever reaching the system, prompt or no prompt.

use super::{Art, Command, Media, ART_EDGE};
use crate::audio::AppAudio;
use crate::log::{ldebug, linfo, lwarn};

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use objc2::msg_send;
use objc2::rc::{autoreleasepool, Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2_foundation::{NSData, NSNumber, NSPoint, NSRect, NSSize, NSString};

struct Player {
    bundle: &'static str,
    /// Spotify gives a track's length in milliseconds, whatever its
    /// dictionary says; Music gives seconds.
    millis: bool,
    /// Where the cover comes from, and the read script's last item, which
    /// says which cover it is.
    cover: Cover,
}

#[derive(Clone, Copy, PartialEq)]
enum Cover {
    /// Spotify: the track's `artwork url`, an image on Spotify's servers.
    Url,
    /// Music: the image itself, from the track's artwork. The read gets the
    /// album, so the image is asked for only when the album changes.
    Data,
}

const PLAYERS: [Player; 2] = [
    Player {
        bundle: "com.spotify.client",
        millis: true,
        cover: Cover::Url,
    },
    Player {
        bundle: "com.apple.Music",
        millis: false,
        cover: Cover::Data,
    },
];

const ERR_NOT_PERMITTED: i32 = -1743;

/// Read the player in one round trip: its state, then the track, then what
/// names its cover. Stopped, or with nothing loaded, it is the state alone.
fn read_script(p: &Player) -> String {
    let bundle = p.bundle;
    let cover = match p.cover {
        Cover::Url => "artwork url of t",
        Cover::Data => "album of t",
    };
    format!(
        r#"with timeout of 1 second
    tell application id "{bundle}"
        set s to player state as string
        if s is "stopped" then return {{s}}
        try
            set t to current track
            return {{s, name of t, artist of t, player position, duration of t, {cover}}}
        on error
            return {{"stopped"}}
        end try
    end tell
end timeout"#
    )
}

/// Music's cover, as the image file it is (JPEG or PNG).
fn music_art_script(bundle: &str) -> String {
    format!(
        r#"with timeout of 2 seconds
    tell application id "{bundle}"
        try
            return raw data of artwork 1 of current track
        end try
    end tell
end timeout"#
    )
}

fn command_script(bundle: &str, command: Command) -> String {
    let verb = match command {
        Command::PlayPause => "playpause".to_string(),
        Command::Next => "next track".to_string(),
        Command::Previous => "previous track".to_string(),
        Command::Seek(s) => format!("set player position to {:.2}", s.max(0.0)),
    };
    format!(
        r#"with timeout of 1 second
    tell application id "{bundle}" to {verb}
end timeout"#
    )
}

/// A script compiled once and run many times.
struct Script(Retained<AnyObject>);

impl Script {
    fn new(source: &str) -> Option<Self> {
        unsafe {
            let class = AnyClass::get(c"NSAppleScript")?;
            let source = NSString::from_str(source);
            let script: Allocated<AnyObject> = msg_send![class, alloc];
            let script: Option<Retained<AnyObject>> = msg_send![script, initWithSource: &*source];
            let script = script?;
            let mut error: *mut AnyObject = std::ptr::null_mut();
            let ok: bool = msg_send![&script, compileAndReturnError: &mut error];
            if !ok {
                lwarn!(
                    "could not compile a player script ({})",
                    error_number(error)
                );
                return None;
            }
            Some(Self(script))
        }
    }

    /// The result, or the AppleScript error number.
    fn run(&self) -> Result<Retained<AnyObject>, i32> {
        unsafe {
            let mut error: *mut AnyObject = std::ptr::null_mut();
            let result: Option<Retained<AnyObject>> =
                msg_send![&self.0, executeAndReturnError: &mut error];
            result.ok_or_else(|| error_number(error))
        }
    }
}

fn error_number(error: *mut AnyObject) -> i32 {
    if error.is_null() {
        return 0;
    }
    unsafe {
        let key = NSString::from_str("NSAppleScriptErrorNumber");
        let n: Option<Retained<NSNumber>> = msg_send![error, objectForKey: &*key];
        n.map(|n| n.intValue()).unwrap_or(0)
    }
}

/// Item `i` (from 1, as AppleScript counts) of a list result.
fn item(list: &AnyObject, i: isize) -> Option<Retained<AnyObject>> {
    unsafe { msg_send![list, descriptorAtIndex: i] }
}

fn text(d: &AnyObject) -> String {
    let s: Option<Retained<NSString>> = unsafe { msg_send![d, stringValue] };
    s.map(|s| s.to_string()).unwrap_or_default()
}

/// Numbers come back as numbers, not text: a real as text would follow the
/// user's locale, and "83,5" is not a number to Rust.
fn number(d: &AnyObject) -> f64 {
    unsafe { msg_send![d, doubleValue] }
}

/// The track, and what names its cover (a URL, or the album).
fn parse(list: &AnyObject, millis: bool) -> Option<(Media, String)> {
    let count: isize = unsafe { msg_send![list, numberOfItems] };
    let state = text(&*item(list, 1)?);
    if count < 5 || state == "stopped" {
        return None;
    }
    let duration = number(&*item(list, 5)?) as f32 / if millis { 1000.0 } else { 1.0 };
    let duration = (duration > 0.0).then_some(duration);
    let position = number(&*item(list, 4)?) as f32;
    let cover = item(list, 6).map(|d| text(&d)).unwrap_or_default();
    let media = Media {
        title: text(&*item(list, 2)?),
        artist: text(&*item(list, 3)?),
        playing: state == "playing",
        can_play_pause: true,
        can_previous: true,
        can_next: true,
        position: Some(match duration {
            Some(d) => position.clamp(0.0, d),
            None => position.max(0.0),
        }),
        duration,
        can_seek: duration.is_some(),
        art: None,
    };
    Some((media, cover))
}

/// An image file's bytes as an `edge` square of RGBA: the middle square of
/// it, scaled. Drawn by AppKit into a bitmap of our own, which is safe off
/// the main thread: nothing on screen is involved.
fn decode(data: &NSData, edge: u32) -> Option<Vec<u8>> {
    unsafe {
        let image: Allocated<AnyObject> = msg_send![AnyClass::get(c"NSImage")?, alloc];
        let image: Option<Retained<AnyObject>> = msg_send![image, initWithData: data];
        let image = image?;
        let size: NSSize = msg_send![&image, size];
        let side = size.width.min(size.height);
        if side <= 0.0 {
            return None;
        }
        let from = NSRect::new(
            NSPoint::new((size.width - side) / 2.0, (size.height - side) / 2.0),
            NSSize::new(side, side),
        );
        let rep: Allocated<AnyObject> = msg_send![AnyClass::get(c"NSBitmapImageRep")?, alloc];
        let space = NSString::from_str("NSDeviceRGBColorSpace");
        let rep: Option<Retained<AnyObject>> = msg_send![
            rep,
            initWithBitmapDataPlanes: std::ptr::null_mut::<*mut u8>(),
            pixelsWide: edge as isize,
            pixelsHigh: edge as isize,
            bitsPerSample: 8isize,
            samplesPerPixel: 4isize,
            hasAlpha: Bool::YES,
            isPlanar: Bool::NO,
            colorSpaceName: &*space,
            bitmapFormat: 0usize,
            bytesPerRow: (edge * 4) as isize,
            bitsPerPixel: 32isize
        ];
        let rep = rep?;
        let contexts = AnyClass::get(c"NSGraphicsContext")?;
        let ctx: Option<Retained<AnyObject>> =
            msg_send![contexts, graphicsContextWithBitmapImageRep: &*rep];
        let ctx = ctx?;
        let _: () = msg_send![contexts, saveGraphicsState];
        let _: () = msg_send![contexts, setCurrentContext: &*ctx];
        // NSImageInterpolationHigh: a 640 px cover down to 120 stays smooth.
        let _: () = msg_send![&ctx, setImageInterpolation: 3usize];
        let to = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(edge as f64, edge as f64),
        );
        // NSCompositingOperationCopy.
        let _: () =
            msg_send![&image, drawInRect: to, fromRect: from, operation: 1usize, fraction: 1.0f64];
        let _: () = msg_send![&ctx, flushGraphics];
        let _: () = msg_send![contexts, restoreGraphicsState];
        let bytes: *const u8 = msg_send![&rep, bitmapData];
        if bytes.is_null() {
            return None;
        }
        // Premultiplied, but covers are opaque: the same as straight alpha.
        Some(std::slice::from_raw_parts(bytes, (edge * edge * 4) as usize).to_vec())
    }
}

/// Spotify's covers are plain https URLs on its image server.
fn fetch(url: &str) -> Option<Retained<NSData>> {
    if !url.starts_with("https://") {
        return None;
    }
    unsafe {
        let url = NSString::from_str(url);
        let url: Option<Retained<AnyObject>> =
            msg_send![AnyClass::get(c"NSURL")?, URLWithString: &*url];
        msg_send![AnyClass::get(c"NSData")?, dataWithContentsOfURL: &*url?]
    }
}

enum Job {
    /// Read these players, which are running.
    Read(Vec<&'static Player>),
    Command(&'static Player, Command),
}

#[derive(Clone, Copy, PartialEq)]
enum Access {
    Denied,
}

/// What the worker last read, by bundle id. Missing: not read yet, or not
/// allowed. `None`: read, and nothing is playing.
type Latest = Arc<Mutex<HashMap<&'static str, Option<Media>>>>;

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

    fn player(key: &str) -> Option<&'static Player> {
        PLAYERS.iter().find(|p| p.bundle == key)
    }

    /// Show what the worker last read, and ask it for a fresh reading for
    /// next time. Never waits.
    pub fn fill(&mut self, apps: &mut [AppAudio]) {
        let running: Vec<&'static Player> =
            apps.iter().filter_map(|a| Self::player(&a.key)).collect();
        if let Ok(mut latest) = self.latest.lock() {
            // A player that quit leaves nothing stale behind for its return.
            latest.retain(|bundle, _| running.iter().any(|p| p.bundle == *bundle));
            for app in apps.iter_mut() {
                if let Some(Some(media)) = latest.get(app.key.as_str()) {
                    app.media = Some(media.clone());
                }
            }
        }
        if running.is_empty() || self.reading.swap(true, Ordering::Relaxed) {
            return;
        }
        if let Some(jobs) = &self.jobs {
            if jobs.send(Job::Read(running)).is_err() {
                self.reading.store(false, Ordering::Relaxed);
            }
        }
    }

    pub fn send(&mut self, key: &str, command: Command) {
        if let (Some(player), Some(jobs)) = (Self::player(key), &self.jobs) {
            let _ = jobs.send(Job::Command(player, command));
        }
    }
}

/// The worker's side: who said no, and the compiled scripts, which
/// never leave its thread.
#[derive(Default)]
struct Worker {
    access: HashMap<&'static str, Access>,
    reads: HashMap<&'static str, Script>,
    commands: HashMap<(&'static str, &'static str), Script>,
    art_scripts: HashMap<&'static str, Script>,
    /// Each player's current cover, and what named it. Fetched again only
    /// when that changes, and kept even when fetching failed, so a cover
    /// that cannot be had is not asked for every second.
    covers: HashMap<&'static str, (String, Option<Art>)>,
}

impl Worker {
    fn run(mut self, jobs: Receiver<Job>, latest: Latest, reading: Arc<AtomicBool>) {
        for job in jobs {
            autoreleasepool(|_| match job {
                Job::Read(players) => {
                    for p in players {
                        self.read(p, &latest);
                    }
                    reading.store(false, Ordering::Relaxed);
                }
                Job::Command(p, command) => {
                    self.command(p, command);
                    // Straight away, so the refresh just after the click
                    // shows the new track or state.
                    self.read(p, &latest);
                }
            });
        }
    }

    /// Whether scripts may be sent to `p`: yes until one fails for want of
    /// permission.
    fn allowed(&self, p: &'static Player) -> bool {
        self.access.get(p.bundle) != Some(&Access::Denied)
    }

    fn read(&mut self, p: &'static Player, latest: &Latest) {
        if !self.allowed(p) {
            return;
        }
        let script = match self.reads.entry(p.bundle) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => match Script::new(&read_script(p)) {
                Some(s) => e.insert(s),
                None => return,
            },
        };
        match script.run() {
            Ok(list) => {
                let media = parse(&list, p.millis).map(|(mut media, cover)| {
                    media.art = self.cover(p, &media, &cover);
                    media
                });
                if let Ok(mut latest) = latest.lock() {
                    latest.insert(p.bundle, media);
                }
            }
            Err(rc) => self.failed(p, rc),
        }
    }

    /// The track's cover, from the cache unless it changed.
    fn cover(&mut self, p: &'static Player, media: &Media, named: &str) -> Option<Art> {
        let key = match p.cover {
            Cover::Url => named.to_string(),
            Cover::Data => format!("{}\n{}\n{named}", media.artist, media.title),
        };
        if key.is_empty() {
            return None;
        }
        if let Some((k, art)) = self.covers.get(p.bundle) {
            if *k == key {
                return art.clone();
            }
        }
        let data = match p.cover {
            Cover::Url => fetch(&key),
            Cover::Data => {
                let script = match self.art_scripts.entry(p.bundle) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => {
                        e.insert(Script::new(&music_art_script(p.bundle))?)
                    }
                };
                script
                    .run()
                    .ok()
                    .and_then(|d| unsafe { msg_send![&d, data] })
            }
        };
        let art = data
            .and_then(|d: Retained<NSData>| decode(&d, ART_EDGE))
            .map(|rgba| Art::new(&key, rgba, ART_EDGE));
        ldebug!(
            "{}: cover {}",
            p.bundle,
            if art.is_some() {
                "loaded"
            } else {
                "unavailable"
            }
        );
        self.covers.insert(p.bundle, (key, art.clone()));
        art
    }

    fn command(&mut self, p: &'static Player, command: Command) {
        if !self.allowed(p) {
            return;
        }
        let name = match command {
            Command::PlayPause => "playpause",
            Command::Next => "next",
            Command::Previous => "previous",
            Command::Seek(_) => "seek",
        };
        // A seek's script names its position, so it is compiled each time.
        let once;
        let script = if let Command::Seek(_) = command {
            match Script::new(&command_script(p.bundle, command)) {
                Some(s) => {
                    once = s;
                    &once
                }
                None => return,
            }
        } else {
            match self.commands.entry((p.bundle, name)) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => {
                    match Script::new(&command_script(p.bundle, command)) {
                        Some(s) => e.insert(s),
                        None => return,
                    }
                }
            }
        };
        match script.run() {
            Ok(_) => ldebug!("{}: {name}", p.bundle),
            Err(rc) => self.failed(p, rc),
        }
    }

    /// A script that failed for want of permission means it was taken away:
    /// stop until Sonus runs again. Anything else (a timeout, a player that
    /// just quit) is worth trying again next time.
    fn failed(&mut self, p: &'static Player, rc: i32) {
        if rc == ERR_NOT_PERMITTED {
            linfo!("not allowed to control {}", p.bundle);
            self.access.insert(p.bundle, Access::Denied);
        } else {
            ldebug!("{}: script failed ({rc})", p.bundle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tall picture, red on top and blue below: the middle square of it
    /// comes out upright, still red on top.
    #[test]
    fn covers_decode_upright_and_cropped_square() {
        let (w, h) = (4u32, 8u32);
        let mut px = Vec::new();
        for y in 0..h {
            for _ in 0..w {
                px.extend_from_slice(if y < h / 2 {
                    &[255, 0, 0, 255]
                } else {
                    &[0, 0, 255, 255]
                });
            }
        }
        let mut file = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut file, w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            enc.write_header().unwrap().write_image_data(&px).unwrap();
        }
        let edge = 16;
        let rgba = decode(&NSData::with_bytes(&file), edge).expect("decoded");
        assert_eq!(rgba.len(), (edge * edge * 4) as usize);
        let at = |x: u32, y: u32| &rgba[((y * edge + x) * 4) as usize..][..3];
        assert!(
            at(8, 1)[0] > 200 && at(8, 1)[2] < 50,
            "top is red: {:?}",
            at(8, 1)
        );
        assert!(
            at(8, 14)[2] > 200 && at(8, 14)[0] < 50,
            "bottom is blue: {:?}",
            at(8, 14)
        );
    }
}
