//! The tray application: the icon, its menu, and the wiring between the panel
//! and the audio side.
//!
//! Everything here runs on the main thread, driven by the event loop in
//! `main`. Left button opens the [`panel`](crate::panel), which is the mixer;
//! right button opens the menu, which holds the commands.
//!
//! The loop sleeps between ticks. While the panel is open it wakes twenty-five
//! times a second for the meters and once a second for a fresh reading of the
//! apps; while it is closed, every two seconds, for the tooltip and the
//! housekeeping (remembered levels on macOS, a hand-edited config, a theme
//! change). Nothing polls faster than it has to.

use crate::audio::{Mixer, Snapshot};
use crate::config::{Config, Level};
use crate::icon::{self, Glyph};
use crate::log::{ldebug, lerror, linfo};
use crate::panel::{Action, Anchor, Look, Panel};
use crate::{art, autostart, log, util};

use std::collections::HashMap;
use std::time::{Duration, Instant};
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoopProxy};
use tray_icon::menu::{
    accelerator::Accelerator, CheckMenuItem, IconMenuItem, Menu, MenuEvent, MenuItem,
    PredefinedMenuItem, Submenu,
};
use tray_icon::{
    Icon as TrayImage, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
};

const ID_PANEL: &str = "panel";
const ID_AUTOSTART: &str = "autostart";
const ID_CONFIG: &str = "config";
const ID_LOG: &str = "log";
const ID_QUIT: &str = "quit";
/// Device rows carry this prefix and then the device's id.
const ID_DEVICE: &str = "device:";
/// Every read-only row shares this id; nothing ever dispatches on it.
const ID_ROW: &str = "row";

/// Windows truncates `szTip` at 128 characters, without saying so.
const TOOLTIP_MAX: usize = 127;

const METER_EVERY: Duration = Duration::from_millis(40);
const REFRESH_EVERY: Duration = Duration::from_millis(1000);
const IDLE_EVERY: Duration = Duration::from_secs(2);

pub enum UserEvent {
    TogglePanel(Option<Anchor>),
    ShowPanel,
    Panel(Action),
    Device(String),
    ToggleAutostart,
    OpenConfig,
    OpenLog,
    Quit,
}

pub struct App {
    tray: TrayIcon,
    mixer: Mixer,
    /// `None` if the popup window could not be created. The tray keeps working
    /// without it: the menu can still switch devices.
    panel: Option<Panel>,
    cfg: Config,
    cfg_seen: Option<std::time::SystemTime>,
    /// Levels to remember, waiting to be written. Written on the idle tick or
    /// when the panel closes, not on every notch of the wheel.
    cfg_dirty: bool,
    /// The last level set on each app, for remembering when a gesture ends.
    last_set: HashMap<String, Level>,
    snapshot: Snapshot,
    /// What the menu was last built from, so it is rebuilt only on a change.
    menu_devices: Vec<(String, String, bool)>,
    dark_taskbar: bool,
    tooltip: String,
    next_meter: Instant,
    next_refresh: Instant,
    next_idle: Instant,
    exit: bool,
}

impl App {
    pub fn new(proxy: EventLoopProxy<UserEvent>) -> Result<Self, String> {
        forward_menu_events(&proxy);
        forward_tray_events(&proxy);

        let dark_taskbar = crate::theme::taskbar_is_dark();
        let image = TrayImage::from_rgba(
            icon::render(dark_taskbar, icon::EDGE),
            icon::EDGE,
            icon::EDGE,
        )
        .map_err(|e| format!("could not build the tray image: {e}"))?;
        let builder = TrayIconBuilder::new()
            .with_tooltip("Sonus")
            // Left button opens the panel (handled below); the menu belongs to
            // the right button, as Windows expects.
            .with_menu_on_left_click(false);
        // On macOS the icon is a template: AppKit tints it for the menu bar.
        #[cfg(target_os = "macos")]
        let builder = builder.with_icon_templated(image);
        #[cfg(windows)]
        let builder = builder.with_icon(image);
        let tray = builder
            .build()
            .map_err(|e| format!("could not create the tray icon: {e}"))?;

        let mut mixer = Mixer::new()?;
        let cfg = Config::load();
        let snapshot = mixer.snapshot();
        log_snapshot(&snapshot);
        let snapshot = filtered(snapshot, &cfg);

        let panel_proxy = proxy.clone();
        let panel = Panel::new(Box::new(move |a| {
            let _ = panel_proxy.send_event(UserEvent::Panel(a));
        }))
        .map_err(|e| lerror!("panel unavailable: {e}"))
        .ok();
        if let Some(p) = &panel {
            p.set_wheel_step(cfg.wheel_step);
        }

        let now = Instant::now();
        let mut app = Self {
            tray,
            mixer,
            panel,
            cfg_seen: Config::modified(),
            cfg,
            cfg_dirty: false,
            last_set: HashMap::new(),
            snapshot,
            menu_devices: Vec::new(),
            dark_taskbar,
            tooltip: String::new(),
            next_meter: now,
            next_refresh: now + REFRESH_EVERY,
            next_idle: now,
            exit: false,
        };
        app.restore_levels();
        app.rebuild_menu();
        app.update_tooltip();
        Ok(app)
    }

    /// Handle one event, and say when to wake up next.
    pub fn handle(&mut self, event: Event<UserEvent>) -> ControlFlow {
        if let Event::UserEvent(e) = event {
            self.user(e);
        }
        if self.exit {
            self.save_levels();
            return ControlFlow::Exit;
        }
        self.ticks()
    }

    fn user(&mut self, e: UserEvent) {
        match e {
            UserEvent::Quit => {
                linfo!("quit requested from the tray");
                self.exit = true;
            }
            UserEvent::TogglePanel(anchor) => self.toggle_panel(anchor),
            UserEvent::ShowPanel => self.show_panel(),
            UserEvent::Panel(a) => self.act(a),
            UserEvent::Device(id) => {
                self.mixer.set_default_device(&id);
                self.refresh();
            }
            UserEvent::ToggleAutostart => self.toggle_autostart(),
            UserEvent::OpenConfig => open_config(),
            UserEvent::OpenLog => log::reveal(),
        }
    }

    fn visible(&self) -> bool {
        self.panel.as_ref().is_some_and(|p| p.is_visible())
    }

    /// Run whatever is due, then sleep until the next thing is.
    fn ticks(&mut self) -> ControlFlow {
        let now = Instant::now();
        let visible = self.visible();
        if visible {
            if now >= self.next_meter {
                let peaks = self.mixer.peaks();
                if let Some(p) = &self.panel {
                    p.set_peaks(&peaks);
                }
                self.next_meter = now + METER_EVERY;
            }
            if now >= self.next_refresh {
                // Not while a slider is held: the reading would be about to be
                // overwritten anyway, and enumerating sessions mid-drag is the
                // one time it could be felt.
                if !self.panel.as_ref().is_some_and(|p| p.busy()) {
                    self.refresh();
                }
                self.next_refresh = now + REFRESH_EVERY;
            }
        }
        if now >= self.next_idle {
            self.idle();
            self.next_idle = now + IDLE_EVERY;
        }
        let next = if visible {
            self.next_meter.min(self.next_refresh).min(self.next_idle)
        } else {
            self.next_idle
        };
        ControlFlow::WaitUntil(next)
    }

    /// The slow tick.
    fn idle(&mut self) {
        let dark = crate::theme::taskbar_is_dark();
        if dark != self.dark_taskbar {
            self.dark_taskbar = dark;
            self.set_tray_image();
        }
        self.reload_config();
        let cfg = &self.cfg;
        self.mixer.tick(&|key| cfg.level_for(key));
        if !self.visible() {
            self.snapshot = self.read();
            self.save_levels();
        }
        self.update_tooltip();
        self.rebuild_menu_if_changed();
    }

    /// A fresh reading, less what the settings hide.
    fn read(&mut self) -> Snapshot {
        filtered(self.mixer.snapshot(), &self.cfg)
    }

    fn refresh(&mut self) {
        self.snapshot = self.read();
        if let Some(p) = &self.panel {
            p.update(self.snapshot.clone(), Look::current());
        }
        self.update_tooltip();
        self.rebuild_menu_if_changed();
    }

    // -- the panel ---------------------------------------------------------

    /// Open the panel from somewhere other than the tray icon: a second
    /// launch, or the installer.
    pub fn show_panel(&mut self) {
        self.snapshot = self.read();
        let anchor = tray_anchor(&self.tray);
        if let Some(p) = &self.panel {
            p.show(self.snapshot.clone(), Look::current(), anchor);
        }
        self.wake_now();
    }

    fn toggle_panel(&mut self, anchor: Option<Anchor>) {
        let was = self.visible();
        if !was {
            // Fresh numbers before it opens: it may have been closed for hours.
            self.snapshot = self.read();
        }
        if let Some(p) = &self.panel {
            p.toggle(self.snapshot.clone(), Look::current(), anchor);
        }
        if self.visible() {
            self.wake_now();
        } else {
            self.save_levels();
        }
    }

    fn wake_now(&mut self) {
        let now = Instant::now();
        self.next_meter = now;
        self.next_refresh = now + REFRESH_EVERY;
    }

    /// Carry out what the panel asked for. The panel has already drawn the
    /// result; this makes it true.
    fn act(&mut self, a: Action) {
        ldebug!("panel: {a:?}");
        match a {
            Action::AppVolume(key, v) => {
                self.mixer.set_app_volume(&key, v);
                let muted = self.last_set.get(&key).is_some_and(|l| l.muted);
                self.last_set.insert(key, Level { volume: v, muted });
            }
            Action::AppMuted(key, m) => {
                self.mixer.set_app_muted(&key, m);
                let volume = self
                    .last_set
                    .get(&key)
                    .map(|l| l.volume)
                    .or_else(|| {
                        self.snapshot
                            .apps
                            .iter()
                            .find(|x| x.key == key)
                            .map(|x| x.volume)
                    })
                    .unwrap_or(1.0);
                self.last_set.insert(key, Level { volume, muted: m });
            }
            Action::Volume(v) => {
                self.mixer.set_volume(v);
                if let Some(o) = &mut self.snapshot.output {
                    o.volume = v;
                }
                self.update_tooltip();
            }
            Action::Muted(m) => {
                self.mixer.set_muted(m);
                if let Some(o) = &mut self.snapshot.output {
                    o.muted = m;
                }
                self.update_tooltip();
            }
            Action::Device(id) => {
                self.mixer.set_default_device(&id);
                self.refresh();
            }
            Action::Settled(key) => {
                if Mixer::needs_memory() {
                    if let Some(level) = self.last_set.get(&key).copied() {
                        self.cfg_dirty |= self.cfg.remember_level(&key, level);
                    }
                }
            }
            Action::OpenPermissionSettings => {
                util::shell_open(
                    "x-apple.systempreferences:com.apple.preference.security?Privacy_AudioCapture",
                );
                if let Some(p) = &self.panel {
                    p.hide();
                }
            }
        }
    }

    // -- remembered levels ---------------------------------------------------

    /// macOS forgets an app's level with the tap that held it, so Sonus puts
    /// the remembered ones back as apps appear. (Windows remembers for itself.)
    fn restore_levels(&mut self) {
        let cfg = &self.cfg;
        self.mixer.tick(&|key| cfg.level_for(key));
    }

    fn save_levels(&mut self) {
        if !self.cfg_dirty {
            return;
        }
        match self.cfg.save() {
            Ok(()) => {
                self.cfg_dirty = false;
                self.cfg_seen = Config::modified();
            }
            Err(e) => lerror!("could not save the config: {e}"),
        }
    }

    /// Pick up a hand edit of config.json.
    fn reload_config(&mut self) {
        let seen = Config::modified();
        if seen == self.cfg_seen {
            return;
        }
        self.cfg_seen = seen;
        if let Some(c) = Config::reload() {
            if c != self.cfg {
                linfo!("config changed on disk; reloaded");
                if let Some(p) = &self.panel {
                    p.set_wheel_step(c.wheel_step);
                }
                self.cfg = c;
            }
        }
    }

    // -- tray plumbing -------------------------------------------------------

    fn set_tray_image(&self) {
        match TrayImage::from_rgba(
            icon::render(self.dark_taskbar, icon::EDGE),
            icon::EDGE,
            icon::EDGE,
        ) {
            Ok(image) => {
                #[cfg(target_os = "macos")]
                let set = self.tray.set_icon_templated(Some(image));
                #[cfg(windows)]
                let set = self.tray.set_icon(Some(image));
                if let Err(e) = set {
                    lerror!("could not set the tray icon: {e}");
                }
            }
            Err(e) => lerror!("could not build the tray image: {e}"),
        }
    }

    fn update_tooltip(&mut self) {
        let text = match &self.snapshot.output {
            Some(o) if o.muted => format!("Sonus \u{2014} {}, muted", o.name),
            Some(o) => format!("Sonus \u{2014} {} {:.0}%", o.name, o.volume * 100.0),
            None => "Sonus \u{2014} no output device".into(),
        };
        if text == self.tooltip {
            return;
        }
        let trimmed: String = text.chars().take(TOOLTIP_MAX).collect();
        if let Err(e) = self.tray.set_tooltip(Some(&trimmed)) {
            lerror!("could not set the tooltip: {e}");
        }
        self.tooltip = text;
    }

    fn rebuild_menu_if_changed(&mut self) {
        let devices: Vec<(String, String, bool)> = self
            .snapshot
            .devices
            .iter()
            .map(|d| (d.id.clone(), d.name.clone(), d.default))
            .collect();
        if devices != self.menu_devices {
            self.rebuild_menu();
        }
    }

    /// Rebuilt rather than mutated: the device list changes length as things
    /// are plugged in and out.
    fn rebuild_menu(&mut self) {
        self.menu_devices = self
            .snapshot
            .devices
            .iter()
            .map(|d| (d.id.clone(), d.name.clone(), d.default))
            .collect();

        let menu = Menu::new();
        let _ = menu.append(&drawn_item(ID_PANEL, "Show mixer", Glyph::Faders));
        let _ = menu.append(&PredefinedMenuItem::separator());

        // The output device, here as well as in the panel: switching to the
        // headset is the one thing people do without wanting to see a slider.
        let output = Submenu::new("Output", !self.snapshot.devices.is_empty());
        set_submenu_icon(&output, art::drawn(Glyph::Speaker(2)));
        for d in &self.snapshot.devices {
            let _ = output.append(&CheckMenuItem::with_id(
                format!("{ID_DEVICE}{}", d.id),
                &d.name,
                true,
                d.default,
                None,
            ));
        }
        let _ = menu.append(&output);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&settings_menu());
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&lucide_item(ID_QUIT, "Quit Sonus", "power", shortcut('q')));

        self.tray.set_menu(Some(Box::new(menu)));
    }

    /// On failure we report what the registry actually says rather than what
    /// was asked for, so the checkbox cannot lie.
    fn toggle_autostart(&mut self) {
        let target = !autostart::is_enabled();
        match autostart::set(target) {
            Ok(()) => linfo!("run at login: {target}"),
            Err(e) => lerror!("could not change autostart: {e}"),
        }
        self.rebuild_menu();
    }
}

fn filtered(mut s: Snapshot, cfg: &Config) -> Snapshot {
    if !cfg.show_system_sounds {
        s.apps.retain(|a| !a.system);
    }
    s
}

fn log_snapshot(s: &Snapshot) {
    match &s.output {
        Some(o) => linfo!(
            "output: {} at {:.0}%{}; {} device(s), {} app(s)",
            o.name,
            o.volume * 100.0,
            if o.muted { ", muted" } else { "" },
            s.devices.len(),
            s.apps.len()
        ),
        None => linfo!("no output device"),
    }
    if let Some(p) = &s.problem {
        linfo!("audio: {p:?}");
    }
}

/// Where the tray icon is, for a panel opened from somewhere else. Windows can
/// say; on macOS the panel uses the pointer, which is at the menu bar anyway
/// when the icon was clicked.
fn tray_anchor(tray: &TrayIcon) -> Option<Anchor> {
    if !cfg!(windows) {
        return None;
    }
    tray.rect().map(|r| Anchor {
        x: r.position.x,
        y: r.position.y,
        w: r.size.width as f64,
        h: r.size.height as f64,
    })
}

fn lucide_item(id: &str, text: &str, glyph: &str, accel: Option<Accelerator>) -> IconMenuItem {
    let item = IconMenuItem::with_id(id, text, true, None, accel);
    set_item_icon(&item, art::glyph(glyph));
    item
}

fn drawn_item(id: &str, text: &str, g: Glyph) -> IconMenuItem {
    let item = IconMenuItem::with_id(id, text, true, None, None);
    set_item_icon(&item, art::drawn(g));
    item
}

fn set_item_icon(item: &IconMenuItem, icon: Option<tray_icon::menu::Icon>) {
    #[cfg(target_os = "macos")]
    item.set_icon_templated(icon);
    #[cfg(windows)]
    item.set_icon(icon);
}

fn set_submenu_icon(sub: &Submenu, icon: Option<tray_icon::menu::Icon>) {
    #[cfg(target_os = "macos")]
    sub.set_icon_templated(icon);
    #[cfg(windows)]
    sub.set_icon(icon);
}

/// Things set once and left alone, the way the rest of the family lays them
/// out.
fn settings_menu() -> Submenu {
    let sub = Submenu::new("Settings", true);
    set_submenu_icon(&sub, art::glyph("settings"));
    let _ = sub.append(&CheckMenuItem::with_id(
        ID_AUTOSTART,
        "Run at login",
        true,
        autostart::is_enabled(),
        None,
    ));
    let _ = sub.append(&PredefinedMenuItem::separator());
    let _ = sub.append(&lucide_item(
        ID_CONFIG,
        "Edit settings file\u{2026}",
        "file-cog",
        shortcut(','),
    ));
    let _ = sub.append(&lucide_item(ID_LOG, "Open log", "scroll-text", None));
    let _ = sub.append(&PredefinedMenuItem::separator());
    let _ = sub.append(&MenuItem::with_id(
        ID_ROW,
        concat!("Sonus ", env!("CARGO_PKG_VERSION")),
        false,
        None,
    ));
    sub
}

/// ⌘ and a key, on macOS. Windows tray menus can't take keyboard shortcuts.
fn shortcut(key: char) -> Option<Accelerator> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    use tray_icon::menu::accelerator::{Code, Modifiers};
    let code = match key {
        'q' => Code::KeyQ,
        ',' => Code::Comma,
        _ => return None,
    };
    Some(Accelerator::new(Modifiers::META, code))
}

/// Open config.json in a text editor, creating it first if need be. The idle
/// tick notices the save and picks it up.
fn open_config() {
    let Some(path) = Config::path() else { return };
    if !path.exists() {
        if let Err(e) = Config::load().save() {
            lerror!("could not write {}: {e}", path.display());
            return;
        }
    }
    // Notepad / TextEdit rather than the .json association, which on many
    // machines is a browser or nothing at all.
    #[cfg(windows)]
    let spawned = std::process::Command::new("notepad.exe").arg(&path).spawn();
    #[cfg(target_os = "macos")]
    let spawned = std::process::Command::new("/usr/bin/open")
        .arg("-t")
        .arg(&path)
        .spawn();
    if let Err(e) = spawned {
        lerror!("could not open {}: {e}", path.display());
    }
}

fn forward_menu_events(proxy: &EventLoopProxy<UserEvent>) {
    let proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        let id = e.id.0.as_str();
        let event = match id {
            ID_PANEL => UserEvent::ShowPanel,
            ID_AUTOSTART => UserEvent::ToggleAutostart,
            ID_CONFIG => UserEvent::OpenConfig,
            ID_LOG => UserEvent::OpenLog,
            ID_QUIT => UserEvent::Quit,
            _ => match id.strip_prefix(ID_DEVICE) {
                Some(device) => UserEvent::Device(device.to_string()),
                None => return,
            },
        };
        let _ = proxy.send_event(event);
    }));
}

fn forward_tray_events(proxy: &EventLoopProxy<UserEvent>) {
    let proxy = proxy.clone();
    TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
        // `button_state` is the important half of this pattern. `Click` fires
        // for both press and release, so matching on the button alone toggles
        // twice per click: the panel would appear on the way down and vanish
        // on the way up.
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            rect,
            ..
        } = e
        {
            let anchor = cfg!(windows).then(|| Anchor {
                x: rect.position.x,
                y: rect.position.y,
                w: rect.size.width as f64,
                h: rect.size.height as f64,
            });
            let _ = proxy.send_event(UserEvent::TogglePanel(anchor));
        }
    }));
}
