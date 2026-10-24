//! The mixer: a borderless popup with a slider for the output device and one
//! for every app playing through it.
//!
//! This module is everything that does not care how pixels reach the screen.
//! [`Ui`] owns the model, the layout, the hit-testing and the interaction
//! (dragging a slider, the wheel, the device list) and turns all of it into a
//! flat list of drawing commands ([`Cmd`]). The platform submodules own a
//! window, forward the mouse to [`Ui`], and paint the commands: Direct2D on
//! Windows, AppKit on macOS, cairo and pango on Linux. One layout, one set of
//! behaviours, three painters, so the apps cannot drift apart.
//!
//! Coordinates are logical pixels (DIPs on Windows, points on macOS, GTK's
//! logical pixels on Linux), top-left origin, y down.

use crate::audio::{AppAudio, DeviceInfo, Output, Peaks, Problem, Snapshot};
use crate::icon::{self, Glyph};
use crate::media::{self, Art, Command, Media};
use std::collections::HashMap;
use std::time::{Duration, Instant};

// Named `win32`, not `windows`: a module called `windows` would collide with the
// `windows` crate the moment this file's submodule does `use super::*`.
#[cfg(windows)]
mod win32;
#[cfg(windows)]
pub use win32::{preview, render_to_rgba, Panel};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{preview, render_to_rgba, Panel};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{preview, render_to_rgba, Panel};

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

pub const WIDTH: f32 = 360.0;
const PAD: f32 = 16.0;
const TOP: f32 = 10.0;
const BOTTOM: f32 = 10.0;

/// One slider row: the app's icon on the left; its name above, and below that
/// the slider, the number and the mute button.
const ROW_H: f32 = 60.0;
const ICON: f32 = 32.0;
const ICON_GAP: f32 = 12.0;
const NAME_Y: f32 = 10.0;
const NAME_H: f32 = 18.0;
const SLIDER_Y: f32 = 28.0;
const SLIDER_H: f32 = 24.0;
const NUMBER_W: f32 = 30.0;
const BUTTON: f32 = 30.0;
const TRACK_H: f32 = 4.0;
/// The thumb's outer radius. Its centre travels inset by this much, so the
/// thumb never pokes out past either end of the track.
const THUMB_R: f32 = 10.0;

/// The player under an app's slider, when the app has one: the track and its
/// buttons on one line, and the progress bar under them when the track's
/// length is known.
const MEDIA_Y: f32 = 54.0;
const MEDIA_LINE_H: f32 = 24.0;
const MEDIA_BUTTON: f32 = 26.0;
const TIMELINE_H: f32 = 14.0;
const TIME_W: f32 = 38.0;
const MEDIA_BOTTOM: f32 = 6.0;
const ART_GAP: f32 = 10.0;

/// How far the device picker pushes the master row's slider down, so the
/// thumb clears the pill above it.
const PICKER_GAP: f32 = 8.0;
const DEVICE_H: f32 = 34.0;
const RULE_GAP: f32 = 8.0;
const LABEL_H: f32 = 26.0;
const EMPTY_H: f32 = 72.0;
const PROBLEM_H: f32 = 112.0;

/// More than this and the panel would be taller than many laptop screens.
/// The quietest go first: they are sorted active-first before the cut.
pub const MAX_APPS: usize = 9;

/// Corner radius: what DWM rounds a popup to on Windows 11, and what macOS
/// rounds its menus and popovers to.
pub const CORNER: f32 = if cfg!(target_os = "macos") { 12.0 } else { 8.0 };

// ---------------------------------------------------------------------------
// Colour
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgba(pub f32, pub f32, pub f32, pub f32);

impl Rgba {
    pub const fn hex(rgb: u32) -> Self {
        Self(
            ((rgb >> 16) & 0xff) as f32 / 255.0,
            ((rgb >> 8) & 0xff) as f32 / 255.0,
            (rgb & 0xff) as f32 / 255.0,
            1.0,
        )
    }
    pub const fn white(a: f32) -> Self {
        Self(1.0, 1.0, 1.0, a)
    }
    pub const fn black(a: f32) -> Self {
        Self(0.0, 0.0, 0.0, a)
    }
    pub fn alpha(self, a: f32) -> Self {
        Self(self.0, self.1, self.2, self.3 * a)
    }
}

/// How the panel looks: light or dark, and the user's accent colour.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Look {
    pub dark: bool,
    pub accent: (u8, u8, u8),
}

impl Look {
    pub fn current() -> Self {
        let dark = crate::theme::taskbar_is_dark();
        Self {
            dark,
            accent: crate::theme::accent(dark),
        }
    }
}

/// The colours, from Fluent's tokens on Windows and AppKit's label colours on
/// macOS, so the panel reads as part of the OS.
pub struct Palette {
    /// Windows only: macOS has its material there instead.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub bg: Rgba,
    pub border: Rgba,
    pub text: Rgba,
    pub secondary: Rgba,
    pub tertiary: Rgba,
    pub track: Rgba,
    pub accent: Rgba,
    pub thumb: Rgba,
    pub thumb_border: Rgba,
    pub meter: Rgba,
    /// A muted slider's fill: there, but plainly not live.
    pub muted: Rgba,
    pub hover: Rgba,
    pub rule: Rgba,
}

impl Palette {
    pub fn new(look: Look) -> Self {
        let (r, g, b) = look.accent;
        let accent = Rgba(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0);
        let mac = cfg!(target_os = "macos");
        match (look.dark, mac) {
            (true, false) => Self {
                bg: Rgba::hex(0x2b2b2b),
                border: Rgba::hex(0x404040),
                text: Rgba::white(1.0),
                secondary: Rgba::white(0.79),
                tertiary: Rgba::white(0.54),
                track: Rgba::white(0.40),
                accent,
                thumb: Rgba::hex(0x454545),
                thumb_border: Rgba::white(0.09),
                meter: Rgba::white(0.42),
                muted: Rgba::white(0.30),
                hover: Rgba::white(0.06),
                rule: Rgba::white(0.08),
            },
            (false, false) => Self {
                bg: Rgba::hex(0xf9f9f9),
                border: Rgba::hex(0xdedede),
                text: Rgba::black(0.90),
                secondary: Rgba::black(0.61),
                tertiary: Rgba::black(0.45),
                track: Rgba::black(0.40),
                accent,
                thumb: Rgba::white(1.0),
                thumb_border: Rgba::black(0.10),
                meter: Rgba::white(0.45),
                muted: Rgba::black(0.24),
                hover: Rgba::black(0.04),
                rule: Rgba::black(0.08),
            },
            // macOS draws over a vibrant material, so these are translucent
            // labels rather than flat greys, and the knob is AppKit's white.
            (true, true) => Self {
                bg: Rgba::hex(0x1e1e1e).alpha(0.0),
                border: Rgba::white(0.16),
                text: Rgba::white(0.90),
                secondary: Rgba::white(0.62),
                tertiary: Rgba::white(0.40),
                track: Rgba::white(0.20),
                accent,
                thumb: Rgba::hex(0xdedede),
                thumb_border: Rgba::black(0.20),
                meter: Rgba::white(0.40),
                muted: Rgba::white(0.32),
                hover: Rgba::white(0.10),
                rule: Rgba::white(0.12),
            },
            (false, true) => Self {
                bg: Rgba::hex(0xf2f2f2).alpha(0.0),
                border: Rgba::black(0.12),
                text: Rgba::black(0.86),
                secondary: Rgba::black(0.55),
                tertiary: Rgba::black(0.36),
                track: Rgba::black(0.12),
                accent,
                thumb: Rgba::white(1.0),
                thumb_border: Rgba::black(0.14),
                meter: Rgba::white(0.45),
                muted: Rgba::black(0.22),
                hover: Rgba::black(0.06),
                rule: Rgba::black(0.10),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Drawing commands
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn cy(&self) -> f32 {
        self.y + self.h / 2.0
    }
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
    fn inset(&self, dx: f32, dy: f32) -> Self {
        Self::new(
            self.x + dx,
            self.y + dy,
            self.w - 2.0 * dx,
            self.h - 2.0 * dy,
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Weight {
    Regular,
    Semibold,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    Fill {
        r: Rect,
        radius: f32,
        color: Rgba,
    },
    Stroke {
        r: Rect,
        radius: f32,
        width: f32,
        color: Rgba,
    },
    /// One line of text, vertically centred in `r`, cut with an ellipsis if it
    /// does not fit.
    Text {
        r: Rect,
        text: String,
        size: f32,
        weight: Weight,
        color: Rgba,
        align: Align,
    },
    Glyph {
        r: Rect,
        glyph: Glyph,
        color: Rgba,
    },
    /// An app's own icon, from its executable or bundle. When it has none, the
    /// painter draws `fallback`: its initial on a coloured tile.
    AppIcon {
        r: Rect,
        path: Option<String>,
        fallback: (Rgba, String),
        opacity: f32,
    },
    /// A track's cover, already decoded and rounded.
    Art {
        r: Rect,
        art: Art,
    },
}

impl Rect {
    pub fn intersects(&self, o: &Rect) -> bool {
        self.x < o.right() && o.x < self.right() && self.y < o.bottom() && o.y < self.bottom()
    }
}

impl Cmd {
    /// Where the command draws, for skipping the ones a partial repaint does
    /// not reach.
    pub fn bounds(&self) -> Rect {
        match self {
            Cmd::Fill { r, .. }
            | Cmd::Text { r, .. }
            | Cmd::Glyph { r, .. }
            | Cmd::AppIcon { r, .. }
            | Cmd::Art { r, .. } => *r,
            Cmd::Stroke { r, width, .. } => r.inset(-width, -width),
        }
    }
}

fn circle(cx: f32, cy: f32, r: f32, color: Rgba) -> Cmd {
    Cmd::Fill {
        r: Rect::new(cx - r, cy - r, r * 2.0, r * 2.0),
        radius: r,
        color,
    }
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

/// Where one slider row's parts go.
#[derive(Clone, Copy, Debug, PartialEq)]
struct RowBox {
    row: Rect,
    icon: Rect,
    name: Rect,
    slider: Rect,
    number: Rect,
    button: Rect,
    media: Option<MediaBox>,
}

/// Where a row's player goes.
#[derive(Clone, Copy, Debug, PartialEq)]
struct MediaBox {
    /// The cover, left of the track and its bar, as tall as both.
    art: Option<Rect>,
    text: Rect,
    previous: Rect,
    play: Rect,
    next: Rect,
    timeline: Option<Timeline>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Timeline {
    elapsed: Rect,
    /// The bar's hit area; the bar itself is drawn thin across its middle.
    bar: Rect,
    remaining: Rect,
}

impl RowBox {
    fn at(top: f32) -> Self {
        let content = PAD + ICON + ICON_GAP;
        // The button is optically aligned: its glyph, not its hover square, sits
        // on the padding line.
        let button = Rect::new(
            WIDTH - PAD - BUTTON + 6.0,
            top + SLIDER_Y + SLIDER_H / 2.0 - BUTTON / 2.0,
            BUTTON,
            BUTTON,
        );
        let number = Rect::new(
            button.x - NUMBER_W - 2.0,
            top + SLIDER_Y,
            NUMBER_W,
            SLIDER_H,
        );
        Self {
            row: Rect::new(0.0, top, WIDTH, ROW_H),
            icon: Rect::new(PAD, top + (ROW_H - ICON) / 2.0, ICON, ICON),
            name: Rect::new(
                content,
                top + NAME_Y,
                button.right() - content - 6.0,
                NAME_H,
            ),
            slider: Rect::new(content, top + SLIDER_Y, number.x - 8.0 - content, SLIDER_H),
            number,
            button,
            media: None,
        }
    }

    /// The same row, taller, with the slider moved down out of the way of the
    /// device picker's pill.
    fn with_picker(mut self) -> Self {
        self.row.h += PICKER_GAP;
        self.icon.y += PICKER_GAP / 2.0;
        self.slider.y += PICKER_GAP;
        self.number.y += PICKER_GAP;
        self.button.y += PICKER_GAP;
        self
    }

    /// The same row, taller, with a player under the slider. The buttons end
    /// where the mute button does, so the right edge stays one line.
    fn with_media(mut self, timeline: bool, art: bool) -> Self {
        let top = self.row.y;
        let y = top + MEDIA_Y;
        let right = self.button.right();
        let next = Rect::new(
            right - MEDIA_BUTTON,
            y + (MEDIA_LINE_H - MEDIA_BUTTON) / 2.0,
            MEDIA_BUTTON,
            MEDIA_BUTTON,
        );
        let play = Rect {
            x: next.x - MEDIA_BUTTON,
            ..next
        };
        let previous = Rect {
            x: play.x - MEDIA_BUTTON,
            ..next
        };
        let art_edge = if timeline {
            MEDIA_LINE_H + 2.0 + TIMELINE_H
        } else {
            MEDIA_LINE_H
        };
        let art = art.then(|| Rect::new(self.name.x, y, art_edge, art_edge));
        let x = art.map_or(self.name.x, |a| a.right() + ART_GAP);
        let text = Rect::new(x, y, previous.x - 8.0 - x, MEDIA_LINE_H);
        let timeline = timeline.then(|| {
            let ty = y + MEDIA_LINE_H + 2.0;
            let elapsed = Rect::new(x, ty, TIME_W, TIMELINE_H);
            let remaining = Rect::new(WIDTH - PAD - TIME_W, ty, TIME_W, TIMELINE_H);
            let bar_x = elapsed.right() + 8.0;
            Timeline {
                elapsed,
                bar: Rect::new(bar_x, ty, remaining.x - 8.0 - bar_x, TIMELINE_H),
                remaining,
            }
        });
        let bottom = timeline.map_or(text.bottom(), |t| t.bar.bottom());
        self.row.h = bottom + MEDIA_BOTTOM - top;
        self.media = Some(MediaBox {
            art,
            text,
            previous,
            play,
            next,
            timeline,
        });
        self
    }

    /// Where the thumb's centre is at `v`.
    fn thumb_x(&self, v: f32) -> f32 {
        let s = self.slider;
        s.x + THUMB_R + (s.w - 2.0 * THUMB_R) * v.clamp(0.0, 1.0)
    }

    /// The value at `x`, in whole percent: clicking the track jumps there.
    fn value_at(&self, x: f32) -> f32 {
        let s = self.slider;
        let v = (x - s.x - THUMB_R) / (s.w - 2.0 * THUMB_R).max(1.0);
        (v.clamp(0.0, 1.0) * 100.0).round() / 100.0
    }
}

#[derive(Debug, Default)]
struct Layout {
    master: Option<RowBox>,
    /// The device picker: the master row's name, as a pill. The device's tile
    /// opens it too.
    picker: Option<Rect>,
    devices: Vec<Rect>,
    /// The card the device list sits on.
    device_card: Option<Rect>,
    /// "No output device", where the master row would be.
    no_output: Option<Rect>,
    rule: Option<f32>,
    label: Option<Rect>,
    apps: Vec<RowBox>,
    empty: Option<Rect>,
    problem: Option<Rect>,
    problem_button: Option<Rect>,
    height: f32,
}

// ---------------------------------------------------------------------------
// Interaction
// ---------------------------------------------------------------------------

/// What the user asked for. The app carries it out, and the panel shows the
/// result straight away rather than waiting to be told.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    AppVolume(String, f32),
    AppMuted(String, bool),
    Volume(f32),
    Muted(bool),
    Device(String),
    /// A gesture on an app's slider finished: worth remembering now.
    Settled(String),
    OpenPermissionSettings,
    /// One of a player's buttons, or a click on its progress bar.
    Media(String, Command),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Control {
    Previous,
    PlayPause,
    Next,
    Seek,
}

#[derive(Clone, Debug, PartialEq, Default)]
enum Hit {
    #[default]
    None,
    Slider(Which),
    Button(Which),
    /// An app's icon: a bigger target for mute.
    Icon(usize),
    Media(usize, Control),
    Picker,
    Device(usize),
    ProblemButton,
}

#[derive(Clone, Debug, PartialEq)]
enum Which {
    Master,
    App(usize),
}

/// What the platform should do after an event.
#[derive(Debug, Default, PartialEq)]
pub struct Response {
    pub actions: Vec<Action>,
    pub repaint: bool,
    /// The panel's height changed: the window must follow.
    pub resized: bool,
}

/// A slider being dragged, by key, so a refresh that reorders the rows does
/// not hand the drag to a different app halfway through.
#[derive(Clone, Debug, PartialEq)]
enum Drag {
    Master,
    App(String),
}

pub struct Ui {
    model: Snapshot,
    look: Look,
    hover: Hit,
    pressed: Hit,
    drag: Option<Drag>,
    devices_open: bool,
    /// Meter levels as drawn: each new reading, or the last one falling away,
    /// whichever is higher. Raw peaks flicker; this reads as a meter.
    peaks: HashMap<String, f32>,
    output_peak: f32,
    wheel_step: f32,
    /// A trackpad scrolls in fractions of a notch.
    wheel_carry: f32,
    /// What a click on a player asked for, by app, held over readings that
    /// say otherwise until one agrees. A reading can predate the click: the
    /// player had not acted yet when it was taken, and letting it win would
    /// flick the button back and forth.
    pending: HashMap<String, Pending>,
}

#[derive(Clone, Debug, Default)]
struct Pending {
    playing: Option<bool>,
    position: Option<f32>,
    until: Option<Instant>,
}

/// How long a click is held over readings that disagree with it: longer than
/// any player takes to act, short enough that a click it ignored shows.
const HOLD: Duration = Duration::from_secs(2);

/// A reading this close to a seek's target has caught up with it.
const SEEK_SLACK: f32 = 2.0;

/// How much of a meter survives each frame at ~30 fps: about a tenth of a
/// second to fall halfway.
const DECAY: f32 = 0.80;

impl Default for Ui {
    fn default() -> Self {
        Self {
            model: Snapshot::default(),
            look: Look {
                dark: true,
                accent: (0x60, 0xcd, 0xff),
            },
            hover: Hit::None,
            pressed: Hit::None,
            drag: None,
            devices_open: false,
            peaks: HashMap::new(),
            output_peak: 0.0,
            wheel_step: 0.02,
            wheel_carry: 0.0,
            pending: HashMap::new(),
        }
    }
}

impl Ui {
    pub fn look(&self) -> Look {
        self.look
    }

    pub fn set_wheel_step(&mut self, percent: u8) {
        self.wheel_step = percent as f32 / 100.0;
    }

    /// Opened afresh: the device list starts closed and nothing is hovered.
    pub fn open(&mut self, model: Snapshot, look: Look) {
        self.devices_open = false;
        self.hover = Hit::None;
        self.pressed = Hit::None;
        self.drag = None;
        self.peaks.clear();
        self.output_peak = 0.0;
        self.set_model(model, look);
    }

    /// A fresh reading from the audio side. Returns whether the height changed.
    ///
    /// The slider under the mouse keeps the value the mouse gave it: the
    /// reading may predate the last drag event, and letting it win would make
    /// the thumb stutter backwards under the pointer.
    pub fn set_model(&mut self, mut model: Snapshot, look: Look) -> bool {
        let before = self.height();
        prepare(&mut model);
        match &self.drag {
            Some(Drag::Master) => {
                if let (Some(new), Some(old)) = (&mut model.output, &self.model.output) {
                    new.volume = old.volume;
                    new.muted = old.muted;
                }
            }
            Some(Drag::App(key)) => {
                let old = self.model.apps.iter().find(|a| &a.key == key).cloned();
                if let (Some(new), Some(old)) = (model.apps.iter_mut().find(|a| &a.key == key), old)
                {
                    new.volume = old.volume;
                    new.muted = old.muted;
                }
            }
            None => {}
        }
        if model.devices.len() < 2 {
            self.devices_open = false;
        }
        self.hold_pending(&mut model);
        self.model = model;
        self.look = look;
        self.height() != before
    }

    /// Returns where the meters that moved are, so a repaint can be just
    /// those strips. A silent panel costs no frames at all.
    pub fn set_peaks(&mut self, p: &Peaks) -> Vec<Rect> {
        let settle = |v: f32| if v < 0.005 { 0.0 } else { v };
        let l = self.layout();
        let mut dirty = Vec::new();

        let output = settle(
            p.output
                .unwrap_or(0.0)
                .max(self.output_peak * DECAY)
                .clamp(0.0, 1.0),
        );
        if (output - self.output_peak).abs() > 0.002 {
            if let Some(row) = l.master {
                dirty.push(meter_rect(&row));
            }
        }
        self.output_peak = output;

        let mut next = HashMap::with_capacity(p.apps.len());
        for (key, v) in &p.apps {
            let old = self.peaks.get(key).copied().unwrap_or(0.0);
            let new = settle(v.max(old * DECAY).clamp(0.0, 1.0));
            if (new - old).abs() > 0.002 {
                if let Some(i) = self.model.apps.iter().position(|a| &a.key == key) {
                    if let Some(row) = l.apps.get(i) {
                        dirty.push(meter_rect(row));
                    }
                }
            }
            next.insert(key.clone(), new);
        }
        self.peaks = next;
        dirty
    }

    pub fn height(&self) -> f32 {
        self.layout().height
    }

    fn layout(&self) -> Layout {
        let m = &self.model;
        let mut l = Layout::default();
        let mut y = TOP;

        match &m.output {
            Some(_) => {
                let mut row = RowBox::at(y);
                if m.devices.len() > 1 {
                    row = row.with_picker();
                    l.picker = Some(Rect::new(
                        row.name.x - 8.0,
                        row.name.y - 4.0,
                        row.name.w + 12.0,
                        row.name.h + 8.0,
                    ));
                }
                l.master = Some(row);
                y += row.row.h;
                if self.devices_open {
                    let top = y + 2.0;
                    y = top + 4.0;
                    for _ in &m.devices {
                        l.devices
                            .push(Rect::new(PAD + 4.0, y, WIDTH - 2.0 * PAD - 8.0, DEVICE_H));
                        y += DEVICE_H;
                    }
                    y += 4.0;
                    l.device_card =
                        Some(Rect::new(PAD - 4.0, top, WIDTH - 2.0 * PAD + 8.0, y - top));
                    y += 6.0;
                }
            }
            None => {
                l.no_output = Some(Rect::new(PAD, y, WIDTH - 2.0 * PAD, EMPTY_H));
                l.height = y + EMPTY_H + BOTTOM;
                return l;
            }
        }

        y += RULE_GAP;
        l.rule = Some(y);
        y += 1.0 + RULE_GAP - 4.0;

        if let Some(_p) = &m.problem {
            let r = Rect::new(PAD, y, WIDTH - 2.0 * PAD, PROBLEM_H);
            if matches!(m.problem, Some(Problem::NeedsPermission)) {
                l.problem_button = Some(Rect::new(r.x, r.bottom() - 40.0, 132.0, 30.0));
            }
            l.problem = Some(r);
            y += PROBLEM_H;
        } else {
            l.label = Some(Rect::new(PAD, y, WIDTH - 2.0 * PAD, LABEL_H));
            y += LABEL_H - 4.0;
            if m.apps.is_empty() {
                l.empty = Some(Rect::new(PAD, y, WIDTH - 2.0 * PAD, EMPTY_H));
                y += EMPTY_H;
            } else {
                for a in &m.apps {
                    let mut row = RowBox::at(y);
                    if let Some(media) = &a.media {
                        row = row.with_media(media.duration.is_some(), media.art.is_some());
                    }
                    y += row.row.h;
                    l.apps.push(row);
                }
            }
        }
        l.height = (y + BOTTOM).ceil();
        l
    }

    fn hit(&self, x: f32, y: f32) -> Hit {
        let l = self.layout();
        if let Some(b) = l.problem_button {
            if b.contains(x, y) {
                return Hit::ProblemButton;
            }
        }
        if let Some(row) = &l.master {
            if row.button.contains(x, y) {
                return Hit::Button(Which::Master);
            }
            if row.slider.inset(0.0, -4.0).contains(x, y) {
                return Hit::Slider(Which::Master);
            }
        }
        if let (Some(p), Some(row)) = (l.picker, &l.master) {
            if p.contains(x, y) || row.icon.contains(x, y) {
                return Hit::Picker;
            }
        }
        for (i, r) in l.devices.iter().enumerate() {
            if r.contains(x, y) {
                return Hit::Device(i);
            }
        }
        for (i, row) in l.apps.iter().enumerate() {
            if !row.row.contains(x, y) {
                continue;
            }
            let media = self.model.apps.get(i).and_then(|a| a.media.as_ref());
            if let (Some(b), Some(media)) = (row.media, media) {
                for (r, control, enabled) in [
                    (b.previous, Control::Previous, media.can_previous),
                    (b.play, Control::PlayPause, media.can_play_pause),
                    (b.next, Control::Next, media.can_next),
                ] {
                    if enabled && r.contains(x, y) {
                        return Hit::Media(i, control);
                    }
                }
                if let Some(t) = b.timeline {
                    if media.can_seek && t.bar.contains(x, y) {
                        return Hit::Media(i, Control::Seek);
                    }
                }
            }
            if row.button.contains(x, y) {
                return Hit::Button(Which::App(i));
            }
            if row.icon.contains(x, y) {
                return Hit::Icon(i);
            }
            if row.slider.inset(0.0, -4.0).contains(x, y) {
                return Hit::Slider(Which::App(i));
            }
        }
        Hit::None
    }

    /// Which slider row the pointer is over, for the wheel: anywhere on a row
    /// counts, not just the track.
    fn row_at(&self, x: f32, y: f32) -> Option<Which> {
        let l = self.layout();
        if l.master.is_some_and(|r| r.row.contains(x, y)) {
            return Some(Which::Master);
        }
        l.apps
            .iter()
            .position(|r| r.row.contains(x, y))
            .map(Which::App)
    }

    pub fn mouse_move(&mut self, x: f32, y: f32) -> Response {
        let mut out = Response::default();
        if let Some(drag) = self.drag.clone() {
            self.drag_to(&drag, x, &mut out);
            return out;
        }
        let hit = self.hit(x, y);
        if hit != self.hover {
            self.hover = hit;
            out.repaint = true;
        }
        out
    }

    pub fn mouse_leave(&mut self) -> Response {
        let changed = self.hover != Hit::None;
        self.hover = Hit::None;
        Response {
            repaint: changed,
            ..Default::default()
        }
    }

    pub fn mouse_down(&mut self, x: f32, y: f32) -> Response {
        let mut out = Response {
            repaint: true,
            ..Default::default()
        };
        let hit = self.hit(x, y);
        self.pressed = hit.clone();
        match hit {
            Hit::Slider(Which::Master) => {
                self.drag = Some(Drag::Master);
                self.drag_to(&Drag::Master, x, &mut out);
            }
            Hit::Slider(Which::App(i)) => {
                if let Some(key) = self.model.apps.get(i).map(|a| a.key.clone()) {
                    let d = Drag::App(key);
                    self.drag = Some(d.clone());
                    self.drag_to(&d, x, &mut out);
                }
            }
            Hit::Button(which) => self.toggle_mute(&which, &mut out),
            Hit::Icon(i) => self.toggle_mute(&Which::App(i), &mut out),
            Hit::Picker => {
                self.devices_open = !self.devices_open;
                out.resized = true;
            }
            Hit::Device(i) => {
                if let Some(d) = self.model.devices.get(i).cloned() {
                    if !d.default {
                        for other in &mut self.model.devices {
                            other.default = other.id == d.id;
                        }
                        if let Some(o) = &mut self.model.output {
                            o.name = d.name.clone();
                        }
                        out.actions.push(Action::Device(d.id));
                    }
                }
                self.devices_open = false;
                out.resized = true;
            }
            Hit::ProblemButton => out.actions.push(Action::OpenPermissionSettings),
            Hit::Media(i, control) => self.media_control(i, control, x, &mut out),
            Hit::None => {}
        }
        out
    }

    /// Keep what a click asked for over a reading that does not show it yet.
    /// Each part lets go as soon as a reading agrees, and all of it after
    /// [`HOLD`].
    fn hold_pending(&mut self, model: &mut Snapshot) {
        let now = Instant::now();
        self.pending.retain(|key, p| {
            if p.until.is_none_or(|t| now >= t) {
                return false;
            }
            let Some(media) = model
                .apps
                .iter_mut()
                .find(|a| &a.key == key)
                .and_then(|a| a.media.as_mut())
            else {
                return false;
            };
            if let Some(want) = p.playing {
                if media.playing == want {
                    p.playing = None;
                } else {
                    media.playing = want;
                }
            }
            if let Some(at) = p.position {
                if media.position.is_some_and(|x| (x - at).abs() < SEEK_SLACK) {
                    p.position = None;
                } else {
                    media.position = Some(at);
                }
            }
            p.playing.is_some() || p.position.is_some()
        });
    }

    /// A player's button, shown done at once where that can be guessed: the
    /// play button flips, a seek moves the bar. A skip waits for the player
    /// to say what came next.
    fn media_control(&mut self, i: usize, control: Control, x: f32, out: &mut Response) {
        let bar = self
            .layout()
            .apps
            .get(i)
            .and_then(|r| r.media)
            .and_then(|m| m.timeline)
            .map(|t| t.bar);
        let Some(a) = self.model.apps.get_mut(i) else {
            return;
        };
        let Some(media) = &mut a.media else { return };
        let command = match control {
            Control::PlayPause => {
                media.playing = !media.playing;
                let p = self.pending.entry(a.key.clone()).or_default();
                p.playing = Some(media.playing);
                p.until = Some(Instant::now() + HOLD);
                Command::PlayPause
            }
            Control::Previous => Command::Previous,
            Control::Next => Command::Next,
            Control::Seek => {
                let (Some(bar), Some(length)) = (bar, media.duration) else {
                    return;
                };
                let at = ((x - bar.x) / bar.w.max(1.0)).clamp(0.0, 1.0) * length;
                media.position = Some(at);
                let p = self.pending.entry(a.key.clone()).or_default();
                p.position = Some(at);
                p.until = Some(Instant::now() + HOLD);
                Command::Seek(at)
            }
        };
        out.actions.push(Action::Media(a.key.clone(), command));
    }

    pub fn mouse_up(&mut self, x: f32, y: f32) -> Response {
        let mut out = Response {
            repaint: true,
            ..Default::default()
        };
        if let Some(Drag::App(key)) = self.drag.take() {
            out.actions.push(Action::Settled(key));
        }
        self.drag = None;
        self.pressed = Hit::None;
        self.hover = self.hit(x, y);
        out
    }

    /// `notches` up is positive. A mouse wheel sends whole notches; a trackpad
    /// sends fractions, which are carried until they add up to one.
    pub fn wheel(&mut self, x: f32, y: f32, notches: f32) -> Response {
        let mut out = Response::default();
        let Some(which) = self.row_at(x, y) else {
            return out;
        };
        self.wheel_carry += notches;
        let steps = self.wheel_carry.trunc();
        if steps == 0.0 {
            return out;
        }
        self.wheel_carry -= steps;
        let delta = steps * self.wheel_step;
        match which {
            Which::Master => {
                if let Some(o) = &mut self.model.output {
                    let v = ((o.volume + delta).clamp(0.0, 1.0) * 100.0).round() / 100.0;
                    o.volume = v;
                    out.actions.push(Action::Volume(v));
                    if o.muted && delta > 0.0 {
                        o.muted = false;
                        out.actions.push(Action::Muted(false));
                    }
                }
            }
            Which::App(i) => {
                if let Some(a) = self.model.apps.get_mut(i) {
                    let v = ((a.volume + delta).clamp(0.0, 1.0) * 100.0).round() / 100.0;
                    a.volume = v;
                    out.actions.push(Action::AppVolume(a.key.clone(), v));
                    if a.muted && delta > 0.0 {
                        a.muted = false;
                        out.actions.push(Action::AppMuted(a.key.clone(), false));
                    }
                    out.actions.push(Action::Settled(a.key.clone()));
                }
            }
        }
        out.repaint = true;
        out
    }

    fn toggle_mute(&mut self, which: &Which, out: &mut Response) {
        match which {
            Which::Master => {
                if let Some(o) = &mut self.model.output {
                    o.muted = !o.muted;
                    out.actions.push(Action::Muted(o.muted));
                }
            }
            Which::App(i) => {
                if let Some(a) = self.model.apps.get_mut(*i) {
                    a.muted = !a.muted;
                    out.actions.push(Action::AppMuted(a.key.clone(), a.muted));
                    out.actions.push(Action::Settled(a.key.clone()));
                }
            }
        }
    }

    /// Move a dragged slider to `x`. Dragging a muted slider unmutes it, as
    /// Windows' own flyout does: moving the volume is asking to hear it.
    fn drag_to(&mut self, drag: &Drag, x: f32, out: &mut Response) {
        let l = self.layout();
        match drag {
            Drag::Master => {
                let (Some(row), Some(o)) = (l.master, &mut self.model.output) else {
                    return;
                };
                let v = row.value_at(x);
                if v != o.volume {
                    o.volume = v;
                    out.actions.push(Action::Volume(v));
                    out.repaint = true;
                }
                if o.muted {
                    o.muted = false;
                    out.actions.push(Action::Muted(false));
                    out.repaint = true;
                }
            }
            Drag::App(key) => {
                let Some(i) = self.model.apps.iter().position(|a| &a.key == key) else {
                    return;
                };
                let Some(row) = l.apps.get(i) else { return };
                let a = &mut self.model.apps[i];
                let v = row.value_at(x);
                if v != a.volume {
                    a.volume = v;
                    out.actions.push(Action::AppVolume(a.key.clone(), v));
                    out.repaint = true;
                }
                if a.muted {
                    a.muted = false;
                    out.actions.push(Action::AppMuted(a.key.clone(), false));
                    out.repaint = true;
                }
            }
        }
    }

    /// Open the device list, for the sample pictures.
    pub fn show_devices(&mut self) {
        self.devices_open = self.model.devices.len() > 1;
    }

    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    // -- painting ----------------------------------------------------------

    /// Everything to draw, in order. The window background and border are the
    /// painter's: Windows fills and outlines it, macOS puts a vibrant material
    /// there instead.
    pub fn scene(&self) -> Vec<Cmd> {
        let p = Palette::new(self.look);
        let l = self.layout();
        let m = &self.model;
        let mut out = Vec::new();

        if let Some(r) = l.no_output {
            empty_state(
                &mut out,
                &p,
                r,
                "No output device",
                "Plug in speakers or headphones to get started.",
            );
            return out;
        }

        // -- the output device ------------------------------------------------
        if let (Some(row), Some(o)) = (l.master, &m.output) {
            if let Some(picker) = l.picker {
                if self.hover == Hit::Picker || self.devices_open {
                    out.push(Cmd::Fill {
                        r: picker,
                        radius: 6.0,
                        color: p.hover,
                    });
                }
            }
            // The device's tile carries the accent, so the master row reads as
            // the one in charge of the rows below it.
            out.push(Cmd::Fill {
                r: row.icon,
                radius: 8.0,
                color: p.accent.alpha(if self.look.dark { 0.22 } else { 0.16 }),
            });
            out.push(Cmd::Glyph {
                r: row.icon.inset(7.0, 7.0),
                glyph: Glyph::Speaker(2),
                color: p.accent,
            });
            let chevron = if m.devices.len() > 1 { 18.0 } else { 0.0 };
            out.push(Cmd::Text {
                r: Rect::new(
                    row.name.x,
                    row.name.y,
                    row.name.w - chevron - 4.0,
                    row.name.h,
                ),
                text: o.name.clone(),
                size: 13.5,
                weight: Weight::Semibold,
                color: p.text,
                align: Align::Left,
            });
            if chevron > 0.0 {
                out.push(Cmd::Glyph {
                    r: Rect::new(row.name.right() - 16.0, row.name.cy() - 8.0, 16.0, 16.0),
                    glyph: if self.devices_open {
                        Glyph::ChevronUp
                    } else {
                        Glyph::ChevronDown
                    },
                    color: p.secondary,
                });
            }
            self.slider(
                &mut out,
                &p,
                &row,
                o.volume,
                o.muted,
                self.output_peak,
                Which::Master,
            );
        }

        // -- the device list --------------------------------------------------
        if let Some(card) = l.device_card {
            out.push(Cmd::Fill {
                r: card,
                radius: 8.0,
                color: p.hover,
            });
        }
        for (i, (r, d)) in l.devices.iter().zip(&m.devices).enumerate() {
            if self.hover == Hit::Device(i) {
                out.push(Cmd::Fill {
                    r: *r,
                    radius: 6.0,
                    color: p.hover,
                });
            }
            if d.default {
                out.push(Cmd::Glyph {
                    r: Rect::new(PAD + (ICON - 16.0) / 2.0, r.cy() - 8.0, 16.0, 16.0),
                    glyph: Glyph::Check,
                    color: p.accent,
                });
            }
            let text_x = PAD + ICON + ICON_GAP;
            out.push(Cmd::Text {
                r: Rect::new(text_x, r.y, r.right() - text_x - 8.0, r.h),
                text: d.name.clone(),
                size: 13.0,
                weight: if d.default {
                    Weight::Semibold
                } else {
                    Weight::Regular
                },
                color: if d.default { p.text } else { p.secondary },
                align: Align::Left,
            });
        }

        if let Some(y) = l.rule {
            out.push(Cmd::Fill {
                r: Rect::new(PAD, y, WIDTH - 2.0 * PAD, 1.0),
                radius: 0.0,
                color: p.rule,
            });
        }

        // -- a problem, instead of the apps -------------------------------------
        if let (Some(r), Some(problem)) = (l.problem, &m.problem) {
            let (title, body) = match problem {
                Problem::NeedsPermission => (
                    "Sonus needs a permission",
                    "To change each app's volume, allow Sonus under Privacy & Security \u{203a} Screen & System Audio Recording.",
                ),
                Problem::Unsupported => (
                    "App volumes need macOS 14.2",
                    "This Mac can still change the output device and its volume from here.",
                ),
            };
            out.push(Cmd::Text {
                r: Rect::new(r.x, r.y + 8.0, r.w, 20.0),
                text: title.into(),
                size: 13.5,
                weight: Weight::Semibold,
                color: p.text,
                align: Align::Left,
            });
            for (i, line) in wrap(body, 52).iter().enumerate() {
                out.push(Cmd::Text {
                    r: Rect::new(r.x, r.y + 30.0 + i as f32 * 17.0, r.w, 17.0),
                    text: line.clone(),
                    size: 12.0,
                    weight: Weight::Regular,
                    color: p.secondary,
                    align: Align::Left,
                });
            }
            if let Some(b) = l.problem_button {
                let pressed = self.pressed == Hit::ProblemButton;
                out.push(Cmd::Fill {
                    r: b,
                    radius: 6.0,
                    color: p.accent.alpha(if pressed { 0.8 } else { 1.0 }),
                });
                out.push(Cmd::Text {
                    r: b,
                    text: "Open Settings".into(),
                    size: 12.5,
                    weight: Weight::Semibold,
                    color: if self.look.dark {
                        Rgba::black(0.9)
                    } else {
                        Rgba::white(1.0)
                    },
                    align: Align::Center,
                });
            }
            return out;
        }

        // -- the apps -----------------------------------------------------------
        if let Some(r) = l.label {
            out.push(Cmd::Text {
                r,
                text: "Apps".into(),
                size: 12.0,
                weight: Weight::Semibold,
                color: p.secondary,
                align: Align::Left,
            });
        }
        if let Some(r) = l.empty {
            empty_state(
                &mut out,
                &p,
                r,
                "Nothing is playing",
                "Apps show up here when they make a sound.",
            );
        }
        for (i, (row, a)) in l.apps.iter().zip(&m.apps).enumerate() {
            let muted = a.muted || a.volume <= 0.0;
            let opacity = if muted { 0.45 } else { 1.0 };
            if a.system {
                // The chimes have no app to borrow an icon from.
                out.push(Cmd::Fill {
                    r: row.icon,
                    radius: 8.0,
                    color: p.hover.alpha(if self.look.dark { 2.0 } else { 1.6 }),
                });
                out.push(Cmd::Glyph {
                    r: row.icon.inset(7.0, 7.0),
                    glyph: Glyph::Speaker(1),
                    color: p.secondary.alpha(opacity),
                });
            } else {
                out.push(Cmd::AppIcon {
                    r: row.icon,
                    path: a.icon.clone(),
                    fallback: (tile_colour(&a.name), initial(&a.name)),
                    opacity,
                });
            }
            if self.hover == Hit::Icon(i) {
                // A hint that the icon is a mute button too.
                out.push(Cmd::Fill {
                    r: Rect::new(
                        row.icon.right() - 13.0,
                        row.icon.bottom() - 13.0,
                        15.0,
                        15.0,
                    ),
                    radius: 7.5,
                    color: if self.look.dark {
                        Rgba::hex(0x3a3a3a)
                    } else {
                        Rgba::hex(0xffffff)
                    },
                });
                out.push(Cmd::Glyph {
                    r: Rect::new(
                        row.icon.right() - 11.5,
                        row.icon.bottom() - 11.5,
                        12.0,
                        12.0,
                    ),
                    glyph: if a.muted {
                        Glyph::Speaker(1)
                    } else {
                        Glyph::SpeakerMuted
                    },
                    color: p.text,
                });
            }
            out.push(Cmd::Text {
                r: row.name,
                text: a.name.clone(),
                size: 13.0,
                weight: Weight::Regular,
                color: if a.active { p.text } else { p.secondary },
                align: Align::Left,
            });
            let peak = self.peaks.get(&a.key).copied().unwrap_or(0.0);
            self.slider(&mut out, &p, row, a.volume, a.muted, peak, Which::App(i));
            if let (Some(b), Some(media)) = (row.media, &a.media) {
                self.player(&mut out, &p, i, &b, media);
            }
        }
        out
    }

    /// The track, the three buttons, and the progress bar with its times.
    fn player(&self, out: &mut Vec<Cmd>, p: &Palette, i: usize, b: &MediaBox, media: &Media) {
        if let (Some(r), Some(art)) = (b.art, &media.art) {
            out.push(Cmd::Art {
                r,
                art: art.clone(),
            });
        }
        let line = match (media.title.is_empty(), media.artist.is_empty()) {
            (false, false) => format!("{} \u{b7} {}", media.title, media.artist),
            (false, true) => media.title.clone(),
            (true, false) => media.artist.clone(),
            (true, true) => String::new(),
        };
        if !line.is_empty() {
            out.push(Cmd::Text {
                r: b.text,
                text: line,
                size: 12.0,
                weight: Weight::Regular,
                color: p.secondary,
                align: Align::Left,
            });
        }
        let play = if media.playing {
            Glyph::Pause
        } else {
            Glyph::Play
        };
        for (r, control, glyph, enabled) in [
            (
                b.previous,
                Control::Previous,
                Glyph::Previous,
                media.can_previous,
            ),
            (b.play, Control::PlayPause, play, media.can_play_pause),
            (b.next, Control::Next, Glyph::Next, media.can_next),
        ] {
            let hit = Hit::Media(i, control);
            if enabled && (self.hover == hit || self.pressed == hit) {
                out.push(Cmd::Fill {
                    r,
                    radius: 6.0,
                    color: if self.pressed == hit {
                        p.hover.alpha(0.6)
                    } else {
                        p.hover
                    },
                });
            }
            out.push(Cmd::Glyph {
                r: r.inset(6.0, 6.0),
                glyph,
                color: if !enabled {
                    p.tertiary.alpha(0.5)
                } else if control == Control::PlayPause {
                    p.text
                } else {
                    p.secondary
                },
            });
        }
        if let (Some(t), Some(length)) = (b.timeline, media.duration) {
            let at = media.position.unwrap_or(0.0).clamp(0.0, length);
            // The elapsed time lines up under the track's name; the remaining
            // time sits on the right edge, where right alignment also gives
            // it fixed-width digits on macOS.
            for (r, text, align) in [
                (t.elapsed, media::clock(at), Align::Left),
                (
                    t.remaining,
                    format!("-{}", media::clock(length - at)),
                    Align::Right,
                ),
            ] {
                out.push(Cmd::Text {
                    r,
                    text,
                    size: 11.0,
                    weight: Weight::Regular,
                    color: p.tertiary,
                    align,
                });
            }
            let hovered = self.hover == Hit::Media(i, Control::Seek);
            let h = if hovered { 4.0 } else { 3.0 };
            let track = Rect::new(t.bar.x, t.bar.cy() - h / 2.0, t.bar.w, h);
            out.push(Cmd::Fill {
                r: track,
                radius: h / 2.0,
                color: p.track,
            });
            let w = track.w * at / length.max(0.001);
            if w > 0.5 {
                out.push(Cmd::Fill {
                    r: Rect::new(track.x, track.y, w.max(h), h),
                    radius: h / 2.0,
                    color: p.secondary,
                });
            }
        }
    }

    /// Track, fill, meter, thumb, number and mute button for one row.
    #[allow(clippy::too_many_arguments)]
    fn slider(
        &self,
        out: &mut Vec<Cmd>,
        p: &Palette,
        row: &RowBox,
        volume: f32,
        muted: bool,
        peak: f32,
        which: Which,
    ) {
        let silent = muted || volume <= 0.0;
        let s = row.slider;
        let cy = s.cy();
        let tx = row.thumb_x(volume);
        let track = Rect::new(s.x, cy - TRACK_H / 2.0, s.w, TRACK_H);
        out.push(Cmd::Fill {
            r: track,
            radius: TRACK_H / 2.0,
            color: p.track,
        });
        // Muted, there is no fill at all: the thumb says where it will come
        // back to, and the bare track says nothing is coming through.
        let fill = if silent { p.muted } else { p.accent };
        if !silent {
            out.push(Cmd::Fill {
                r: Rect::new(s.x, track.y, (tx - s.x).max(TRACK_H), TRACK_H),
                radius: TRACK_H / 2.0,
                color: fill,
            });
        }
        // The meter: how loud it is right now, as a brighter band inside the
        // fill, so the slider says both where it is set and what it is doing.
        if !silent && peak > 0.01 {
            let w = (tx - s.x) * peak.clamp(0.0, 1.0);
            if w > 1.0 {
                out.push(Cmd::Fill {
                    r: Rect::new(s.x, track.y, w.max(TRACK_H), TRACK_H),
                    radius: TRACK_H / 2.0,
                    color: p.meter,
                });
            }
        }

        let hovered = self.hover == Hit::Slider(which.clone());
        let dragging = match (&self.drag, &which) {
            (Some(Drag::Master), Which::Master) => true,
            (Some(Drag::App(k)), Which::App(i)) => {
                self.model.apps.get(*i).map(|a| &a.key) == Some(k)
            }
            _ => false,
        };
        if cfg!(target_os = "macos") {
            // AppKit's knob: a white disc with a soft shadow.
            out.push(circle(tx, cy + 0.6, THUMB_R - 0.5, Rgba::black(0.18)));
            out.push(circle(tx, cy, THUMB_R - 1.0, p.thumb));
            out.push(Cmd::Stroke {
                r: Rect::new(
                    tx - THUMB_R + 1.0,
                    cy - THUMB_R + 1.0,
                    2.0 * THUMB_R - 2.0,
                    2.0 * THUMB_R - 2.0,
                ),
                radius: THUMB_R - 1.0,
                width: 0.5,
                color: p.thumb_border,
            });
        } else {
            // Fluent's thumb: a ring in the surface colour around an accent dot
            // that grows under the pointer and shrinks while held.
            out.push(circle(tx, cy, THUMB_R, p.thumb));
            out.push(Cmd::Stroke {
                r: Rect::new(
                    tx - THUMB_R + 0.5,
                    cy - THUMB_R + 0.5,
                    2.0 * THUMB_R - 1.0,
                    2.0 * THUMB_R - 1.0,
                ),
                radius: THUMB_R - 0.5,
                width: 1.0,
                color: p.thumb_border,
            });
            let inner = if dragging {
                5.0
            } else if hovered {
                7.0
            } else {
                6.0
            };
            out.push(circle(tx, cy, inner, fill));
        }

        out.push(Cmd::Text {
            r: row.number,
            text: format!("{:.0}", volume * 100.0),
            size: 12.5,
            weight: Weight::Regular,
            color: if silent { p.tertiary } else { p.secondary },
            align: Align::Right,
        });

        let button_hit = Hit::Button(which);
        if self.hover == button_hit || self.pressed == button_hit {
            out.push(Cmd::Fill {
                r: row.button,
                radius: 6.0,
                color: if self.pressed == button_hit {
                    p.hover.alpha(0.6)
                } else {
                    p.hover
                },
            });
        }
        out.push(Cmd::Glyph {
            r: row.button.inset(6.0, 6.0),
            glyph: icon::speaker_for(volume, muted),
            color: if silent { p.tertiary } else { p.secondary },
        });
    }
}

/// The strip a meter tick redraws: the track with room for the thumb.
fn meter_rect(row: &RowBox) -> Rect {
    row.slider.inset(-2.0, 0.0)
}

fn empty_state(out: &mut Vec<Cmd>, p: &Palette, r: Rect, title: &str, body: &str) {
    out.push(Cmd::Text {
        r: Rect::new(r.x, r.y + 14.0, r.w, 20.0),
        text: title.into(),
        size: 13.5,
        weight: Weight::Semibold,
        color: p.secondary,
        align: Align::Center,
    });
    out.push(Cmd::Text {
        r: Rect::new(r.x, r.y + 36.0, r.w, 18.0),
        text: body.into(),
        size: 12.0,
        weight: Weight::Regular,
        color: p.tertiary,
        align: Align::Center,
    });
}

/// Ready a snapshot for the panel: playing apps first when there are more
/// than fit, then back into the stable order the audio side gave them, so a
/// row does not jump when its app pauses.
fn prepare(model: &mut Snapshot) {
    if model.apps.len() > MAX_APPS {
        let mut order: Vec<usize> = (0..model.apps.len()).collect();
        order.sort_by_key(|&i| !model.apps[i].active);
        let mut keep: Vec<usize> = order.into_iter().take(MAX_APPS).collect();
        keep.sort_unstable();
        let apps = std::mem::take(&mut model.apps);
        model.apps = apps
            .into_iter()
            .enumerate()
            .filter(|(i, _)| keep.contains(i))
            .map(|(_, a)| a)
            .collect();
    }
}

/// Greedy word wrap at roughly `width` characters. The panel's few paragraphs
/// are fixed English text, so measuring them properly would be wasted effort.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// A tile colour for an app with no icon, picked from its name so the same
/// app always gets the same one.
fn tile_colour(name: &str) -> Rgba {
    const TILES: [u32; 8] = [
        0x4f6bed, 0x8764b8, 0xc239b3, 0xe3008c, 0xda3b01, 0xca5010, 0x00a2ad, 0x498205,
    ];
    let h = name
        .bytes()
        .fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
    Rgba::hex(TILES[(h % TILES.len() as u32) as usize])
}

fn initial(name: &str) -> String {
    name.chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_else(|| "?".into())
}

// ---------------------------------------------------------------------------
// Dismiss guard (shared by both platforms)
// ---------------------------------------------------------------------------

/// Long enough to cover the gap between the panel losing focus and the tray
/// click that caused it arriving, short enough that a deliberate second click
/// still opens the panel.
pub(crate) const REOPEN_GUARD: std::time::Duration = std::time::Duration::from_millis(400);

#[derive(PartialEq, Eq, Debug)]
pub(crate) enum Toggle {
    Close,
    Open,
    /// It was just dismissed by the very click being handled. Doing nothing is
    /// what makes the panel close rather than flicker.
    Ignore,
}

/// Clicking the tray icon while the panel is open takes the focus away from it,
/// so the platform hides it *before* the click event reaches us, at which point
/// a naive toggle sees a hidden panel and reopens the thing the user was trying
/// to dismiss. So the panel remembers when it last went away and declines to
/// come straight back.
pub(crate) fn toggle_action(visible: bool, since_hidden: Option<std::time::Duration>) -> Toggle {
    if visible {
        Toggle::Close
    } else if since_hidden.is_some_and(|d| d < REOPEN_GUARD) {
        Toggle::Ignore
    } else {
        Toggle::Open
    }
}

/// Where the tray icon is, in screen pixels, for the panel to open beside.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Anchor {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

// ---------------------------------------------------------------------------
// Sample content
// ---------------------------------------------------------------------------

/// A stand-in cover for the sample: a sunset of a gradient.
fn sample_art() -> Art {
    let edge = media::ART_EDGE;
    let mut rgba = Vec::with_capacity((edge * edge * 4) as usize);
    for y in 0..edge {
        for x in 0..edge {
            let t = y as f32 / edge as f32;
            let u = x as f32 / edge as f32;
            rgba.extend_from_slice(&[
                (250.0 - 60.0 * t) as u8,
                (150.0 - 90.0 * t + 30.0 * u) as u8,
                (90.0 + 40.0 * t) as u8,
                255,
            ]);
        }
    }
    Art::new("sample", rgba, edge)
}

/// What `--preview-panel` and `--render-panel` show, and the landing page's
/// pictures. The apps are real ones, with their real icons: taken from what is
/// playing on this machine when it can be, then from where each app usually
/// installs, and a lettered tile when neither has it.
pub fn sample(dark: bool, devices: bool) -> (Snapshot, Look, Peaks, bool) {
    let live: HashMap<String, String> = crate::audio::Mixer::new()
        .map(|mut m| {
            m.snapshot()
                .apps
                .into_iter()
                .filter_map(|a| Some((a.name, a.icon?)))
                .collect()
        })
        .unwrap_or_default();
    let expand = |p: &str| {
        let mut p = p.to_string();
        for var in ["LOCALAPPDATA", "APPDATA", "ProgramFiles"] {
            p = p.replace(&format!("%{var}%"), &std::env::var(var).unwrap_or_default());
        }
        p
    };
    let app =
        |key: &str, name: &str, paths: &[&str], volume: f32, muted: bool, active: bool| AppAudio {
            key: key.into(),
            name: name.into(),
            icon: live.get(name).cloned().or_else(|| {
                paths
                    .iter()
                    .map(|p| expand(p))
                    .find(|p| std::path::Path::new(p).exists())
            }),
            volume,
            muted,
            active,
            system: false,
            media: None,
        };
    let mut snap = Snapshot {
        output: Some(Output {
            name: "Speakers (Realtek(R) Audio)".into(),
            volume: 0.64,
            muted: false,
        }),
        devices: vec![
            DeviceInfo {
                id: "a".into(),
                name: "Headphones (WH-1000XM5)".into(),
                default: false,
            },
            DeviceInfo {
                id: "b".into(),
                name: "Speakers (Realtek(R) Audio)".into(),
                default: true,
            },
            DeviceInfo {
                id: "c".into(),
                name: "DELL U2723QE (NVIDIA High Definition Audio)".into(),
                default: false,
            },
        ],
        apps: vec![
            app(
                "spotify",
                "Spotify",
                &[
                    r"%APPDATA%\Spotify\Spotify.exe",
                    "/Applications/Spotify.app",
                ],
                0.72,
                false,
                true,
            ),
            app(
                "chrome",
                "Google Chrome",
                &[
                    r"%ProgramFiles%\Google\Chrome\Application\chrome.exe",
                    r"%LOCALAPPDATA%\Google\Chrome\Application\chrome.exe",
                    "/Applications/Google Chrome.app",
                ],
                0.38,
                false,
                true,
            ),
            app(
                "slack",
                "Slack",
                &[r"%LOCALAPPDATA%\slack\slack.exe", "/Applications/Slack.app"],
                0.55,
                false,
                false,
            ),
            app(
                "discord",
                "Discord",
                &[
                    r"%LOCALAPPDATA%\Discord\app.ico",
                    "/Applications/Discord.app",
                ],
                0.85,
                true,
                false,
            ),
            app(
                "teams",
                "Microsoft Teams",
                &["/Applications/Microsoft Teams.app"],
                1.0,
                false,
                false,
            ),
        ],
        problem: None,
    };
    if let Some(spotify) = snap.apps.iter_mut().find(|a| a.key == "spotify") {
        spotify.media = Some(Media {
            title: "Relax, Take It Easy".into(),
            artist: "Nexeri".into(),
            playing: true,
            can_play_pause: true,
            can_previous: true,
            can_next: true,
            position: Some(143.0),
            duration: Some(164.0),
            can_seek: true,
            art: Some(sample_art()),
        });
    }
    // No Chrome here: Brave is the same browser to look at.
    if let Some(chrome) = snap.apps.iter_mut().find(|a| a.key == "chrome") {
        if chrome.icon.is_none() {
            if let Some(brave) = live.get("Brave Browser") {
                chrome.key = "brave".into();
                chrome.name = "Brave Browser".into();
                chrome.icon = Some(brave.clone());
            }
        }
    }
    let peaks = Peaks {
        output: Some(0.55),
        apps: vec![
            ("spotify".into(), 0.8),
            ("chrome".into(), 0.45),
            ("brave".into(), 0.45),
        ],
    };
    let look = Look {
        dark,
        accent: crate::theme::accent(dark),
    };
    (snap, look, peaks, devices)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn app(key: &str, volume: f32) -> AppAudio {
        AppAudio {
            key: key.into(),
            name: key.into(),
            icon: None,
            volume,
            muted: false,
            active: true,
            system: false,
            media: None,
        }
    }

    fn track(playing: bool, duration: Option<f32>) -> Media {
        Media {
            title: "Song".into(),
            artist: "Band".into(),
            playing,
            can_play_pause: true,
            can_previous: true,
            can_next: false,
            position: Some(30.0),
            duration,
            can_seek: duration.is_some(),
            art: None,
        }
    }

    fn ui(apps: usize, devices: usize) -> Ui {
        let mut ui = Ui::default();
        ui.open(
            Snapshot {
                output: Some(Output {
                    name: "Speakers".into(),
                    volume: 0.5,
                    muted: false,
                }),
                devices: (0..devices)
                    .map(|i| DeviceInfo {
                        id: format!("d{i}"),
                        name: format!("Device {i}"),
                        default: i == 0,
                    })
                    .collect(),
                apps: (0..apps).map(|i| app(&format!("app{i}"), 0.5)).collect(),
                problem: None,
            },
            Look {
                dark: true,
                accent: (0, 120, 215),
            },
        );
        ui
    }

    fn app_row(ui: &Ui, i: usize) -> RowBox {
        ui.layout().apps[i]
    }

    #[test]
    fn the_height_grows_with_the_apps_and_the_device_list() {
        let none = ui(0, 3).height();
        let two = ui(2, 3).height();
        assert!(two > none);
        let mut u = ui(2, 3);
        let picker = u.layout().picker.unwrap();
        let r = u.mouse_down(picker.x + 10.0, picker.cy());
        assert!(r.resized);
        assert_eq!(u.height(), two + 3.0 * DEVICE_H + 16.0);
    }

    #[test]
    fn there_is_no_picker_with_one_device() {
        assert!(ui(1, 1).layout().picker.is_none());
    }

    #[test]
    fn clicking_the_track_jumps_there_and_dragging_follows() {
        let mut u = ui(2, 1);
        let row = app_row(&u, 1);
        let r = u.mouse_down(row.thumb_x(0.25), row.slider.cy());
        assert_eq!(r.actions, vec![Action::AppVolume("app1".into(), 0.25)]);
        let r = u.mouse_move(row.thumb_x(0.8), row.slider.cy() + 40.0);
        assert_eq!(
            r.actions,
            vec![Action::AppVolume("app1".into(), 0.8)],
            "a drag follows the pointer even off the row"
        );
        let r = u.mouse_up(0.0, 0.0);
        assert_eq!(r.actions, vec![Action::Settled("app1".into())]);
    }

    #[test]
    fn the_ends_of_the_track_are_zero_and_full() {
        let row = RowBox::at(0.0);
        assert_eq!(row.value_at(row.slider.x - 50.0), 0.0);
        assert_eq!(row.value_at(row.slider.right() + 50.0), 1.0);
        assert_eq!(row.value_at(row.thumb_x(0.5)), 0.5);
    }

    #[test]
    fn a_refresh_does_not_fight_the_drag() {
        let mut u = ui(2, 1);
        let row = app_row(&u, 0);
        u.mouse_down(row.thumb_x(0.9), row.slider.cy());
        // A reading taken before the drag landed: app0 still at 0.5.
        let stale = Snapshot {
            output: u.model.output.clone(),
            devices: u.model.devices.clone(),
            apps: vec![app("app0", 0.5), app("app1", 0.5)],
            problem: None,
        };
        u.set_model(stale, u.look);
        assert_eq!(u.model.apps[0].volume, 0.9);
    }

    #[test]
    fn dragging_a_muted_app_unmutes_it() {
        let mut u = ui(1, 1);
        u.model.apps[0].muted = true;
        let row = app_row(&u, 0);
        let r = u.mouse_down(row.thumb_x(0.3), row.slider.cy());
        assert!(r.actions.contains(&Action::AppMuted("app0".into(), false)));
    }

    #[test]
    fn the_button_and_the_icon_both_mute() {
        let mut u = ui(1, 1);
        let row = app_row(&u, 0);
        let r = u.mouse_down(row.button.x + 5.0, row.button.cy());
        assert!(r.actions.contains(&Action::AppMuted("app0".into(), true)));
        let r = u.mouse_down(row.icon.x + 5.0, row.icon.cy());
        assert!(r.actions.contains(&Action::AppMuted("app0".into(), false)));
    }

    #[test]
    fn the_wheel_steps_and_carries_fractions() {
        let mut u = ui(1, 1);
        let row = app_row(&u, 0);
        let (x, y) = (row.name.x, row.name.cy());
        let r = u.wheel(x, y, 1.0);
        assert!(r.actions.contains(&Action::AppVolume("app0".into(), 0.52)));
        assert!(u.wheel(x, y, 0.4).actions.is_empty(), "less than a notch");
        let r = u.wheel(x, y, 0.7);
        assert!(r.actions.contains(&Action::AppVolume("app0".into(), 0.54)));
        u.wheel(x, y, -100.0);
        assert_eq!(u.model.apps[0].volume, 0.0, "clamped at zero");
    }

    #[test]
    fn choosing_a_device_marks_it_at_once() {
        let mut u = ui(1, 3);
        let picker = u.layout().picker.unwrap();
        u.mouse_down(picker.x + 10.0, picker.cy());
        let item = u.layout().devices[2];
        let r = u.mouse_down(item.x + 20.0, item.cy());
        assert_eq!(r.actions, vec![Action::Device("d2".into())]);
        assert!(u.model.devices[2].default && !u.model.devices[0].default);
        assert!(u.layout().devices.is_empty(), "the list closes");
    }

    #[test]
    fn too_many_apps_keeps_the_playing_ones_in_order() {
        let mut apps: Vec<AppAudio> = (0..12).map(|i| app(&format!("a{i:02}"), 1.0)).collect();
        for a in apps.iter_mut().take(5) {
            a.active = false;
        }
        let mut s = Snapshot {
            apps,
            ..Default::default()
        };
        prepare(&mut s);
        assert_eq!(s.apps.len(), MAX_APPS);
        let keys: Vec<&str> = s.apps.iter().map(|a| a.key.as_str()).collect();
        // All seven playing, then the first two of the silent ones, back in
        // their original order.
        assert_eq!(
            keys,
            ["a00", "a01", "a05", "a06", "a07", "a08", "a09", "a10", "a11"]
        );
    }

    #[test]
    fn only_moving_meters_ask_for_a_repaint() {
        let mut u = ui(3, 1);
        let dirty = u.set_peaks(&Peaks {
            output: None,
            apps: vec![("app1".into(), 0.6), ("app2".into(), 0.0)],
        });
        assert_eq!(dirty, vec![meter_rect(&app_row(&u, 1))]);
        let silent = Peaks {
            output: None,
            apps: vec![("app1".into(), 0.0), ("app2".into(), 0.0)],
        };
        for _ in 0..60 {
            u.set_peaks(&silent);
        }
        assert!(u.set_peaks(&silent).is_empty(), "a settled meter is still");
    }

    #[test]
    fn meters_fall_away_rather_than_flicker() {
        let mut u = ui(1, 1);
        u.set_peaks(&Peaks {
            output: Some(1.0),
            apps: vec![("app0".into(), 1.0)],
        });
        u.set_peaks(&Peaks {
            output: Some(0.0),
            apps: vec![("app0".into(), 0.0)],
        });
        assert!((u.output_peak - DECAY).abs() < 1e-6);
        assert!((u.peaks["app0"] - DECAY).abs() < 1e-6);
    }

    #[test]
    fn every_scene_paints_and_text_stays_inside() {
        for (apps, devices) in [(0, 1), (3, 3), (MAX_APPS, 2)] {
            let u = ui(apps, devices);
            let scene = u.scene();
            assert!(!scene.is_empty());
            for c in scene {
                if let Cmd::Text { r, .. } = c {
                    assert!(r.x >= 0.0 && r.right() <= WIDTH, "{r:?}");
                    assert!(r.bottom() <= u.height(), "{r:?}");
                }
            }
        }
    }

    #[test]
    fn no_output_is_said_plainly() {
        let mut u = Ui::default();
        u.open(Snapshot::default(), u.look);
        let texts: Vec<String> = u
            .scene()
            .into_iter()
            .filter_map(|c| match c {
                Cmd::Text { text, .. } => Some(text),
                _ => None,
            })
            .collect();
        assert!(texts.iter().any(|t| t == "No output device"));
    }

    #[test]
    fn a_permission_problem_offers_the_way_to_fix_it() {
        let mut u = ui(2, 1);
        u.model.problem = Some(Problem::NeedsPermission);
        let b = u.layout().problem_button.unwrap();
        let r = u.mouse_down(b.x + 5.0, b.cy());
        assert_eq!(r.actions, vec![Action::OpenPermissionSettings]);
    }

    #[test]
    fn a_player_makes_its_row_taller_and_pushes_the_rest_down() {
        let plain = ui(2, 1);
        let mut u = ui(2, 1);
        u.model.apps[0].media = Some(track(true, Some(120.0)));
        let l = u.layout();
        let b = l.apps[0].media.unwrap();
        assert!(b.timeline.is_some());
        assert!(l.apps[0].row.h > ROW_H);
        assert_eq!(l.apps[1].row.y, l.apps[0].row.bottom());
        assert_eq!(u.height(), plain.height() + l.apps[0].row.h - ROW_H);

        u.model.apps[0].media = Some(track(true, None));
        let short = u.layout().apps[0];
        assert!(short.media.unwrap().timeline.is_none(), "no length, no bar");
        assert!(short.row.h < l.apps[0].row.h);
    }

    #[test]
    fn a_cover_moves_the_track_and_its_bar_right() {
        let mut u = ui(1, 1);
        u.model.apps[0].media = Some(track(true, Some(120.0)));
        let plain = u.layout().apps[0].media.unwrap();
        u.model.apps[0].media.as_mut().unwrap().art = Some(sample_art());
        let l = u.layout();
        let b = l.apps[0].media.unwrap();
        let art = b.art.unwrap();
        assert_eq!(art.w, art.h, "square");
        assert_eq!(art.x, plain.text.x, "where the text used to start");
        assert!(b.text.x > art.right() && b.timeline.unwrap().elapsed.x > art.right());
        assert_eq!(art.bottom(), b.timeline.unwrap().bar.bottom());
        assert_eq!(b.play, plain.play, "the buttons stay put");
        assert!(u.scene().iter().any(|c| matches!(c, Cmd::Art { .. })));
    }

    #[test]
    fn play_flips_at_once_and_a_disabled_button_does_nothing() {
        let mut u = ui(1, 1);
        u.model.apps[0].media = Some(track(true, Some(120.0)));
        let b = u.layout().apps[0].media.unwrap();
        let r = u.mouse_down(b.play.x + 5.0, b.play.cy());
        assert_eq!(
            r.actions,
            vec![Action::Media("app0".into(), Command::PlayPause)]
        );
        assert!(!u.model.apps[0].media.as_ref().unwrap().playing);
        let r = u.mouse_down(b.previous.x + 5.0, b.previous.cy());
        assert_eq!(
            r.actions,
            vec![Action::Media("app0".into(), Command::Previous)]
        );
        let r = u.mouse_down(b.next.x + 5.0, b.next.cy());
        assert!(r.actions.is_empty(), "this player cannot skip ahead");
    }

    #[test]
    fn a_reading_from_before_the_click_does_not_flick_the_button_back() {
        let mut u = ui(1, 1);
        u.model.apps[0].media = Some(track(true, Some(120.0)));
        let b = u.layout().apps[0].media.unwrap();
        u.mouse_down(b.play.x + 5.0, b.play.cy());
        let output = u.model.output.clone();
        let reading = |playing: bool| {
            let mut a = app("app0", 0.5);
            a.media = Some(track(playing, Some(120.0)));
            Snapshot {
                output: output.clone(),
                apps: vec![a],
                ..Default::default()
            }
        };
        let playing = |u: &Ui| u.model.apps[0].media.as_ref().unwrap().playing;
        u.set_model(reading(true), u.look);
        assert!(!playing(&u), "still paused: the reading predates the click");
        u.set_model(reading(false), u.look);
        assert!(!playing(&u));
        // The player has caught up, so the next change is its own: played
        // from the keyboard, say.
        u.set_model(reading(true), u.look);
        assert!(playing(&u));
    }

    #[test]
    fn a_click_is_not_held_for_ever() {
        let mut u = ui(1, 1);
        u.model.apps[0].media = Some(track(true, Some(120.0)));
        let b = u.layout().apps[0].media.unwrap();
        u.mouse_down(b.play.x + 5.0, b.play.cy());
        u.pending.get_mut("app0").unwrap().until = Some(Instant::now());
        let mut a = app("app0", 0.5);
        a.media = Some(track(true, Some(120.0)));
        u.set_model(
            Snapshot {
                output: u.model.output.clone(),
                apps: vec![a],
                ..Default::default()
            },
            u.look,
        );
        assert!(
            u.model.apps[0].media.as_ref().unwrap().playing,
            "it ignored the click"
        );
        assert!(u.pending.is_empty());
    }

    #[test]
    fn clicking_the_progress_bar_seeks_there() {
        let mut u = ui(1, 1);
        u.model.apps[0].media = Some(track(true, Some(200.0)));
        let bar = u.layout().apps[0].media.unwrap().timeline.unwrap().bar;
        let r = u.mouse_down(bar.x + bar.w * 0.75, bar.cy());
        assert_eq!(
            r.actions,
            vec![Action::Media("app0".into(), Command::Seek(150.0))]
        );
        assert_eq!(
            u.model.apps[0].media.as_ref().unwrap().position,
            Some(150.0)
        );
    }

    #[test]
    fn a_player_row_paints_inside_the_panel() {
        let mut u = ui(3, 1);
        u.model.apps[1].media = Some(track(false, Some(3700.0)));
        for c in u.scene() {
            if let Cmd::Text { r, .. } | Cmd::Glyph { r, .. } = c {
                assert!(r.x >= 0.0 && r.right() <= WIDTH, "{r:?}");
                assert!(r.bottom() <= u.height(), "{r:?}");
            }
        }
    }

    #[test]
    fn wrapping_keeps_every_word() {
        let text = "To change each app's volume, allow Sonus under Privacy & Security.";
        let lines = wrap(text, 20);
        assert!(lines.iter().all(|l| l.chars().count() <= 20));
        assert_eq!(lines.join(" "), text);
    }

    #[test]
    fn a_click_that_already_dismissed_the_panel_does_not_reopen_it() {
        assert_eq!(toggle_action(true, None), Toggle::Close);
        assert_eq!(
            toggle_action(false, Some(Duration::from_millis(30))),
            Toggle::Ignore
        );
        assert_eq!(
            toggle_action(false, Some(Duration::from_secs(2))),
            Toggle::Open
        );
    }
}
