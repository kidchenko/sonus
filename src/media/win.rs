//! Windows: the system's media sessions, the ones the volume flyout shows.
//!
//! Each session names its app by an app id, which [`session_belongs`] matches
//! to a row. A browser can hold several (one per tab with media); the one
//! Windows calls current wins, then one that is playing.
//!
//! Reads wait on WinRT's async calls, which answer in a few milliseconds.
//! Commands are sent and not waited on: the next refresh shows the result.
//! A track's thumbnail is read and decoded (WIC) once, when the track starts.

use super::{session_belongs, Art, Command, Media, ART_EDGE};
use crate::audio::AppAudio;
use crate::log::{ldebug, lwarn};

use std::collections::HashMap;

use windows::core::Interface;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
};
use windows::Storage::Streams::{DataReader, IInputStream};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppRGBA, IWICImagingFactory,
    WICBitmapDitherTypeNone, WICBitmapInterpolationModeFant, WICBitmapPaletteTypeCustom,
    WICDecodeMetadataCacheOnDemand, WICRect,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime;

pub struct Players {
    manager: Option<Manager>,
    tried: bool,
    /// The session behind each row that has controls, for the buttons.
    sessions: HashMap<String, Session>,
    /// Covers by app and track. `None` kept too: a track without one is not
    /// asked again every second.
    covers: HashMap<String, Option<Art>>,
}

impl Players {
    pub fn new() -> Self {
        Self {
            manager: None,
            tried: false,
            sessions: HashMap::new(),
            covers: HashMap::new(),
        }
    }

    /// Asked for on first use rather than at startup: nothing needs it until
    /// the panel opens.
    fn manager(&mut self) -> Option<&Manager> {
        if !self.tried {
            self.tried = true;
            match Manager::RequestAsync().and_then(|op| op.join()) {
                Ok(m) => self.manager = Some(m),
                Err(e) => lwarn!("media sessions unavailable: {e}"),
            }
        }
        self.manager.as_ref()
    }

    pub fn fill(&mut self, apps: &mut [AppAudio]) {
        self.sessions.clear();
        let Some(manager) = self.manager().cloned() else {
            return;
        };
        let current = manager
            .GetCurrentSession()
            .and_then(|s| s.SourceAppUserModelId())
            .map(|id| id.to_string())
            .unwrap_or_default();
        let Ok(list) = manager.GetSessions() else {
            return;
        };
        // (row, rank, session, media): the best session found for each row.
        let mut best: HashMap<usize, (u8, Session, Media)> = HashMap::new();
        for i in 0..list.Size().unwrap_or(0) {
            let Ok(session) = list.GetAt(i) else { continue };
            let Ok(id) = session.SourceAppUserModelId().map(|s| s.to_string()) else {
                continue;
            };
            let Some(row) = apps
                .iter()
                .position(|a| !a.system && session_belongs(&id, &a.key))
            else {
                ldebug!("media session {id} matches no app");
                continue;
            };
            let Some(media) = read(&session, &id, &mut self.covers) else {
                continue;
            };
            let rank = 2 * (id == current) as u8 + media.playing as u8;
            if best.get(&row).is_none_or(|(r, _, _)| rank > *r) {
                best.insert(row, (rank, session, media));
            }
        }
        for (row, (_, session, media)) in best {
            self.sessions.insert(apps[row].key.clone(), session);
            apps[row].media = Some(media);
        }
    }

    pub fn send(&mut self, key: &str, command: Command) {
        let Some(session) = self.sessions.get(key) else {
            return;
        };
        let sent = match command {
            Command::PlayPause => session.TryTogglePlayPauseAsync().map(drop),
            Command::Next => session.TrySkipNextAsync().map(drop),
            Command::Previous => session.TrySkipPreviousAsync().map(drop),
            Command::Seek(s) => {
                let start = session
                    .GetTimelineProperties()
                    .and_then(|t| t.StartTime())
                    .map(|t| t.Duration)
                    .unwrap_or(0);
                session
                    .TryChangePlaybackPositionAsync(start + (s.max(0.0) as f64 * 1e7) as i64)
                    .map(drop)
            }
        };
        match sent {
            Ok(()) => ldebug!("{key}: {command:?}"),
            Err(e) => lwarn!("{key}: {command:?} failed: {e}"),
        }
    }
}

/// What a session is playing, and what it lets us do about it.
fn read(session: &Session, id: &str, covers: &mut HashMap<String, Option<Art>>) -> Option<Media> {
    let info = session.GetPlaybackInfo().ok()?;
    let controls = info.Controls().ok()?;
    let props = session
        .TryGetMediaPropertiesAsync()
        .and_then(|op| op.join())
        .ok();
    let title = props
        .as_ref()
        .and_then(|p| p.Title().ok())
        .map(|s| s.to_string())
        .unwrap_or_default();
    let artist = props
        .as_ref()
        .and_then(|p| p.Artist().ok())
        .map(|s| s.to_string())
        .unwrap_or_default();
    let playing = info.PlaybackStatus().ok() == Some(Status::Playing);
    let can = |f: windows::core::Result<bool>| f.unwrap_or(false);
    let can_play_pause = can(controls.IsPlayPauseToggleEnabled());
    let can_previous = can(controls.IsPreviousEnabled());
    let can_next = can(controls.IsNextEnabled());
    if title.is_empty() && !can_play_pause && !can_previous && !can_next {
        return None;
    }
    let key = format!("{id}\n{artist}\n{title}");
    let art = match covers.get(&key) {
        Some(art) => art.clone(),
        None => {
            let art = props
                .as_ref()
                .and_then(|p| thumbnail(p).ok())
                .and_then(|bytes| decode(&bytes, ART_EDGE).ok())
                .map(|rgba| Art::new(&key, rgba, ART_EDGE));
            ldebug!(
                "{id}: cover {}",
                if art.is_some() {
                    "loaded"
                } else {
                    "unavailable"
                }
            );
            if covers.len() >= 32 {
                covers.clear();
            }
            covers.insert(key, art.clone());
            art
        }
    };

    // Times are in 100 ns ticks. The position is as of the app's last update,
    // which may be seconds old: while playing, the time since is added on.
    let (mut position, mut duration) = (None, None);
    if let Ok(t) = session.GetTimelineProperties() {
        let start = t.StartTime().map(|t| t.Duration).unwrap_or(0);
        let end = t.EndTime().map(|t| t.Duration).unwrap_or(0);
        if end > start {
            let length = (end - start) as f64 / 1e7;
            let mut at = (t.Position().map(|t| t.Duration).unwrap_or(0) - start) as f64 / 1e7;
            if playing {
                if let Ok(updated) = t.LastUpdatedTime() {
                    let now = unsafe { GetSystemTimeAsFileTime() };
                    let now = (now.dwHighDateTime as i64) << 32 | now.dwLowDateTime as i64;
                    let since = (now - updated.UniversalTime) as f64 / 1e7;
                    if since > 0.0 {
                        at += since;
                    }
                }
            }
            duration = Some(length as f32);
            position = Some(at.clamp(0.0, length) as f32);
        }
    }
    Some(Media {
        title,
        artist,
        playing,
        can_play_pause,
        can_previous,
        can_next,
        position,
        duration,
        can_seek: duration.is_some() && can(controls.IsPlaybackPositionEnabled()),
        art,
    })
}

/// The thumbnail's bytes: an image file, usually JPEG or PNG.
fn thumbnail(
    props: &windows::Media::Control::GlobalSystemMediaTransportControlsSessionMediaProperties,
) -> windows::core::Result<Vec<u8>> {
    let stream = props.Thumbnail()?.OpenReadAsync()?.join()?;
    let size = stream.Size()?.min(16 << 20) as u32;
    let reader = DataReader::CreateDataReader(&stream.cast::<IInputStream>()?)?;
    let got = reader.LoadAsync(size)?.join()?;
    let mut bytes = vec![0u8; got as usize];
    reader.ReadBytes(&mut bytes)?;
    Ok(bytes)
}

/// An image file as an `edge` square of straight-alpha RGBA: its middle
/// square, scaled. A video's thumbnail is wide; a cover is square already.
fn decode(bytes: &[u8], edge: u32) -> windows::core::Result<Vec<u8>> {
    unsafe {
        let wic: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let stream = wic.CreateStream()?;
        stream.InitializeFromMemory(bytes)?;
        let decoder =
            wic.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)?;
        let frame = decoder.GetFrame(0)?;
        let (mut w, mut h) = (0u32, 0u32);
        frame.GetSize(&mut w, &mut h)?;
        let side = w.min(h);
        if side == 0 {
            return Err(windows::core::Error::empty());
        }
        let clipper = wic.CreateBitmapClipper()?;
        let square = WICRect {
            X: ((w - side) / 2) as i32,
            Y: ((h - side) / 2) as i32,
            Width: side as i32,
            Height: side as i32,
        };
        clipper.Initialize(&frame, &square)?;
        let scaler = wic.CreateBitmapScaler()?;
        scaler.Initialize(&clipper, edge, edge, WICBitmapInterpolationModeFant)?;
        let converter = wic.CreateFormatConverter()?;
        converter.Initialize(
            &scaler,
            &GUID_WICPixelFormat32bppRGBA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )?;
        let mut rgba = vec![0u8; (edge * edge * 4) as usize];
        converter.CopyPixels(std::ptr::null(), edge * 4, &mut rgba)?;
        Ok(rgba)
    }
}
