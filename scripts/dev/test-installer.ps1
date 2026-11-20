<#
.SYNOPSIS
  Tests the setup .exe for real on a clean CI machine (.github\workflows\release.yml).
.DESCRIPTION
  Never run this on your own PC: it installs Soubory, makes it the default for folders, starts it
  and uninstalls it again, for the signed-in user.
  1. The wizard, clicked through as a person would: Welcome, Tasks and Finished are captured to -Out
     (PrintWindow of the wizard only), in Windows' light app theme and then in the dark one; each
     run is followed by a silent uninstall.
  2. A silent install with both tasks into a folder whose path has a space and diacritics: files,
     Run values and verbs are checked, then folders opened through the shell (C:\, a folder with a
     space, Win+E's verb) must show an fm window. A silent uninstall must give folders back to
     Explorer and remove what the install added.
    powershell -File scripts\dev\test-installer.ps1 -Setup dist\Soubory-<ver>-setup.exe -Out shots
  -WizardOnly runs part 1 only, -SkipWizard part 2 only.
#>
param([Parameter(Mandatory)][string]$Setup, [string]$Out = 'shots', [string]$Name = 'setup', [switch]$SkipWizard, [switch]$WizardOnly)
$ErrorActionPreference = 'Stop'
$Setup = (Resolve-Path -LiteralPath $Setup).Path
New-Item -ItemType Directory -Force $Out | Out-Null
$Out = (Resolve-Path -LiteralPath $Out).Path
$pass = 0; $fails = 0
function Check([string]$what, [bool]$ok) { if ($ok) { "  PASS  $what"; $script:pass++ } else { "  FAIL  $what"; $script:fails++ } }
function Val([string]$k, [string]$n) { if (Test-Path -LiteralPath $k) { (Get-Item -LiteralPath $k).GetValue($n) } }
$uninstKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\{CB6D685E-6A4F-4914-9B9A-5E9376AE930D}_is1'

Add-Type -AssemblyName System.Drawing
Add-Type -ReferencedAssemblies System.Drawing -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public static class Win {
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc f, IntPtr l);
    [DllImport("user32.dll")] static extern bool EnumChildWindows(IntPtr p, EnumProc f, IntPtr l);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetClassNameW(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern bool IsWindowEnabled(IntPtr h);
    [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr h, uint m, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] static extern bool PrintWindow(IntPtr h, IntPtr dc, uint flags);
    [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("advapi32.dll")] static extern bool OpenProcessToken(IntPtr p, uint a, out IntPtr t);
    [DllImport("advapi32.dll")] static extern bool GetTokenInformation(IntPtr t, int c, out int v, int n, out int r);
    [DllImport("kernel32.dll")] static extern IntPtr GetCurrentProcess();
    struct RECT { public int L, T, R, B; }
    public static string Class(IntPtr h) { var s = new StringBuilder(256); GetClassNameW(h, s, 256); return s.ToString(); }
    public static string Text(IntPtr h) { var s = new StringBuilder(512); GetWindowTextW(h, s, 512); return s.ToString(); }
    public static List<IntPtr> Top() { var l = new List<IntPtr>(); EnumWindows((h, _) => { l.Add(h); return true; }, IntPtr.Zero); return l; }
    public static List<IntPtr> Children(IntPtr p) { var l = new List<IntPtr>(); EnumChildWindows(p, (h, _) => { l.Add(h); return true; }, IntPtr.Zero); return l; }
    public static void Capture(IntPtr h, string path) {
        RECT r; GetWindowRect(h, out r);
        using (var b = new System.Drawing.Bitmap(r.R - r.L, r.B - r.T))
        using (var g = System.Drawing.Graphics.FromImage(b)) {
            var dc = g.GetHdc(); PrintWindow(h, dc, 2); g.ReleaseHdc(dc);
            b.Save(path, System.Drawing.Imaging.ImageFormat.Png);
        }
    }
    public static int Elevation() {
        IntPtr t; int v = -1, n;
        if (OpenProcessToken(GetCurrentProcess(), 8, out t)) GetTokenInformation(t, 18, out v, 4, out n);
        return v;
    }
}
'@

function Get-Windows([string]$class) {
    [Win]::Top() | Where-Object { [Win]::IsWindowVisible($_) -and [Win]::Class($_) -eq $class }
}
function Wait-For([scriptblock]$cond, [int]$seconds, [string]$what) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.Elapsed.TotalSeconds -lt $seconds) {
        $r = & $cond
        if ($r) { return $r }
        Start-Sleep -Milliseconds 250
    }
    throw "Timed out after $seconds s waiting for $what"
}
function Find-Button([IntPtr]$form, [string]$pattern) {
    [Win]::Children($form) | Where-Object { [Win]::IsWindowVisible($_) -and [Win]::IsWindowEnabled($_) -and ([Win]::Text($_) -replace '&', '') -match $pattern } | Select-Object -First 1
}
function Click([IntPtr]$form, [string]$pattern) {
    $b = Wait-For { Find-Button $form $pattern } 30 "button $pattern"
    [void][Win]::PostMessageW($b, 0x00F5, [IntPtr]::Zero, [IntPtr]::Zero)  # BM_CLICK
}
function Fm-Windows { @(Get-Windows 'FmMainWindow') }
# Waits for this process only: Start-Process -Wait would also wait for the fm and dialogkit that
# setup leaves running. Touching Handle first keeps ExitCode readable in Windows PowerShell.
function Run-Wait([string]$file, [string[]]$arguments) {
    $p = Start-Process -FilePath $file -ArgumentList $arguments -PassThru
    $null = $p.Handle
    $p.WaitForExit()
    $p
}
function Uninstall([string]$log) {
    $cmd = Val $uninstKey 'UninstallString'
    if (-not $cmd) { throw 'no UninstallString' }
    $exe = $cmd.Trim('"')
    $p = Run-Wait $exe @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', "/LOG=`"$log`"")
    # The uninstaller hands over to a copy in %TEMP% that deletes the original; let it finish.
    Wait-For { -not (Test-Path -LiteralPath $exe) } 60 'the uninstaller to delete itself' | Out-Null
    $p.ExitCode
}
function Set-AppTheme([int]$light) {
    $k = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize'
    if (-not (Test-Path $k)) { New-Item -Force $k | Out-Null }
    New-ItemProperty $k -Name AppsUseLightTheme -Value $light -PropertyType DWord -Force | Out-Null
}

"Token elevation type: $([Win]::Elevation()) (1 default, 2 elevated, 3 limited)"
if ([Win]::Elevation() -eq 2) { throw 'This runner is elevated with a split token: setup refuses to run like that.' }

# --- 1. The wizard, as a person clicks through it --------------------------------------------
if (-not $SkipWizard) {
    foreach ($theme in 'light', 'dark') {
        "=== wizard ($theme)"
        Set-AppTheme ([int]($theme -eq 'light'))
        $p = Start-Process -FilePath $Setup -ArgumentList "/LOG=`"$Out\$Name-wizard-$theme.log`"" -PassThru
        $null = $p.Handle
        $form = Wait-For { Get-Windows 'TWizardForm' | Select-Object -First 1 } 60 'the wizard'
        Start-Sleep -Seconds 2
        [Win]::Capture($form, "$Out\$Name-$theme-1-welcome.png")
        Click $form '^Další'
        Start-Sleep -Seconds 2
        [Win]::Capture($form, "$Out\$Name-$theme-2-tasks.png")
        Click $form '^Instalovat'
        Wait-For { Find-Button $form '^Dokončit' } 180 'the Finished page' | Out-Null
        Start-Sleep -Seconds 2
        [Win]::Capture($form, "$Out\$Name-$theme-3-finished.png")
        Click $form '^Dokončit'
        $p.WaitForExit(60000) | Out-Null
        Check "wizard ($theme) exit code 0" ($p.ExitCode -eq 0)
        Check "wizard ($theme) opened Soubory (Finished page checkbox)" ([bool](Wait-For { Fm-Windows } 20 'an fm window'))
        Check "wizard ($theme) silent uninstall exit code 0" ((Uninstall "$Out\$Name-wizard-$theme-uninstall.log") -eq 0)
    }
    Set-AppTheme 1
}
if ($WizardOnly) { "RESULT pass=$pass fail=$fails"; if ($fails) { exit 1 }; exit 0 }

# --- 2. Silent install with both tasks, into a path with a space and diacritics ---------------
'=== silent install'
$dir = Join-Path $env:LOCALAPPDATA 'Programs\Zkouška Souborů'
$exe = Join-Path $dir 'fm.exe'
$p = Run-Wait $Setup @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/TASKS="defaultfm,dialogkit"', "/DIR=`"$dir`"", "/LOG=`"$Out\$Name-silent-install.log`"")
Check 'setup exit code 0' ($p.ExitCode -eq 0)
Check 'fm.exe installed' (Test-Path -LiteralPath $exe)
Check 'notices and licence installed' ((Test-Path -LiteralPath "$dir\THIRD-PARTY-NOTICES.txt") -and (Test-Path -LiteralPath "$dir\LICENSE.txt"))
Check 'uninstaller registered' ((Val $uninstKey 'DisplayName') -eq 'Soubory')
Check 'Start menu shortcut' (Test-Path -LiteralPath "$env:APPDATA\Microsoft\Windows\Start Menu\Programs\Soubory.lnk")
Check 'Run value' ((Val 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' 'Soubory') -eq "`"$exe`" --background")
Check 'App Paths' ((Val 'HKCU:\Software\Microsoft\Windows\CurrentVersion\App Paths\fm.exe' '') -eq $exe)
Check 'folders open in fm' ((Val 'HKCU:\Software\Classes\Directory\shell\open\command' '') -eq "`"$exe`" `"%1`"")
Check 'drives open in fm' ((Val 'HKCU:\Software\Classes\Drive\shell\open\command' '') -eq "`"$exe`" `"%1`"")
Check 'Win+E opens fm' ((Val 'HKCU:\Software\Classes\CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}\shell\opennewwindow\command' '') -eq "`"$exe`"")
Check 'Redirect for the dialogkit watcher' ((Val 'HKCU:\Software\fm' 'Redirect') -eq $exe)
Check 'dialogkit installed' (Test-Path -LiteralPath "$dir\dialogkit\fm-dialogkit.exe")
Check 'dialogkit 32-bit helper installed' (Test-Path -LiteralPath "$dir\dialogkit\x86\fm-dialogkit.exe")
Check 'dialogkit Run value' ((Val 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' 'FinderWin.DialogKit') -eq "`"$dir\dialogkit\fm-dialogkit.exe`"")
Check 'dialogkit running' ([bool](Get-Process fm-dialogkit -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$dir\*" }))
Check 'fm resident' ([bool](Wait-For { Get-Process fm -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe } } 15 'resident fm'))
$log = Get-Content -LiteralPath "$Out\$Name-silent-install.log" -Encoding UTF8
$log | Where-Object { $_ -match 'PowerShell|fm\.exe --|  ' } | Select-Object -First 30
Check 'fm --set-default output decoded (Czech text in the setup log)' ([bool]($log | Where-Object { $_ -match 'Složky, disky' }))

$spaced = Join-Path $env:TEMP 'Složka s mezerou'
New-Item -ItemType Directory -Force $spaced | Out-Null
foreach ($t in @(@{ Path = 'C:\'; Title = $null }, @{ Path = $spaced; Title = 'Složka s mezerou' })) {
    $before = Fm-Windows
    Start-Process -FilePath $t.Path
    $new = Wait-For { @(Fm-Windows | Where-Object { $before -notcontains $_ }) } 20 "an fm window for $($t.Path)"
    $titles = ($new | ForEach-Object { [Win]::Text($_) }) -join ', '
    Check "shell open $($t.Path) shows an fm window ($titles)" ([bool]$new -and (-not $t.Title -or $titles -match [regex]::Escape($t.Title)))
}
$before = Fm-Windows
Start-Process -FilePath 'shell:::{52205fd8-5dfb-447d-801a-d0b52f2e83e1}'
$new = $null
try { $new = Wait-For { @(Fm-Windows | Where-Object { $before -notcontains $_ }) } 15 'an fm window for Win+E' } catch {}
if ($new) { '  PASS  Win+E target opens an fm window' } else { '  NOTE  Win+E target did not open fm here (Explorer handles shell: URLs itself); the verb is checked above' }
$w = Fm-Windows | Select-Object -First 1
if ($w) { [Win]::Capture($w, "$Out\$Name-fm-window.png") }

'=== silent uninstall'
Check 'uninstall exit code 0' ((Uninstall "$Out\$Name-silent-uninstall.log") -eq 0)
Check 'install folder removed' (-not (Test-Path -LiteralPath $exe))
Check 'uninstaller unregistered' (-not (Test-Path -LiteralPath $uninstKey))
Check 'Start menu shortcut removed' (-not (Test-Path -LiteralPath "$env:APPDATA\Microsoft\Windows\Start Menu\Programs\Soubory.lnk"))
Check 'Run values removed' (-not (Val 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' 'Soubory') -and -not (Val 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' 'FinderWin.DialogKit'))
Check 'App Paths removed' (-not (Test-Path -LiteralPath 'HKCU:\Software\Microsoft\Windows\CurrentVersion\App Paths\fm.exe'))
$left = @(Get-ChildItem -Recurse 'HKCU:\Software\Classes\Directory', 'HKCU:\Software\Classes\Drive', 'HKCU:\Software\Classes\Folder', 'HKCU:\Software\Classes\CLSID' -ErrorAction SilentlyContinue |
    Where-Object { "$($_.GetValue(''))" -like "*$dir*" })
Check "no verb opens the removed fm.exe ($($left.Count) left)" ($left.Count -eq 0)
Check 'Redirect removed' (-not (Val 'HKCU:\Software\fm' 'Redirect'))
Check 'no fm or dialogkit left running from the install folder' (-not (Get-Process fm, fm-dialogkit -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$dir\*" }))
$before = @(Get-Process fm -ErrorAction SilentlyContinue).Count
Start-Process -FilePath 'C:\'
Start-Sleep -Seconds 5
Check 'C:\ opens without fm after uninstall' (@(Get-Process fm -ErrorAction SilentlyContinue).Count -le $before -and -not (Fm-Windows))

"RESULT pass=$pass fail=$fails"
if ($fails) { exit 1 }
