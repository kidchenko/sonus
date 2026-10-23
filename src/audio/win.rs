//! Windows: WASAPI.
//!
//! Every app that plays sound opens an *audio session* on the output device,
//! and every session has its own volume (`ISimpleAudioVolume`) and peak meter
//! (`IAudioMeterInformation`). That is all a per-app mixer needs; this is the
//! same machinery Windows' own Volume Mixer drives, which is also why a level
//! set here is still there after the app restarts. Windows remembers it.
//!
//! Sessions are enumerated on each snapshot and kept until the next one, so the
//! meters, read thirty times a second, are one COM call each.

use super::{AppAudio, DeviceInfo, Output, Peaks, Snapshot};
use crate::config::Level;
use crate::log::{ldebug, lwarn};
use crate::util::{pcwstr, wide};
use std::collections::HashMap;

use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, S_OK};
use windows::Win32::Media::Audio::Endpoints::{IAudioEndpointVolume, IAudioMeterInformation};
use windows::Win32::Media::Audio::{
    eCommunications, eConsole, eMultimedia, eRender, AudioSessionStateActive,
    AudioSessionStateExpired, ERole, IAudioSessionControl2, IAudioSessionManager2, IMMDevice,
    IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
    STGM_READ,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// Every volume change carries this, so a notification that comes back about
/// it can be told apart from one about somebody else's change. Nothing listens
/// for notifications yet; it costs nothing to be a good citizen.
const CONTEXT: GUID = GUID::from_u128(0x8f6a1c2e_3b7d_4e59_9a41_5c0de0a5f001);

const SYSTEM_KEY: &str = "system";

struct Endpoint {
    id: String,
    name: String,
    volume: IAudioEndpointVolume,
    meter: Option<IAudioMeterInformation>,
    sessions: IAudioSessionManager2,
}

struct Session {
    volume: ISimpleAudioVolume,
    meter: Option<IAudioMeterInformation>,
}

struct Group {
    key: String,
    sessions: Vec<Session>,
}

pub struct Mixer {
    enumerator: IMMDeviceEnumerator,
    endpoint: Option<Endpoint>,
    groups: Vec<Group>,
    /// Display names by executable path. Reading a file's version resource is
    /// a few file reads, and the snapshot runs every second the panel is open.
    names: HashMap<String, String>,
    own_pid: u32,
}

impl Mixer {
    pub fn new() -> Result<Self, String> {
        unsafe {
            // S_FALSE ("already initialised") and RPC_E_CHANGED_MODE (someone
            // got there first with another model) both leave COM usable on
            // this thread, which is all that matters.
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|e| format!("no audio device enumerator: {e}"))?;
            Ok(Self {
                enumerator,
                endpoint: None,
                groups: Vec::new(),
                names: HashMap::new(),
                own_pid: std::process::id(),
            })
        }
    }

    /// The default output, re-resolved every time: the user can switch it from
    /// Windows' own flyout while the panel is closed, and a cached device would
    /// leave every slider talking to the headset they just unplugged.
    fn refresh_endpoint(&mut self) {
        let device = unsafe {
            self.enumerator
                .GetDefaultAudioEndpoint(eRender, eMultimedia)
        };
        let Ok(device) = device else {
            self.endpoint = None;
            return;
        };
        let id = device_id(&device);
        if self.endpoint.as_ref().is_some_and(|e| e.id == id) {
            return;
        }
        self.endpoint = open_endpoint(&device, id);
        if let Some(e) = &self.endpoint {
            ldebug!("output device: {} ({})", e.name, e.id);
        }
    }

    pub fn snapshot(&mut self) -> Snapshot {
        self.refresh_endpoint();
        let mut snap = Snapshot {
            devices: self.devices(),
            ..Default::default()
        };
        let Some(endpoint) = &self.endpoint else {
            self.groups.clear();
            return snap;
        };
        unsafe {
            snap.output = Some(Output {
                name: endpoint.name.clone(),
                volume: endpoint.volume.GetMasterVolumeLevelScalar().unwrap_or(0.0),
                muted: endpoint
                    .volume
                    .GetMute()
                    .map(|b| b.as_bool())
                    .unwrap_or(false),
            });
        }

        let mut groups: Vec<Group> = Vec::new();
        let mut apps: Vec<AppAudio> = Vec::new();
        for found in self.sessions() {
            let index = match groups.iter().position(|g| g.key == found.key) {
                Some(i) => i,
                None => {
                    groups.push(Group {
                        key: found.key.clone(),
                        sessions: Vec::new(),
                    });
                    let (volume, muted) = unsafe {
                        (
                            found.session.volume.GetMasterVolume().unwrap_or(1.0),
                            found
                                .session
                                .volume
                                .GetMute()
                                .map(|b| b.as_bool())
                                .unwrap_or(false),
                        )
                    };
                    apps.push(AppAudio {
                        key: found.key.clone(),
                        name: found.name.clone(),
                        icon: found.exe.clone(),
                        volume,
                        muted,
                        active: false,
                        system: found.system,
                    });
                    groups.len() - 1
                }
            };
            apps[index].active |= found.active;
            groups[index].sessions.push(found.session);
        }
        self.groups = groups;
        snap.apps = apps;
        snap
    }

    /// Every live session on the current output, with what it belongs to.
    fn sessions(&mut self) -> Vec<Found> {
        let Some(endpoint) = &self.endpoint else {
            return Vec::new();
        };
        let mut out = Vec::new();
        unsafe {
            let Ok(list) = endpoint.sessions.GetSessionEnumerator() else {
                return out;
            };
            let count = list.GetCount().unwrap_or(0);
            for i in 0..count {
                let Ok(control) = list.GetSession(i) else {
                    continue;
                };
                let Ok(control) = control.cast::<IAudioSessionControl2>() else {
                    continue;
                };
                let state = control.GetState().unwrap_or(AudioSessionStateExpired);
                if state == AudioSessionStateExpired {
                    continue;
                }
                let Ok(volume) = control.cast::<ISimpleAudioVolume>() else {
                    continue;
                };
                let meter = control.cast::<IAudioMeterInformation>().ok();
                let system = control.IsSystemSoundsSession() == S_OK;
                let pid = control.GetProcessId().unwrap_or(0);
                let active = state == AudioSessionStateActive;

                if system {
                    out.push(Found {
                        key: SYSTEM_KEY.into(),
                        name: "System sounds".into(),
                        exe: None,
                        system: true,
                        active,
                        session: Session { volume, meter },
                    });
                    continue;
                }
                if pid == self.own_pid || pid == 0 {
                    continue;
                }
                let exe = process_path(pid);
                // A session outlives its process for a while. If the process is
                // gone (no path, and nothing playing), so is the row.
                if exe.is_none() && !active {
                    continue;
                }
                let display = take_pwstr(control.GetDisplayName().ok());
                let name = match &exe {
                    _ if !display.is_empty() && !display.starts_with('@') => display,
                    Some(path) => self
                        .names
                        .entry(path.clone())
                        .or_insert_with(|| app_name(path))
                        .clone(),
                    None => format!("Process {pid}"),
                };
                let key = exe
                    .as_ref()
                    .map(|p| p.to_lowercase())
                    .unwrap_or_else(|| format!("pid:{pid}"));
                out.push(Found {
                    key,
                    name,
                    exe,
                    system: false,
                    active,
                    session: Session { volume, meter },
                });
            }
        }
        out
    }

    pub fn peaks(&mut self) -> Peaks {
        unsafe {
            Peaks {
                output: self
                    .endpoint
                    .as_ref()
                    .and_then(|e| e.meter.as_ref())
                    .and_then(|m| m.GetPeakValue().ok()),
                apps: self
                    .groups
                    .iter()
                    .map(|g| {
                        let peak = g
                            .sessions
                            .iter()
                            .filter_map(|s| s.meter.as_ref()?.GetPeakValue().ok())
                            .fold(0.0f32, f32::max);
                        (g.key.clone(), peak)
                    })
                    .collect(),
            }
        }
    }

    pub fn set_app_volume(&mut self, key: &str, volume: f32) {
        for s in self.group(key) {
            if let Err(e) = unsafe { s.volume.SetMasterVolume(volume, &CONTEXT) } {
                lwarn!("could not set the volume of {key}: {e}");
            }
        }
    }

    pub fn set_app_muted(&mut self, key: &str, muted: bool) {
        for s in self.group(key) {
            if let Err(e) = unsafe { s.volume.SetMute(muted, &CONTEXT) } {
                lwarn!("could not mute {key}: {e}");
            }
        }
    }

    fn group<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a Session> + 'a {
        self.groups
            .iter()
            .filter(move |g| g.key == key)
            .flat_map(|g| g.sessions.iter())
    }

    pub fn set_volume(&mut self, volume: f32) {
        if let Some(e) = &self.endpoint {
            if let Err(err) = unsafe { e.volume.SetMasterVolumeLevelScalar(volume, &CONTEXT) } {
                lwarn!("could not set the device volume: {err}");
            }
        }
    }

    pub fn set_muted(&mut self, muted: bool) {
        if let Some(e) = &self.endpoint {
            if let Err(err) = unsafe { e.volume.SetMute(muted, &CONTEXT) } {
                lwarn!("could not mute the device: {err}");
            }
        }
    }

    pub fn set_default_device(&mut self, id: &str) {
        match policy::set_default(id) {
            Ok(()) => {
                crate::log::linfo!("default output set to {id}");
                // Picked up properly on the next snapshot; dropping the cached
                // endpoint now means nothing talks to the old one meanwhile.
                self.endpoint = None;
                self.groups.clear();
            }
            Err(e) => lwarn!("could not set the default output: {e}"),
        }
    }

    pub fn tick(&mut self, _remembered: &dyn Fn(&str) -> Option<Level>) {}

    fn devices(&self) -> Vec<DeviceInfo> {
        let default = self.endpoint.as_ref().map(|e| e.id.as_str()).unwrap_or("");
        let mut out = Vec::new();
        unsafe {
            let Ok(list) = self
                .enumerator
                .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
            else {
                return out;
            };
            for i in 0..list.GetCount().unwrap_or(0) {
                let Ok(device) = list.Item(i) else { continue };
                let id = device_id(&device);
                out.push(DeviceInfo {
                    name: device_name(&device),
                    default: id == default,
                    id,
                });
            }
        }
        out.sort_by_key(|d| d.name.to_lowercase());
        out
    }
}

struct Found {
    key: String,
    name: String,
    exe: Option<String>,
    system: bool,
    active: bool,
    session: Session,
}

fn open_endpoint(device: &IMMDevice, id: String) -> Option<Endpoint> {
    unsafe {
        let volume = device
            .Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
            .map_err(|e| lwarn!("no endpoint volume: {e}"))
            .ok()?;
        let sessions = device
            .Activate::<IAudioSessionManager2>(CLSCTX_ALL, None)
            .map_err(|e| lwarn!("no session manager: {e}"))
            .ok()?;
        let meter = device
            .Activate::<IAudioMeterInformation>(CLSCTX_ALL, None)
            .ok();
        Some(Endpoint {
            name: device_name(device),
            id,
            volume,
            meter,
            sessions,
        })
    }
}

fn device_id(device: &IMMDevice) -> String {
    take_pwstr(unsafe { device.GetId() }.ok())
}

/// "Speakers (Realtek(R) Audio)": the name the Sound settings page shows.
fn device_name(device: &IMMDevice) -> String {
    unsafe {
        device
            .OpenPropertyStore(STGM_READ)
            .and_then(|store| store.GetValue(&PKEY_Device_FriendlyName))
            .map(|v| v.to_string())
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Output device".into())
    }
}

/// A COM-allocated string, copied out and freed.
fn take_pwstr(p: Option<PWSTR>) -> String {
    let Some(p) = p else { return String::new() };
    if p.is_null() {
        return String::new();
    }
    unsafe {
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

fn process_path(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = vec![0u16; 1024];
        let mut size = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);
        ok.ok()?;
        Some(String::from_utf16_lossy(&buf[..size as usize]))
    }
}

/// "Google Chrome" rather than "chrome": the executable's FileDescription,
/// which is what Task Manager and the Volume Mixer show. The file name, tidied,
/// when there is none.
fn app_name(exe: &str) -> String {
    file_description(exe)
        .filter(|d| !d.trim().is_empty())
        .map(|d| d.trim().to_string())
        .unwrap_or_else(|| {
            let stem = std::path::Path::new(exe)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| exe.to_string());
            let mut c = stem.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        })
}

fn file_description(exe: &str) -> Option<String> {
    let path = wide(exe);
    unsafe {
        let size = GetFileVersionInfoSizeW(pcwstr(&path), None);
        if size == 0 {
            return None;
        }
        let mut block = vec![0u8; size as usize];
        GetFileVersionInfoW(pcwstr(&path), None, size, block.as_mut_ptr().cast()).ok()?;

        // The language/codepage pairs the resource has strings for; the first
        // is the one it was built with. The two common ones follow, for the
        // files that forget to say.
        let mut langs: Vec<String> = Vec::new();
        let mut ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut len = 0u32;
        let translation = wide(r"\VarFileInfo\Translation");
        if VerQueryValueW(
            block.as_ptr().cast(),
            pcwstr(&translation),
            &mut ptr,
            &mut len,
        )
        .as_bool()
            && len >= 4
        {
            let pairs = std::slice::from_raw_parts(ptr as *const u16, (len / 2) as usize);
            for pair in pairs.chunks(2).filter(|p| p.len() == 2) {
                langs.push(format!("{:04x}{:04x}", pair[0], pair[1]));
            }
        }
        langs.push("040904b0".into());
        langs.push("040904e4".into());

        for lang in langs {
            let query = wide(&format!(r"\StringFileInfo\{lang}\FileDescription"));
            let mut ptr: *mut std::ffi::c_void = std::ptr::null_mut();
            let mut len = 0u32;
            if VerQueryValueW(block.as_ptr().cast(), pcwstr(&query), &mut ptr, &mut len).as_bool()
                && len > 1
            {
                let s = std::slice::from_raw_parts(ptr as *const u16, len as usize);
                let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
                return Some(String::from_utf16_lossy(&s[..end]));
            }
        }
        None
    }
}

/// Changing the default output device.
///
/// There is no documented API for this: Windows' own Sound settings use the
/// undocumented `IPolicyConfig`, and so does every device switcher (EarTrumpet,
/// SoundSwitch). Its layout has been stable since Windows 7. It is called
/// through its vtable here rather than declared as an interface, because the
/// one method needed is the only part of it worth trusting.
mod policy {
    use super::*;
    use windows::core::{IUnknown, HRESULT, PCWSTR};

    const CLSID_POLICY_CONFIG: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
    const IID_POLICY_CONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);
    /// IUnknown's three, then GetMixFormat … SetPropertyValue (ten), then
    /// SetDefaultEndpoint.
    const SET_DEFAULT_ENDPOINT: usize = 13;

    type SetDefaultEndpoint =
        unsafe extern "system" fn(*mut std::ffi::c_void, PCWSTR, ERole) -> HRESULT;

    pub fn set_default(id: &str) -> Result<(), String> {
        unsafe {
            let unknown: IUnknown = CoCreateInstance(&CLSID_POLICY_CONFIG, None, CLSCTX_ALL)
                .map_err(|e| format!("policy config: {e}"))?;
            let mut raw: *mut std::ffi::c_void = std::ptr::null_mut();
            unknown
                .query(&IID_POLICY_CONFIG, &mut raw)
                .ok()
                .map_err(|e| format!("IPolicyConfig: {e}"))?;
            if raw.is_null() {
                return Err("IPolicyConfig: null".into());
            }
            // Owned from here: `from_raw` releases it when it goes.
            let policy = IUnknown::from_raw(raw);
            let vtable = *(policy.as_raw() as *const *const usize);
            let call: SetDefaultEndpoint = std::mem::transmute(*vtable.add(SET_DEFAULT_ENDPOINT));
            let id = wide(id);
            // All three roles, as Sound settings does: "switch to my headset"
            // means for calls too.
            for role in [eConsole, eMultimedia, eCommunications] {
                call(policy.as_raw(), pcwstr(&id), role)
                    .ok()
                    .map_err(|e| format!("SetDefaultEndpoint: {e}"))?;
            }
            Ok(())
        }
    }
}
