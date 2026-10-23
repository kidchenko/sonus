// No console window: this is a tray app. No-op off Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

//! Sonus ("sound") is a volume slider for every app, in the tray, on Windows
//! and macOS.
//!
//! | module            | responsibility                                        |
//! |-------------------|-------------------------------------------------------|
//! | `audio`           | the output device, its apps, and their levels         |
//! | `panel`           | the mixer: layout, sliders, and painting it           |
//! | `app`             | the tray icon and its menu, and wiring the two above  |
//! | `icon`            | draws the faders, the app icon and the glyphs         |
//! | `art`             | the menu's glyphs                                     |
//! | `program_icon`    | an app's own icon, for its row                        |
//! | `config`          | the persisted settings and remembered levels          |
//! | `single_instance` | one copy at a time; relaunching opens the mixer       |
//! | `autostart`       | run at login                                          |

mod app;
mod art;
mod audio;
mod autostart;
mod config;
mod icon;
mod log;
mod panel;
#[cfg(windows)]
mod program_icon;
mod single_instance;
mod theme;
mod util;

use app::{App, UserEvent};
use log::{lerror, linfo};
use tao::event_loop::EventLoopBuilder;

const USAGE: &str = "\
Sonus: a volume slider for every app.

usage:
  sonus                    run in the tray (or open the running copy's mixer)
  sonus list               print the output device and every app's level
  sonus set <app> <0-100>  set an app's volume (Windows); <app> is part of
                           its name, or \"output\" for the device
  sonus mute <app>         mute an app, or the output
  sonus unmute <app>       unmute it";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("--show-panel") => tray(!args.is_empty()),
        // Every tray face and app icon size, as raw RGBA for
        // assets/make_icons.py.
        Some("--dump-icons") => {
            let dir = args.get(1).map(String::as_str).unwrap_or("target/icons");
            std::process::exit(match icon::dump(dir) {
                Ok(()) => 0,
                Err(_) => 1,
            });
        }
        // Holds the panel on screen with sample content, so its rendering can
        // be looked at rather than assumed.
        Some("--preview-panel") => {
            let dark = args.get(1).map(|t| t != "light").unwrap_or(true);
            panel::preview(dark, 20);
        }
        // Renders the panel to raw RGBA without ever putting it on screen, for
        // the landing page and for looking at a change.
        Some("--render-panel") => {
            let path = args.get(1).cloned().unwrap_or("target/panel.rgba".into());
            let dark = flag(&args, "--theme").map(|t| t != "light").unwrap_or(true);
            let scale = flag(&args, "--scale")
                .and_then(|s| s.parse().ok())
                .unwrap_or(2);
            let devices = args.iter().any(|a| a == "--devices");
            render_panel(&path, dark, scale, devices);
        }
        Some(_) => {
            util::attach_console();
            std::process::exit(cli(&args));
        }
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1).cloned()
}

fn cli(args: &[String]) -> i32 {
    match args[0].as_str() {
        "list" | "ls" => {
            let mut mixer = match audio::Mixer::new() {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("sonus: {e}");
                    return 1;
                }
            };
            let snap = mixer.snapshot();
            match &snap.output {
                Some(o) => println!(
                    "{:<32} {:>4}%{}",
                    o.name,
                    (o.volume * 100.0).round(),
                    if o.muted { "  muted" } else { "" }
                ),
                None => println!("No output device."),
            }
            for a in &snap.apps {
                println!(
                    "  {:<30} {:>4}%{}{}",
                    a.name,
                    (a.volume * 100.0).round(),
                    if a.muted { "  muted" } else { "" },
                    if a.active { "  playing" } else { "" }
                );
            }
            if snap.apps.is_empty() {
                println!("  (no apps have sound open)");
            }
            0
        }
        "set" | "mute" | "unmute" => set(args),
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            0
        }
        other => {
            eprintln!("sonus: unknown command `{other}`\n\n{USAGE}");
            2
        }
    }
}

/// `sonus set spotify 30`, `sonus mute chrome`, `sonus set output 50`.
///
/// Windows keeps an app's level itself, so a level set here outlasts the
/// command. On macOS an app's level lives in the running Sonus's tap, so a
/// command that exits would take it with it: there, it says so instead.
fn set(args: &[String]) -> i32 {
    let verb = args[0].as_str();
    let Some(target) = args.get(1).map(|a| a.to_lowercase()) else {
        eprintln!(
            "sonus: {verb} needs an app, like `sonus {verb} spotify`

{USAGE}"
        );
        return 2;
    };
    let level = match verb {
        "set" => match args
            .get(2)
            .and_then(|v| v.trim_end_matches('%').parse::<f32>().ok())
        {
            Some(v) if (0.0..=100.0).contains(&v) => Some(v / 100.0),
            _ => {
                eprintln!("sonus: set needs a level from 0 to 100, like `sonus set {target} 30`");
                return 2;
            }
        },
        _ => None,
    };
    let mute = match verb {
        "mute" => Some(true),
        "unmute" => Some(false),
        _ => None,
    };
    let mut mixer = match audio::Mixer::new() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("sonus: {e}");
            return 1;
        }
    };
    let snap = mixer.snapshot();
    if target == "output" || target == "device" {
        if let Some(v) = level {
            mixer.set_volume(v);
        }
        if let Some(m) = mute {
            mixer.set_muted(m);
        }
        return 0;
    }
    if audio::Mixer::needs_memory() {
        eprintln!("sonus: on macOS, app volumes are held by the menu bar app; use its panel");
        return 1;
    }
    let found: Vec<_> = snap
        .apps
        .iter()
        .filter(|a| a.name.to_lowercase().contains(&target) || a.key.contains(&target))
        .collect();
    if found.is_empty() {
        eprintln!("sonus: no app matching `{target}` has sound open (see `sonus list`)");
        return 1;
    }
    for a in found {
        if let Some(v) = level {
            mixer.set_app_volume(&a.key, v);
        }
        if let Some(m) = mute {
            mixer.set_app_muted(&a.key, m);
        }
        println!("{}: done", a.name);
    }
    0
}

/// Raw RGBA plus a sidecar with the dimensions, so this costs no PNG encoder;
/// `assets/make_icons.py --panel` turns it into an image.
fn render_panel(path: &str, dark: bool, scale: u32, devices: bool) {
    let Some((w, h, rgba)) = panel::render_to_rgba(panel::sample(dark, devices), scale) else {
        eprintln!("render failed");
        std::process::exit(1);
    };
    if let Some(dir) = std::path::Path::new(path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, &rgba);
    let _ = std::fs::write(format!("{path}.size"), format!("{w} {h}"));
}

fn tray(show_panel: bool) {
    log::init();
    linfo!(
        "--- Sonus {} starting (pid {}) ---",
        env!("CARGO_PKG_VERSION"),
        std::process::id()
    );

    // If we are the second launch, the first has been asked to open its mixer
    // and there is nothing left for us to do.
    let Some(signal) = single_instance::acquire() else {
        linfo!("another instance owns the tray; asked it to open, exiting");
        return;
    };

    #[allow(unused_mut)]
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    // Menu-bar agent, no Dock icon. `LSUIElement` in Info.plist sets this before
    // launch, but tao resets the activation policy to Regular as it starts, so
    // it must be asked for again here.
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
    }
    let proxy = event_loop.create_proxy();

    let summon = proxy.clone();
    signal.watch(move || {
        let _ = summon.send_event(UserEvent::ShowPanel);
    });

    let mut app = match App::new(proxy) {
        Ok(app) => app,
        Err(e) => {
            lerror!("{e}");
            return;
        }
    };
    // The installer's "Launch Sonus" passes this, so a fresh install shows the
    // thing it just installed.
    if show_panel {
        app.show_panel();
    }
    linfo!("tray icon created");

    event_loop.run(move |event, _target, control_flow| {
        *control_flow = app.handle(event);
    });
}
