; Inno Setup script for Sonus.
;   iscc /DAppVersion=x.y.z packaging\inno\sonus.iss
; Expects target\release\sonus.exe to already be built (build.ps1 does both).

#define AppName      "Sonus"
#ifndef AppVersion
  #define AppVersion "0.0.1"
#endif
#define AppPublisher "kidchenko"
#define AppURL       "https://github.com/kidchenko/sonus"
#define AppExe       "sonus.exe"
; Data folder and Run value name, without the space. Must match Config::dir
; and VALUE_NAME in src/autostart.rs.
#define AppKey       "Sonus"

[Setup]
; Never reuse this GUID for another product: it is how Windows matches
; upgrades and uninstalls, and winget's ProductCode is derived from it.
AppId={{1829A447-FE0C-405E-948B-D5B7180788B3}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppURL}
AppSupportURL={#AppURL}/issues
AppUpdatesURL={#AppURL}/releases
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
DisableDirPage=auto
LicenseFile=..\..\LICENSE
OutputDir=..\..\dist
OutputBaseFilename=Sonus-Setup-{#AppVersion}
SetupIconFile=..\..\assets\icon.ico
UninstallDisplayIcon={app}\{#AppExe}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; The app itself never needs elevation; only Program Files does.
PrivilegesRequired=admin
; Close a running instance on upgrade/uninstall instead of hitting a locked exe.
CloseApplications=yes
CloseApplicationsFilter=*.exe
; [Tasks], [UninstallRun] and [Code] touch HKCU and the user's profile on
; purpose; see the notes there.
UsedUserAreasWarning=no
MinVersion=10.0

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
; Ticked by default: a mixer that is not running after a reboot is a mixer you
; have to go and start before you can use it. Same HKCU value the app's own
; "Run at login" toggle uses.
Name: "autostart"; Description: "Start {#AppName} when I sign in";   GroupDescription: "Startup:"

[Files]
Source: "..\..\target\release\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{group}\Uninstall {#AppName}"; Filename: "{uninstallexe}"

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run";   ValueType: string; ValueName: "{#AppKey}"; ValueData: """{app}\{#AppExe}""";   Tasks: autostart; Flags: uninsdeletevalue

[Run]
; As the signed-in user, not the elevated installer, so it sees that user's
; audio sessions and writes that user's settings. --show-panel opens the mixer
; once it is up, so a fresh install shows what it just installed.
Filename: "{app}\{#AppExe}"; Parameters: "--show-panel"; Description: "Launch {#AppName}";   Flags: nowait postinstall skipifsilent runasoriginaluser

[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/f /im {#AppExe}";   Flags: runhidden; RunOnceId: "KillSonus"

[UninstallDelete]
Type: files; Name: "{userappdata}\{#AppKey}\sonus.log"
Type: files; Name: "{userappdata}\{#AppKey}\sonus.log.1"

[Code]
// The app's own "Run at login" toggle writes the same HKCU value; clear it
// even if the installer task was never ticked. Under UAC an admin elevating
// their own account keeps the same HKCU, which is the normal case.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
  begin
    RegDeleteValue(HKEY_CURRENT_USER,
      'Software\Microsoft\Windows\CurrentVersion\Run', '{#AppKey}');
    // Only succeeds once config.json is gone: settings survive a reinstall.
    RemoveDir(ExpandConstant('{userappdata}\{#AppKey}'));
    RemoveDir(ExpandConstant('{app}'));
  end;
end;
