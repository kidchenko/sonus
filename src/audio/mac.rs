//! macOS: Core Audio, and process taps.
//!
//! macOS has no per-app volume. What it has, since 14.2, is the *process tap*:
//! a way to capture what one app is playing, optionally silencing the original
//! while doing so. That is enough to build per-app volume, which is how this
//! works for an app whose level you change:
//!
//! 1. a tap on the app's processes, set to mute them while it is read;
//! 2. a private aggregate device made of that tap (as input) and the current
//!    output device (as output);
//! 3. an IO callback on the aggregate that copies the tap's audio to the
//!    output, times the level you set.
//!
//! An app nobody has touched gets none of this: it plays straight to the
//! device as usual. If Sonus quits or crashes, macOS destroys its taps with it,
//! and with them the muting, so the worst case is an app back at full volume,
//! never an app gone silent.
//!
//! Taps need the "System Audio Recording" permission (Privacy & Security).
//! It is checked before any tap is made, because a tap made without it reads
//! silence, and with the original muted, that would silence the app.
//!
//! The tap API is looked up at run time rather than linked, so on an older
//! macOS the app still starts, with the device controls only.

use super::{AppAudio, DeviceInfo, Output, Peaks, Problem, Snapshot};
use crate::config::Level;
use crate::log::{ldebug, linfo, lwarn};

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};

use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, Bool, Sel};
use objc2::{msg_send, sel};
use objc2_foundation::{NSArray, NSFileManager, NSMutableDictionary, NSNumber, NSString};

// ---------------------------------------------------------------------------
// Core Audio, by hand
// ---------------------------------------------------------------------------

type AudioObjectID = u32;
type OSStatus = i32;

#[repr(C)]
#[derive(Clone, Copy)]
struct Address {
    selector: u32,
    scope: u32,
    element: u32,
}

#[repr(C)]
struct AudioBuffer {
    channels: u32,
    bytes: u32,
    data: *mut c_void,
}

/// `AudioBufferList`: a count, then that many buffers. Declared with one; the
/// rest follow it in memory.
#[repr(C)]
struct AudioBufferList {
    count: u32,
    buffers: [AudioBuffer; 1],
}

type IoProc = unsafe extern "C" fn(
    AudioObjectID,
    *const c_void,
    *const AudioBufferList,
    *const c_void,
    *mut AudioBufferList,
    *const c_void,
    *mut c_void,
) -> OSStatus;

const fn fourcc(s: &[u8; 4]) -> u32 {
    (s[0] as u32) << 24 | (s[1] as u32) << 16 | (s[2] as u32) << 8 | s[3] as u32
}

const SYSTEM_OBJECT: AudioObjectID = 1;
const SCOPE_GLOBAL: u32 = fourcc(b"glob");
const SCOPE_OUTPUT: u32 = fourcc(b"outp");
const ELEMENT_MAIN: u32 = 0;

const HW_DEVICES: u32 = fourcc(b"dev#");
const HW_DEFAULT_OUTPUT: u32 = fourcc(b"dOut");
const HW_PROCESSES: u32 = fourcc(b"prs#");
const DEVICE_UID: u32 = fourcc(b"uid ");
const OBJECT_NAME: u32 = fourcc(b"lnam");
const DEVICE_STREAMS: u32 = fourcc(b"stm#");
const DEVICE_MUTE: u32 = fourcc(b"mute");
const DEVICE_VOLUME: u32 = fourcc(b"volm");
/// `kAudioHardwareServiceDeviceProperty_VirtualMainVolume`: the one volume
/// every device answers to, whatever its channels look like underneath. The
/// same control the volume keys move.
const DEVICE_VIRTUAL_VOLUME: u32 = fourcc(b"vmvc");
const PROCESS_PID: u32 = fourcc(b"ppid");
const PROCESS_BUNDLE_ID: u32 = fourcc(b"pbid");
const PROCESS_RUNNING_OUTPUT: u32 = fourcc(b"piro");

/// Our own aggregate devices carry this in their UID, so the device list can
/// leave them out.
const UID_PREFIX: &str = "com.kidchenko.sonus.";

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioObjectGetPropertyDataSize(
        id: AudioObjectID,
        address: *const Address,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: *mut u32,
    ) -> OSStatus;
    fn AudioObjectGetPropertyData(
        id: AudioObjectID,
        address: *const Address,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: *mut u32,
        data: *mut c_void,
    ) -> OSStatus;
    fn AudioObjectSetPropertyData(
        id: AudioObjectID,
        address: *const Address,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: u32,
        data: *const c_void,
    ) -> OSStatus;
    fn AudioObjectHasProperty(id: AudioObjectID, address: *const Address) -> u8;
    fn AudioHardwareCreateAggregateDevice(
        description: *const c_void,
        device: *mut AudioObjectID,
    ) -> OSStatus;
    fn AudioHardwareDestroyAggregateDevice(device: AudioObjectID) -> OSStatus;
    fn AudioDeviceCreateIOProcID(
        device: AudioObjectID,
        proc_: IoProc,
        client: *mut c_void,
        id: *mut Option<IoProc>,
    ) -> OSStatus;
    fn AudioDeviceDestroyIOProcID(device: AudioObjectID, id: Option<IoProc>) -> OSStatus;
    fn AudioDeviceStart(device: AudioObjectID, id: Option<IoProc>) -> OSStatus;
    fn AudioDeviceStop(device: AudioObjectID, id: Option<IoProc>) -> OSStatus;
}

fn addr(selector: u32, scope: u32) -> Address {
    Address {
        selector,
        scope,
        element: ELEMENT_MAIN,
    }
}

fn has(id: AudioObjectID, a: Address) -> bool {
    unsafe { AudioObjectHasProperty(id, &a) != 0 }
}

fn get<T: Copy + Default>(id: AudioObjectID, a: Address) -> Option<T> {
    let mut value = T::default();
    let mut size = std::mem::size_of::<T>() as u32;
    let rc = unsafe {
        AudioObjectGetPropertyData(
            id,
            &a,
            0,
            std::ptr::null(),
            &mut size,
            (&mut value as *mut T).cast(),
        )
    };
    (rc == 0).then_some(value)
}

fn set<T: Copy>(id: AudioObjectID, a: Address, value: T) -> bool {
    let rc = unsafe {
        AudioObjectSetPropertyData(
            id,
            &a,
            0,
            std::ptr::null(),
            std::mem::size_of::<T>() as u32,
            (&value as *const T).cast(),
        )
    };
    rc == 0
}

fn get_ids(id: AudioObjectID, a: Address) -> Vec<AudioObjectID> {
    unsafe {
        let mut size = 0u32;
        if AudioObjectGetPropertyDataSize(id, &a, 0, std::ptr::null(), &mut size) != 0 {
            return Vec::new();
        }
        let mut ids =
            vec![0 as AudioObjectID; size as usize / std::mem::size_of::<AudioObjectID>()];
        if AudioObjectGetPropertyData(
            id,
            &a,
            0,
            std::ptr::null(),
            &mut size,
            ids.as_mut_ptr().cast(),
        ) != 0
        {
            return Vec::new();
        }
        ids.truncate(size as usize / std::mem::size_of::<AudioObjectID>());
        ids
    }
}

/// A CFString property. The caller of a Core Audio getter owns the string it
/// returns, so it is adopted, not retained.
fn get_string(id: AudioObjectID, a: Address) -> Option<String> {
    let mut s: *mut NSString = std::ptr::null_mut();
    let mut size = std::mem::size_of::<*mut NSString>() as u32;
    let rc = unsafe {
        AudioObjectGetPropertyData(
            id,
            &a,
            0,
            std::ptr::null(),
            &mut size,
            (&mut s as *mut *mut NSString).cast(),
        )
    };
    if rc != 0 {
        return None;
    }
    let s = unsafe { Retained::from_raw(s) }?;
    Some(s.to_string())
}

// ---------------------------------------------------------------------------
// Run-time lookups: the tap API, TCC, and process responsibility
// ---------------------------------------------------------------------------

type CreateTap = unsafe extern "C" fn(*mut AnyObject, *mut AudioObjectID) -> OSStatus;
type DestroyTap = unsafe extern "C" fn(AudioObjectID) -> OSStatus;
type Preflight = unsafe extern "C" fn(*const NSString, *const c_void) -> i32;
type Request = unsafe extern "C" fn(*const NSString, *const c_void, *const c_void);
type Responsible = unsafe extern "C" fn(libc::pid_t) -> libc::pid_t;

fn symbol<T: Copy>(handle: *mut c_void, name: &std::ffi::CStr) -> Option<T> {
    let p = unsafe { libc::dlsym(handle, name.as_ptr()) };
    (!p.is_null()).then(|| unsafe { std::mem::transmute_copy::<*mut c_void, T>(&p) })
}

struct Api {
    create_tap: Option<CreateTap>,
    destroy_tap: Option<DestroyTap>,
    /// TCC is private, so this may be missing on some future macOS. Without it
    /// Sonus asks Core Audio directly and lets the system prompt happen.
    preflight: Option<Preflight>,
    request: Option<Request>,
    /// Private too: which app a helper process belongs to. Safari's audio
    /// plays from a WebKit process outside Safari.app; this is how it ends up
    /// on Safari's row.
    responsible: Option<Responsible>,
}

impl Api {
    fn load() -> Self {
        unsafe {
            let all = libc::RTLD_DEFAULT;
            let tcc = libc::dlopen(
                c"/System/Library/PrivateFrameworks/TCC.framework/Versions/A/TCC".as_ptr(),
                libc::RTLD_NOW,
            );
            let has_class = AnyClass::get(c"CATapDescription").is_some();
            Self {
                create_tap: has_class
                    .then(|| symbol(all, c"AudioHardwareCreateProcessTap"))
                    .flatten(),
                destroy_tap: symbol(all, c"AudioHardwareDestroyProcessTap"),
                preflight: (!tcc.is_null())
                    .then(|| symbol(tcc, c"TCCAccessPreflight"))
                    .flatten(),
                request: (!tcc.is_null())
                    .then(|| symbol(tcc, c"TCCAccessRequest"))
                    .flatten(),
                responsible: symbol(all, c"responsibility_get_pid_responsible_for_pid"),
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Permission {
    Granted,
    Denied,
    /// Never asked, or no way to tell.
    Unknown,
}

const AUDIO_CAPTURE: &str = "kTCCServiceAudioCapture";

impl Api {
    fn permission(&self) -> Permission {
        let Some(preflight) = self.preflight else {
            return Permission::Unknown;
        };
        let service = NSString::from_str(AUDIO_CAPTURE);
        match unsafe { preflight(&*service, std::ptr::null()) } {
            0 => Permission::Granted,
            1 => Permission::Denied,
            _ => Permission::Unknown,
        }
    }

    /// Show the system prompt. The answer comes back on its own; the next tick
    /// reads it with `permission`.
    fn ask(&self) {
        let Some(request) = self.request else { return };
        let service = NSString::from_str(AUDIO_CAPTURE);
        // `void (^)(Boolean granted)`. TCC copies the block, so ours can go.
        let done = block2::RcBlock::new(|_granted: u8| {});
        unsafe {
            request(
                &*service,
                std::ptr::null(),
                block2::RcBlock::as_ptr(&done).cast(),
            )
        };
    }
}

// ---------------------------------------------------------------------------
// The tap's audio path
// ---------------------------------------------------------------------------

/// Shared with the real-time IO thread, which may not lock or allocate: plain
/// atomics only. f32s travel as their bits.
struct TapState {
    /// The level asked for.
    gain: AtomicU32,
    /// The level the last buffer ended at, so a change is a ramp across one
    /// buffer rather than a step (a step clicks).
    current: AtomicU32,
    /// The loudest sample since the meter last looked.
    peak: AtomicU32,
}

/// Copy the tap's audio to the output at the set level.
///
/// The tap delivers a stereo mixdown. The output may be anything: one
/// interleaved buffer, a buffer per channel, mono, or eight channels on an
/// interface. Channels map one to one, mono gets the average, and any channel
/// beyond the tap's two is left silent rather than given a copy, which on a
/// surround setup would play the music out of every speaker.
unsafe extern "C" fn io_proc(
    _device: AudioObjectID,
    _now: *const c_void,
    input: *const AudioBufferList,
    _input_time: *const c_void,
    output: *mut AudioBufferList,
    _output_time: *const c_void,
    client: *mut c_void,
) -> OSStatus {
    if input.is_null() || output.is_null() || client.is_null() {
        return 0;
    }
    let state = &*(client as *const TapState);
    let ins = std::slice::from_raw_parts((*input).buffers.as_ptr(), (*input).count as usize);
    let outs =
        std::slice::from_raw_parts_mut((*output).buffers.as_mut_ptr(), (*output).count as usize);

    // Where input channel `c` lives: (buffer, channel within it, its width).
    let mut map: [(usize, usize, usize); 2] = [(usize::MAX, 0, 1); 2];
    let mut in_channels = 0usize;
    for (b, buf) in ins.iter().enumerate() {
        for ch in 0..buf.channels as usize {
            if in_channels < 2 {
                map[in_channels] = (b, ch, buf.channels as usize);
            }
            in_channels += 1;
        }
    }
    let in_frames = ins
        .first()
        .filter(|b| b.channels > 0 && !b.data.is_null())
        .map(|b| b.bytes as usize / (4 * b.channels as usize))
        .unwrap_or(0);
    let sample = |c: usize, f: usize| -> f32 {
        let (b, ch, width) = map[c.min(1)];
        if b == usize::MAX || f >= in_frames {
            return 0.0;
        }
        let buf = &ins[b];
        if buf.data.is_null() {
            return 0.0;
        }
        *(buf.data as *const f32).add(f * width + ch)
    };

    let target = f32::from_bits(state.gain.load(Ordering::Relaxed));
    let start = f32::from_bits(state.current.load(Ordering::Relaxed));
    let total_out: usize = outs.iter().map(|b| b.channels as usize).sum();
    let mut peak = 0.0f32;
    let mut out_channel = 0usize;
    for buf in outs.iter_mut() {
        let width = buf.channels as usize;
        if width == 0 || buf.data.is_null() {
            continue;
        }
        let frames = buf.bytes as usize / (4 * width);
        let data = std::slice::from_raw_parts_mut(buf.data as *mut f32, frames * width);
        for f in 0..frames {
            let g = start + (target - start) * (f as f32 / frames.max(1) as f32);
            for ch in 0..width {
                let oc = out_channel + ch;
                let v = if f >= in_frames || in_channels == 0 {
                    0.0
                } else if total_out == 1 {
                    if in_channels >= 2 {
                        (sample(0, f) + sample(1, f)) * 0.5
                    } else {
                        sample(0, f)
                    }
                } else if in_channels == 1 && oc < 2 {
                    sample(0, f)
                } else if oc < in_channels.min(2) {
                    sample(oc, f)
                } else {
                    0.0
                };
                let v = v * g;
                peak = peak.max(v.abs());
                data[f * width + ch] = v;
            }
        }
        out_channel += width;
    }
    state.current.store(target.to_bits(), Ordering::Relaxed);
    // Positive floats order the same as their bits, so this is a float max.
    state
        .peak
        .fetch_max(peak.min(1.0).to_bits(), Ordering::Relaxed);
    0
}

struct Tap {
    processes: Vec<AudioObjectID>,
    device_uid: String,
    tap: AudioObjectID,
    aggregate: AudioObjectID,
    io: Option<IoProc>,
    state: Box<TapState>,
    destroy_tap: DestroyTap,
}

impl Drop for Tap {
    /// In reverse order of making: stop the audio, then the device, then the
    /// tap. Only then is the state the IO thread was reading freed.
    fn drop(&mut self) {
        unsafe {
            if self.aggregate != 0 {
                AudioDeviceStop(self.aggregate, self.io);
                AudioDeviceDestroyIOProcID(self.aggregate, self.io);
                AudioHardwareDestroyAggregateDevice(self.aggregate);
            }
            if self.tap != 0 {
                (self.destroy_tap)(self.tap);
            }
        }
    }
}

impl Tap {
    fn set_gain(&self, level: Level) {
        let g = if level.muted { 0.0 } else { level.volume };
        self.state.gain.store(g.to_bits(), Ordering::Relaxed);
    }

    fn take_peak(&self) -> f32 {
        f32::from_bits(self.state.peak.swap(0, Ordering::Relaxed))
    }
}

/// Make a tap for `processes`, playing to the device `device_uid` at `level`.
fn make_tap(
    api: &Api,
    name: &str,
    processes: &[AudioObjectID],
    device_uid: &str,
    level: Level,
) -> Result<Tap, String> {
    let (Some(create_tap), Some(destroy_tap)) = (api.create_tap, api.destroy_tap) else {
        return Err("process taps are not available".into());
    };
    let class = AnyClass::get(c"CATapDescription").ok_or("no CATapDescription")?;
    unsafe {
        let ids: Vec<Retained<NSNumber>> =
            processes.iter().map(|p| NSNumber::new_u32(*p)).collect();
        let ids = NSArray::from_retained_slice(&ids);
        let desc: Allocated<AnyObject> = msg_send![class, alloc];
        let desc: Option<Retained<AnyObject>> =
            msg_send![desc, initStereoMixdownOfProcesses: &*ids];
        let desc = desc.ok_or("could not describe the tap")?;
        // CATapMutedWhenTapped: the app is silent only while Sonus is reading
        // the tap. If Sonus stops reading, the app is heard again.
        let _: () = msg_send![&desc, setMuteBehavior: 2isize];
        // Private: no other app can see or join this tap. The setter is asked
        // about before it is called: an unknown selector would be an
        // exception, and the tap works without it.
        if responds(&desc, sel!(setPrivate:)) {
            let _: () = msg_send![&desc, setPrivate: Bool::YES];
        } else if responds(&desc, sel!(setPrivateTap:)) {
            let _: () = msg_send![&desc, setPrivateTap: Bool::YES];
        }
        let name_ns = NSString::from_str(&format!("Sonus: {name}"));
        let _: () = msg_send![&desc, setName: &*name_ns];
        let uuid: Retained<AnyObject> = msg_send![&desc, UUID];
        let tap_uid: Retained<NSString> = msg_send![&uuid, UUIDString];

        let mut tap: AudioObjectID = 0;
        let rc = create_tap(Retained::as_ptr(&desc) as *mut AnyObject, &mut tap);
        if rc != 0 || tap == 0 {
            return Err(format!("AudioHardwareCreateProcessTap failed ({rc})"));
        }

        let state = Box::new(TapState {
            gain: AtomicU32::new(0f32.to_bits()),
            current: AtomicU32::new(0f32.to_bits()),
            peak: AtomicU32::new(0),
        });
        let mut made = Tap {
            processes: processes.to_vec(),
            device_uid: device_uid.to_string(),
            tap,
            aggregate: 0,
            io: None,
            state,
            destroy_tap,
        };
        made.set_gain(level);
        // Start from the level rather than ramping up from silence.
        made.state
            .current
            .store(made.state.gain.load(Ordering::Relaxed), Ordering::Relaxed);

        let uid = format!("{UID_PREFIX}tap.{tap_uid}");
        let dict = aggregate_description(
            &format!("Sonus: {name}"),
            &uid,
            device_uid,
            &tap_uid.to_string(),
        );
        let mut aggregate: AudioObjectID = 0;
        let rc = AudioHardwareCreateAggregateDevice(Retained::as_ptr(&dict).cast(), &mut aggregate);
        if rc != 0 || aggregate == 0 {
            return Err(format!("AudioHardwareCreateAggregateDevice failed ({rc})"));
        }
        made.aggregate = aggregate;

        let client = &*made.state as *const TapState as *mut c_void;
        let mut io: Option<IoProc> = None;
        let rc = AudioDeviceCreateIOProcID(aggregate, io_proc, client, &mut io);
        if rc != 0 || io.is_none() {
            return Err(format!("AudioDeviceCreateIOProcID failed ({rc})"));
        }
        made.io = io;
        let rc = AudioDeviceStart(aggregate, io);
        if rc != 0 {
            return Err(format!("AudioDeviceStart failed ({rc})"));
        }
        Ok(made)
    }
}

fn responds(obj: &AnyObject, sel: Sel) -> bool {
    let yes: Bool = unsafe { msg_send![obj, respondsToSelector: sel] };
    yes.as_bool()
}

/// The aggregate device: the output device as its clock and its only output,
/// the tap as its input. Private, so it never shows up in Sound settings.
fn aggregate_description(
    name: &str,
    uid: &str,
    device_uid: &str,
    tap_uid: &str,
) -> Retained<NSMutableDictionary<NSString, AnyObject>> {
    fn put(d: &NSMutableDictionary<NSString, AnyObject>, key: &str, value: &AnyObject) {
        let key = NSString::from_str(key);
        unsafe {
            let _: () = msg_send![d, setObject: value, forKey: &*key];
        }
    }
    fn string(s: &str) -> Retained<AnyObject> {
        Retained::into_super(Retained::into_super(NSString::from_str(s)))
    }
    fn number(n: i32) -> Retained<AnyObject> {
        Retained::into_super(Retained::into_super(Retained::into_super(
            NSNumber::new_i32(n),
        )))
    }

    let sub: Retained<NSMutableDictionary<NSString, AnyObject>> = NSMutableDictionary::new();
    put(&sub, "uid", &string(device_uid));
    let tap: Retained<NSMutableDictionary<NSString, AnyObject>> = NSMutableDictionary::new();
    put(&tap, "uid", &string(tap_uid));
    put(&tap, "drift", &number(1));

    let subs = NSArray::from_retained_slice(&[sub]);
    let taps = NSArray::from_retained_slice(&[tap]);

    let d: Retained<NSMutableDictionary<NSString, AnyObject>> = NSMutableDictionary::new();
    put(&d, "name", &string(name));
    put(&d, "uid", &string(uid));
    put(&d, "master", &string(device_uid));
    put(&d, "clock", &string(device_uid));
    put(&d, "private", &number(1));
    put(&d, "stacked", &number(0));
    put(&d, "tapautostart", &number(1));
    unsafe {
        let _: () = msg_send![&d, setObject: &*subs, forKey: &*NSString::from_str("subdevices")];
        let _: () = msg_send![&d, setObject: &*taps, forKey: &*NSString::from_str("taps")];
    }
    d
}

// ---------------------------------------------------------------------------
// Apps
// ---------------------------------------------------------------------------

/// One app, as gathered from Core Audio's process objects.
#[derive(Clone, Debug)]
struct Group {
    key: String,
    name: String,
    bundle: Option<String>,
    processes: Vec<AudioObjectID>,
    active: bool,
}

/// "/Applications/Docker.app/Contents/MacOS/com.docker.backend" becomes
/// "/Applications/Docker.app". The outermost bundle, so a helper app nested
/// inside another shows the app people know.
fn app_bundle(exe: &str) -> Option<&str> {
    let end = exe.find(".app/")? + ".app".len();
    Some(&exe[..end])
}

fn pid_path(pid: libc::pid_t) -> Option<String> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    String::from_utf8(buf).ok()
}

fn bundle_id(bundle: &str) -> Option<String> {
    unsafe {
        let class = AnyClass::get(c"NSBundle")?;
        let path = NSString::from_str(bundle);
        let b: Option<Retained<AnyObject>> = msg_send![class, bundleWithPath: &*path];
        let id: Option<Retained<NSString>> = msg_send![&b?, bundleIdentifier];
        id.map(|s| s.to_string())
    }
}

fn display_name(bundle: &str) -> String {
    let name = NSFileManager::defaultManager()
        .displayNameAtPath(&NSString::from_str(bundle))
        .to_string();
    name.strip_suffix(".app").unwrap_or(&name).to_string()
}

/// Every process with audio open, grouped by the app it belongs to.
fn scan(
    api: &Api,
    own_pid: libc::pid_t,
    names: &mut HashMap<String, (String, String, Option<String>)>,
) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for obj in get_ids(SYSTEM_OBJECT, addr(HW_PROCESSES, SCOPE_GLOBAL)) {
        let Some(pid) = get::<libc::pid_t>(obj, addr(PROCESS_PID, SCOPE_GLOBAL)) else {
            continue;
        };
        if pid <= 0 || pid == own_pid {
            continue;
        }
        let running = get::<u32>(obj, addr(PROCESS_RUNNING_OUTPUT, SCOPE_GLOBAL)).unwrap_or(0) != 0;
        let owner = api
            .responsible
            .map(|f| unsafe { f(pid) })
            .filter(|p| *p > 0)
            .unwrap_or(pid);
        let Some(path) = pid_path(owner).or_else(|| pid_path(pid)) else {
            continue;
        };
        let (key, name, bundle) = names
            .entry(path.clone())
            .or_insert_with(|| match app_bundle(&path) {
                Some(b) => (
                    bundle_id(b).unwrap_or_else(|| b.to_string()),
                    display_name(b),
                    Some(b.to_string()),
                ),
                None => {
                    let id = get_string(obj, addr(PROCESS_BUNDLE_ID, SCOPE_GLOBAL))
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| path.clone());
                    let file = path.rsplit('/').next().unwrap_or(&path).to_string();
                    (id, file, None)
                }
            })
            .clone();
        // Core Audio's own helpers and the system's sound server never play
        // anything a person would want a slider for.
        if key.starts_with("com.apple.audio") || key == "com.apple.coreaudiod" {
            continue;
        }
        match groups.iter_mut().find(|g| g.key == key) {
            Some(g) => {
                g.processes.push(obj);
                g.active |= running;
            }
            None => groups.push(Group {
                key,
                name,
                bundle,
                processes: vec![obj],
                active: running,
            }),
        }
    }
    for g in &mut groups {
        g.processes.sort_unstable();
    }
    groups
}

// ---------------------------------------------------------------------------
// Devices
// ---------------------------------------------------------------------------

fn default_output() -> Option<AudioObjectID> {
    get::<AudioObjectID>(SYSTEM_OBJECT, addr(HW_DEFAULT_OUTPUT, SCOPE_GLOBAL)).filter(|d| *d != 0)
}

fn is_output(device: AudioObjectID) -> bool {
    let a = addr(DEVICE_STREAMS, SCOPE_OUTPUT);
    let mut size = 0u32;
    unsafe {
        AudioObjectGetPropertyDataSize(device, &a, 0, std::ptr::null(), &mut size) == 0 && size > 0
    }
}

fn device_volume(device: AudioObjectID) -> Option<f32> {
    let a = addr(DEVICE_VIRTUAL_VOLUME, SCOPE_OUTPUT);
    if has(device, a) {
        return get::<f32>(device, a);
    }
    // No virtual volume: average the channels that have one.
    let levels: Vec<f32> = (1..=2)
        .filter_map(|ch| {
            get::<f32>(
                device,
                Address {
                    selector: DEVICE_VOLUME,
                    scope: SCOPE_OUTPUT,
                    element: ch,
                },
            )
        })
        .collect();
    (!levels.is_empty()).then(|| levels.iter().sum::<f32>() / levels.len() as f32)
}

fn set_device_volume(device: AudioObjectID, v: f32) {
    let a = addr(DEVICE_VIRTUAL_VOLUME, SCOPE_OUTPUT);
    if has(device, a) {
        if !set(device, a, v) {
            lwarn!("could not set the device volume");
        }
        return;
    }
    for ch in 1..=2 {
        set(
            device,
            Address {
                selector: DEVICE_VOLUME,
                scope: SCOPE_OUTPUT,
                element: ch,
            },
            v,
        );
    }
}

// ---------------------------------------------------------------------------
// Mixer
// ---------------------------------------------------------------------------

pub struct Mixer {
    api: Api,
    own_pid: libc::pid_t,
    groups: Vec<Group>,
    taps: HashMap<String, Tap>,
    /// The level asked for, by app. An app missing here is at full volume.
    levels: HashMap<String, Level>,
    /// Apps whose remembered level has been put back this run, so it is put
    /// back once rather than after every change the user makes.
    restored: std::collections::HashSet<String>,
    /// (key, name, bundle) by executable path. Looking a bundle up is a few
    /// file reads, and the scan runs every second the panel is open.
    names: HashMap<String, (String, String, Option<String>)>,
    output_uid: String,
    asked: bool,
}

impl Mixer {
    pub fn new() -> Result<Self, String> {
        let api = Api::load();
        linfo!(
            "process taps: {}; permission check: {}",
            if api.create_tap.is_some() {
                "available"
            } else {
                "unavailable (macOS < 14.2)"
            },
            if api.preflight.is_some() {
                "available"
            } else {
                "unavailable"
            }
        );
        Ok(Self {
            api,
            own_pid: std::process::id() as libc::pid_t,
            groups: Vec::new(),
            taps: HashMap::new(),
            levels: HashMap::new(),
            restored: Default::default(),
            names: HashMap::new(),
            output_uid: String::new(),
            asked: false,
        })
    }

    fn problem(&self) -> Option<Problem> {
        if self.api.create_tap.is_none() {
            return Some(Problem::Unsupported);
        }
        (self.api.permission() == Permission::Denied).then_some(Problem::NeedsPermission)
    }

    pub fn snapshot(&mut self) -> Snapshot {
        self.follow_output();
        self.groups = scan(&self.api, self.own_pid, &mut self.names);
        self.reconcile();

        let output = default_output().map(|d| Output {
            name: get_string(d, addr(OBJECT_NAME, SCOPE_GLOBAL)).unwrap_or_else(|| "Output".into()),
            volume: device_volume(d).unwrap_or(1.0),
            muted: get::<u32>(d, addr(DEVICE_MUTE, SCOPE_OUTPUT)).unwrap_or(0) != 0,
        });
        let apps = self
            .groups
            .iter()
            .filter(|g| g.active || self.taps.contains_key(&g.key))
            .map(|g| {
                let level = self.levels.get(&g.key).copied().unwrap_or(Level {
                    volume: 1.0,
                    muted: false,
                });
                AppAudio {
                    key: g.key.clone(),
                    name: g.name.clone(),
                    icon: g.bundle.clone(),
                    volume: level.volume,
                    muted: level.muted,
                    active: g.active,
                    system: false,
                }
            })
            .collect();
        Snapshot {
            output,
            devices: devices(),
            apps,
            problem: self.problem(),
        }
    }

    pub fn peaks(&mut self) -> Peaks {
        Peaks {
            output: None,
            apps: self
                .taps
                .iter()
                .map(|(k, t)| (k.clone(), t.take_peak()))
                .collect(),
        }
    }

    pub fn set_app_volume(&mut self, key: &str, volume: f32) {
        let level = self.levels.entry(key.to_string()).or_insert(Level {
            volume: 1.0,
            muted: false,
        });
        level.volume = volume;
        self.apply(key);
    }

    pub fn set_app_muted(&mut self, key: &str, muted: bool) {
        let level = self.levels.entry(key.to_string()).or_insert(Level {
            volume: 1.0,
            muted: false,
        });
        level.muted = muted;
        self.apply(key);
    }

    /// Make the app's level true: a tap if it needs one, a new gain if it has
    /// one. A tap, once made, stays for as long as the app runs, even back at
    /// full volume: tearing it down and making it again as a slider crosses
    /// 100% would be heard.
    fn apply(&mut self, key: &str) {
        let level = self.levels.get(key).copied().unwrap_or(Level {
            volume: 1.0,
            muted: false,
        });
        if let Some(tap) = self.taps.get(key) {
            tap.set_gain(level);
            return;
        }
        if !level.muted && level.volume >= 0.999 {
            return;
        }
        match self.api.permission() {
            Permission::Denied => return,
            Permission::Unknown if self.api.preflight.is_some() => {
                // Ask, and hold the level until the answer: a tap made before
                // it would read silence, and silence the app with it.
                if !self.asked {
                    self.asked = true;
                    linfo!("asking for the System Audio Recording permission");
                    self.api.ask();
                }
                return;
            }
            _ => {}
        }
        let Some(group) = self.groups.iter().find(|g| g.key == key).cloned() else {
            return;
        };
        if self.output_uid.is_empty() {
            return;
        }
        match make_tap(
            &self.api,
            &group.name,
            &group.processes,
            &self.output_uid,
            level,
        ) {
            Ok(tap) => {
                ldebug!(
                    "tapped {} ({} process(es))",
                    group.name,
                    group.processes.len()
                );
                self.taps.insert(key.to_string(), tap);
            }
            Err(e) => lwarn!("could not tap {}: {e}", group.name),
        }
    }

    /// Keep the taps in step with the apps: gone when the app quits, remade
    /// when it gains a process (a new tab, a new helper) or the output moves.
    fn reconcile(&mut self) {
        let groups = &self.groups;
        let output = &self.output_uid;
        let before = self.taps.len();
        self.taps.retain(|key, tap| {
            groups
                .iter()
                .find(|g| &g.key == key)
                .is_some_and(|g| g.processes == tap.processes && &tap.device_uid == output)
        });
        // Whatever was dropped above and still has a level gets a fresh tap.
        if self.taps.len() != before {
            let keys: Vec<String> = self.levels.keys().cloned().collect();
            for key in keys {
                if !self.taps.contains_key(&key) && self.groups.iter().any(|g| g.key == key) {
                    self.apply(&key);
                }
            }
        }
        // An app that has gone keeps no level: when it comes back it starts
        // from the remembered one again.
        let groups = &self.groups;
        self.levels
            .retain(|k, _| groups.iter().any(|g| &g.key == k));
        self.restored.retain(|k| groups.iter().any(|g| &g.key == k));
    }

    fn follow_output(&mut self) {
        let uid = default_output()
            .and_then(|d| get_string(d, addr(DEVICE_UID, SCOPE_GLOBAL)))
            .unwrap_or_default();
        if uid != self.output_uid {
            if !self.output_uid.is_empty() {
                linfo!("output device changed; moving {} tap(s)", self.taps.len());
            }
            self.output_uid = uid;
        }
    }

    pub fn set_volume(&mut self, volume: f32) {
        if let Some(d) = default_output() {
            set_device_volume(d, volume);
        }
    }

    pub fn set_muted(&mut self, muted: bool) {
        if let Some(d) = default_output() {
            if !set(d, addr(DEVICE_MUTE, SCOPE_OUTPUT), muted as u32) {
                lwarn!("could not mute the device");
            }
        }
    }

    pub fn set_default_device(&mut self, uid: &str) {
        let found = get_ids(SYSTEM_OBJECT, addr(HW_DEVICES, SCOPE_GLOBAL))
            .into_iter()
            .find(|d| get_string(*d, addr(DEVICE_UID, SCOPE_GLOBAL)).as_deref() == Some(uid));
        match found {
            Some(d) if set(SYSTEM_OBJECT, addr(HW_DEFAULT_OUTPUT, SCOPE_GLOBAL), d) => {
                linfo!("default output set to {uid}");
                self.snapshot();
            }
            _ => lwarn!("could not set the default output to {uid}"),
        }
    }

    /// Put remembered levels back on apps as they appear, and follow the
    /// output device and the apps' processes while the panel is closed.
    pub fn tick(&mut self, remembered: &dyn Fn(&str) -> Option<Level>) {
        self.follow_output();
        self.groups = scan(&self.api, self.own_pid, &mut self.names);
        self.reconcile();
        let keys: Vec<String> = self.groups.iter().map(|g| g.key.clone()).collect();
        for key in keys {
            if self.restored.contains(&key) {
                continue;
            }
            self.restored.insert(key.clone());
            if let Some(level) = remembered(&key) {
                if !self.levels.contains_key(&key) {
                    ldebug!("restoring {key} to {:.0}%", level.volume * 100.0);
                    self.levels.insert(key.clone(), level);
                    self.apply(&key);
                }
            }
        }
        // A level asked for before the permission was granted, now granted.
        if self.asked && self.api.permission() == Permission::Granted {
            self.asked = false;
            let pending: Vec<String> = self
                .levels
                .keys()
                .filter(|k| !self.taps.contains_key(*k))
                .cloned()
                .collect();
            for key in pending {
                self.apply(&key);
            }
        }
    }
}

fn devices() -> Vec<DeviceInfo> {
    let default = default_output();
    let mut out: Vec<DeviceInfo> = get_ids(SYSTEM_OBJECT, addr(HW_DEVICES, SCOPE_GLOBAL))
        .into_iter()
        .filter(|d| is_output(*d))
        .filter_map(|d| {
            let id = get_string(d, addr(DEVICE_UID, SCOPE_GLOBAL))?;
            if id.starts_with(UID_PREFIX) {
                return None;
            }
            Some(DeviceInfo {
                name: get_string(d, addr(OBJECT_NAME, SCOPE_GLOBAL)).unwrap_or_else(|| id.clone()),
                default: Some(d) == default,
                id,
            })
        })
        .collect();
    out.sort_by_key(|d| d.name.to_lowercase());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fourcc_is_big_endian() {
        assert_eq!(fourcc(b"glob"), 0x676c6f62);
    }

    #[test]
    fn bundles() {
        assert_eq!(
            app_bundle("/Applications/Google Chrome.app/Contents/Frameworks/Google Chrome Framework.framework/Helpers/Google Chrome Helper.app/Contents/MacOS/Google Chrome Helper"),
            Some("/Applications/Google Chrome.app")
        );
        assert_eq!(app_bundle("/usr/bin/afplay"), None);
    }
}
