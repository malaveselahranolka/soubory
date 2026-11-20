<#
.SYNOPSIS
  Removes exactly what install-shim.ps1 added: the CLSID key first, then its own directory.
.DESCRIPTION
  The key is deleted and checked gone before any file is touched, so the Open dialog never points
  at a missing DLL. A DLL still loaded in a running app cannot be deleted; it is renamed to *.del
  and an HKCU RunOnce entry removes the directory at the next sign-in.
#>
param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\fm\shim'),
    [string]$ClassesRoot = 'HKCU:\Software\Classes',
    [string]$RegRoot = 'HKCU:\Software\Microsoft\Windows\CurrentVersion'
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'lib\install-common.ps1')
$CleanupName = 'FinderWin.Shim.Cleanup'

Assert-HkcuRoot $ClassesRoot
Assert-HkcuRoot $RegRoot
$InstallDir = Get-InstallDir $InstallDir
# Checked first, so a refused uninstall changes nothing.
Assert-OwnDir $InstallDir 'fm-dialog-shim' -Uninstall
$key = Join-Path $ClassesRoot 'CLSID\{DC1C5A9C-E88A-4dde-A5A1-60F82A20AEF7}'

if (Test-Path $key) {
    $cur = (Get-Item $key).OpenSubKey('InprocServer32')
    $path = if ($cur) { $cur.GetValue('') } else { $null }
    if ($path -and $path.StartsWith($InstallDir.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase)) {
        Remove-Item $key -Recurse -Force
        if (Test-Path $key) { throw "Klíč $key se nepodařilo smazat; soubory nechávám." }
        $clsids = Join-Path $ClassesRoot 'CLSID'
        if (-not (Get-ChildItem $clsids) -and -not (Get-Item $clsids).ValueCount) { Remove-Item $clsids -Force }
    } else {
        Write-Warning "Klíč $key nepatří Souborům ($path); nechávám ho být."
    }
}

if (-not (Test-Path -LiteralPath $InstallDir)) { Write-Host 'Nic k odinstalaci.'; return }
if (Remove-InstallDir $InstallDir) {
    Clear-DelCleanup $RegRoot
    Write-Host 'Odinstalováno.'
} else {
    Register-DelCleanup $RegRoot $InstallDir -WholeDir
    Write-Host 'Odinstalováno. DLL ještě drží otevřená aplikace; smaže se po příštím přihlášení.'
}
