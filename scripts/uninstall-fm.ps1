<#
.SYNOPSIS
  Removes Soubory (fm.exe) for the current user.
.DESCRIPTION
  First gives folders back to Explorer (scripts\restore-explorer.ps1) if fm was the default,
  then removes the Start menu shortcut, the App Paths entry and the install directory (only a
  directory carrying our marker). Settings, tags and the search index in %LOCALAPPDATA%\fm stay
  unless -RemoveData is given.
#>
param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\fm\app'),
    [string]$RegRoot = 'HKCU:\Software\Microsoft\Windows\CurrentVersion',
    [string]$ClassesRoot = 'HKCU:\Software\Classes',
    [string]$BackupRoot = 'HKCU:\Software\fm\Backup',
    [string]$StartMenuDir = (Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'),
    [string]$DataDir = (Join-Path $env:LOCALAPPDATA 'fm'),
    [switch]$RemoveData
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'lib\install-common.ps1')

Assert-HkcuRoot $RegRoot
Assert-HkcuRoot $ClassesRoot
$InstallDir = [IO.Path]::GetFullPath($InstallDir)
if ((Test-Path $InstallDir) -and -not (Test-Path (Join-Path $InstallDir $Marker))) {
    throw "Složka $InstallDir nemá značku $Marker — nic nemažu."
}

& (Join-Path $PSScriptRoot 'restore-explorer.ps1') -ClassesRoot $ClassesRoot -BackupRoot $BackupRoot -Quiet

Stop-FromDir $InstallDir 'fm'
Remove-Item -Path (Join-Path $RegRoot 'App Paths\fm.exe') -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item -Path (Join-Path $StartMenuDir 'Soubory.lnk') -Force -ErrorAction SilentlyContinue
if (Test-Path $InstallDir) {
    if (-not (Remove-InstallDir $InstallDir)) {
        Register-DelCleanup $RegRoot $InstallDir -WholeDir
        "Některé soubory jsou otevřené; zbytek se smaže po příštím přihlášení."
    }
}
if ($RemoveData -and (Test-Path $DataDir)) {
    Remove-Item -LiteralPath $DataDir -Recurse -Force
    "Smazána data: $DataDir"
}
"Soubory odinstalovány."
