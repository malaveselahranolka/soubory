<#
.SYNOPSIS
  Installs Soubory (fm.exe) for the current user.
.DESCRIPTION
  Copies dist\app to -InstallDir, adds a Start menu shortcut "Soubory", an App Paths entry
  (so "fm" works in Win+R; another program's entry under that name is left alone) and the HKCU
  Run value "Soubory" = "fm.exe" --background (fm stays resident: it starts at sign-in without a
  window) and starts it that way right away unless -NoStart. A running fm is asked to quit first,
  also one started from elsewhere (for example a build in dist or target); if it is still
  finishing a copy or move (or has a dialog open) after -QuitWait seconds, the script stops
  without changing anything. Each file is copied in full before the old one is replaced, so a
  failed copy leaves the old fm.exe in place. Touches only HKCU and the user's own Start menu. It
  does NOT make fm the default for folders; that is scripts\set-default-fm.ps1, a separate step.
  Run scripts\build-release.ps1 first, from a normal (not elevated) 64-bit PowerShell. Tests pass
  -RegRoot, -RunRoot, -InstallDir, -StartMenuDir, -SourceDir, -NoStart, -QuitWait and -Mutex.
#>
param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\fm\app'),
    [string]$RegRoot = 'HKCU:\Software\Microsoft\Windows\CurrentVersion',
    [string]$RunRoot = (Join-Path $RegRoot 'Run'),
    [string]$StartMenuDir = (Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'),
    [string]$SourceDir = (Join-Path (Split-Path -Parent $PSScriptRoot) 'dist\app'),
    [switch]$NoStart,
    [int]$QuitWait = 120,
    [string]$Mutex = 'Local\fm.main'
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'lib\install-common.ps1')
$CleanupName = 'Soubory.Cleanup'

Assert-NotElevated
Assert-HkcuRoot $RegRoot
Assert-HkcuRoot $RunRoot
if (-not (Test-Path -LiteralPath (Join-Path $SourceDir 'fm.exe'))) { throw "Chybí $SourceDir\fm.exe — nejdřív spusť scripts\build-release.ps1." }
$InstallDir = Get-InstallDir $InstallDir
Assert-OwnDir $InstallDir 'fm'

Stop-Fm $InstallDir $QuitWait $Mutex (Join-Path $SourceDir 'fm.exe')
New-Item -ItemType Directory -Force $InstallDir | Out-Null
Set-Content -LiteralPath (Join-Path $InstallDir $Marker) -Value 'fm' -Encoding ascii
# Replaces a pending whole-folder delete from an earlier uninstall before anything is copied.
Register-DelCleanup $RegRoot $InstallDir
# Everything is copied as *.new first; the old files go only once all copies are complete, so a
# failed copy (disk full, antivirus, an offline cloud file) leaves the install as it was.
Get-ChildItem -LiteralPath $InstallDir -File -Filter '*.new' | ForEach-Object { Remove-Item -LiteralPath $_.FullName -Force }
$files = @(Get-ChildItem -LiteralPath $SourceDir -File)
foreach ($f in $files) { Copy-Item -LiteralPath $f.FullName (Join-Path $InstallDir "$($f.Name).new") -Force }
foreach ($f in $files) {
    $dest = Join-Path $InstallDir $f.Name
    $aside = $null
    if (Test-Path -LiteralPath $dest) {
        $aside = "$dest.$([guid]::NewGuid().ToString('N')).del"
        Rename-Item -LiteralPath $dest -NewName (Split-Path $aside -Leaf)
    }
    # A failed swap puts the old file back (the *.del cleanup would otherwise remove it).
    try { Rename-Item -LiteralPath "$dest.new" -NewName $f.Name }
    catch { if ($aside) { Rename-Item -LiteralPath $aside -NewName $f.Name }; throw }
}
# Old files still mapped somewhere go at the next sign-in; otherwise nothing stays pending.
Get-ChildItem -LiteralPath $InstallDir -File -Filter '*.del' | ForEach-Object { Remove-Item -LiteralPath $_.FullName -Force -ErrorAction SilentlyContinue }
if (-not (Get-ChildItem -LiteralPath $InstallDir -File -Filter '*.del')) { Clear-DelCleanup $RegRoot }
$exe = Join-Path $InstallDir 'fm.exe'

# "fm" is a generic name: an entry of another program is never replaced.
$appPaths = Join-Path $RegRoot 'App Paths\fm.exe'
$cur = if (Test-Path -LiteralPath $appPaths) { "$((Get-Item -LiteralPath $appPaths).GetValue(''))".Trim('"', ' ') } else { '' }
$ours = -not $cur -or $cur -eq $exe -or $(try { Test-Marker (Split-Path $cur) 'fm' } catch { $false })
if ($ours) {
    New-KeyIfMissing $appPaths
    Set-ItemProperty -LiteralPath $appPaths -Name '(default)' -Value $exe
} else {
    Write-Warning "App Paths\fm.exe patří jiné aplikaci ($cur); příkaz `„fm`“ ve Win+R nenastavuji."
}

New-Item -ItemType Directory -Force $StartMenuDir | Out-Null
$lnk = Join-Path $StartMenuDir 'Soubory.lnk'
$ws = New-Object -ComObject WScript.Shell
$s = $ws.CreateShortcut($lnk)
$s.TargetPath = $exe
$s.WorkingDirectory = $InstallDir
$s.Description = 'Soubory — správce souborů'
$s.Save()

New-KeyIfMissing $RunRoot
Set-ItemProperty -Path $RunRoot -Name 'Soubory' -Value "`"$exe`" --background"
# A neutral working folder, as at sign-in: fm would otherwise keep this shell's folder in use.
if (-not $NoStart) { Start-Process -FilePath $exe -ArgumentList '--background' -WorkingDirectory "$env:SystemRoot\System32" }

"Nainstalováno do $InstallDir"
"Zástupce: $lnk"
"Soubory běží na pozadí a spustí se při každém přihlášení."
"Aby se složky otevíraly v Souborech místo Průzkumníka, spusť scripts\set-default-fm.ps1."
