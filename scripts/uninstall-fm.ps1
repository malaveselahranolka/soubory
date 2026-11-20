<#
.SYNOPSIS
  Removes Soubory (fm.exe) for the current user.
.DESCRIPTION
  First asks a running fm to quit; if it is still finishing a copy or move (or has a dialog open)
  after -QuitWait seconds, the script stops without changing anything. Then gives folders back to
  Explorer (scripts\restore-explorer.ps1) if fm was the default, and removes the Run value
  "Soubory", the Start menu shortcut, the App Paths entry (only while it points at this install)
  and the install directory (only a directory marked as fm's). Settings,
  tags and the search index in %LOCALAPPDATA%\fm stay unless -RemoveData is given; that folder is
  shared with dialogkit and the dialog shim, whose files always stay.
#>
param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\fm\app'),
    [string]$RegRoot = 'HKCU:\Software\Microsoft\Windows\CurrentVersion',
    [string]$RunRoot = (Join-Path $RegRoot 'Run'),
    [string]$ClassesRoot = 'HKCU:\Software\Classes',
    [string]$FmRoot = 'HKCU:\Software\fm',
    [string]$BackupRoot = (Join-Path $FmRoot 'Backup'),
    [string]$StartMenuDir = (Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'),
    [string]$DataDir = (Join-Path $env:LOCALAPPDATA 'fm'),
    [switch]$RemoveData,
    [int]$QuitWait = 120
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'lib\install-common.ps1')
$CleanupName = 'Soubory.Cleanup'

Assert-HkcuRoot $RegRoot
Assert-HkcuRoot $RunRoot
Assert-HkcuRoot $ClassesRoot
$InstallDir = Get-InstallDir $InstallDir
Assert-OwnDir $InstallDir 'fm' -Uninstall
# First, so an fm still copying stops the script before anything changes.
Stop-Fm $InstallDir $QuitWait

& (Join-Path $PSScriptRoot 'restore-explorer.ps1') -ClassesRoot $ClassesRoot -FmRoot $FmRoot -BackupRoot $BackupRoot -Quiet

Remove-ItemProperty -Path $RunRoot -Name 'Soubory' -ErrorAction SilentlyContinue
$ap = Join-Path $RegRoot 'App Paths\fm.exe'
if ((Test-Path -LiteralPath $ap) -and (Get-Item -LiteralPath $ap).GetValue('') -eq (Join-Path $InstallDir 'fm.exe')) { Remove-Item -LiteralPath $ap -Recurse -Force }
Remove-Item -LiteralPath (Join-Path $StartMenuDir 'Soubory.lnk') -Force -ErrorAction SilentlyContinue
if (Test-Path -LiteralPath $InstallDir) {
    if (Remove-InstallDir $InstallDir) {
        Clear-DelCleanup $RegRoot
    } else {
        Register-DelCleanup $RegRoot $InstallDir -WholeDir
        "Některé soubory jsou otevřené; zbytek se smaže po příštím přihlášení."
    }
}
if ($RemoveData -and (Test-Path -LiteralPath $DataDir)) {
    # Only what fm.exe writes; dialogkit and the shim keep theirs (disable-shim, shim-allow.txt, logs).
    foreach ($n in 'prefs.txt', 'prefs.tmp', 'tags.tsv', 'tags.tmp', 'fm.log', 'index', 'index-dev') {
        $p = Join-Path $DataDir $n
        if (Test-Path -LiteralPath $p) { Remove-Item -LiteralPath $p -Recurse -Force }
    }
    if (-not (Get-ChildItem -LiteralPath $DataDir -Force)) { Remove-Item -LiteralPath $DataDir -Force }
    "Smazána data Souborů v $DataDir"
}
"Soubory odinstalovány."
