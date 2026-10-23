<#
.SYNOPSIS
  Builds sonus.exe and, if Inno Setup is installed, the installer.

.EXAMPLE
  .\build.ps1                  # test + release binary + installer
  .\build.ps1 -Run             # ... then launch it
  .\build.ps1 -Icons           # ... then regenerate assets\icon.ico and the PNGs
  .\build.ps1 -Choco           # ... then pack the Chocolatey package (local test)
  .\build.ps1 -SkipTests -SkipInstaller
#>
[CmdletBinding()]
param(
  [switch]$Run,
  [switch]$Icons,
  [switch]$Choco,
  [switch]$SkipTests,
  [switch]$SkipInstaller
)

$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

function Step($m) { Write-Host "`n==> $m" -ForegroundColor Cyan }
function Ok($m)   { Write-Host "    $m" -ForegroundColor Green }
function Warn($m) { Write-Host "    $m" -ForegroundColor Yellow }

function Find($name, $fallbacks) {
  $c = Get-Command $name -ErrorAction SilentlyContinue
  if ($c) { return $c.Source }
  foreach ($f in $fallbacks) { if (Test-Path $f) { return $f } }
  return $null
}

# [package].version specifically: a bare '^version =' would also match a
# dependency table that happens to come first one day.
function Get-PackageVersion {
  $inPackage = $false
  foreach ($line in Get-Content (Join-Path $PSScriptRoot 'Cargo.toml')) {
    if ($line -match '^\s*\[(.+)\]\s*$') { $inPackage = ($Matches[1] -eq 'package'); continue }
    if ($inPackage -and $line -match '^\s*version\s*=\s*"([^"]+)"') { return $Matches[1] }
  }
  throw 'could not read [package].version from Cargo.toml'
}

$cargo = Find 'cargo' @("$env:USERPROFILE\.cargo\bin\cargo.exe")
if (-not $cargo) {
  Write-Host 'cargo not found. Install Rust: winget install Rustlang.Rustup' -ForegroundColor Red
  exit 1
}

# A running instance locks the exe and the link fails with "Access is denied".
$running = Get-Process sonus -ErrorAction SilentlyContinue
if ($running) {
  Warn "stopping $($running.Count) running instance(s) first"
  $running | Stop-Process -Force
  Start-Sleep -Milliseconds 500
}

if (-not $SkipTests) {
  Step 'Running tests'
  & $cargo test --quiet
  if ($LASTEXITCODE -ne 0) { throw "cargo test failed ($LASTEXITCODE)" }
  Ok 'tests passed'
}

Step 'Building release binary'
& $cargo build --release
if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
$exe = Join-Path $PSScriptRoot 'target\release\sonus.exe'
Ok ("sonus.exe   {0:N2} MB" -f ((Get-Item $exe).Length / 1MB))
$version = Get-PackageVersion

if ($Icons) {
  Step 'Regenerating icons'
  $py = Find 'python' @()
  if (-not $py) { throw 'python not found' }
  $dir = Join-Path $PSScriptRoot 'target\icons'
  & $exe --dump-icons $dir
  if ($LASTEXITCODE -ne 0) { throw "--dump-icons failed ($LASTEXITCODE)" }
  & $py (Join-Path $PSScriptRoot 'assets\make_icons.py') $dir
  # The .ico is embedded at compile time, so the exe has to be rebuilt.
  & $cargo build --release
  Ok 'assets\icon.ico, icon-256.png, icon-1024.png'
}

New-Item -ItemType Directory -Force (Join-Path $PSScriptRoot 'dist') | Out-Null
Copy-Item $exe (Join-Path $PSScriptRoot "dist\sonus-$version-x64.exe") -Force
Ok "dist\sonus-$version-x64.exe (sonusble)"

if (-not $SkipInstaller) {
  $iscc = Find 'iscc' @(
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe",
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
  )
  if (-not $iscc) {
    Warn 'Inno Setup not found; skipping the installer.'
    Warn '  winget install JRSoftware.InnoSetup'
  } else {
    Step 'Building installer'
    & $iscc /Q "/DAppVersion=$version" (Join-Path $PSScriptRoot 'packaging\inno\sonus.iss')
    if ($LASTEXITCODE -ne 0) { throw "iscc failed ($LASTEXITCODE)" }
    $setup = Join-Path $PSScriptRoot "dist\Sonus-Setup-$version.exe"
    Ok ("Sonus-Setup-$version.exe   {0:N2} MB" -f ((Get-Item $setup).Length / 1MB))
    Ok ("sha256  " + (Get-FileHash $setup -Algorithm SHA256).Hash.ToLower())
    # Same name every release, so /releases/latest/download/Sonus-Setup.exe works.
    Copy-Item $setup (Join-Path $PSScriptRoot 'dist\Sonus-Setup.exe') -Force
  }
}

if ($Choco) {
  Step 'Packing Chocolatey package'
  $chocoExe = Find 'choco' @()
  if (-not $chocoExe) { throw 'choco not found' }
  & $chocoExe pack (Join-Path $PSScriptRoot 'packaging\choco\sonus.nuspec') --version $version `
    --outputdirectory (Join-Path $PSScriptRoot 'dist')
  if ($LASTEXITCODE -ne 0) { throw "choco pack failed ($LASTEXITCODE)" }
  Ok "dist\sonus.$version.nupkg"
}

if ($Run) {
  Step 'Launching'
  Start-Process $exe
  Ok 'running in the tray'
}
