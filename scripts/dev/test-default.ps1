<#
  Sandbox test of install-fm / set-default-fm / restore-explorer / uninstall-fm: fake HKCU roots,
  temp install and Start menu dirs. Fails if the real keys these scripts touch change.
#>
param([Parameter(Mandatory)][string]$Root)
$ErrorActionPreference = 'Stop'
$scripts = Split-Path -Parent $PSScriptRoot
$t = 'HKCU:\Software\fm-test'
$reg = "$t\CurrentVersion"; $cls = "$t\Classes"; $bak = "$t\Backup"
$dir = Join-Path $Root 'inst\app'; $menu = Join-Path $Root 'menu'
$fails = 0
function Check([string]$name, [bool]$ok) { if ($ok) { "  PASS  $name" } else { "  FAIL  $name"; $script:fails++ } }
function Def([string]$k) { if (Test-Path $k) { (Get-Item $k).GetValue('', '<none>') } else { '<nokey>' } }
function RealState {
    @(
        (Def 'HKCU:\Software\Classes\Directory\shell'),
        (Def 'HKCU:\Software\Classes\Drive\shell'),
        (Test-Path 'HKCU:\Software\Classes\CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}'),
        (Test-Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\App Paths\fm.exe'),
        (Test-Path 'HKCU:\Software\fm'),
        (Test-Path (Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\Soubory.lnk'))
    ) -join '|'
}

$before = RealState
Remove-Item -Recurse -Force $t -ErrorAction SilentlyContinue
Remove-Item -Recurse -Force $Root -ErrorAction SilentlyContinue
# Another app already set a default verb for drives: it must come back afterwards.
New-Item -Force "$cls\Drive\shell" | Out-Null
Set-ItemProperty "$cls\Drive\shell" -Name '(default)' -Value 'other.open'

'=== install'
& "$scripts\install-fm.ps1" -InstallDir $dir -RegRoot $reg -StartMenuDir $menu | Out-Null
Check 'exe + marker copied' ((Test-Path "$dir\fm.exe") -and (Test-Path "$dir\.fm-install"))
Check 'App Paths entry' ((Get-Item "$reg\App Paths\fm.exe").GetValue('') -eq "$dir\fm.exe")
Check 'Start menu shortcut' (Test-Path "$menu\Soubory.lnk")

'=== set default'
& "$scripts\set-default-fm.ps1" -Exe "$dir\fm.exe" -ClassesRoot $cls -BackupRoot $bak | Out-Null
Check 'Directory verb is ours' ((Def "$cls\Directory\shell") -eq 'fm.open')
Check 'Directory command' ((Def "$cls\Directory\shell\fm.open\command") -eq "`"$dir\fm.exe`" `"%1`"")
Check 'Drive verb is ours' ((Def "$cls\Drive\shell") -eq 'fm.open')
$wine = "$cls\CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}\shell\opennewwindow\command"
Check 'Win+E command' ((Def $wine) -eq "`"$dir\fm.exe`"" -and (Get-Item $wine).GetValue('DelegateExecute', $null) -eq '')
& "$scripts\set-default-fm.ps1" -Exe "$dir\fm.exe" -ClassesRoot $cls -BackupRoot $bak | Out-Null
Check 'second run keeps the original backup' ((Get-ItemProperty $bak).'Drive.shell' -eq 'other.open')

'=== restore'
& "$scripts\restore-explorer.ps1" -ClassesRoot $cls -BackupRoot $bak | Out-Null
Check 'Drive default restored' ((Def "$cls\Drive\shell") -eq 'other.open' -and -not (Test-Path "$cls\Drive\shell\fm.open"))
Check 'Directory key removed' (-not (Test-Path "$cls\Directory\shell"))
Check 'Win+E key removed' (-not (Test-Path "$cls\CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}"))
Check 'backup removed' (-not (Test-Path $bak))

'=== uninstall (with fm still the default)'
& "$scripts\set-default-fm.ps1" -Exe "$dir\fm.exe" -ClassesRoot $cls -BackupRoot $bak | Out-Null
& "$scripts\uninstall-fm.ps1" -InstallDir $dir -RegRoot $reg -ClassesRoot $cls -BackupRoot $bak -StartMenuDir $menu | Out-Null
Check 'folders given back' ((Def "$cls\Drive\shell") -eq 'other.open' -and -not (Test-Path "$cls\Directory\shell"))
Check 'install dir removed' (-not (Test-Path $dir))
Check 'App Paths and shortcut removed' (-not (Test-Path "$reg\App Paths\fm.exe") -and -not (Test-Path "$menu\Soubory.lnk"))

'=== refusals'
New-Item -ItemType Directory -Force "$Root\foreign" | Out-Null; Set-Content "$Root\foreign\keep.txt" 'x'
$threw = $false; try { & "$scripts\uninstall-fm.ps1" -InstallDir "$Root\foreign" -RegRoot $reg -ClassesRoot $cls -BackupRoot $bak -StartMenuDir $menu | Out-Null } catch { $threw = $true }
Check 'foreign dir untouched' ($threw -and (Test-Path "$Root\foreign\keep.txt"))
$threw = $false; try { & "$scripts\set-default-fm.ps1" -Exe "$Root\foreign\keep.txt" -ClassesRoot 'HKLM:\Software\Classes' | Out-Null } catch { $threw = $true }
Check 'non-HKCU root refused' $threw

Remove-Item -Recurse -Force $t -ErrorAction SilentlyContinue
Check 'real keys unchanged (guard)' ((RealState) -eq $before)
if ($fails) { "FAILED: $fails" } else { 'ALL PASSED' }
