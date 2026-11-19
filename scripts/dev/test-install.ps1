<#
  Sandbox test of install-/uninstall-dialogkit.ps1: fake HKCU root, temp install dir, a second
  process that keeps the hook DLL mapped. Fails if the real Run/RunOnce values change.
#>
param([Parameter(Mandatory)][string]$Root)
$ErrorActionPreference = 'Stop'
$scripts = Split-Path -Parent $PSScriptRoot
$regRoot = 'HKCU:\Software\fm-test\CurrentVersion'
$dir = Join-Path $Root 'inst\dialogkit'
$real = 'HKCU:\Software\Microsoft\Windows\CurrentVersion'
$fails = 0
function Check([string]$name, [bool]$ok) { if ($ok) { "  PASS  $name" } else { "  FAIL  $name"; $script:fails++ } }
function RealState { (Get-ItemProperty "$real\Run" -ErrorAction SilentlyContinue).'FinderWin.DialogKit', (Get-ItemProperty "$real\RunOnce" -ErrorAction SilentlyContinue).'FinderWin.DialogKit.Cleanup' -join '|' }

$before = RealState
Remove-Item -Recurse -Force 'HKCU:\Software\fm-test' -ErrorAction SilentlyContinue
Remove-Item -Recurse -Force $Root -ErrorAction SilentlyContinue

'=== install'
& "$scripts\install-dialogkit.ps1" -InstallDir $dir -RegRoot $regRoot -NoStart | Out-Null
Check 'exe + dll + x86 copied' ((Test-Path "$dir\fm-dialogkit.exe") -and (Test-Path "$dir\fm_dialog_hook.dll") -and (Test-Path "$dir\x86\fm_dialog_hook.dll"))
Check 'Run value points at exe' ((Get-ItemProperty "$regRoot\Run").'FinderWin.DialogKit' -eq "`"$dir\fm-dialogkit.exe`"")
Check 'AppContainer read granted' ([bool]((Get-Acl $dir).GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier]) | Where-Object { $_.IdentityReference.Value -eq 'S-1-15-2-1' }))
Check 'no cleanup pending' (-not (Get-ItemProperty "$regRoot\RunOnce" -ErrorAction SilentlyContinue).'FinderWin.DialogKit.Cleanup')

'=== uninstall while the hook DLL is mapped'
$holderCmd = @"
Add-Type -Name K -Namespace W -MemberDefinition '[DllImport("kernel32", CharSet=CharSet.Unicode)] public static extern System.IntPtr LoadLibraryW(string p);'
if ([W.K]::LoadLibraryW('$dir\fm_dialog_hook.dll') -ne 0) { Set-Content '$Root\ready' 1 }; Start-Sleep 60
"@
$enc = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($holderCmd))
$holder = Start-Process powershell -PassThru -WindowStyle Hidden -ArgumentList ('-NoProfile', '-EncodedCommand', $enc)
for ($i = 0; $i -lt 150 -and -not (Test-Path "$Root\ready"); $i++) { Start-Sleep -Milliseconds 100 }
Check 'holder mapped the dll' (Test-Path "$Root\ready")
& "$scripts\uninstall-dialogkit.ps1" -InstallDir $dir -RegRoot $regRoot | Out-Null
Check 'Run value removed' (-not (Get-ItemProperty "$regRoot\Run" -ErrorAction SilentlyContinue).'FinderWin.DialogKit')
Check 'locked dll set aside' (@(Get-ChildItem $dir -Filter '*.del').Count -eq 1 -and -not (Test-Path "$dir\fm-dialogkit.exe"))
Check 'whole-dir cleanup registered' ((Get-ItemProperty "$regRoot\RunOnce").'FinderWin.DialogKit.Cleanup' -match 'rd /s /q')

'=== reinstall while still locked'
& "$scripts\install-dialogkit.ps1" -InstallDir $dir -RegRoot $regRoot -NoStart | Out-Null
Check 'reinstalled' (Test-Path "$dir\fm_dialog_hook.dll")
Check 'cleanup switched to *.del only' ((Get-ItemProperty "$regRoot\RunOnce").'FinderWin.DialogKit.Cleanup' -match 'del /f /s /q')

'=== uninstall after the holder exits'
Stop-Process -Id $holder.Id -Force; $holder.WaitForExit()
& "$scripts\uninstall-dialogkit.ps1" -InstallDir $dir -RegRoot $regRoot | Out-Null
Check 'directory removed' (-not (Test-Path $dir))
Check 'cleanup value removed' (-not (Get-ItemProperty "$regRoot\RunOnce" -ErrorAction SilentlyContinue).'FinderWin.DialogKit.Cleanup')

'=== refuses a directory without marker'
New-Item -ItemType Directory -Force "$Root\foreign" | Out-Null; Set-Content "$Root\foreign\keep.txt" 'x'
$threw = $false; try { & "$scripts\uninstall-dialogkit.ps1" -InstallDir "$Root\foreign" -RegRoot $regRoot | Out-Null } catch { $threw = $true }
Check 'foreign dir untouched' ($threw -and (Test-Path "$Root\foreign\keep.txt"))
$threw = $false; try { & "$scripts\install-dialogkit.ps1" -InstallDir $dir -RegRoot 'HKLM:\Software\x' -NoStart | Out-Null } catch { $threw = $true }
Check 'non-HKCU root refused' $threw

Remove-Item -Recurse -Force 'HKCU:\Software\fm-test' -ErrorAction SilentlyContinue
Check 'real Run/RunOnce unchanged (guard)' ((RealState) -eq $before)
if ($fails) { "FAILED: $fails" } else { 'ALL PASSED' }
