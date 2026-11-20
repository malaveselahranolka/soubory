<#
.SYNOPSIS
  Try fm's picker in Claude's Open dialog: runs dist\app\fm.exe as the resident fm and installs the shim.
.DESCRIPTION
  Run from your own PowerShell (not a terminal inside Claude) after scripts\build-release.ps1.
  Until the next sign-in the resident fm is the one from dist\app; the next sign-in starts the
  installed Soubory again, and the shim then falls back to the system dialog.
  Undo: scripts\uninstall-shim.ps1.
#>
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$new = Join-Path $root 'dist\app\fm.exe'
if (-not (Test-Path -LiteralPath $new)) { throw "Chybí $new. Nejdřív spusťte scripts\build-release.ps1 -SkipX86." }

# Dev builds under .fm-target never hold the door; only the resident (installed or not) matters.
function Get-Resident { @(Get-Process fm -ErrorAction SilentlyContinue | Where-Object { $_.Path -notlike '*\.fm-target*' }) }
if (Get-Resident) {
    Write-Host 'Ukončuji běžící Soubory…'
    & $new --quit
    $deadline = (Get-Date).AddSeconds(30)
    while ((Get-Resident) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 250 }
    if (Get-Resident) { throw 'Soubory se neukončily. Zavřete je (ikona u hodin → Ukončit Soubory) a spusťte skript znovu.' }
}
Start-Process -FilePath $new -ArgumentList '--background'
& (Join-Path $root 'scripts\install-shim.ps1')
Write-Host ''
Write-Host 'Hotovo. Teď Clauda úplně ukončete (ikona u hodin → Quit), spusťte ho znovu a klikněte na „+“.'
