//! Linux: the PulseAudio protocol, which PipeWire answers too, so this is the
//! sound server on every desktop distribution of the last decade.
//!
//! Every app that plays sound opens a *sink input* on an output (a *sink*),
//! and every sink input has its own volume, as on Windows. The server keeps
//! an app's level between runs itself, so Sonus needs to remember nothing.
//!
//! The server is asked for everything once, then tells us about changes
//! (subscribe), and a copy of its tables is kept here. All of it runs on the
//! UI thread, without waiting: each call first lets the connection catch up
//! on whatever the server has said since, which never blocks.
//!
//! Meters are what pavucontrol does: a record stream on the output's monitor,
//! narrowed to one sink input, with the server doing the peak detection at
//! twenty-five readings a second. They exist only while the panel asks for
//! them.

use super::{AppAudio, DeviceInfo, Output, Peaks, Snapshot};
use crate::config::Level;
use crate::log::{ldebug, linfo, lwarn};

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::time::{Duration, Instant};

use libpulse_binding as pulse;
use pulse::callbacks::ListResult;
use pulse::context::introspect::{SinkInfo, SinkInputInfo};
use pulse::context::subscribe::{Facility, InterestMaskSet, Operation as Change};
use pulse::context::{Context, FlagSet as ContextFlags, State as ContextState};
use pulse::def::BufferAttr;
use pulse::mainloop::standard::{IterateResult, Mainloop};
use pulse::operation::{Operation, State as OperationState};
use pulse::proplist::{properties, Proplist};
use pulse::sample::{Format, Spec};
use pulse::stream::{FlagSet as StreamFlags, PeekResult, Stream};
use pulse::volume::{ChannelVolumes, Volume};

/// Meters stop this long after the panel last asked, which is when it closed.
const METERS_IDLE: Duration = Duration::from_secs(2);
/// After the server goes away (PipeWire restarted, say), how often to try it
/// again.
const RECONNECT_EVERY: Duration = Duration::from_secs(5);
/// How long a closing mixer waits for its last changes to be sent: `sonus
/// set` exits right after asking.
const FLUSH_FOR: Duration = Duration::from_secs(1);

type Pending = Operation<dyn FnMut(bool)>;

struct Sink {
    name: String,
    description: String,
    volume: ChannelVolumes,
    muted: bool,
    monitor: String,
}

struct Input {
    sink: u32,
    key: String,
    name: String,
    icon: Option<String>,
    volume: ChannelVolumes,
    muted: bool,
    corked: bool,
    system: bool,
}

/// The server's tables, as last heard. Shared with the callbacks, which run
/// inside `Mainloop::iterate` on this same thread.
#[derive(Default)]
struct Tables {
    default_sink: Option<String>,
    sinks: BTreeMap<u32, Sink>,
    inputs: BTreeMap<u32, Input>,
    /// What subscribe said changed, for `pump` to ask about.
    changes: Vec<(Facility, Change, u32)>,
    /// Replies still to come from the first full read.
    loading: u32,
}

type Shared = Rc<RefCell<Tables>>;

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
enum Meter {
    /// The output, by its sink index.
    Output(u32),
    Input(u32),
}

pub struct Mixer {
    // Streams and the context hold pointers into the main loop, so they are
    // declared, and dropped, before it.
    meters: HashMap<Meter, Stream>,
    /// Changes sent and not yet answered.
    pending: Vec<Pending>,
    context: Context,
    mainloop: Mainloop,
    tables: Shared,
    meters_asked: Option<Instant>,
    lost_at: Option<Instant>,
}

impl Mixer {
    pub fn new() -> Result<Self, String> {
        let mut mainloop = Mainloop::new().ok_or("could not create a PulseAudio main loop")?;
        let tables = Shared::default();
        let context = connect(&mut mainloop, &tables)?;
        linfo!("connected to the sound server");
        Ok(Self {
            meters: HashMap::new(),
            pending: Vec::new(),
            context,
            mainloop,
            tables,
            meters_asked: None,
            lost_at: None,
        })
    }

    pub fn snapshot(&mut self) -> Snapshot {
        self.pump();
        let t = self.tables.borrow();
        let mut snap = Snapshot::default();

        let default = t.default_sink.as_deref();
        for s in t.sinks.values() {
            snap.devices.push(DeviceInfo {
                id: s.name.clone(),
                name: s.description.clone(),
                default: Some(s.name.as_str()) == default,
            });
            if Some(s.name.as_str()) == default {
                snap.output = Some(Output {
                    name: s.description.clone(),
                    volume: level(&s.volume),
                    muted: s.muted,
                });
            }
        }

        for i in t.inputs.values() {
            match snap.apps.iter_mut().find(|a| a.key == i.key) {
                Some(app) => app.active |= !i.corked,
                None => snap.apps.push(AppAudio {
                    key: i.key.clone(),
                    name: i.name.clone(),
                    icon: i.icon.clone(),
                    volume: level(&i.volume),
                    muted: i.muted,
                    active: !i.corked,
                    system: i.system,
                    media: None,
                }),
            }
        }
        snap
    }

    pub fn peaks(&mut self) -> Peaks {
        self.meters_asked = Some(Instant::now());
        self.watch();
        self.pump();

        let t = self.tables.borrow();
        let mut peaks = Peaks::default();
        for (meter, stream) in self.meters.iter_mut() {
            let Some(peak) = drain(stream) else { continue };
            match *meter {
                Meter::Output(_) => peaks.output = Some(peaks.output.unwrap_or(0.0).max(peak)),
                Meter::Input(index) => {
                    let Some(input) = t.inputs.get(&index) else {
                        continue;
                    };
                    match peaks.apps.iter_mut().find(|(k, _)| *k == input.key) {
                        Some((_, p)) => *p = p.max(peak),
                        None => peaks.apps.push((input.key.clone(), peak)),
                    }
                }
            }
        }
        peaks
    }

    pub fn set_app_volume(&mut self, key: &str, volume: f32) {
        let mut introspect = self.context.introspect();
        let mut t = self.tables.borrow_mut();
        for (index, input) in t.inputs.iter_mut().filter(|(_, i)| i.key == key) {
            let channels = input.volume.len();
            input.volume.set(channels, to_volume(volume));
            self.pending
                .push(introspect.set_sink_input_volume(*index, &input.volume, None));
            ldebug!("{key} (#{index}): volume {:.0}%", volume * 100.0);
        }
    }

    pub fn set_app_muted(&mut self, key: &str, muted: bool) {
        let mut introspect = self.context.introspect();
        let mut t = self.tables.borrow_mut();
        for (index, input) in t.inputs.iter_mut().filter(|(_, i)| i.key == key) {
            input.muted = muted;
            self.pending
                .push(introspect.set_sink_input_mute(*index, muted, None));
            ldebug!("{key} (#{index}): muted {muted}");
        }
    }

    pub fn set_volume(&mut self, volume: f32) {
        let mut introspect = self.context.introspect();
        let mut t = self.tables.borrow_mut();
        let default = t.default_sink.clone();
        if let Some(sink) = t
            .sinks
            .values_mut()
            .find(|s| Some(&s.name) == default.as_ref())
        {
            let channels = sink.volume.len();
            sink.volume.set(channels, to_volume(volume));
            self.pending
                .push(introspect.set_sink_volume_by_name(&sink.name, &sink.volume, None));
        }
    }

    pub fn set_muted(&mut self, muted: bool) {
        let mut introspect = self.context.introspect();
        let mut t = self.tables.borrow_mut();
        let default = t.default_sink.clone();
        if let Some(sink) = t
            .sinks
            .values_mut()
            .find(|s| Some(&s.name) == default.as_ref())
        {
            sink.muted = muted;
            self.pending
                .push(introspect.set_sink_mute_by_name(&sink.name, muted, None));
        }
    }

    pub fn set_default_device(&mut self, id: &str) {
        linfo!("switching output to {id}");
        self.tables.borrow_mut().default_sink = Some(id.to_string());
        let op = self.context.set_default_sink(id, |ok| {
            if !ok {
                lwarn!("the sound server refused the new output");
            }
        });
        self.pending.push(op);
        // Older PulseAudio leaves playing streams where they were; PipeWire
        // and PulseAudio 15 move them along with the default.
        let mut t = self.tables.borrow_mut();
        let sink = t.sinks.iter().find(|(_, s)| s.name == id).map(|(i, _)| *i);
        let mut introspect = self.context.introspect();
        for (&index, input) in t.inputs.iter_mut() {
            if let Some(sink) = sink {
                input.sink = sink;
            }
            self.pending
                .push(introspect.move_sink_input_by_name(index, id, None));
        }
    }

    /// Keeps the connection read while the panel is closed, puts the meters
    /// away once it has closed, and reconnects after the server went away.
    pub fn tick(&mut self, _remembered: &dyn Fn(&str) -> Option<Level>) {
        if self
            .meters_asked
            .is_some_and(|at| at.elapsed() > METERS_IDLE)
        {
            self.meters_asked = None;
            for (_, mut stream) in self.meters.drain() {
                let _ = stream.disconnect();
            }
        }
        self.pump();
    }

    /// Let the connection catch up: run every callback that is ready, then
    /// ask about whatever subscribe said changed. Never waits.
    fn pump(&mut self) {
        if !self.connected() {
            return;
        }
        for _ in 0..2 {
            for _ in 0..64 {
                match self.mainloop.iterate(false) {
                    IterateResult::Success(0) => break,
                    IterateResult::Success(_) => {}
                    IterateResult::Quit(_) | IterateResult::Err(_) => break,
                }
            }
            self.pending
                .retain(|op| op.get_state() == OperationState::Running);
            let changes = std::mem::take(&mut self.tables.borrow_mut().changes);
            if changes.is_empty() {
                break;
            }
            for (facility, change, index) in changes {
                self.follow(facility, change, index);
            }
        }
    }

    /// Whether the context is up, reconnecting first if it fell over.
    fn connected(&mut self) -> bool {
        match self.context.get_state() {
            ContextState::Ready => return true,
            ContextState::Failed | ContextState::Terminated => {}
            _ => return false,
        }
        if self.lost_at.is_none() {
            lwarn!("lost the sound server; will try again");
            self.lost_at = Some(Instant::now() - RECONNECT_EVERY);
        }
        if self
            .lost_at
            .is_some_and(|at| at.elapsed() < RECONNECT_EVERY)
        {
            return false;
        }
        self.lost_at = Some(Instant::now());
        self.meters.clear();
        *self.tables.borrow_mut() = Tables::default();
        match connect(&mut self.mainloop, &self.tables) {
            Ok(context) => {
                self.context = context;
                self.lost_at = None;
                linfo!("reconnected to the sound server");
                true
            }
            Err(e) => {
                ldebug!("reconnect failed: {e}");
                false
            }
        }
    }

    fn follow(&mut self, facility: Facility, change: Change, index: u32) {
        let introspect = self.context.introspect();
        match (facility, change) {
            (Facility::SinkInput, Change::Removed) => {
                self.tables.borrow_mut().inputs.remove(&index);
                if let Some(mut stream) = self.meters.remove(&Meter::Input(index)) {
                    let _ = stream.disconnect();
                }
            }
            (Facility::SinkInput, _) => {
                let tables = self.tables.clone();
                introspect.get_sink_input_info(index, move |r| {
                    if let ListResult::Item(info) = r {
                        store_input(&tables, info);
                    }
                });
            }
            (Facility::Sink, Change::Removed) => {
                self.tables.borrow_mut().sinks.remove(&index);
            }
            (Facility::Sink, _) => {
                let tables = self.tables.clone();
                introspect.get_sink_info_by_index(index, move |r| {
                    if let ListResult::Item(info) = r {
                        store_sink(&tables, info);
                    }
                });
            }
            (Facility::Server, _) => {
                let tables = self.tables.clone();
                introspect.get_server_info(move |info| {
                    tables.borrow_mut().default_sink =
                        info.default_sink_name.as_ref().map(|s| s.to_string());
                });
            }
            _ => {}
        }
    }

    /// One meter for the output and one per sink input, opened as they
    /// appear and closed as they go.
    fn watch(&mut self) {
        let t = self.tables.borrow();
        let mut wanted: Vec<(Meter, String)> = Vec::new();
        if let Some((&index, sink)) = t
            .sinks
            .iter()
            .find(|(_, s)| Some(&s.name) == t.default_sink.as_ref())
        {
            wanted.push((Meter::Output(index), sink.monitor.clone()));
        }
        for (&index, input) in &t.inputs {
            if let Some(sink) = t.sinks.get(&input.sink) {
                wanted.push((Meter::Input(index), sink.monitor.clone()));
            }
        }
        drop(t);

        self.meters.retain(|meter, stream| {
            let keep = wanted.iter().any(|(m, _)| m == meter);
            if !keep {
                let _ = stream.disconnect();
            }
            keep
        });
        for (meter, monitor) in wanted {
            if self.meters.contains_key(&meter) {
                continue;
            }
            match open_meter(&mut self.context, meter, &monitor) {
                Some(stream) => {
                    self.meters.insert(meter, stream);
                }
                // Tried again on the next call.
                None => ldebug!("no meter for {monitor}"),
            }
        }
    }
}

impl Drop for Mixer {
    fn drop(&mut self) {
        let until = Instant::now() + FLUSH_FOR;
        while self
            .pending
            .iter()
            .any(|op| op.get_state() == OperationState::Running)
            && Instant::now() < until
            && self.context.get_state() == ContextState::Ready
        {
            if let IterateResult::Quit(_) | IterateResult::Err(_) = self.mainloop.iterate(true) {
                break;
            }
        }
        self.pending.clear();
        for (_, mut stream) in self.meters.drain() {
            let _ = stream.disconnect();
        }
        self.context.disconnect();
    }
}

/// Connect, wait until the server is ready, subscribe, and read everything
/// once. The one place that waits: the server answers at once, or not at all.
fn connect(mainloop: &mut Mainloop, tables: &Shared) -> Result<Context, String> {
    let mut props = Proplist::new().ok_or("could not create a property list")?;
    let _ = props.set_str(properties::APPLICATION_NAME, "Sonus");
    let _ = props.set_str(properties::APPLICATION_ID, "dev.josebarbosa.Sonus");
    let _ = props.set_str(properties::APPLICATION_ICON_NAME, "sonus");
    let mut context = Context::new_with_proplist(mainloop, "Sonus", &props)
        .ok_or("could not create a PulseAudio context")?;
    context
        .connect(None, ContextFlags::NOAUTOSPAWN, None)
        .map_err(|e| format!("no sound server: {e}"))?;
    loop {
        if let IterateResult::Quit(_) | IterateResult::Err(_) = mainloop.iterate(true) {
            return Err("the sound server connection failed".into());
        }
        match context.get_state() {
            ContextState::Ready => break,
            ContextState::Failed | ContextState::Terminated => {
                return Err("no sound server (PipeWire or PulseAudio) is running".into())
            }
            _ => {}
        }
    }

    let changes = tables.clone();
    context.set_subscribe_callback(Some(Box::new(move |facility, change, index| {
        if let (Some(f), Some(c)) = (facility, change) {
            changes.borrow_mut().changes.push((f, c, index));
        }
    })));
    context.subscribe(
        InterestMaskSet::SINK | InterestMaskSet::SINK_INPUT | InterestMaskSet::SERVER,
        |_| {},
    );

    let introspect = context.introspect();
    tables.borrow_mut().loading = 3;
    let t = tables.clone();
    introspect.get_server_info(move |info| {
        let mut t = t.borrow_mut();
        t.default_sink = info.default_sink_name.as_ref().map(|s| s.to_string());
        t.loading -= 1;
    });
    let t = tables.clone();
    introspect.get_sink_info_list(move |r| match r {
        ListResult::Item(info) => store_sink(&t, info),
        _ => t.borrow_mut().loading -= 1,
    });
    let t = tables.clone();
    introspect.get_sink_input_info_list(move |r| match r {
        ListResult::Item(info) => store_input(&t, info),
        _ => t.borrow_mut().loading -= 1,
    });
    while tables.borrow().loading > 0 {
        if let IterateResult::Quit(_) | IterateResult::Err(_) = mainloop.iterate(true) {
            return Err("the sound server connection failed".into());
        }
    }
    // The first read already has everything subscribe said meanwhile.
    tables.borrow_mut().changes.clear();
    Ok(context)
}

fn store_sink(tables: &Shared, info: &SinkInfo) {
    let Some(name) = info.name.as_ref().map(|s| s.to_string()) else {
        return;
    };
    let description = info
        .description
        .as_ref()
        .map(|s| s.to_string())
        .unwrap_or_else(|| name.clone());
    let sink = Sink {
        monitor: info
            .monitor_source_name
            .as_ref()
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("{name}.monitor")),
        name,
        description,
        volume: info.volume,
        muted: info.mute,
    };
    tables.borrow_mut().sinks.insert(info.index, sink);
}

fn store_input(tables: &Shared, info: &SinkInputInfo) {
    let props = &info.proplist;
    let get = |key: &str| props.get_str(key).filter(|v| !v.trim().is_empty());
    // Some streams have no volume of their own (passthrough); there is
    // nothing to show for them.
    if !info.has_volume {
        return;
    }
    let system = get(properties::MEDIA_ROLE).as_deref() == Some("event");
    let binary = get(properties::APPLICATION_PROCESS_BINARY);
    let app_name = get(properties::APPLICATION_NAME);
    let (key, name) = if system {
        ("system".to_string(), "System sounds".to_string())
    } else {
        let name = app_name
            .clone()
            .or_else(|| binary.clone())
            .or_else(|| info.name.as_ref().map(|s| s.to_string()))
            .unwrap_or_else(|| format!("Stream {}", info.index));
        let key = binary
            .clone()
            .or(app_name)
            .unwrap_or_else(|| name.clone())
            .to_lowercase();
        (key, display_name(&name))
    };
    let input = Input {
        sink: info.sink,
        icon: get(properties::APPLICATION_ICON_NAME).or(binary),
        key,
        name,
        volume: info.volume,
        muted: info.mute,
        corked: info.corked,
        system,
    };
    tables.borrow_mut().inputs.insert(info.index, input);
}

/// A name as people write it: `spotify` reads as `Spotify`.
fn display_name(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if name.chars().all(|c| !c.is_uppercase()) => {
            first.to_uppercase().chain(chars).collect()
        }
        _ => name.to_string(),
    }
}

/// The level a mixer shows: the volume as a fraction of normal, which is what
/// pavucontrol and the desktops' own sliders show too. Past 100% reads as
/// full.
fn level(v: &ChannelVolumes) -> f32 {
    (v.avg().0 as f32 / Volume::NORMAL.0 as f32).clamp(0.0, 1.0)
}

fn to_volume(level: f32) -> Volume {
    Volume((level.clamp(0.0, 1.0) * Volume::NORMAL.0 as f32).round() as u32)
}

fn open_meter(context: &mut Context, meter: Meter, monitor: &str) -> Option<Stream> {
    let spec = Spec {
        format: Format::F32le,
        channels: 1,
        rate: 25,
    };
    let mut stream = Stream::new(context, "Peak", &spec, None)?;
    if let Meter::Input(index) = meter {
        stream.set_monitor_stream(index).ok()?;
    }
    let attr = BufferAttr {
        maxlength: u32::MAX,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: 4,
    };
    stream
        .connect_record(
            Some(monitor),
            Some(&attr),
            StreamFlags::PEAK_DETECT
                | StreamFlags::ADJUST_LATENCY
                | StreamFlags::DONT_MOVE
                | StreamFlags::DONT_INHIBIT_AUTO_SUSPEND,
        )
        .ok()?;
    Some(stream)
}

/// The loudest reading a meter has had since the last call, or `None` if it
/// has said nothing yet.
fn drain(stream: &mut Stream) -> Option<f32> {
    let mut peak: Option<f32> = None;
    loop {
        match stream.peek() {
            Ok(PeekResult::Data(data)) => {
                for s in data.as_chunks::<4>().0 {
                    let v = f32::from_le_bytes(*s).abs();
                    peak = Some(peak.unwrap_or(0.0).max(v));
                }
                let _ = stream.discard();
            }
            Ok(PeekResult::Hole(_)) => {
                let _ = stream.discard();
            }
            Ok(PeekResult::Empty) | Err(_) => break,
        }
    }
    peak.map(|p| p.min(1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_read_as_written() {
        assert_eq!(display_name("spotify"), "Spotify");
        assert_eq!(display_name("Firefox"), "Firefox");
        assert_eq!(display_name("mpv Media Player"), "mpv Media Player");
        assert_eq!(display_name(""), "");
    }

    /// Against a real server, with something playing:
    /// packaging/linux/test-live.sh.
    #[test]
    #[ignore]
    fn meters_move_and_outputs_switch() {
        let mut mixer = Mixer::new().expect("a sound server");
        let snap = mixer.snapshot();
        let playing: Vec<_> = snap.apps.iter().filter(|a| a.active).collect();
        assert!(!playing.is_empty(), "something should be playing");

        let mut loudest = Peaks::default();
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(50));
            let p = mixer.peaks();
            if let Some(v) = p.output {
                loudest.output = Some(loudest.output.unwrap_or(0.0).max(v));
            }
            for (key, v) in p.apps {
                match loudest.apps.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, w)) => *w = w.max(v),
                    None => loudest.apps.push((key, v)),
                }
            }
        }
        assert!(loudest.output.unwrap_or(0.0) > 0.05, "{:?}", loudest.output);
        for app in &playing {
            let peak = loudest.apps.iter().find(|(k, _)| *k == app.key);
            assert!(
                peak.is_some_and(|(_, v)| *v > 0.05),
                "{}: {peak:?}",
                app.key
            );
        }

        let other = snap
            .devices
            .iter()
            .find(|d| !d.default)
            .expect("two outputs");
        mixer.set_default_device(&other.id);
        std::thread::sleep(Duration::from_millis(300));
        let snap = mixer.snapshot();
        assert_eq!(snap.output.map(|o| o.name), Some(other.name.clone()));
        let t = mixer.tables.borrow();
        let sink = t
            .sinks
            .iter()
            .find(|(_, s)| s.name == other.id)
            .map(|(i, _)| *i);
        assert!(
            t.inputs.values().all(|i| Some(i.sink) == sink),
            "streams follow"
        );
    }

    #[test]
    fn levels_round_trip() {
        let mut v = ChannelVolumes::default();
        v.set(2, to_volume(0.3));
        assert!((level(&v) - 0.3).abs() < 0.001);
        v.set(2, Volume(Volume::NORMAL.0 * 3 / 2));
        assert_eq!(level(&v), 1.0, "past 100% reads as full");
    }
}
