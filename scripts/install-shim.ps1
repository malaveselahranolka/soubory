<#
.SYNOPSIS
  Option 2: puts fm_dialog_shim.dll in front of the system Open dialog, for the current user.
.DESCRIPTION
  Claude.exe, chrome.exe and the apps listed in %LOCALAPPDATA%\fm\shim-allow.txt (one per line) get fm's own
  picker for their Open dialog: from the running fm, or from an installed fm.exe new enough for
  --pick. Whatever the picker cannot do, and every other app, goes straight to Windows (comdlg32).
  One line per dialog goes to %LOCALAPPDATA%\fm\shim.log (inside a Store/MSIX app such as Claude:
  %LOCALAPPDATA%\Packages\<app>\LocalCache\Local\fm\shim.log). Nothing about files or folders is
  logged.

  Registers HKCU\Software\Classes\CLSID\{DC1C5A9C-…} (64-bit apps only), copies the DLL to its
  own directory first and writes the key last. The file %LOCALAPPDATA%\fm\disable-shim turns the
  picker and the log off (every dialog is Windows' again); the shim stays in place. Remove:
  scripts\uninstall-shim.ps1. An app keeps the DLL it loaded until it is restarted. Emergency recovery:
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
$Clsid = '{DC1C5A9C-E88A-4dde-A5A1-60F82A20AEF7}'
$src = Join-Path $SourceDir 'fm_dialog_shim.dll'
if (-not (Test-Path -LiteralPath $src)) { throw "Chybí $src — nejdřív spusť scripts\build-release.ps1." }
$InstallDir = Get-InstallDir $InstallDir
Assert-OwnDir $InstallDir 'fm-dialog-shim'
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
Set-Content -LiteralPath (Join-Path $InstallDir $Marker) -Value 'fm-dialog-shim' -Encoding ascii
Get-ChildItem $InstallDir -File -Filter '*.del' | ForEach-Object { Remove-Item -LiteralPath $_.FullName -Force -ErrorAction SilentlyContinue }
# An old DLL still loaded in some app is renamed aside, never overwritten in place.
$locked = -not (Remove-OrSetAside $dll)
Copy-Item $src $dll -Force
Grant-AppContainerRead $InstallDir
$stale = @(Get-ChildItem $InstallDir -File -Filter '*.del')
if ($locked -or $stale.Count) { Register-DelCleanup $RegRoot $InstallDir } else { Clear-DelCleanup $RegRoot $InstallDir }

# The key last: it only ever points at a DLL that is already in place.
New-KeyIfMissing "$key\InprocServer32"
Set-ItemProperty $key -Name '(default)' -Value 'fm Option 2 (FileOpenDialog)'
Set-ItemProperty "$key\InprocServer32" -Name '(default)' -Value $dll
Set-ItemProperty "$key\InprocServer32" -Name 'ThreadingModel' -Value $threading
if ((Get-Item "$key\InprocServer32").GetValue('') -ne $dll -or -not (Test-Path $dll)) { throw 'Registrace se nepovedla.' }

Write-Host "Nainstalováno: $dll"
Write-Host "Výběr souborů z fm dostane Claude.exe a chrome.exe (další aplikace: $env:LOCALAPPDATA\fm\shim-allow.txt). Aplikace, která už dialog otevřela, potřebuje restart."
Write-Host "Log: $env:LOCALAPPDATA\fm\shim.log (u Clauda v ...\Packages\Claude_…\LocalCache\Local\fm\)  ·  vypnout: soubor $env:LOCALAPPDATA\fm\disable-shim  ·  odinstalovat: scripts\uninstall-shim.ps1"
