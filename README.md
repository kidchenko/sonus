<div align="center">

<img src="assets/icon-256.png" width="112" height="112" alt="Sonus">

# Sonus

A volume slider for every app, one click from the tray.<br>
On Windows and macOS.

<sub><i>sonus</i>: Latin for <i>sound</i></sub>

**[josebarbosa.dev/sonus](https://josebarbosa.dev/sonus/)**

</div>

The call is too quiet and the music is too loud. macOS still has no per-app
volume at all, and Windows keeps its Volume Mixer two clicks deep in
Settings. Sonus is a waveform in the tray: click it, and every app playing
sound has its own slider, live level meter and mute button, with the output
device at the top.

It's a single 0.7 MB binary with no runtime dependencies. Asleep in the tray it
uses a few MB of memory and no measurable CPU.

<p align="center">
<img src="docs/img/panel-dark.png" width="360" alt="The Sonus mixer, dark">
<img src="docs/img/panel-light.png" width="360" alt="The Sonus mixer, light">
</p>

## The panel

Left-click the waveform to open it; click anywhere else, or press Esc, to close
it.

| | |
|---|---|
| **The output device** | The top row: its volume, and its name. Click the name to switch to another device: headphones, speakers, a monitor. |
| **Apps** | One row per app playing sound, with the app's own icon. An app that plays from several processes (Chrome, Teams, Discord) is still one row. On Windows, apps holding a session open without playing are listed too, dimmed. |
| **The slider** | Click anywhere on the track to jump there, or drag. The brighter band inside the fill is the app's level right now. |
| **The wheel** | Over any row, nudges it by 2% a notch (`wheelStep`). |
| **Mute** | The speaker at the end of each row, or click the app's icon. Dragging a muted slider unmutes it. |

On Windows the panel is drawn the way Windows 11's own flyouts are, Fluent
sliders in your accent colour, and follows the taskbar's light or dark theme. On
macOS it sits on the same frosted material as the menu bar's menus.

## The menu

Right-click the waveform:

```
      Show mixer
      ───────────────────────────
      Output                     ▸   ✓ Speakers (Realtek(R) Audio)
      ───────────────────────────      Headphones (WH-1000XM5)
      Settings                   ▸
      ───────────────────────────
  ⏻   Quit Sonus                ⌘Q
```

**Output** switches the device without opening the panel. **Settings** holds
**Run at login**, **Edit settings file…** and **Open log**.

Running `sonus` again while it's in the tray opens its panel, so a Start menu
or Spotlight search for Sonus is a way to the sliders too.

## From a terminal

Windows:

```
sonus list               # the output device and every app's level
sonus set spotify 30     # set an app's volume; any part of its name works
sonus set output 50      # the device
sonus mute chrome        # or unmute
```

On macOS an app's level lives in the running Sonus (see below), so `set` there
only works on `output`.

## Install

### Windows

Download [**Sonus-Setup.exe**](https://github.com/kidchenko/sonus/releases/latest/download/Sonus-Setup.exe).
Each release also ships a portable `sonus-x.y.z-x64.exe`. winget and
Chocolatey packages are on the way.

The binary isn't code-signed, so SmartScreen warns on first run: click **More
info → Run anyway**.

### macOS

```bash
brew install --cask kidchenko/tap/sonus
```

Or download [**Sonus.dmg**](https://github.com/kidchenko/sonus/releases/latest/download/Sonus.dmg)
(Apple silicon and Intel) and drag Sonus into **Applications**. The build isn't
notarized: the Homebrew cask clears Gatekeeper for you, or run
`xattr -dr com.apple.quarantine /Applications/Sonus.app` once.

The first time you change an app's volume, macOS asks to let Sonus record
system audio: that's how per-app volume works there (below). If you said no,
the panel says so and has a button to the right page of System Settings.

### Requirements

Windows 10 or later. macOS 14.2 or later for app volumes; on older versions
Sonus still switches the output device and sets its volume.

## How it works

| | Apps and their levels | The device |
|---|---|---|
| Windows | WASAPI audio sessions: `IAudioSessionManager2`, `ISimpleAudioVolume`, `IAudioMeterInformation` | `IAudioEndpointVolume`; switching it uses `IPolicyConfig`, as Sound settings does |
| macOS | Core Audio process taps, and an aggregate device per app | Core Audio's device properties |

**Windows** has had per-app volume since Vista: every app that plays sound opens
an audio session, and every session has its own volume and meter. Sonus is a
quicker way to the same controls Windows' Volume Mixer drives, which is also why
a level set in Sonus is still there after the app, or Windows, restarts.

**macOS** has nothing of the kind, so Sonus builds it. When you change an app's
level, Sonus makes a *process tap* on it, which captures what the app plays and
silences the original, and plays that sound back to your output device at the
level you chose. An app you never touch plays straight to the device as usual.
The taps go when Sonus does, so if it quits, an app returns to full volume, never
to silence; and since the taps hold the levels, Sonus remembers them in its
config and puts them back when the app next plays (`remember`). Nothing it
captures is recorded or leaves your computer.

The panel is painted with Direct2D and DirectWrite on Windows and AppKit on
macOS, from one layout shared by both, rather than a WebView: the app stays
under a megabyte and opens instantly.

## Configuration

`%APPDATA%\Sonus\config.json` on Windows,
`~/Library/Application Support/Sonus/config.json` on macOS. Sonus picks up a
saved edit within a couple of seconds.

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
| `wheelStep` | Percent per notch of the mouse wheel, 1 to 25. |
| `showSystemSounds` | Windows: list the "System sounds" session, the one that plays the chimes. |
| `remember` | macOS: put apps back at the level you left them. |
| `levels` | macOS: what is remembered, by bundle id. Written by Sonus. |

The log is `sonus.log` in the same folder. Set `SONUS_LOG=debug` for every
session found and every level set.

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

`sonus --render-panel target/panel.rgba --theme light --scale 2` draws the panel
offscreen with sample content, and `python assets/panel_png.py` turns that into
a PNG: that's where the pictures above come from, with the real icons of
whichever sample apps (Spotify, Chrome or Brave, Slack, Discord, Teams) this
machine has. `sonus --dump-app-icons docs/img/apps` exports those icons for the
landing page's demo, and `sonus --preview-panel` holds the panel on screen to
play with.

## Releasing

1. Bump `version` in `Cargo.toml` and add a section to `CHANGELOG.md`.
2. `git tag vX.Y.Z && git push origin vX.Y.Z`

The `release` workflow builds the DMG and the installer, publishes them to a
GitHub Release, and fills the new version and checksums into the Chocolatey,
winget and Homebrew manifests. Chocolatey is pushed when the `CHOCO_API_KEY`
secret is set; submit the `manifest-winget` artifact to
[microsoft/winget-pkgs](https://github.com/microsoft/winget-pkgs). The cask in
[kidchenko/homebrew-tap](https://github.com/kidchenko/homebrew-tap) is bumped
by the tap's `bump` workflow: straight away when the `HOMEBREW_TAP_TOKEN`
secret is set, or by its daily run.

## Not yet

- A different output device per app (music on the speakers, the call on the
  headset).
- Boost above 100%.
- Turning music down automatically during a call.
- A hotkey to mute the app in front.
- On macOS, the device's own level meter, and meters for apps you haven't
  touched: both would mean tapping everything.

## License

MIT. The menu's general glyphs are from [Lucide](https://lucide.dev) (ISC); see
[assets/icons/LICENSES.md](assets/icons/LICENSES.md).
