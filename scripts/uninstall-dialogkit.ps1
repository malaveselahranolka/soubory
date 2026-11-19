<#
.SYNOPSIS
  Removes exactly what install-dialogkit.ps1 added: the Run value and its own directory.
.DESCRIPTION
  A hook DLL still mapped in an open dialog cannot be deleted; it is renamed to *.del and an
  HKCU RunOnce entry removes the directory at the next sign-in.
#>
param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\fm\dialogkit'),
    [string]$RegRoot = 'HKCU:\Software\Microsoft\Windows\CurrentVersion'
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'lib\install-common.ps1')

Assert-HkcuRoot $RegRoot
$InstallDir = [IO.Path]::GetFullPath($InstallDir)
$run = Join-Path $RegRoot 'Run'
if (Get-ItemProperty -Path $run -Name $RunName -ErrorAction SilentlyContinue) {
    Remove-ItemProperty -Path $run -Name $RunName
}
Stop-FromDir $InstallDir 'fm-dialogkit'

if (-not (Test-Path $InstallDir)) { Write-Host 'Nic k odinstalaci.'; return }
if (-not (Test-Path (Join-Path $InstallDir $Marker))) {
    throw "$InstallDir neobsahuje značku instalace ($Marker) — pro jistotu nic nemažu."
}
if (Remove-InstallDir $InstallDir) {
    Clear-DelCleanup $RegRoot
    Write-Host 'Odinstalováno.'
} else {
    Register-DelCleanup $RegRoot $InstallDir -WholeDir
    Write-Host 'Odinstalováno. Některé soubory drží otevřený dialog; smažou se po příštím přihlášení.'
}
