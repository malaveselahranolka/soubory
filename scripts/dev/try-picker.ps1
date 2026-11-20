<#
.SYNOPSIS
  Try fm's picker in the Open dialogs of Claude and Chrome: installs the shim and runs
  dist\app\fm.exe as the resident fm.
.DESCRIPTION
  Double-click scripts\dev\vyzkouset-picker.cmd (or run this from your own PowerShell, not from a
  terminal inside Claude) after scripts\build-release.ps1. Until the next sign-in the resident fm is the one
  from dist\app; the next sign-in starts the installed Soubory again. Everything it prints also
  goes to dist\try-picker.log. Undo: scripts\uninstall-shim.ps1.
#>
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$log = Join-Path $root 'dist\try-picker.log'
try { Start-Transcript -LiteralPath $log -Force | Out-Null } catch {}
$ok = $true
try {
    $new = Join-Path $root 'dist\app\fm.exe'
    $dll = Join-Path $root 'dist\shim\fm_dialog_shim.dll'
    foreach ($f in $new, $dll) {
        if (-not (Test-Path -LiteralPath $f)) { throw "Chybí $f. Nejdřív spusťte scripts\build-release.ps1 -SkipX86." }
    }

    # The shim first: it does not depend on which fm runs. Into the folder that already holds ours
    # (the installer's {app}\shim), else the scripts' default.
    $openKey = 'HKCU:\Software\Classes\CLSID\{DC1C5A9C-E88A-4dde-A5A1-60F82A20AEF7}\InprocServer32'
    $shimDir = Join-Path $env:LOCALAPPDATA 'Programs\fm\shim'
    $cur = (Get-ItemProperty $openKey -ErrorAction SilentlyContinue).'(default)'
    if ($cur -and (Get-Content -LiteralPath (Join-Path (Split-Path $cur) '.fm-install') -TotalCount 1 -ErrorAction SilentlyContinue) -eq 'fm-dialog-shim') { $shimDir = Split-Path $cur }
    & (Join-Path $root 'scripts\install-shim.ps1') -InstallDir $shimDir

    # Dev builds under .fm-target never hold the door; only the resident (installed or not) matters.
    function Get-Resident { @(Get-Process fm -ErrorAction SilentlyContinue | Where-Object { $_.Path -notlike '*\.fm-target*' }) }
    # Old: another exe, or dist\app\fm.exe started before it was rebuilt (a running one is renamed aside).
    $newTime = (Get-Item -LiteralPath $new).LastWriteTime
    function Get-Old { @(Get-Resident | Where-Object { $_.Path -ne $new -or $_.StartTime -lt $newTime }) }
    function Get-Current { @(Get-Resident | Where-Object { $_.Path -eq $new -and $_.StartTime -ge $newTime }) }
    if (Get-Old) {
        Write-Host 'Ukončuji běžící Soubory (uložení indexu může trvat i minutu)…'
        & $new --quit
        $deadline = (Get-Date).AddSeconds(90)
        while ((Get-Old) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 500 }
        if (Get-Old) { throw 'Soubory se neukončily. Ukončete je (ikona u hodin → Ukončit Soubory) a spusťte test znovu.' }
    }
    Get-ChildItem -LiteralPath (Split-Path $new) -Filter 'fm-running-*.exe' | Remove-Item -Force -ErrorAction SilentlyContinue
    if (-not (Get-Current)) {
        Start-Process -FilePath $new -ArgumentList '--background'
        $deadline = (Get-Date).AddSeconds(15)
        while (-not (Get-Current) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 250 }
    }

    # What the test needs, checked.
    $installed = Join-Path $shimDir 'fm_dialog_shim.dll'
    $checks = [ordered]@{
        'Doplněk pro okno Otevřít je nový'       = (Test-Path -LiteralPath $installed) -and (Get-FileHash -LiteralPath $installed).Hash -eq (Get-FileHash -LiteralPath $dll).Hash
        'Okno Otevřít vede na doplněk'           = (Get-ItemProperty $openKey -ErrorAction SilentlyContinue).'(default)' -eq $installed
        'Běží nové Soubory (dist\app\fm.exe)'    = [bool](Get-Current)
    }
    Write-Host ''
    foreach ($c in $checks.GetEnumerator()) {
        if ($c.Value) { Write-Host "  OK     $($c.Key)" -ForegroundColor Green } else { Write-Host "  CHYBA  $($c.Key)" -ForegroundColor Red; $ok = $false }
    }
} catch {
    $ok = $false
    Write-Host ''
    Write-Host "  CHYBA  $($_.Exception.Message)" -ForegroundColor Red
}
Write-Host ''
if ($ok) {
    Write-Host 'Hotovo. Teď Clauda i Chrome úplně ukončete (Claude: ikona u hodin → Quit), spusťte je znovu a otevřete výběr souboru.'
} else {
    Write-Host 'Test se nepřipravil. Napište Claudovi „hotovo“, výpis si přečte z dist\try-picker.log.'
}
try { Stop-Transcript | Out-Null } catch {}
if (-not $ok) { exit 1 }
