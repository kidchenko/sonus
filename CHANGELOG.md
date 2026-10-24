# Changelog

## [Unreleased]

- Linux: the tray app on any desktop with PipeWire or PulseAudio, with app
  volumes, meters, the output device, and play / pause / skip with the cover
  for every player that offers MPRIS. The panel follows the desktop's light
  or dark style, accent colour, font and icons. Packaged as a .deb (Ubuntu,
  Debian, Mint), an .rpm (Fedora, openSUSE) and a .tar.gz for the rest.
- A player under each app that's playing: the cover, the track, previous /
  play / next, and a progress bar to click and jump. Windows: every app in
  the system's media controls. macOS: Spotify and Music, by AppleScript,
  asking for the Automation permission the first time.
- macOS: the panel says when app volumes are waiting on the System Audio
  Recording permission, rather than only once it is refused, and the log
  warns when Sonus runs outside Sonus.app, where that prompt may never show.
- Check for updates… in the menu opens Sonus's page, which says whether a
  newer version is out. Sonus itself still never goes online.

## [0.0.1] - 2026-10-23

First release.

- Tray app for Windows and macOS: a panel with a slider for the output device
  and one for every app playing through it, each with the app's own icon, a
  live level meter and a mute button. Click the track to jump, drag, scroll
  the wheel over a row, or click an app's icon to mute it.
- Apps that play from several processes (Chrome, Teams, Discord) are one row.
- Switch the output device from the top of the panel or from the menu.
- Windows: WASAPI audio sessions, so levels are the ones Windows' own Volume
  Mixer shows and keeps. Painted with Direct2D and DirectWrite, Fluent
  sliders in the accent colour, following the taskbar's theme.
- macOS 14.2+: per-app volume built on Core Audio process taps, with levels
  remembered and put back when an app next plays. Asks for the System Audio
  Recording permission when first needed, and says so in the panel if it was
  refused. Frosted-glass panel in the menu-bar style.
- `sonus list`, `sonus set <app> <0-100>`, `sonus mute` / `unmute` from a
  terminal (Windows).
- Running Sonus again opens the running copy's panel. Run at login.
- Windows installer (Inno Setup), Chocolatey and winget manifests, a universal
  macOS DMG, and a Homebrew cask.
