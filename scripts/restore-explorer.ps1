<#
.SYNOPSIS
  Gives folders, drives, Win+E, This PC, Recycle Bin and Network back to File Explorer (undoes
  scripts\set-default-fm.ps1).
.DESCRIPTION
  Undoes what the backup under HKCU\Software\fm\Backup records, and leaves alone every place
  another app has taken over since. In this order: deletes each verb key set-default-fm.ps1
  created ("verb:") while its command is still fm's; puts back each previous default value with
  its kind ("default:") or deletes the one that was absent before ("absent:"), only while the
  shell's default is still "open" (or was deleted) and no other app's "open" verb is there; then
  prunes each key tree it had to create ("created:") bottom-up, deleting only keys left with no
  subkeys and no values. Then removes HKCU\Software\fm\Redirect and the backup. Also undoes an
  install by the previous version (verb "fm.open", backup values "Directory.shell",
  "WinE.existed" …). Does nothing if fm was never made the default.

  Emergency recovery when fm.exe or these scripts are gone (cmd or PowerShell). These lines also
  delete whatever another app had at these places and do not put earlier values back; if you can,
  run this script afterwards, it restores them from the backup:
    reg delete "HKCU\Software\Classes\Directory\shell\open" /f
    reg delete "HKCU\Software\Classes\Directory\shell" /ve /f
    reg delete "HKCU\Software\Classes\Drive\shell\open" /f
    reg delete "HKCU\Software\Classes\Drive\shell" /ve /f
    reg delete "HKCU\Software\Classes\Folder\shell\explore" /f
    reg delete "HKCU\Software\Classes\CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}\shell\opennewwindow" /f
    reg delete "HKCU\Software\Classes\CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}\shell\open" /f
    reg delete "HKCU\Software\Classes\CLSID\{20D04FE0-3AEA-1069-A2D8-08002B30309D}\shell\open" /f
    reg delete "HKCU\Software\Classes\CLSID\{20D04FE0-3AEA-1069-A2D8-08002B30309D}\shell" /ve /f
    reg delete "HKCU\Software\Classes\CLSID\{645FF040-5081-101B-9F08-00AA002F954E}\shell\open" /f
    reg delete "HKCU\Software\Classes\CLSID\{645FF040-5081-101B-9F08-00AA002F954E}\shell" /ve /f
    reg delete "HKCU\Software\Classes\CLSID\{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}\shell\open" /f
    reg delete "HKCU\Software\Classes\CLSID\{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}\shell" /ve /f
    reg delete "HKCU\Software\fm" /v Redirect /f
#>
param(
    [string]$ClassesRoot = 'HKCU:\Software\Classes',
    [string]$FmRoot = 'HKCU:\Software\fm',
    [string]$BackupRoot = (Join-Path $FmRoot 'Backup'),
    [switch]$Quiet
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'lib\install-common.ps1')
Assert-HkcuRoot $ClassesRoot
Assert-HkcuRoot $FmRoot
Assert-HkcuRoot $BackupRoot
Assert-NotElevated

if (-not (Test-Path $BackupRoot)) {
    if (-not $Quiet) { "Soubory nebyly výchozí; není co vracet." }
    return
}

function Remove-IfEmpty([string]$key) {
    if ((Test-Path $key) -and -not (Get-ChildItem $key) -and -not ((Get-Item $key).ValueCount)) { Remove-Item $key -Force }
}
function Remove-EmptyTree([string]$key) {
    if (-not (Test-Path $key)) { return }
    foreach ($c in @(Get-ChildItem $key)) { Remove-EmptyTree $c.PSPath }
    Remove-IfEmpty $key
}

$b = Get-Item $BackupRoot
$names = @($b.GetValueNames())
if ($names -contains 'WinE.existed') {
    # Previous version: verb "fm.open" on Directory and Drive, and Win+E's opennewwindow.
    foreach ($k in 'Directory\shell', 'Drive\shell') {
        $shell = Join-Path $ClassesRoot $k
        if (Test-Path "$shell\fm.open") { Remove-Item "$shell\fm.open" -Recurse -Force }
        $name = $k -replace '\\', '.'
        if ($names -contains $name) {
            Set-ItemProperty $shell -Name '(default)' -Value $b.GetValue($name)
        } elseif (Test-Path $shell) {
            Remove-RegValue $shell ''
            # That version did not record whether the key existed; drop it when nothing else lives there.
            Remove-IfEmpty $shell
        }
    }
    if (-not $b.GetValue('WinE.existed')) {
        $root = Join-Path $ClassesRoot 'CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}'
        if (Test-Path "$root\shell\opennewwindow") { Remove-Item "$root\shell\opennewwindow" -Recurse -Force }
        Remove-IfEmpty "$root\shell"
        Remove-IfEmpty $root
    }
} else {
    foreach ($n in $names -like 'verb:*') {
        $k = Join-Path $ClassesRoot $n.Substring(5)
        if (-not (Test-Path $k)) { continue }
        if (Test-FmVerb $k $b.GetValue($n)) { Remove-Item $k -Recurse -Force }
        else { Write-Warning "Klíč $k teď patří jiné aplikaci; nechávám ho být." }
    }
    # An "open" verb still there now is another app's. A deleted default (the emergency lines) is
    # put back; any value other than "open" was set by another app since.
    foreach ($n in @($names -like 'default:*') + @($names -like 'absent:*')) {
        $k = Join-Path $ClassesRoot $n.Substring($n.IndexOf(':') + 1)
        if (-not (Test-Path $k)) { continue }
        $cur = (Get-Item $k).GetValue('', $null, 'DoNotExpandEnvironmentNames')
        if (($null -ne $cur -and $cur -ne 'open') -or (Test-Path "$k\open")) {
            Write-Warning "Výchozí akci $k teď určuje jiná aplikace; nechávám ji být."
        } elseif ($n -like 'default:*') { Copy-RegValue $BackupRoot $n $k '' }
        else { Remove-RegValue $k '' }
    }
    foreach ($n in $names -like 'created:*') { Remove-EmptyTree (Join-Path $ClassesRoot $n.Substring(8)) }
}
Remove-ItemProperty -Path $FmRoot -Name 'Redirect' -ErrorAction SilentlyContinue
Remove-Item -Path $BackupRoot -Recurse -Force
Remove-IfEmpty $FmRoot
if (-not $Quiet) { "Složky se zase otevírají v Průzkumníkovi." }
