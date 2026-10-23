<div align="center">

<img src="assets/icon-256.png" width="112" height="112" alt="Sonus">

# Sonus

A volume slider for every app, in the tray.<br>
For Windows and macOS.

<sub><i>sonus</i>: Latin for <i>sound</i></sub>

**[josebarbosa.dev/sonus](https://josebarbosa.dev/sonus/)**

</div>

The call is too quiet and the music is too loud. macOS has no per-app volume,
and on Windows the Volume Mixer is two clicks deep in Settings. Click Sonus in
the tray and you get a slider, a level meter and a mute button for every app
playing sound, with your output device at the top.

It's one 0.7 MB file with nothing else to install. Idle in the tray, it uses a
few MB of memory and almost no CPU.

<p align="center">
<img src="docs/img/panel-dark.png" width="360" alt="The Sonus mixer, dark">
<img src="docs/img/panel-light.png" width="360" alt="The Sonus mixer, light">
</p>

## The panel

Left-click the icon to open it. Click anywhere else or press Esc to close it.

- The top row is your output device. Click its name to switch to headphones,
  speakers or a monitor.
- Below it, each app playing sound gets a row with its own icon. Apps that
  play from several processes, like Chrome or Teams, still get one row. On
  Windows, apps that have sound open but are silent show up dimmed.
- Click the track to jump to a level, or drag. The lighter band inside the
  slider shows how loud the app is right now.
- Scroll over a row to change it by 2% per notch.
- Click the speaker at the end of a row, or the app's icon, to mute it.
  Dragging a muted slider unmutes it.

On Windows the panel looks like Windows 11's own flyouts and uses your accent
colour and the taskbar's light or dark theme. On macOS it uses the same
frosted background as the menu bar's menus.

## The menu

Right-click the icon:

```
      Show mixer
      ───────────────────────────
      Output                     ▸   ✓ Speakers (Realtek(R) Audio)
      ───────────────────────────      Headphones (WH-1000XM5)
      Settings                   ▸
      ───────────────────────────
  ⏻   Quit Sonus                ⌘Q
```

Output switches the device without opening the panel. Settings has Run at
login, Edit settings file… and Open log.

Starting Sonus again while it's running opens the panel, so you can also
reach it from the Start menu or Spotlight.

## From a terminal

On Windows:

```
sonus list               # the output device and every app's level
sonus set spotify 30     # set an app's volume; any part of its name works
sonus set output 50      # the device
sonus mute chrome        # or unmute
```

On macOS, app levels only exist while Sonus is running (see below), so `set`
only works with `output` there.

## Install

### Windows

```powershell
winget install kidchenko.Sonus
# or
choco install sonus
# or
scoop bucket add kidchenko https://github.com/kidchenko/scoop-bucket
scoop install kidchenko/sonus
```

Or grab [**Sonus-Setup.exe**](https://github.com/kidchenko/sonus/releases/latest/download/Sonus-Setup.exe)
directly. Each release also ships a portable `sonus-x.y.z-x64.exe` that runs
without installing; see [Releases](https://github.com/kidchenko/sonus/releases).

The app isn't code-signed yet, so SmartScreen warns you the first time you run
it. Click **More info**, then **Run anyway**.

### macOS

```bash
brew install --cask kidchenko/tap/sonus
```

Or download [**Sonus.dmg**](https://github.com/kidchenko/sonus/releases/latest/download/Sonus.dmg)
(Apple silicon and Intel), open it, and drag Sonus into **Applications**. It
lives in the menu bar and doesn't show up in the Dock.

The app isn't notarized yet, so macOS blocks it the first time you open it.
Homebrew takes care of that for you. Otherwise, do one of these once:

- Go to **System Settings > Privacy & Security** and click **Open Anyway**
  next to Sonus.
- Run `xattr -dr com.apple.quarantine /Applications/Sonus.app`

Move it into Applications before you do this. It won't work while the app is
still inside the disk image.

The first time you change an app's volume, macOS asks whether Sonus may record
system audio. Per-app volume on macOS depends on it (see How it works). If you
say no, the panel tells you and links to the right page in System Settings.

### Requirements

Windows 10 or later. App volumes on macOS need 14.2 or later; on older
versions Sonus can still switch the output device and change its volume.

## How it works

| | Apps and their levels | The device |
|---|---|---|
| Windows | WASAPI audio sessions: `IAudioSessionManager2`, `ISimpleAudioVolume`, `IAudioMeterInformation` | `IAudioEndpointVolume`; switching it uses `IPolicyConfig`, as Sound settings does |
| macOS | Core Audio process taps, and an aggregate device per app | Core Audio's device properties |

Windows has had per-app volume since Vista. Each app that plays sound opens an
audio session with its own volume and meter, and Sonus changes the same
settings as Windows' Volume Mixer. That's why a level you set stays after the
app or Windows restarts.

macOS has no per-app volume, so Sonus adds it. When you change an app's level,
Sonus creates a process tap for that app. The tap captures the app's sound and
mutes the original, and Sonus plays the sound to your output device at the
level you picked. Apps you haven't touched play directly, as usual.

The taps close when Sonus quits, so apps go back to full volume. Sonus saves
your levels in its config and restores them the next time each app plays
(the `remember` setting). The captured audio is never recorded and never
leaves your computer.

The panel is drawn with Direct2D and DirectWrite on Windows and AppKit on
macOS, from one layout. There's no WebView, which keeps the app under a
megabyte and quick to open.

## Configuration

`%APPDATA%\Sonus\config.json` on Windows,
`~/Library/Application Support/Sonus/config.json` on macOS. Changes apply a
couple of seconds after you save.

```json
{
  "wheelStep": 2,
  "showSystemSounds": true,
  "remember": true,
  "levels": {
    "com.spotify.client": { "volume": 0.4, "muted": false }
  }
}
```

| | |
|---|---|
| `wheelStep` | Percent per mouse wheel notch, from 1 to 25. |
| `showSystemSounds` | Windows: show the "System sounds" row, which controls notification chimes. |
| `remember` | macOS: restore each app's level when it plays again. |
| `levels` | macOS: the saved levels, by bundle id. Sonus writes these. |

The log is `sonus.log` in the same folder. Set `SONUS_LOG=debug` to log every
session Sonus finds and every level it sets.

## Build

Rust 1.77 or newer.

```powershell
.\build.ps1           # Windows: tests, release exe, installer if Inno Setup is installed
.\build.ps1 -Icons    # ... and regenerate the icons
```

```bash
./build.sh            # macOS: tests, release binary, dist/Sonus.app
./build.sh --dmg      # ... and a disk image
```

The screenshots above come from
`sonus --render-panel target/panel.rgba --theme light --scale 2`, converted
with `python assets/panel_png.py`. They use the real icons of whichever sample
apps are installed (Spotify, Chrome or Brave, Slack, Discord, Teams).
`sonus --dump-app-icons docs/img/apps` saves those icons for the website demo,
and `sonus --preview-panel` opens the panel with sample content.

## Releasing

1. Bump `version` in `Cargo.toml` and add a section to `CHANGELOG.md`.
2. `git tag vX.Y.Z && git push origin vX.Y.Z`

The `release` workflow builds the DMG on macOS and the installer and portable
exe on Windows, puts them on a GitHub Release with checksums, and updates the
Chocolatey and winget manifests with the new version and checksums:

- **Chocolatey** is pushed automatically when the `CHOCO_API_KEY` secret is set.
- **winget**: submit the `manifest-winget` artifact to
  [microsoft/winget-pkgs](https://github.com/microsoft/winget-pkgs), or use
  `wingetcreate submit`.
- **Homebrew**: [kidchenko/homebrew-tap](https://github.com/kidchenko/homebrew-tap)
  bumps `Casks/sonus.rb` itself, right away when the `HOMEBREW_TAP_TOKEN` secret
  is set and otherwise on its daily run.

If you run the workflow by hand from the Actions tab, it's a dry run. It builds
everything and uploads it as artifacts, but doesn't publish anything.

## Not yet

- A different output device per app, like music on the speakers and the call
  on the headset.
- Volume above 100%.
- Lowering music automatically during a call.
- A hotkey to mute the app in front.
- On macOS, a level meter for the device and for apps you haven't changed.
  Both would mean tapping every app.

## License

MIT. The menu's general icons are from [Lucide](https://lucide.dev) (ISC); see
[assets/icons/LICENSES.md](assets/icons/LICENSES.md).
