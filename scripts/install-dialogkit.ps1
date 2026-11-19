<#
.SYNOPSIS
  Installs fm-dialogkit (the "Nový ▾" button in every Open/Save dialog) for the current user.
.DESCRIPTION
  Copies dist\dialogkit to -InstallDir, grants AppContainer read access, adds an HKCU Run value
  and starts the helper. Touches only HKCU. Run scripts\build-release.ps1 first.
  Tests pass -RegRoot and -InstallDir to work in a sandbox instead of the real locations.
#>
param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\fm\dialogkit'),
    [string]$RegRoot = 'HKCU:\Software\Microsoft\Windows\CurrentVersion',
    [string]$SourceDir = (Join-Path (Split-Path -Parent $PSScriptRoot) 'dist\dialogkit'),
    [switch]$NoStart
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'lib\install-common.ps1')

Assert-HkcuRoot $RegRoot
if (-not (Test-Path (Join-Path $SourceDir 'fm-dialogkit.exe'))) { throw "Chybí $SourceDir\fm-dialogkit.exe — nejdřív spusť scripts\build-release.ps1." }
$InstallDir = [IO.Path]::GetFullPath($InstallDir)

Stop-FromDir $InstallDir 'fm-dialogkit'
New-Item -ItemType Directory -Force $InstallDir | Out-Null
Set-Content -Path (Join-Path $InstallDir $Marker) -Value 'fm-dialogkit' -Encoding ascii

$locked = $false
Get-ChildItem $SourceDir -Recurse -File | ForEach-Object {
    $rel = $_.FullName.Substring($SourceDir.TrimEnd('\').Length + 1)
    $dest = Join-Path $InstallDir $rel
    New-Item -ItemType Directory -Force (Split-Path $dest) | Out-Null
    if (-not (Remove-OrSetAside $dest)) { $locked = $true }
    Copy-Item $_.FullName $dest -Force
}
Grant-AppContainerRead $InstallDir

$exe = Join-Path $InstallDir 'fm-dialogkit.exe'
$run = Join-Path $RegRoot 'Run'
New-Item -Force -ErrorAction SilentlyContinue $run | Out-Null
Set-ItemProperty -Path $run -Name $RunName -Value "`"$exe`""

# Old DLLs still mapped in some dialog: delete them at the next sign-in. This also replaces a
# pending whole-directory cleanup from an earlier uninstall.
Get-ChildItem $InstallDir -Recurse -File -Filter '*.del' | ForEach-Object { Remove-Item -LiteralPath $_.FullName -Force -ErrorAction SilentlyContinue }
$stale = @(Get-ChildItem $InstallDir -Recurse -File | Where-Object { $_.Name -like '*.del' })
if ($locked -or $stale.Count) { Register-DelCleanup $RegRoot $InstallDir } else { Clear-DelCleanup $RegRoot }

if (-not $NoStart) { Start-Process $exe }
Write-Host "Nainstalováno do $InstallDir. Tlačítko „Nový“ se objeví v každém dialogu Otevřít/Uložit."
Write-Host "Odinstalace: scripts\uninstall-dialogkit.ps1"
