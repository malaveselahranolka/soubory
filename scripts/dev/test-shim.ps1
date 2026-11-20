<#
  Sandbox test of install-/uninstall-shim.ps1: fake HKCU roots, temp install dirs, a second
  process that keeps the shim DLL mapped. Fails if the real CLSID key or RunOnce values change.
  -SourceDir: a folder with a built fm_dialog_shim.dll (dist\shim after build-release.ps1).
#>
param([Parameter(Mandatory)][string]$Root, [string]$SourceDir = (Join-Path (Split-Path -Parent (Split-Path -Parent $PSScriptRoot)) 'dist\shim'))
$ErrorActionPreference = 'Stop'
$scripts = Split-Path -Parent $PSScriptRoot
$t = 'HKCU:\Software\fm-test'
$cls = "$t\Classes"; $reg = "$t\CurrentVersion"
$clsid = '{DC1C5A9C-E88A-4dde-A5A1-60F82A20AEF7}'
$key = "$cls\CLSID\$clsid"
$dir = Join-Path $Root 'inst\shim'; $dll = "$dir\fm_dialog_shim.dll"
$fails = 0
function Check([string]$name, [bool]$ok) { if ($ok) { "  PASS  $name" } else { "  FAIL  $name"; $script:fails++ } }
function Server { if (Test-Path "$key\InprocServer32") { (Get-Item "$key\InprocServer32").GetValue('') } }
function Cleanup { (Get-ItemProperty "$reg\RunOnce" -ErrorAction SilentlyContinue).'FinderWin.Shim.Cleanup' }
function RealState {
    $ro = Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\RunOnce' -ErrorAction SilentlyContinue
    @((Test-Path "HKCU:\Software\Classes\CLSID\$clsid"), $ro.'FinderWin.Shim.Cleanup', $ro.'FinderWin.DialogKit.Cleanup') -join '|'
}
$Install = { param($d) & "$scripts\install-shim.ps1" -InstallDir $d -ClassesRoot $cls -RegRoot $reg -SourceDir $SourceDir *>&1 | Out-Null }
$Uninstall = { param($d) & "$scripts\uninstall-shim.ps1" -InstallDir $d -ClassesRoot $cls -RegRoot $reg *>&1 | Out-Null }

if (-not (Test-Path "$SourceDir\fm_dialog_shim.dll")) { throw "Chybí $SourceDir\fm_dialog_shim.dll" }
$before = RealState
Remove-Item -Recurse -Force $t -ErrorAction SilentlyContinue
Remove-Item -Recurse -Force $Root -ErrorAction SilentlyContinue

'=== install'
& $Install $dir
Check 'dll + marker copied' ((Test-Path $dll) -and (Test-Path "$dir\.fm-install"))
Check 'key points at the installed dll' ((Server) -eq $dll)
$tm = (Get-Item "HKLM:\SOFTWARE\Classes\CLSID\$clsid\InprocServer32").GetValue('ThreadingModel')
Check 'ThreadingModel copied from HKLM' ((Get-Item "$key\InprocServer32").GetValue('ThreadingModel') -eq $tm)
Check 'AppContainer read granted' ([bool]((Get-Acl $dir).GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier]) | Where-Object { $_.IdentityReference.Value -eq 'S-1-15-2-1' }))
Check 'no cleanup pending' (-not (Cleanup))
& $Install $dir
Check 'reinstall over our own key' ((Server) -eq $dll)

'=== uninstall while an app has the dll loaded'
$holderCmd = @"
Add-Type -Name K -Namespace W -MemberDefinition '[DllImport("kernel32", CharSet=CharSet.Unicode)] public static extern System.IntPtr LoadLibraryW(string p);'
if ([W.K]::LoadLibraryW('$dll') -ne 0) { Set-Content '$Root\ready' 1 }; Start-Sleep 60
"@
$enc = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($holderCmd))
$holder = Start-Process powershell -PassThru -WindowStyle Hidden -ArgumentList ('-NoProfile', '-EncodedCommand', $enc)
for ($i = 0; $i -lt 150 -and -not (Test-Path "$Root\ready"); $i++) { Start-Sleep -Milliseconds 100 }
Check 'holder mapped the dll' (Test-Path "$Root\ready")
& $Uninstall $dir
Check 'key removed (before any file)' (-not (Test-Path $key))
Check 'locked dll set aside' (@(Get-ChildItem $dir -Filter '*.del').Count -eq 1)
Check 'whole-dir cleanup under its own name' ((Cleanup) -match 'rd /s /q')

'=== reinstall while still locked'
& $Install $dir
Check 'reinstalled' ((Server) -eq $dll -and (Test-Path $dll))
Check 'cleanup switched to *.del only' ((Cleanup) -match 'del /f /s /q')
Stop-Process -Id $holder.Id -Force; $holder.WaitForExit()
& $Uninstall $dir
Check 'directory removed' (-not (Test-Path $dir))
Check 'cleanup value removed' (-not (Cleanup))
Check 'nothing left under the classes root' (-not (Test-Path "$cls\CLSID"))

'=== move to another folder while the old dll is locked (the installer after a script install)'
& $Install $dir
Remove-Item "$Root\ready" -ErrorAction SilentlyContinue
$holder = Start-Process powershell -PassThru -WindowStyle Hidden -ArgumentList ('-NoProfile', '-EncodedCommand', $enc)
for ($i = 0; $i -lt 150 -and -not (Test-Path "$Root\ready"); $i++) { Start-Sleep -Milliseconds 100 }
& $Uninstall $dir
$dir2 = Join-Path $Root 'inst2\shim'
& $Install $dir2
Check 'new folder installed' ((Server) -eq "$dir2\fm_dialog_shim.dll")
Check "the old folder's cleanup stays pending" ((Cleanup) -like "*`"$dir`"*")
Stop-Process -Id $holder.Id -Force; $holder.WaitForExit()
& $Uninstall $dir2
Check "uninstalling the new folder leaves the old folder's cleanup" ((Cleanup) -like "*`"$dir`"*" -and -not (Test-Path $dir2))
Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
Remove-ItemProperty "$reg\RunOnce" -Name 'FinderWin.Shim.Cleanup' -ErrorAction SilentlyContinue

'=== shim and dialogkit side by side'
$dk = Join-Path (Split-Path -Parent $scripts) 'dist\dialogkit'
if (Test-Path "$dk\fm-dialogkit.exe") {
    $dkDir = Join-Path $Root 'inst\dialogkit'
    & "$scripts\install-dialogkit.ps1" -InstallDir $dkDir -RegRoot $reg -SourceDir $dk -NoStart *>&1 | Out-Null
    & $Install $dir
    & "$scripts\uninstall-dialogkit.ps1" -InstallDir $dkDir -RegRoot $reg *>&1 | Out-Null
    Check 'uninstalling dialogkit keeps the shim' ((Test-Path $dll) -and (Server) -eq $dll -and -not (Test-Path $dkDir))
    & $Uninstall $dir
} else { '  SKIP  dist\dialogkit not built' }

'=== refusals'
New-Item -Force "$key\InprocServer32" | Out-Null
Set-ItemProperty "$key\InprocServer32" -Name '(default)' -Value 'C:\Other\their-shim.dll'
$threw = $false; try { & $Install $dir } catch { $threw = $true }
Check "another app's override is not replaced" ($threw -and (Server) -eq 'C:\Other\their-shim.dll' -and -not (Test-Path $dll))
& $Uninstall $dir
Check "uninstall leaves another app's override" ((Server) -eq 'C:\Other\their-shim.dll')
Remove-Item -Recurse -Force "$cls\CLSID"
New-Item -ItemType Directory -Force "$Root\foreign" | Out-Null; Set-Content "$Root\foreign\keep.txt" 'x'
$threw = $false; try { & $Uninstall "$Root\foreign" } catch { $threw = $true }
Check 'foreign dir untouched' ($threw -and (Test-Path "$Root\foreign\keep.txt"))
$threw = $false; try { & "$scripts\install-shim.ps1" -InstallDir $dir -ClassesRoot 'HKLM:\Software\Classes' -SourceDir $SourceDir *>&1 | Out-Null } catch { $threw = $true }
Check 'non-HKCU root refused' ($threw -and -not (Test-Path $dll))

Remove-Item -Recurse -Force $t -ErrorAction SilentlyContinue
Check 'real CLSID key and RunOnce unchanged (guard)' ((RealState) -eq $before)
if ($fails) { "FAILED: $fails" } else { 'ALL PASSED' }
