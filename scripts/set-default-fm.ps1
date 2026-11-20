<#
.SYNOPSIS
  Makes Soubory (fm.exe) open folders, drives, Win+E, This PC, Recycle Bin and Network instead of
  File Explorer, for the current user.
.DESCRIPTION
  Writes only under HKCU\Software\Classes. CMD = "fm.exe" "%1"; every command key also gets an
  empty DelegateExecute, so HKLM's does not leak through the merged HKCR:
    Directory\shell, Drive\shell     default verb "open", open\command = CMD
    Folder\shell\explore\command     CMD (Chrome/Edge open a folder with verb explore, class folder)
    CLSID\{52205fd8-…}\shell         opennewwindow and open -> "fm.exe" (Win+E, Start "File Explorer")
    CLSID\{20D04FE0-…} This PC, {645FF040-…} Recycle Bin, {F02C1A0D-…} Network:
                                     default verb "open", open\command = "fm.exe" "::{GUID}"
  plus the value HKCU\Software\fm\Redirect = fm.exe (the Explorer-window watcher in fm-dialogkit
  reads it). Folder\shell\open and Control Panel are never touched.

  The backup under HKCU\Software\fm\Backup is written before each change:
    verb:<key>      a verb key this script created, holding the command it wrote; restore deletes
                    it only while its command is still fm's
    default:<key>   the previous default value of a *\shell key it set, copied with its kind
                    (absent: there was none), also for a shell key it created itself
    created:<key>   the topmost key it had to create above a verb; restore only prunes that tree
                    bottom-up while keys are empty, so whatever another app adds there later stays
  A verb key another app put there, before or since, is left alone and its place stays with
  Explorer or that app. When another app changed a shell's default since, the backup takes that
  value, so restore brings back that app's choice. A second run changes nothing. An install by the
  previous version (verb "fm.open") is undone first. scripts\restore-explorer.ps1 puts everything
  back. Takes effect at once; no restart.

  Emergency recovery when fm.exe or these scripts are gone (cmd or PowerShell). These lines also
  delete whatever another app had at these places and do not put earlier values back; if you can,
  run scripts\restore-explorer.ps1 afterwards, it restores them from the backup:
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

  Tests pass -ClassesRoot, -FmRoot and -BackupRoot to work in a sandbox.
#>
param(
    [string]$Exe = (Join-Path $env:LOCALAPPDATA 'Programs\fm\app\fm.exe'),
    [string]$ClassesRoot = 'HKCU:\Software\Classes',
    [string]$FmRoot = 'HKCU:\Software\fm',
    [string]$BackupRoot = (Join-Path $FmRoot 'Backup')
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'lib\install-common.ps1')
Assert-HkcuRoot $ClassesRoot
Assert-HkcuRoot $FmRoot
Assert-HkcuRoot $BackupRoot
Assert-NotElevated
if (-not (Test-Path -LiteralPath $Exe)) { throw "Chybí $Exe — nejdřív spusť scripts\install-fm.ps1." }
$Exe = (Resolve-Path -LiteralPath $Exe).ProviderPath

# The previous version kept one all-or-nothing backup ("WinE.existed"); undo that install first so
# its keys are not mistaken for another app's and "fm.open" is not saved as the original value.
if ((Test-Path $BackupRoot) -and $null -ne (Get-Item $BackupRoot).GetValue('WinE.existed', $null)) {
    & (Join-Path $PSScriptRoot 'restore-explorer.ps1') -ClassesRoot $ClassesRoot -FmRoot $FmRoot -BackupRoot $BackupRoot -Quiet
}
foreach ($k in 'Directory\shell\fm.open', 'Drive\shell\fm.open') {
    $old = Join-Path $ClassesRoot $k
    if (Test-Path $old) { Remove-Item $old -Recurse -Force }
}

$WinE = '{52205fd8-5dfb-447d-801a-d0b52f2e83e1}'
$cmd = "`"$Exe`" `"%1`""
$entries = @(
    @{ Shell = 'Directory\shell'; Verb = 'open'; Cmd = $cmd; Default = $true; Name = 'složky' },
    @{ Shell = 'Drive\shell'; Verb = 'open'; Cmd = $cmd; Default = $true; Name = 'disky' },
    @{ Shell = 'Folder\shell'; Verb = 'explore'; Cmd = $cmd; Name = 'složky otevírané z prohlížeče' },
    @{ Shell = "CLSID\$WinE\shell"; Verb = 'opennewwindow'; Cmd = "`"$Exe`""; Name = 'Win+E' },
    @{ Shell = "CLSID\$WinE\shell"; Verb = 'open'; Cmd = "`"$Exe`""; Name = 'Průzkumník souborů v nabídce Start' }
)
# This PC, Recycle Bin, Network: %1 is not reliable for virtual folders, so the target is fixed.
$virtual = [ordered]@{ '{20D04FE0-3AEA-1069-A2D8-08002B30309D}' = 'Tento počítač'; '{645FF040-5081-101B-9F08-00AA002F954E}' = 'Koš'; '{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}' = 'Síť' }
foreach ($g in $virtual.Keys) {
    $entries += @{ Shell = "CLSID\$g\shell"; Verb = 'open'; Cmd = "`"$Exe`" `"::$g`""; Default = $true; Name = $virtual[$g] }
}

function Get-Saved { (Get-Item $BackupRoot).GetValueNames() }
function Save([string]$name, [string]$value) { New-ItemProperty $BackupRoot -Name $name -Value $value -PropertyType String -Force | Out-Null }
function Set-String([string]$key, [string]$name, [string]$value) { New-ItemProperty $key -Name $name -Value $value -PropertyType String -Force | Out-Null }

function Test-InCreated([string]$rel) {
    foreach ($n in (Get-Saved)) {
        if ($n -like 'created:*') {
            $c = $n.Substring(8)
            if ($rel -eq $c -or $rel.StartsWith("$c\", [StringComparison]::OrdinalIgnoreCase)) { return $true }
        }
    }
    $false
}

function Get-FirstMissing([string]$rel) {
    $path = ''
    foreach ($part in $rel -split '\\') {
        $path = if ($path) { "$path\$part" } else { $part }
        if (-not (Test-Path (Join-Path $ClassesRoot $path))) { return $path }
    }
}

New-KeyIfMissing $BackupRoot
$skipped = @()
foreach ($e in $entries) {
    $verbRel = "$($e.Shell)\$($e.Verb)"
    $verb = Join-Path $ClassesRoot $verbRel
    $shell = Join-Path $ClassesRoot $e.Shell
    $rec = "verb:$verbRel"; $d = "default:$($e.Shell)"; $a = "absent:$($e.Shell)"
    $saved = Get-Saved
    if ((Test-Path $verb) -and ($saved -notcontains $rec -or -not (Test-FmVerb $verb (Get-Item $BackupRoot).GetValue($rec)))) {
        # Ours once and taken over since: forget it, so restore leaves it to that app too.
        foreach ($n in $rec, $d, $a) { if ($saved -contains $n) { Remove-RegValue $BackupRoot $n } }
        Write-Warning "Klíč $verb už vytvořila jiná aplikace; nechávám ho být."
        $skipped += $e.Name
        continue
    }
    # Each change is recorded before it is made, so an interrupted run can still be restored.
    if (-not (Test-Path $verb)) {
        $first = Get-FirstMissing $verbRel
        if ($first -ne $verbRel -and -not (Test-InCreated $first)) { Save "created:$first" '' }
    }
    # Refreshed on every run, so a moved install stays ours.
    Save $rec $e.Cmd
    if ($e.Default) {
        $cur = if (Test-Path $shell) { (Get-Item $shell).GetValue('', $null, 'DoNotExpandEnvironmentNames') } else { $null }
        # "fm.open" is a leftover of the previous version, not another app's value.
        $changed = $null -ne $cur -and $cur -ne 'open' -and $cur -ne 'fm.open'
        if ($changed) {
            # Set by another app since the backup (or before the first run): that is what restore brings back.
            Remove-RegValue $BackupRoot $a
            Copy-RegValue $shell '' $BackupRoot $d
        } elseif ($saved -notcontains $d -and $saved -notcontains $a) {
            if ($cur -eq 'open') { Copy-RegValue $shell '' $BackupRoot $d } else { Save $a '' }
        }
    }
    New-KeyIfMissing "$verb\command"
    Set-String "$verb\command" '(default)' $e.Cmd
    Set-String "$verb\command" 'DelegateExecute' ''
    if ($e.Default) { Set-String $shell '(default)' 'open' }
}

New-KeyIfMissing $FmRoot
Set-String $FmRoot 'Redirect' $Exe

if ($skipped) {
    "Soubory jsou výchozí, ale v Průzkumníkovi zůstává: $($skipped -join ', ') (už je převzala jiná aplikace). Zpět: scripts\restore-explorer.ps1"
} else {
    "Složky, disky, Win+E, Tento počítač, Koš a Síť se teď otevírají v Souborech. Zpět: scripts\restore-explorer.ps1"
}
