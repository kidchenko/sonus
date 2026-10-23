$ErrorActionPreference = 'Stop'

$version = '0.0.1'
$url     = "https://github.com/kidchenko/sonus/releases/download/v$version/Sonus-Setup-$version.exe"

$packageArgs = @{
  packageName    = 'sonus'
  fileType       = 'EXE'
  url64bit       = $url
  # The release workflow rewrites the version and this checksum from the
  # artifact it just built.
  checksum64     = 'REPLACE_WITH_SHA256'
  checksumType64 = 'sha256'
  # Inno Setup silent switches. The autostart task stays at its default (on).
  silentArgs     = '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP-'
  validExitCodes = @(0)
  softwareName   = 'Sonus*'
}

Install-ChocolateyPackage @packageArgs
