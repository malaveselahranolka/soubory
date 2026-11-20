<#
.SYNOPSIS
  Installs Soubory (fm.exe) for the current user.
.DESCRIPTION
  Copies dist\app to -InstallDir, adds a Start menu shortcut "Soubory" and an App Paths entry
  (so "fm" works in Win+R). Touches only HKCU and the user's own Start menu. It does NOT make fm
  the default for folders; that is scripts\set-default-fm.ps1, a separate step.
  Run scripts\build-release.ps1 first. Tests pass -RegRoot, -InstallDir and -StartMenuDir.
#>
param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\fm\app'),
    [string]$RegRoot = 'HKCU:\Software\Microsoft\Windows\CurrentVersion',
    [string]$StartMenuDir = (Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'),
    [string]$SourceDir = (Join-Path (Split-Path -Parent $PSScriptRoot) 'dist\app')
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'lib\install-common.ps1')

Assert-HkcuRoot $RegRoot
if (-not (Test-Path (Join-Path $SourceDir 'fm.exe'))) { throw "Chybí $SourceDir\fm.exe — nejdřív spusť scripts\build-release.ps1." }
$InstallDir = [IO.Path]::GetFullPath($InstallDir)
if ((Test-Path $InstallDir) -and (Get-ChildItem $InstallDir -Force | Select-Object -First 1) -and -not (Test-Path (Join-Path $InstallDir $Marker))) {
    throw "Složka $InstallDir existuje a není naše (chybí $Marker). Zvol jinou -InstallDir."
}

# A running fm holds the exe; it is closed (it saves its windows and index on exit).
Stop-FromDir $InstallDir 'fm'
New-Item -ItemType Directory -Force $InstallDir | Out-Null
Set-Content -Path (Join-Path $InstallDir $Marker) -Value 'fm' -Encoding ascii
Get-ChildItem $SourceDir -File | ForEach-Object {
    $dest = Join-Path $InstallDir $_.Name
    Remove-OrSetAside $dest | Out-Null
    Copy-Item $_.FullName $dest -Force
}
$exe = Join-Path $InstallDir 'fm.exe'

$appPaths = Join-Path $RegRoot 'App Paths\fm.exe'
New-Item -Force $appPaths | Out-Null
Set-ItemProperty -Path $appPaths -Name '(default)' -Value $exe

New-Item -ItemType Directory -Force $StartMenuDir | Out-Null
$lnk = Join-Path $StartMenuDir 'Soubory.lnk'
$ws = New-Object -ComObject WScript.Shell
$s = $ws.CreateShortcut($lnk)
$s.TargetPath = $exe
$s.WorkingDirectory = $InstallDir
$s.Description = 'Soubory — správce souborů'
$s.Save()

"Nainstalováno do $InstallDir"
"Zástupce: $lnk"
"Aby se složky otevíraly v Souborech místo Průzkumníka, spusť scripts\set-default-fm.ps1."
