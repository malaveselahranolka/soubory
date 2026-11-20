<#
.SYNOPSIS
  Option 2 gate: puts fm_dialog_shim.dll in front of the system Open dialog, for the current user.
.DESCRIPTION
  The shim hands every Open dialog straight to Windows (comdlg32) and only writes one line per
  dialog to %LOCALAPPDATA%\fm\shim.log: which app, its integrity level, and whether it would get
  fm's own picker later. Nothing about files or folders is logged.

  Registers HKCU\Software\Classes\CLSID\{DC1C5A9C-…} (64-bit apps only), copies the DLL to its
  own directory first and writes the key last. Turn it off without uninstalling: create the file
  %LOCALAPPDATA%\fm\disable-shim. Remove: scripts\uninstall-shim.ps1. Emergency recovery:
    reg delete "HKCU\Software\Classes\CLSID\{DC1C5A9C-E88A-4dde-A5A1-60F82A20AEF7}" /f
  Run from your own 64-bit PowerShell (not from inside another app's sandbox). Tests pass
  -ClassesRoot, -RegRoot and -InstallDir.
#>
param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\fm\shim'),
    [string]$ClassesRoot = 'HKCU:\Software\Classes',
    [string]$RegRoot = 'HKCU:\Software\Microsoft\Windows\CurrentVersion',
    [string]$SourceDir = (Join-Path (Split-Path -Parent $PSScriptRoot) 'dist\shim')
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'lib\install-common.ps1')
$CleanupName = 'FinderWin.Shim.Cleanup'

Assert-HkcuRoot $ClassesRoot
Assert-HkcuRoot $RegRoot
# A 32-bit PowerShell would write the 32-bit view and point 32-bit apps at a 64-bit DLL.
if (-not [Environment]::Is64BitProcess) { throw 'Spusť 64bitový PowerShell.' }
$Clsid = '{DC1C5A9C-E88A-4dde-A5A1-60F82A20AEF7}'
$src = Join-Path $SourceDir 'fm_dialog_shim.dll'
if (-not (Test-Path $src)) { throw "Chybí $src — nejdřív spusť scripts\build-release.ps1." }
$InstallDir = [IO.Path]::GetFullPath($InstallDir)
$dll = Join-Path $InstallDir 'fm_dialog_shim.dll'

# Another app's override of the Open dialog is never replaced.
$key = Join-Path $ClassesRoot "CLSID\$Clsid"
if (Test-Path $key) {
    $cur = (Get-Item $key).OpenSubKey('InprocServer32')
    $path = if ($cur) { $cur.GetValue('') } else { $null }
    if (-not $path -or -not $path.StartsWith($InstallDir.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase)) {
        throw "Dialog Otevřít už přesměrovává jiná aplikace ($key → $path); nic neměním."
    }
}
$system = Get-Item "HKLM:\SOFTWARE\Classes\CLSID\$Clsid\InprocServer32"
$threading = $system.GetValue('ThreadingModel')
if (-not $threading) { throw 'Systémová registrace dialogu nemá ThreadingModel; nic neměním.' }

New-Item -ItemType Directory -Force $InstallDir | Out-Null
Set-Content -Path (Join-Path $InstallDir $Marker) -Value 'fm-dialog-shim' -Encoding ascii
Get-ChildItem $InstallDir -File -Filter '*.del' | ForEach-Object { Remove-Item -LiteralPath $_.FullName -Force -ErrorAction SilentlyContinue }
# An old DLL still loaded in some app is renamed aside, never overwritten in place.
$locked = -not (Remove-OrSetAside $dll)
Copy-Item $src $dll -Force
Grant-AppContainerRead $InstallDir
$stale = @(Get-ChildItem $InstallDir -File -Filter '*.del')
if ($locked -or $stale.Count) { Register-DelCleanup $RegRoot $InstallDir } else { Clear-DelCleanup $RegRoot }

# The key last: it only ever points at a DLL that is already in place.
New-Item -Force "$key\InprocServer32" | Out-Null
Set-ItemProperty $key -Name '(default)' -Value 'fm Option 2 gate (FileOpenDialog)'
Set-ItemProperty "$key\InprocServer32" -Name '(default)' -Value $dll
Set-ItemProperty "$key\InprocServer32" -Name 'ThreadingModel' -Value $threading
if ((Get-Item "$key\InprocServer32").GetValue('') -ne $dll -or -not (Test-Path $dll)) { throw 'Registrace se nepovedla.' }

Write-Host "Nainstalováno: $dll"
Write-Host "Log: $env:LOCALAPPDATA\fm\shim.log  ·  vypnout: soubor $env:LOCALAPPDATA\fm\disable-shim  ·  odinstalovat: scripts\uninstall-shim.ps1"
