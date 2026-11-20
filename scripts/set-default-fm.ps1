<#
.SYNOPSIS
  Makes Soubory (fm.exe) open folders and drives instead of File Explorer, for the current user.
.DESCRIPTION
  Writes only under HKCU\Software\Classes:
    Directory\shell, Drive\shell   default verb "fm.open" -> "fm.exe" "%1"
    CLSID\{52205fd8-…} (Win+E)     opennewwindow -> fm.exe
  Whatever was there before is saved under HKCU\Software\fm\Backup and put back by
  scripts\restore-explorer.ps1. Virtual folders (Control Panel, Recycle Bin from the desktop)
  and apps that call "show in folder" still open Explorer. Takes effect at once; no restart.
  Tests pass -ClassesRoot and -BackupRoot to work in a sandbox.
#>
param(
    [string]$Exe = (Join-Path $env:LOCALAPPDATA 'Programs\fm\app\fm.exe'),
    [string]$ClassesRoot = 'HKCU:\Software\Classes',
    [string]$BackupRoot = 'HKCU:\Software\fm\Backup'
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'lib\install-common.ps1')
Assert-HkcuRoot $ClassesRoot
Assert-HkcuRoot $BackupRoot
if (-not (Test-Path $Exe)) { throw "Chybí $Exe — nejdřív spusť scripts\install-fm.ps1." }

$Verb = 'fm.open'
$WinEKey = 'CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}\shell\opennewwindow'
$cmd = "`"$Exe`" `"%1`""

function Get-Default([string]$key) {
    if (Test-Path $key) { (Get-Item $key).GetValue('', $null) } else { $null }
}

# Back up once: a second run must not overwrite the original values with our own.
if (-not (Test-Path $BackupRoot)) {
    New-Item -Force $BackupRoot | Out-Null
    foreach ($k in 'Directory\shell', 'Drive\shell') {
        $v = Get-Default (Join-Path $ClassesRoot $k)
        $name = $k -replace '\\', '.'
        if ($null -ne $v) { Set-ItemProperty $BackupRoot -Name $name -Value $v } else { Set-ItemProperty $BackupRoot -Name "$name.absent" -Value 1 }
    }
    $winE = Join-Path $ClassesRoot $WinEKey
    Set-ItemProperty $BackupRoot -Name 'WinE.existed' -Value ([int](Test-Path $winE))
}

foreach ($k in 'Directory\shell', 'Drive\shell') {
    $shell = Join-Path $ClassesRoot $k
    New-Item -Force "$shell\$Verb\command" | Out-Null
    Set-ItemProperty $shell -Name '(default)' -Value $Verb
    Set-ItemProperty "$shell\$Verb" -Name '(default)' -Value 'Otevřít v Souborech'
    Set-ItemProperty "$shell\$Verb\command" -Name '(default)' -Value $cmd
}

# Win+E opens "This PC" through this CLSID; an empty DelegateExecute makes Windows use our command.
$winE = Join-Path $ClassesRoot $WinEKey
if (-not ((Get-ItemProperty $BackupRoot).'WinE.existed')) {
    New-Item -Force "$winE\command" | Out-Null
    Set-ItemProperty "$winE\command" -Name '(default)' -Value "`"$Exe`""
    Set-ItemProperty "$winE\command" -Name 'DelegateExecute' -Value ''
} else {
    Write-Warning "Klávesa Win+E už je přesměrovaná jinou aplikací; nechávám ji být."
}

"Složky a disky se teď otevírají v Souborech. Zpět: scripts\restore-explorer.ps1"
