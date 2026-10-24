//! System light/dark preference, and the accent colour.
//!
//! The panel opens from the tray, so it follows the tray: the Windows taskbar,
//! the macOS menu bar, or the Linux desktop's panel. The accent colour is the user's, as in every slider
//! the OS draws itself.

pub use imp::{accent, taskbar_is_dark};

/// Whether the bar the tray icon sits on is dark, for the icon's colour. The
/// same answer as the panel's everywhere but GNOME, whose top bar stays dark
/// in light mode too.
pub fn tray_is_dark() -> bool {
    let gnome = cfg!(target_os = "linux")
        && std::env::var("XDG_CURRENT_DESKTOP")
            .map(|d| d.split(':').any(|d| d.eq_ignore_ascii_case("gnome")))
            .unwrap_or(false);
    gnome || taskbar_is_dark()
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------
#[cfg(windows)]
mod imp {
    use crate::util::{pcwstr, wide};
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE,
        REG_VALUE_TYPE,
    };

    const PERSONALIZE: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";
    const ACCENT: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\Accent";

    /// True when the *taskbar* is dark.
    ///
    /// Deliberately `SystemUsesLightTheme` and not `AppsUseLightTheme`: the two
    /// are set independently, and everything we draw opens from the
    /// notification area. Windows' own flyouts (volume, network) follow the
    /// taskbar too, so a light-app/dark-taskbar user (a common combination)
    /// gets a panel that matches its neighbours.
    ///
    /// Defaults to dark when the value is missing: that is the Windows 11
    /// default and the safer guess.
    pub fn taskbar_is_dark() -> bool {
        read(PERSONALIZE, "SystemUsesLightTheme", 4)
            .map(|b| b == [0, 0, 0, 0])
            .unwrap_or(true)
    }

    /// The slider colour Windows' own flyouts use: a shade of the accent, not
    /// the accent itself.
    ///
    /// `AccentPalette` is eight RGBA entries, light to dark around the base
    /// colour at index 3. Fluent fills controls with Light 2 on dark surfaces
    /// and Dark 1 on light ones, which is what keeps a pale accent legible on
    /// white and a deep one visible on charcoal. The fallbacks are those two
    /// shades of the default Windows 11 blue.
    pub fn accent(dark: bool) -> (u8, u8, u8) {
        let index = if dark { 1 } else { 4 };
        read(ACCENT, "AccentPalette", 32)
            .map(|b| (b[index * 4], b[index * 4 + 1], b[index * 4 + 2]))
            .unwrap_or(if dark {
                (0x60, 0xcd, 0xff)
            } else {
                (0x00, 0x5f, 0xb8)
            })
    }

    /// A value of exactly `len` bytes, or nothing.
    fn read(subkey: &str, value: &str, len: usize) -> Option<Vec<u8>> {
        let sub = wide(subkey);
        let name = wide(value);
        let mut hkey = HKEY::default();

        unsafe {
            if RegOpenKeyExW(
                HKEY_CURRENT_USER,
                pcwstr(&sub),
                None,
                KEY_QUERY_VALUE,
                &mut hkey,
            ) != ERROR_SUCCESS
            {
                return None;
            }

            let mut data = vec![0u8; len];
            let mut size = len as u32;
            let mut kind = REG_VALUE_TYPE::default();
            let rc = RegQueryValueExW(
                hkey,
                pcwstr(&name),
                None,
                Some(&mut kind),
                Some(data.as_mut_ptr()),
                Some(&mut size),
            );
            let _ = RegCloseKey(hkey);

            (rc == ERROR_SUCCESS && size as usize == len).then_some(data)
        }
    }
}

// ---------------------------------------------------------------------------
// macOS
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
mod imp {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2_app_kit::{NSColor, NSColorSpace};
    use objc2_foundation::{NSString, NSUserDefaults};

    /// True when the menu bar is dark.
    ///
    /// `AppleInterfaceStyle` in the global defaults is the string `"Dark"` in
    /// dark mode and absent entirely in light mode: the same signal the menu
    /// bar follows. Absent therefore means light, which is why this defaults to
    /// `false` where the Windows side defaults to `true`.
    pub fn taskbar_is_dark() -> bool {
        let key = NSString::from_str("AppleInterfaceStyle");
        let defaults = NSUserDefaults::standardUserDefaults();
        defaults
            .stringForKey(&key)
            .map(|s| s.to_string().eq_ignore_ascii_case("dark"))
            .unwrap_or(false)
    }

    /// The user's accent colour, from System Settings › Appearance, as sRGB.
    /// `controlAccentColor` is a dynamic catalogue colour, so it is converted
    /// before its components are read; a colour that will not convert falls
    /// back to the default blue.
    pub fn accent(_dark: bool) -> (u8, u8, u8) {
        let accent = NSColor::controlAccentColor();
        let space = NSColorSpace::sRGBColorSpace();
        let srgb: Option<Retained<NSColor>> =
            unsafe { msg_send![&accent, colorUsingColorSpace: &*space] };
        match srgb {
            Some(c) => {
                let byte = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
                (
                    byte(c.redComponent()),
                    byte(c.greenComponent()),
                    byte(c.blueComponent()),
                )
            }
            None => (0x00, 0x7a, 0xff),
        }
    }
}

// ---------------------------------------------------------------------------
// Linux
// ---------------------------------------------------------------------------
#[cfg(target_os = "linux")]
mod imp {
    use gtk::gio::{self, prelude::*};
    use gtk::glib::Variant;

    /// True when the desktop asks apps for dark.
    ///
    /// Asked of the desktop portal, the one place GNOME, KDE and the others
    /// all answer: `color-scheme` is 1 for dark, 2 for light and 0 for no
    /// preference, which is light.
    pub fn taskbar_is_dark() -> bool {
        portal("color-scheme").and_then(|v| v.get::<u32>()) == Some(1)
    }

    /// The user's accent colour, where the desktop has one (GNOME 47 and
    /// later, KDE): three doubles from 0 to 1. Otherwise GNOME's default
    /// blue, a shade lighter on dark as on the other two.
    pub fn accent(dark: bool) -> (u8, u8, u8) {
        let rgb = portal("accent-color").and_then(|v| v.get::<(f64, f64, f64)>());
        match rgb {
            Some((r, g, b)) if [r, g, b].iter().all(|c| (0.0..=1.0).contains(c)) => {
                let byte = |v: f64| (v * 255.0).round() as u8;
                (byte(r), byte(g), byte(b))
            }
            _ if dark => (0x78, 0xae, 0xed),
            _ => (0x35, 0x84, 0xe4),
        }
    }

    /// One `org.freedesktop.appearance` setting, unwrapped from the variant
    /// the portal wraps it in. `None` with no session bus or no portal, as in
    /// a bare window manager; the callers fall back to light and blue.
    fn portal(key: &str) -> Option<Variant> {
        let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).ok()?;
        let reply = bus
            .call_sync(
                Some("org.freedesktop.portal.Desktop"),
                "/org/freedesktop/portal/desktop",
                "org.freedesktop.portal.Settings",
                "ReadOne",
                Some(&("org.freedesktop.appearance", key).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                200,
                gio::Cancellable::NONE,
            )
            .ok()?;
        let mut v = reply.child_value(0);
        // ReadOne answers v; the older Read answers v inside v.
        while let Some(inner) = v.as_variant() {
            v = inner;
        }
        Some(v)
    }
}
