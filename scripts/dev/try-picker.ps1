<#
.SYNOPSIS
  Try fm's picker in the Open dialogs of Claude and Chrome: installs the shim and runs
  dist\app\fm.exe as the resident fm.
.DESCRIPTION
  Double-click scripts\dev\vyzkouset-picker.cmd (or run this from your own PowerShell, not from a
  terminal inside Claude) after scripts\build-release.ps1. Adds chrome.exe to
  %LOCALAPPDATA%\fm\shim-allow.txt for the test. Until the next sign-in the resident fm is the one
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

    # Chrome is not on the shim's built-in list; the test adds it.
    $allow = Join-Path $env:LOCALAPPDATA 'fm\shim-allow.txt'
    New-Item -ItemType Directory -Force (Split-Path $allow) | Out-Null
    $lines = @(if (Test-Path -LiteralPath $allow) { Get-Content -LiteralPath $allow })
    if (-not ($lines | Where-Object { $_.Trim() -ieq 'chrome.exe' })) { Set-Content -LiteralPath $allow -Value ($lines + 'chrome.exe') -Encoding ascii }

    # The shim first: it does not depend on which fm runs.
    & (Join-Path $root 'scripts\install-shim.ps1')

    # Dev builds under .fm-target never hold the door; only the resident (installed or not) matters.
    function Get-Resident { @(Get-Process fm -ErrorAction SilentlyContinue | Where-Object { $_.Path -notlike '*\.fm-target*' }) }
    $old = @(Get-Resident | Where-Object { $_.Path -ne $new })
    if ($old) {
        Write-Host 'Ukončuji běžící Soubory (uložení indexu může trvat i minutu)…'
        & $new --quit
        $deadline = (Get-Date).AddSeconds(90)
        while ((Get-Resident | Where-Object { $_.Path -ne $new }) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 500 }
        if (Get-Resident | Where-Object { $_.Path -ne $new }) { throw 'Soubory se neukončily. Ukončete je (ikona u hodin → Ukončit Soubory) a spusťte test znovu.' }
    }
    if (-not (Get-Resident | Where-Object { $_.Path -eq $new })) {
        Start-Process -FilePath $new -ArgumentList '--background'
        $deadline = (Get-Date).AddSeconds(15)
        while (-not (Get-Resident | Where-Object { $_.Path -eq $new }) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 250 }
    }

    # What the test needs, checked.
    $installed = Join-Path $env:LOCALAPPDATA 'Programs\fm\shim\fm_dialog_shim.dll'
    $checks = [ordered]@{
        'Doplněk pro okno Otevřít je nový'       = (Test-Path -LiteralPath $installed) -and (Get-FileHash -LiteralPath $installed).Hash -eq (Get-FileHash -LiteralPath $dll).Hash
        'Okno Otevřít vede na doplněk'           = (Get-ItemProperty 'HKCU:\Software\Classes\CLSID\{DC1C5A9C-E88A-4dde-A5A1-60F82A20AEF7}\InprocServer32' -ErrorAction SilentlyContinue).'(default)' -eq $installed
        'Běží nové Soubory (dist\app\fm.exe)'    = [bool](Get-Resident | Where-Object { $_.Path -eq $new })
        'Chrome je v seznamu (shim-allow.txt)'   = [bool](Get-Content -LiteralPath $allow | Where-Object { $_.Trim() -ieq 'chrome.exe' })
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
