//! Run-at-login.
//!
//! On macOS this is more than convenience: an app's volume is only held while
//! Sonus is running to hold it, so a Sonus that did not start at login is every
//! app back at full volume.
//!
//! On Windows this is the per-user Run key; on macOS a LaunchAgent plist; on
//! Linux a `.desktop` file in `~/.config/autostart`, which every desktop that
//! follows freedesktop.org starts at login. All three are deliberate for the
//! same reasons: no elevation, trivially inspectable (Task Manager's Startup
//! tab / `~/Library/LaunchAgents` / the desktop's startup settings), and
//! uninstalling leaves one stale entry rather than something nobody can find.

pub use imp::{is_enabled, set};

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------
#[cfg(windows)]
mod imp {
    use crate::util::{exe_path_quoted, pcwstr, wide};
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
        HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ,
    };

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE_NAME: &str = "Sonus";

    /// Closes the key on drop, so early returns cannot leak a handle.
    struct Key(HKEY);

    impl Drop for Key {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                unsafe {
                    let _ = RegCloseKey(self.0);
                }
            }
        }
    }

    fn open(access: windows::Win32::System::Registry::REG_SAM_FLAGS) -> Option<Key> {
        let sub = wide(RUN_KEY);
        let mut hkey = HKEY::default();
        let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, pcwstr(&sub), None, access, &mut hkey) };
        (rc == ERROR_SUCCESS).then_some(Key(hkey))
    }

    /// Is the Run value present at all? We deliberately do not compare it against
    /// the current executable path: if the user moved the binary we would rather
    /// report "on" and let `set(true)` rewrite the path than silently show
    /// "off".
    pub fn is_enabled() -> bool {
        let Some(key) = open(KEY_QUERY_VALUE) else {
            return false;
        };
        let name = wide(VALUE_NAME);
        let rc = unsafe { RegQueryValueExW(key.0, pcwstr(&name), None, None, None, None) };
        rc == ERROR_SUCCESS
    }

    pub fn set(enabled: bool) -> Result<(), String> {
        let Some(key) = open(KEY_SET_VALUE) else {
            return Err("could not open the HKCU Run key".into());
        };
        let name = wide(VALUE_NAME);

        if !enabled {
            let rc = unsafe { RegDeleteValueW(key.0, pcwstr(&name)) };
            // Deleting something already absent is the desired end state.
            return if rc == ERROR_SUCCESS || rc.0 == 2 {
                Ok(())
            } else {
                Err(format!(
                    "could not remove the autostart entry (error {})",
                    rc.0
                ))
            };
        }

        let Some(cmd) = exe_path_quoted() else {
            return Err("could not resolve the executable path".into());
        };
        let value = wide(&cmd);
        // REG_SZ length is counted in bytes and must include the terminator.
        let bytes =
            unsafe { std::slice::from_raw_parts(value.as_ptr() as *const u8, value.len() * 2) };
        let rc = unsafe { RegSetValueExW(key.0, pcwstr(&name), None, REG_SZ, Some(bytes)) };
        if rc == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!(
                "could not write the autostart entry (error {})",
                rc.0
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// macOS
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
mod imp {
    use std::path::PathBuf;

    const LABEL: &str = "com.kidchenko.sonus";

    fn plist_path() -> Option<PathBuf> {
        std::env::var_os("HOME").map(|h| {
            PathBuf::from(h)
                .join("Library/LaunchAgents")
                .join(format!("{LABEL}.plist"))
        })
    }

    /// Presence of the plist is the switch. As on Windows, we do not verify the
    /// path inside it points at this binary: report "on" and let `set(true)`
    /// rewrite it rather than silently showing "off" after a move.
    pub fn is_enabled() -> bool {
        plist_path().map(|p| p.exists()).unwrap_or(false)
    }

    pub fn set(enabled: bool) -> Result<(), String> {
        let path = plist_path().ok_or("no HOME in environment")?;

        if !enabled {
            return match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                // Already gone is the desired end state.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(format!("could not remove the LaunchAgent: {e}")),
            };
        }

        let exe = std::env::current_exe()
            .map_err(|e| format!("could not resolve the executable path: {e}"))?;
        let exe = exe.to_string_lossy();

        // Deliberately not `launchctl load`ed here: RunAtLoad would immediately
        // spawn a second copy on top of the one writing this file. launchd picks
        // the agent up at the next login, which is exactly when it is wanted.
        let plist = plist_xml(&exe);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("could not create LaunchAgents dir: {e}"))?;
        }
        std::fs::write(&path, plist).map_err(|e| format!("could not write the LaunchAgent: {e}"))
    }

    fn plist_xml(exe: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>LimitLoadToSessionType</key>
    <string>Aqua</string>
</dict>
</plist>
"#,
            exe = xml_escape(exe)
        )
    }

    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }
}

// ---------------------------------------------------------------------------
// Linux
// ---------------------------------------------------------------------------
#[cfg(target_os = "linux")]
mod imp {
    use std::path::PathBuf;

    fn entry_path() -> Option<PathBuf> {
        crate::config::Config::dir()
            .and_then(|d| d.parent().map(|p| p.join("autostart/sonus.desktop")))
    }

    /// Presence of the file is the switch, as on the other two.
    pub fn is_enabled() -> bool {
        entry_path().map(|p| p.exists()).unwrap_or(false)
    }

    pub fn set(enabled: bool) -> Result<(), String> {
        let path = entry_path().ok_or("no HOME in environment")?;

        if !enabled {
            return match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(format!("could not remove the autostart entry: {e}")),
            };
        }

        // An AppImage runs from a mount that moves every launch; the file
        // itself is in $APPIMAGE.
        let exe = match std::env::var_os("APPIMAGE") {
            Some(a) => PathBuf::from(a),
            None => std::env::current_exe()
                .map_err(|e| format!("could not resolve the executable path: {e}"))?,
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("could not create the autostart dir: {e}"))?;
        }
        std::fs::write(&path, desktop_entry(&exe.to_string_lossy()))
            .map_err(|e| format!("could not write the autostart entry: {e}"))
    }

    fn desktop_entry(exe: &str) -> String {
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Sonus\n\
             Comment=A volume slider for every app\n\
             Exec={}\n\
             Icon=sonus\n\
             Terminal=false\n\
             X-GNOME-Autostart-enabled=true\n",
            exec_quote(exe)
        )
    }

    /// The Exec key's quoting: double quotes around anything with a space or
    /// a reserved character, and backslash before `"`, `` ` ``, `$` and `\`.
    fn exec_quote(s: &str) -> String {
        if !s.contains(|c: char| " \t\"'\\><~|&;$*?#()`".contains(c)) {
            return s.to_string();
        }
        let mut out = String::from("\"");
        for c in s.chars() {
            if matches!(c, '"' | '`' | '$' | '\\') {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
        // The file format escapes backslashes once more, on top of Exec's own.
        out.replace('\\', "\\\\")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn exec_paths_are_quoted_only_when_needed() {
            assert_eq!(exec_quote("/usr/bin/sonus"), "/usr/bin/sonus");
            assert_eq!(
                exec_quote("/home/me/My Apps/Sonus.AppImage"),
                "\"/home/me/My Apps/Sonus.AppImage\""
            );
            assert_eq!(exec_quote("/opt/$x/sonus"), "\"/opt/\\\\$x/sonus\"");
        }
    }
}
