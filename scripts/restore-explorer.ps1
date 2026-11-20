<#
.SYNOPSIS
  Gives folders, drives and Win+E back to File Explorer (undoes scripts\set-default-fm.ps1).
.DESCRIPTION
  Removes only the keys set-default-fm.ps1 created and restores the values it backed up under
  HKCU\Software\fm\Backup. Does nothing if fm was never made the default.
#>
param(
    [string]$ClassesRoot = 'HKCU:\Software\Classes',
    [string]$BackupRoot = 'HKCU:\Software\fm\Backup',
    [switch]$Quiet
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'lib\install-common.ps1')
Assert-HkcuRoot $ClassesRoot
Assert-HkcuRoot $BackupRoot

if (-not (Test-Path $BackupRoot)) {
    if (-not $Quiet) { "Soubory nebyly výchozí; není co vracet." }
    return
}
$Verb = 'fm.open'
$WinE = 'CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}\shell\opennewwindow'
$b = Get-ItemProperty $BackupRoot

foreach ($k in 'Directory\shell', 'Drive\shell') {
    $shell = Join-Path $ClassesRoot $k
    Remove-Item -Path "$shell\$Verb" -Recurse -Force -ErrorAction SilentlyContinue
    $name = $k -replace '\\', '.'
    if ($b.PSObject.Properties[$name]) {
        Set-ItemProperty $shell -Name '(default)' -Value $b.$name
    } elseif (Test-Path $shell) {
        # Remove-ItemProperty cannot delete a key's default value; the registry API can.
        $k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey(($shell -replace '^HKCU:\\', ''), $true)
        if ($k) { $k.DeleteValue('', $false); $k.Close() }
        # The key was ours alone: drop it when nothing else lives there.
        if (-not (Get-ChildItem $shell) -and -not ((Get-Item $shell).ValueCount)) { Remove-Item $shell -Force }
    }
}
if (-not $b.'WinE.existed') {
    $root = Join-Path $ClassesRoot 'CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}'
    Remove-Item -Path (Join-Path $ClassesRoot $WinE) -Recurse -Force -ErrorAction SilentlyContinue
    foreach ($p in "$root\shell", $root) {
        if ((Test-Path $p) -and -not (Get-ChildItem $p) -and -not ((Get-Item $p).ValueCount)) { Remove-Item $p -Force }
    }
}
Remove-Item -Path $BackupRoot -Recurse -Force
$parent = Split-Path $BackupRoot
if ((Test-Path $parent) -and -not (Get-ChildItem $parent) -and -not ((Get-Item $parent).ValueCount)) { Remove-Item $parent -Force }
if (-not $Quiet) { "Složky se zase otevírají v Průzkumníkovi." }
