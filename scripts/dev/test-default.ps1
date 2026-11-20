<#
  Sandbox test of install-fm / set-default-fm / restore-explorer / uninstall-fm (plus the folder
  checks of the dialogkit and shim scripts). Every registry root is a fake key under
  HKCU\Software\fm-test, every folder is under -Root, and fm.exe is a small fake compiled here.
  Fails if the real keys these scripts touch change. Fake roots prove the script logic only;
  Windows never reads them.
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts\dev\test-default.ps1
#>
param([string]$Root = (Join-Path $env:TEMP 'fm-test-default'))
$ErrorActionPreference = 'Stop'
$scripts = Split-Path -Parent $PSScriptRoot
$t = 'HKCU:\Software\fm-test'
$reg = "$t\CurrentVersion"; $run = "$reg\Run"; $runOnce = "$reg\RunOnce"; $cls = "$t\Classes"; $fmr = "$t\fm"; $bak = "$fmr\Backup"
$dir = Join-Path $Root 'inst\app'; $menu = Join-Path $Root 'menu'; $src = Join-Path $Root 'src'
$exe = "$dir\fm.exe"
# Every script call takes all of its roots from these two; a forgotten one would hit the real key.
$def = @{ ClassesRoot = $cls; FmRoot = $fmr; BackupRoot = $bak }
# The fake fm owns this mutex instead of fm's Local\fm.main, which no test may ever touch.
$testMutex = 'Local\fm-test.main'
$inst = @{ InstallDir = $dir; RegRoot = $reg; RunRoot = $run; StartMenuDir = $menu; Mutex = $testMutex }
foreach ($r in @($def.Values) + $reg, $run) { if (-not $r.StartsWith("$t\")) { throw "Test root outside $t`: $r" } }
if ($inst.Mutex -eq 'Local\fm.main') { throw 'The test must not use fm''s own mutex.' }
$env:FM_FAKE_LOG = Join-Path $Root 'fake.log'

$winE = '{52205fd8-5dfb-447d-801a-d0b52f2e83e1}'
$virt = '{20D04FE0-3AEA-1069-A2D8-08002B30309D}', '{645FF040-5081-101B-9F08-00AA002F954E}', '{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}'
$cmd = "`"$exe`" `"%1`""
$foreign = '"C:\Other\other.exe" "%1"'
$pass = 0; $fails = 0
function Check([string]$name, [bool]$ok) { if ($ok) { "  PASS  $name"; $script:pass++ } else { "  FAIL  $name"; $script:fails++ } }
function Val([string]$k, [string]$n) { if (Test-Path $k) { (Get-Item $k).GetValue($n, '<none>') } else { '<nokey>' } }
function Def([string]$k) { Val $k '' }
function Raw([string]$k, [string]$n) { (Get-Item $k).GetValue($n, $null, 'DoNotExpandEnvironmentNames') }
function Names { if (Test-Path $bak) { @((Get-Item $bak).GetValueNames() | Sort-Object) } else { @() } }
function DelDefault([string]$k) { $h = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey(($k -replace '^HKCU:\\', ''), $true); $h.DeleteValue('', $false); $h.Close() }
# Every key under $k with every value (name, kind, raw data), one sorted line per key.
function Dump([string]$k) {
    if (-not (Test-Path $k)) { return '<nokey>' }
    (@(Get-Item $k) + @(Get-ChildItem $k -Recurse) | ForEach-Object {
            $i = $_
            $vals = $i.GetValueNames() | Sort-Object | ForEach-Object { "$_=$($i.GetValueKind($_)):$($i.GetValue($_, $null, 'DoNotExpandEnvironmentNames'))" }
            "$($i.Name) {$($vals -join '; ')}"
        } | Sort-Object) -join "`n"
}
function RealState {
    $c = 'HKCU:\Software\Classes'; $cv = 'HKCU:\Software\Microsoft\Windows\CurrentVersion'
    $keys = @("$c\Directory\shell", "$c\Drive\shell", "$c\Folder\shell", "$c\CLSID\$winE", 'HKCU:\Software\fm', "$cv\App Paths\fm.exe") +
        ($virt | ForEach-Object { "$c\CLSID\$_\shell" })
    (@($keys | ForEach-Object { Dump $_ }) + (Val "$cv\Run" 'Soubory') + (Val "$cv\RunOnce" 'Soubory.Cleanup') +
        (Test-Path (Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\Soubory.lnk'))) -join "`n"
}
# Runs set-default-fm.ps1; $warn gets its warnings, $said its closing line.
function SetDefault {
    $out = @(& "$scripts\set-default-fm.ps1" -Exe $exe @def 3>&1)
    $script:warn = @($out | Where-Object { $_ -is [Management.Automation.WarningRecord] })
    $script:said = @($out | Where-Object { $_ -is [string] })[-1]
}
function Restore { $script:rwarn = @(& "$scripts\restore-explorer.ps1" @def 3>&1 | Where-Object { $_ -is [Management.Automation.WarningRecord] }) }
# The script's own message must come back; a parameter-binding error would also throw.
function Refused([string]$script, [hashtable]$a, [string]$like) {
    try { & "$scripts\$script" @a | Out-Null; $false } catch { $_.Exception.Message -like $like }
}
function FakeProcs { @(Get-Process -Name fm, helper -ErrorAction SilentlyContinue | Where-Object { $_.Path -and $_.Path.StartsWith($Root, [StringComparison]::OrdinalIgnoreCase) }) }
function FakeLog { if (Test-Path $env:FM_FAKE_LOG) { @(Get-Content $env:FM_FAKE_LOG) } else { @() } }
function WaitLog([string]$like) { for ($i = 0; $i -lt 100 -and -not (FakeLog | Where-Object { $_ -like $like }); $i++) { Start-Sleep -Milliseconds 100 } }
# The full layout set-default-fm.ps1 writes; $skip lists verbs another app owns.
function CheckLayout([string]$label, [string[]]$skip = @()) {
    $want = [ordered]@{
        'Directory\shell\open' = $cmd; 'Drive\shell\open' = $cmd; 'Folder\shell\explore' = $cmd
        "CLSID\$winE\shell\opennewwindow" = "`"$exe`""; "CLSID\$winE\shell\open" = "`"$exe`""
    }
    foreach ($g in $virt) { $want["CLSID\$g\shell\open"] = "`"$exe`" `"::$g`"" }
    foreach ($v in $want.Keys) {
        if ($skip -contains $v) { continue }
        $k = "$cls\$v\command"
        Check "$label $v command + empty DelegateExecute" ((Def $k) -eq $want[$v] -and (Val $k 'DelegateExecute') -eq '' -and
            (Get-Item $k).GetValueKind('') -eq 'String' -and (Get-Item $k).GetValueKind('DelegateExecute') -eq 'String')
    }
    foreach ($s in @('Directory\shell', 'Drive\shell') + ($virt | ForEach-Object { "CLSID\$_\shell" })) {
        if ($skip -notcontains "$s\open") { Check "$label $s default verb = open" ((Def "$cls\$s") -eq 'open') }
    }
    Check "$label Folder\shell\open not created" (-not (Test-Path "$cls\Folder\shell\open"))
    Check "$label no old fm.open verb" (-not (Test-Path "$cls\Directory\shell\fm.open") -and -not (Test-Path "$cls\Drive\shell\fm.open"))
    Check "$label Redirect = fm.exe" ((Val $fmr 'Redirect') -eq $exe)
}
# What the previous set-default-fm.ps1 wrote when Directory\shell was absent and Win+E was free.
function SeedLegacy {
    New-Item -Force $bak | Out-Null
    Set-ItemProperty $bak -Name 'Directory.shell.absent' -Value 1
    Set-ItemProperty $bak -Name 'Drive.shell' -Value 'other.open'
    Set-ItemProperty $bak -Name 'WinE.existed' -Value 0
    foreach ($s in 'Directory\shell', 'Drive\shell') {
        New-Item -Force "$cls\$s\fm.open\command" | Out-Null
        Set-ItemProperty "$cls\$s" -Name '(default)' -Value 'fm.open'
        Set-ItemProperty "$cls\$s\fm.open" -Name '(default)' -Value 'Otevřít v Souborech'
        Set-ItemProperty "$cls\$s\fm.open\command" -Name '(default)' -Value $cmd
    }
    New-Item -Force "$cls\CLSID\$winE\shell\opennewwindow\command" | Out-Null
    Set-ItemProperty "$cls\CLSID\$winE\shell\opennewwindow\command" -Name '(default)' -Value "`"$exe`""
    Set-ItemProperty "$cls\CLSID\$winE\shell\opennewwindow\command" -Name 'DelegateExecute' -Value ''
}
# Runs the reg delete lines from set-default-fm.ps1's header, aimed at the fake root.
function Emergency {
    $lines = [regex]::Matches([IO.File]::ReadAllText("$scripts\set-default-fm.ps1"), '(?m)^\s*(reg delete .+)$') | ForEach-Object { $_.Groups[1].Value.Trim() }
    foreach ($l in $lines) {
        if ($l -notmatch '^reg delete "HKCU\\Software\\(?<k>[^"]+)" (?<sw>.+)$') { throw "Unexpected emergency line: $l" }
        $key = "HKCU\Software\fm-test\$($Matches.k)"; $sw = $Matches.sw -split ' '
        & { $ErrorActionPreference = 'Continue'; & reg.exe delete $key @sw 2>&1 } | Out-Null
    }
}
# A fake fm.exe. --background owns the test mutex and waits for --quit, as fm does; a second one
# hands over and exits. "deaf" ignores --quit, "hold" just runs; neither owns the mutex.
$fake = @'
using System; using System.IO; using System.Threading;
public static class FakeFm {
    static void Log(string s) {
        string p = Environment.GetEnvironmentVariable("FM_FAKE_LOG");
        for (int i = 0; p != null && i < 50; i++) { try { File.AppendAllText(p, s + "\r\n"); return; } catch (IOException) { Thread.Sleep(20); } }
    }
    public static void Main(string[] a) {
        string mode = a.Length > 0 ? a[0] : "";
        using (EventWaitHandle quit = new EventWaitHandle(false, EventResetMode.ManualReset, "Local\\fm-test-fake-quit")) {
            if (mode == "--quit") { quit.Set(); Thread.Sleep(300); quit.Reset(); return; }
            if (mode != "--background") { Log("start " + mode + " " + Environment.CurrentDirectory); Thread.Sleep(60000); return; }
            bool first;
            using (Mutex hub = new Mutex(false, "Local\\fm-test.main", out first)) {
                if (!first) { Log("handover"); return; }
                Log("start " + mode + " " + Environment.CurrentDirectory);
                if (quit.WaitOne(60000)) Log("graceful");
            }
        }
    }
}
'@
function TestMutex { $m = $null; if ([Threading.Mutex]::TryOpenExisting($testMutex, [ref]$m)) { $m.Dispose(); $true } else { $false } }
function FromDir([string]$d) { @(Get-Process -Name fm -ErrorAction SilentlyContinue | Where-Object { $_.Path -and $_.Path.StartsWith("$d\", [StringComparison]::OrdinalIgnoreCase) }) }

$before = RealState
try {
    FakeProcs | Stop-Process -Force
    Remove-Item -Recurse -Force $t -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $Root -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force $src | Out-Null
    Add-Type -TypeDefinition $fake -OutputAssembly "$src\fm.exe" -OutputType WindowsApplication
    Copy-Item "$src\fm.exe" "$src\helper.exe"
    # Prior state: an empty Directory\shell (as on the real machine), another app's default verb for
    # drives and Recycle Bin (that one REG_EXPAND_SZ), an empty Network shell key, another app's Run
    # value and dialogkit's pending cleanup.
    New-Item -Force "$cls\Directory\shell" | Out-Null
    New-Item -Force "$cls\Drive\shell" | Out-Null; Set-ItemProperty "$cls\Drive\shell" -Name '(default)' -Value 'other.open'
    New-Item -Force "$cls\CLSID\$($virt[1])\shell" | Out-Null
    New-ItemProperty "$cls\CLSID\$($virt[1])\shell" -Name '(default)' -Value '%SystemRoot%' -PropertyType ExpandString | Out-Null
    New-Item -Force "$cls\CLSID\$($virt[2])\shell" | Out-Null
    New-Item -Force $run | Out-Null; Set-ItemProperty $run -Name 'Other' -Value $foreign
    New-Item -Force $runOnce | Out-Null; Set-ItemProperty $runOnce -Name 'FinderWin.DialogKit.Cleanup' -Value 'dialogkit pending'

    '=== install (starts the resident fm)'
    & "$scripts\install-fm.ps1" @inst -SourceDir $src | Out-Null
    WaitLog 'start --background *'
    Check 'exe + marker "fm" copied' ((Test-Path $exe) -and (Get-Content "$dir\.fm-install" -TotalCount 1) -eq 'fm')
    Check 'App Paths entry' ((Def "$reg\App Paths\fm.exe") -eq $exe)
    Check 'Start menu shortcut' (Test-Path "$menu\Soubory.lnk")
    Check 'Run value Soubory = "fm.exe" --background' ((Val $run 'Soubory') -eq "`"$exe`" --background")
    Check "another app's Run value kept" ((Val $run 'Other') -eq $foreign)
    Check 'resident fm started, in System32 (not the caller''s folder)' (@(FakeLog | Where-Object { $_ -like "start --background $env:SystemRoot\System32" }).Count -eq 1)
    Check "no cleanup pending; dialogkit's pending cleanup untouched" ((Val $runOnce 'Soubory.Cleanup') -eq '<none>' -and (Val $runOnce 'FinderWin.DialogKit.Cleanup') -eq 'dialogkit pending')
    $tt = 0; $tl = 0
    Check 'elevation type read as Assert-NotElevated reads it (this shell: not elevated)' ([FmScripts.Token]::GetTokenInformation([Security.Principal.WindowsIdentity]::GetCurrent().Token, 18, [ref]$tt, 4, [ref]$tl) -and $tt -in 1, 3)

    '=== reinstall while fm runs'
    & "$scripts\install-fm.ps1" @inst -SourceDir $src -NoStart | Out-Null
    Check 'running fm asked to quit and exited by itself' ((FakeLog) -contains 'graceful' -and -not (FakeProcs))
    Check 'clean copy: no cleanup pending' ((Val $runOnce 'Soubory.Cleanup') -eq '<none>')

    '=== a failed copy leaves the installed fm.exe in place'
    $oldHash = (Get-FileHash $exe).Hash
    $src2 = Join-Path $Root 'src2'; New-Item -ItemType Directory -Force $src2 | Out-Null
    [IO.File]::WriteAllBytes("$src2\fm.exe", [byte[]]([IO.File]::ReadAllBytes("$src\fm.exe") + [byte[]](1, 2, 3)))
    $lock = [IO.File]::Open("$src2\fm.exe", 'Open', 'Read', 'None')
    try { Check 'install with a source file it cannot read fails' (Refused 'install-fm.ps1' ($inst + @{ SourceDir = $src2; NoStart = $true }) '*src2\fm.exe*') }
    finally { $lock.Close() }
    Check 'old fm.exe still there, unchanged and registered' ((Get-FileHash $exe).Hash -eq $oldHash -and (Val $run 'Soubory') -eq "`"$exe`" --background" -and (Def "$reg\App Paths\fm.exe") -eq $exe)
    Set-Content "$dir\gone.dll.new" 'x'
    & "$scripts\install-fm.ps1" @inst -SourceDir $src2 -NoStart | Out-Null
    Check 'the next install replaces it and leaves no *.new behind' ((Get-FileHash $exe).Hash -eq (Get-FileHash "$src2\fm.exe").Hash -and -not (Get-ChildItem $dir -Filter '*.new') -and
        (Val $runOnce 'Soubory.Cleanup') -eq '<none>')
    & "$scripts\install-fm.ps1" @inst -SourceDir $src -NoStart | Out-Null

    '=== a failed swap puts the old fm.exe back'
    # No test hook: the new copy (fm.exe.new) inherits a deny on delete and the folder denies
    # "delete child", so it cannot be renamed into place; the old fm.exe keeps its own permissions,
    # so it can still be moved aside and back.
    $oldHash = (Get-FileHash $exe).Hash
    $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    $err = $null
    icacls $exe /inheritance:d | Out-Null
    icacls $dir /deny "*${sid}:(DC)" | Out-Null
    icacls $dir /deny "*${sid}:(OI)(IO)(DE)" | Out-Null
    try { & "$scripts\install-fm.ps1" @inst -SourceDir $src2 -NoStart | Out-Null } catch { $err = $_ }
    finally {
        icacls $dir /remove:d "*$sid" | Out-Null
        icacls $exe /inheritance:e | Out-Null
    }
    Check 'install fails on the rename of fm.exe.new' ($err -and $err.FullyQualifiedErrorId -like 'RenameItem*')
    Check 'old fm.exe is back, unchanged and registered; nothing left aside' ((Get-FileHash $exe).Hash -eq $oldHash -and -not (Get-ChildItem $dir -Filter 'fm.exe.*.del') -and
        (Val $run 'Soubory') -eq "`"$exe`" --background" -and (Def "$reg\App Paths\fm.exe") -eq $exe)
    & "$scripts\install-fm.ps1" @inst -SourceDir $src -NoStart | Out-Null
    Check 'the next install clears the stuck *.new and the pending cleanup' (-not (Get-ChildItem $dir -Filter '*.new') -and -not (Get-ChildItem $dir -Filter '*.del') -and
        (Get-FileHash $exe).Hash -eq $oldHash -and (Val $runOnce 'Soubory.Cleanup') -eq '<none>')

    '=== a resident fm from elsewhere (a dev build) is quit before a first install'
    $dev = Join-Path $Root 'devbuild'; New-Item -ItemType Directory -Force $dev | Out-Null; Copy-Item "$src\fm.exe" "$dev\fm.exe"
    $stray = Start-Process "$dev\fm.exe" -ArgumentList '--background' -PassThru
    for ($i = 0; $i -lt 50 -and -not (TestMutex); $i++) { Start-Sleep -Milliseconds 100 }
    $dir2 = Join-Path $Root 'inst2\app'
    $graceful = @(FakeLog | Where-Object { $_ -eq 'graceful' }).Count
    $a = $inst + @{ SourceDir = $src }; $a.InstallDir = $dir2
    & "$scripts\install-fm.ps1" @a | Out-Null
    for ($i = 0; $i -lt 50 -and -not (FromDir $dir2); $i++) { Start-Sleep -Milliseconds 100 }
    Start-Sleep -Milliseconds 500
    Check 'the stray resident (owner of the mutex) was asked to quit and did' ($stray.HasExited -and @(FakeLog | Where-Object { $_ -eq 'graceful' }).Count -eq $graceful + 1)
    Check 'the new resident runs from the install folder and owns the mutex' ((FromDir $dir2).Count -eq 1 -and (TestMutex) -and (FakeLog) -notcontains 'handover')
    $a = $inst + $def; $a.InstallDir = $dir2
    & "$scripts\uninstall-fm.ps1" @a | Out-Null
    Check 'its uninstall quits it again' (-not (Test-Path $dir2) -and -not (FakeProcs) -and -not (TestMutex))
    & "$scripts\install-fm.ps1" @inst -SourceDir $src -NoStart | Out-Null

    '=== set default (clean)'
    $p1 = Dump $t
    SetDefault
    Check 'no warnings, plain success line' ($warn.Count -eq 0 -and $said -like 'Složky, disky, Win+E*')
    CheckLayout 'clean:'
    $want = @('absent:CLSID\{20D04FE0-3AEA-1069-A2D8-08002B30309D}\shell', 'absent:CLSID\{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}\shell',
        'absent:Directory\shell', 'created:CLSID\{20D04FE0-3AEA-1069-A2D8-08002B30309D}', 'created:CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}',
        'created:Folder', 'default:CLSID\{645FF040-5081-101B-9F08-00AA002F954E}\shell', 'default:Drive\shell',
        'verb:CLSID\{20D04FE0-3AEA-1069-A2D8-08002B30309D}\shell\open', 'verb:CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}\shell\open',
        'verb:CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}\shell\opennewwindow', 'verb:CLSID\{645FF040-5081-101B-9F08-00AA002F954E}\shell\open',
        'verb:CLSID\{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}\shell\open', 'verb:Directory\shell\open', 'verb:Drive\shell\open',
        'verb:Folder\shell\explore') | Sort-Object
    $got = Names
    Check 'backup has one entry per touched place' (($got -join '|') -eq ($want -join '|'))
    if (($got -join '|') -ne ($want -join '|')) { "        got: $($got -join ', ')" }
    Check 'verb records hold the command written' ((Val $bak 'verb:Directory\shell\open') -eq $cmd -and (Val $bak "verb:CLSID\$winE\shell\opennewwindow") -eq "`"$exe`"")
    $rb = "default:CLSID\$($virt[1])\shell"
    Check 'previous defaults saved raw, with their kind' ((Val $bak 'default:Drive\shell') -eq 'other.open' -and
        (Get-Item $bak).GetValueKind($rb) -eq 'ExpandString' -and (Raw $bak $rb) -eq '%SystemRoot%')
    $p1set = Dump $t
    SetDefault
    Check 'second run changes nothing (backup included)' ((Dump $t) -eq $p1set -and $warn.Count -eq 0)

    '=== restore'
    Restore
    Check 'restore returns the fake root to exactly the prior state (kinds included)' ((Dump $t) -eq $p1 -and $rwarn.Count -eq 0)
    Check 'backup and Redirect removed' (-not (Test-Path $bak) -and -not (Test-Path $fmr))
    Restore
    Check 'restore without a backup is a no-op' ((Dump $t) -eq $p1)

    '=== another app adds keys inside trees set-default created'
    SetDefault
    New-Item -Force "$cls\Folder\shell\other\command" | Out-Null
    Set-ItemProperty "$cls\Folder\shell\other\command" -Name '(default)' -Value $foreign
    Set-ItemProperty "$cls\CLSID\$($virt[0])" -Name 'Foreign' -Value 'x'
    Restore
    Check 'foreign verb under created Folder survives' ((Def "$cls\Folder\shell\other\command") -eq $foreign -and @(Get-ChildItem "$cls\Folder" -Recurse).Count -eq 3)
    Check 'foreign value on created This PC CLSID survives' ((Val "$cls\CLSID\$($virt[0])" 'Foreign') -eq 'x' -and (Get-Item "$cls\CLSID\$($virt[0])").ValueCount -eq 1)
    Check 'everything of ours there is gone' (-not (Test-Path "$cls\Folder\shell\explore") -and -not (Test-Path "$cls\CLSID\$($virt[0])\shell") -and
        (Get-Item "$cls\Folder").ValueCount -eq 0 -and (Get-Item "$cls\Folder\shell").ValueCount -eq 0 -and -not (Test-Path $fmr))
    Remove-Item "$cls\Folder", "$cls\CLSID\$($virt[0])" -Recurse -Force
    Check 'otherwise exactly the prior state' ((Dump $t) -eq $p1)

    '=== another app takes places over after set-default'
    SetDefault
    Set-ItemProperty "$cls\Directory\shell" -Name '(default)' -Value 'openinfiles'
    New-Item -Force "$cls\Directory\shell\openinfiles\command" | Out-Null; Set-ItemProperty "$cls\Directory\shell\openinfiles\command" -Name '(default)' -Value $foreign
    Set-ItemProperty "$cls\CLSID\$winE\shell\opennewwindow\command" -Name '(default)' -Value $foreign
    Restore
    Check 'restore warns once per place it leaves' ($rwarn.Count -eq 2 -and @($rwarn | Where-Object { $_.Message -like '*aplikac*' }).Count -eq 2)
    Check "other app's folder default and verb kept" ((Def "$cls\Directory\shell") -eq 'openinfiles' -and (Def "$cls\Directory\shell\openinfiles\command") -eq $foreign)
    Check "other app's Win+E kept" ((Def "$cls\CLSID\$winE\shell\opennewwindow\command") -eq $foreign)
    Check 'our keys around them removed' (-not (Test-Path "$cls\Directory\shell\open") -and -not (Test-Path "$cls\CLSID\$winE\shell\open") -and
        (Def "$cls\Drive\shell") -eq 'other.open' -and -not (Test-Path $fmr))
    Remove-Item "$cls\Directory\shell\openinfiles", "$cls\CLSID\$winE" -Recurse -Force; DelDefault "$cls\Directory\shell"
    Check 'otherwise exactly the prior state' ((Dump $t) -eq $p1)

    '=== emergency lines, then another app, then restore'
    SetDefault
    Emergency
    $ours = @('Directory\shell\open', 'Drive\shell\open', 'Folder\shell\explore', "CLSID\$winE\shell\opennewwindow", "CLSID\$winE\shell\open") + ($virt | ForEach-Object { "CLSID\$_\shell\open" })
    $shells = @('Directory\shell', 'Drive\shell') + ($virt | ForEach-Object { "CLSID\$_\shell" })
    Check 'the header''s reg delete lines remove every key and value set-default wrote' (-not ($ours | Where-Object { Test-Path "$cls\$_" }) -and
        -not ($shells | Where-Object { (Def "$cls\$_") -notin '<none>', '<nokey>' }) -and (Val $fmr 'Redirect') -eq '<none>')
    New-Item -Force "$cls\Directory\shell\open\command" | Out-Null; Set-ItemProperty "$cls\Directory\shell\open\command" -Name '(default)' -Value $foreign
    Set-ItemProperty "$cls\Directory\shell" -Name '(default)' -Value 'open'
    Restore
    Check "other app's open verb and its default kept" ((Def "$cls\Directory\shell\open\command") -eq $foreign -and (Def "$cls\Directory\shell") -eq 'open')
    Check 'defaults the emergency lines deleted come back, with their kind' ((Def "$cls\Drive\shell") -eq 'other.open' -and
        (Get-Item "$cls\CLSID\$($virt[1])\shell").GetValueKind('') -eq 'ExpandString' -and (Raw "$cls\CLSID\$($virt[1])\shell" '') -eq '%SystemRoot%')
    Remove-Item "$cls\Directory\shell\open" -Recurse -Force; DelDefault "$cls\Directory\shell"
    Check 'otherwise exactly the prior state' ((Dump $t) -eq $p1)

    '=== another app''s verb without a command at a recorded place'
    function SeedHandlerVerb {
        Remove-Item "$cls\Folder\shell\explore" -Recurse -Force
        New-Item "$cls\Folder\shell\explore" | Out-Null
        Set-ItemProperty "$cls\Folder\shell\explore" -Name 'ExplorerCommandHandler' -Value '{11111111-2222-3333-4444-555555555555}'
    }
    SetDefault
    SeedHandlerVerb
    $p7 = Dump "$cls\Folder\shell\explore"
    Restore
    Check 'restore leaves it, with a warning' ((Dump "$cls\Folder\shell\explore") -eq $p7 -and $rwarn.Count -eq 1)
    Remove-Item "$cls\Folder" -Recurse -Force
    Check 'otherwise exactly the prior state' ((Dump $t) -eq $p1)
    SetDefault
    SeedHandlerVerb
    SetDefault
    Check 'set-default leaves it, warns and drops the record' ($warn.Count -eq 1 -and (Dump "$cls\Folder\shell\explore") -eq $p7 -and (Names) -notcontains 'verb:Folder\shell\explore')
    Restore
    Check 'a later restore keeps it too' ((Dump "$cls\Folder\shell\explore") -eq $p7)
    Remove-Item "$cls\Folder" -Recurse -Force
    Check 'otherwise exactly the prior state' ((Dump $t) -eq $p1)

    '=== a stale record never claims another app''s key'
    SetDefault
    Remove-Item "$cls\Folder\shell\explore" -Recurse -Force
    New-Item -Force "$cls\Folder\shell\explore\command" | Out-Null; Set-ItemProperty "$cls\Folder\shell\explore\command" -Name '(default)' -Value $foreign
    Set-ItemProperty "$cls\Drive\shell" -Name '(default)' -Value 'files.open'
    Remove-Item "$cls\Directory\shell\open\command" -Recurse -Force
    SetDefault
    Check 'foreign explore skipped with a warning, its record dropped' ($warn.Count -eq 1 -and (Def "$cls\Folder\shell\explore\command") -eq $foreign -and (Names) -notcontains 'verb:Folder\shell\explore')
    Check 'our interrupted verb (no command) completed' ((Def "$cls\Directory\shell\open\command") -eq $cmd)
    Check "default changed by another app: taken back, that app's value saved" ((Def "$cls\Drive\shell") -eq 'open' -and (Val $bak 'default:Drive\shell') -eq 'files.open')
    Restore
    Check "restore keeps the foreign explore and brings back that app's default" ((Def "$cls\Folder\shell\explore\command") -eq $foreign -and (Def "$cls\Drive\shell") -eq 'files.open')
    Remove-Item "$cls\Folder" -Recurse -Force; Set-ItemProperty "$cls\Drive\shell" -Name '(default)' -Value 'other.open'
    Check 'otherwise exactly the prior state' ((Dump $t) -eq $p1)

    "=== another app's Folder\shell\explore, Win+E and Drive\shell\open there before"
    foreach ($v in 'Folder\shell\explore', "CLSID\$winE\shell\opennewwindow", 'Drive\shell\open') {
        New-Item -Force "$cls\$v\command" | Out-Null
        Set-ItemProperty "$cls\$v\command" -Name '(default)' -Value $foreign
    }
    $p2 = Dump $t
    SetDefault
    Check 'one Czech warning per foreign key' ($warn.Count -eq 3 -and @($warn | Where-Object { $_.Message -like '*jiná aplikace*' }).Count -eq 3)
    Check 'closing line names the places left to Explorer' ($said -like '*zůstává: disky, složky otevírané z prohlížeče, Win+E (*')
    CheckLayout 'foreign:' -skip 'Folder\shell\explore', "CLSID\$winE\shell\opennewwindow", 'Drive\shell\open'
    Check 'foreign explore untouched' ((Def "$cls\Folder\shell\explore\command") -eq $foreign -and (Val "$cls\Folder\shell\explore\command" 'DelegateExecute') -eq '<none>')
    Check 'foreign Win+E untouched' ((Def "$cls\CLSID\$winE\shell\opennewwindow\command") -eq $foreign)
    Check 'foreign Drive open untouched, Drive default kept' ((Def "$cls\Drive\shell\open\command") -eq $foreign -and (Def "$cls\Drive\shell") -eq 'other.open')
    Check 'nothing recorded for foreign keys' (-not (Names | Where-Object { $_ -like '*Folder*' -or $_ -like '*opennewwindow*' -or $_ -like '*Drive*' }) -and
        (Names) -contains "verb:CLSID\$winE\shell\open" -and (Names) -notcontains "created:CLSID\$winE")
    Restore
    Check 'restore keeps the foreign keys, prior state exact' ((Dump $t) -eq $p2)
    Remove-Item "$cls\Folder", "$cls\CLSID\$winE", "$cls\Drive\shell\open" -Recurse -Force

    '=== migration from the previous version'
    Remove-Item "$cls\Directory\shell" -Recurse -Force
    $p3 = Dump $t; $p3cls = Dump $cls
    SeedLegacy
    Restore
    Check 'restore undoes an old install exactly' ((Dump $t) -eq $p3)
    SeedLegacy
    SetDefault
    Check 'old Win+E key not treated as foreign' ($warn.Count -eq 0)
    CheckLayout 'migrated:'
    Check 'old backup format replaced' ((Names) -notcontains 'WinE.existed' -and (Names) -notcontains 'Drive.shell')
    Check 'original values kept, not fm.open' ((Val $bak 'default:Drive\shell') -eq 'other.open' -and (Names) -contains 'created:Directory\shell' -and (Names) -contains 'absent:Directory\shell')
    Restore
    Check 'restore after migration returns the prior state' ((Dump $t) -eq $p3)

    '=== uninstall (with fm still the default)'
    SetDefault
    & "$scripts\uninstall-fm.ps1" @inst @def | Out-Null
    Check 'folders given back, Redirect and backup gone' ((Dump $cls) -eq $p3cls -and -not (Test-Path $fmr))
    Check 'Run value removed, other app kept' ((Val $run 'Soubory') -eq '<none>' -and (Val $run 'Other') -eq $foreign)
    Check 'install dir removed, no cleanup pending' (-not (Test-Path $dir) -and (Val $runOnce 'Soubory.Cleanup') -eq '<none>')
    Check 'App Paths and shortcut removed' (-not (Test-Path "$reg\App Paths\fm.exe") -and -not (Test-Path "$menu\Soubory.lnk"))

    '=== uninstall with no fm.exe left does not wait for a resident from elsewhere'
    $elsewhere = Join-Path $Root 'elsewhere'; New-Item -ItemType Directory -Force $elsewhere | Out-Null; Copy-Item "$src\fm.exe" "$elsewhere\fm.exe" -Force
    $res = Start-Process "$elsewhere\fm.exe" -ArgumentList '--background' -PassThru
    for ($i = 0; $i -lt 50 -and -not (TestMutex); $i++) { Start-Sleep -Milliseconds 100 }
    $graceful = @(FakeLog | Where-Object { $_ -eq 'graceful' }).Count
    foreach ($case in 'missing', 'holding only the marker') {
        if ($case -ne 'missing') { New-Item -ItemType Directory -Force $dir | Out-Null; Set-Content "$dir\.fm-install" 'fm' }
        $a = $inst + $def; $a.QuitWait = 5
        $sw = [Diagnostics.Stopwatch]::StartNew(); $threw = $null
        try { & "$scripts\uninstall-fm.ps1" @a | Out-Null } catch { $threw = $_.Exception.Message }
        Check "install folder $case`: uninstall completes at once, no throw" (-not $threw -and $sw.Elapsed.TotalSeconds -lt 4 -and -not (Test-Path $dir))
        if ($threw) { "        threw: $threw" }
    }
    Check 'the resident from elsewhere is left running, not asked to quit' (-not $res.HasExited -and (TestMutex) -and @(FakeLog | Where-Object { $_ -eq 'graceful' }).Count -eq $graceful)
    Start-Process "$elsewhere\fm.exe" -ArgumentList '--quit' -Wait; $res.WaitForExit(5000) | Out-Null
    Check 'it quits when asked' ($res.HasExited -and -not (TestMutex))

    '=== uninstall while a file is in use, then reinstall'
    & "$scripts\install-fm.ps1" @inst -SourceDir $src -NoStart | Out-Null
    $helper = Start-Process "$dir\helper.exe" -ArgumentList 'hold' -PassThru
    WaitLog 'start hold *'
    & "$scripts\uninstall-fm.ps1" @inst @def | Out-Null
    Check 'file in use set aside; marker kept with the leftovers' ((Test-Path "$dir\helper.exe.1.del") -and -not (Test-Path $exe) -and (Get-Content "$dir\.fm-install" -TotalCount 1) -eq 'fm')
    Check "fm's own RunOnce name deletes the folder; dialogkit's untouched" ((Val $runOnce 'Soubory.Cleanup') -eq "cmd.exe /d /c rd /s /q `"$dir`"" -and (Val $runOnce 'FinderWin.DialogKit.Cleanup') -eq 'dialogkit pending')
    & "$scripts\install-fm.ps1" @inst -SourceDir $src -NoStart | Out-Null
    Check 'reinstall accepted; pending folder delete replaced by *.del only' ((Test-Path $exe) -and (Val $runOnce 'Soubory.Cleanup') -eq "cmd.exe /d /c del /f /s /q `"$dir\*.del`"")
    Stop-Process -Id $helper.Id -Force; $helper.WaitForExit()

    '=== an fm still busy after the wait stops the script before anything changes'
    SetDefault
    $deaf = Start-Process $exe -ArgumentList 'deaf' -PassThru
    WaitLog 'start deaf *'
    $p6 = Dump $t; $n6 = @(Get-ChildItem $dir -Force).Count
    $a = $inst + $def; $a.QuitWait = 11
    $out = @(& { try { & "$scripts\uninstall-fm.ps1" @a | Out-Null } catch { "THREW: $($_.Exception.Message)" } } 6>&1)
    Check 'uninstall refuses after the wait, saying nothing changed' (@($out | Where-Object { "$_" -like 'THREW: *Nic jsem nezměnil*' }).Count -eq 1)
    Check 'one Czech progress line per 10 s while waiting' (@($out | Where-Object { $_ -is [Management.Automation.InformationRecord] -and "$_" -like '*dokončují*čekám*' }).Count -eq 1)
    Check 'fm not killed; registry and folder unchanged' (-not $deaf.HasExited -and (Dump $t) -eq $p6 -and @(Get-ChildItem $dir -Force).Count -eq $n6)
    $a = $inst + @{ SourceDir = $src; NoStart = $true; QuitWait = 1 }
    Check 'install refuses the same way, nothing changed' ((Refused 'install-fm.ps1' $a '*Nic jsem nezměnil*') -and -not $deaf.HasExited -and (Dump $t) -eq $p6)
    Stop-Process -Id $deaf.Id -Force; $deaf.WaitForExit()
    & "$scripts\uninstall-fm.ps1" @inst @def | Out-Null
    Check 'once fm is closed, uninstall completes' ((Dump $cls) -eq $p3cls -and -not (Test-Path $fmr) -and -not (Test-Path $dir))
    Check "fm's cleanup cleared, dialogkit's kept" ((Val $runOnce 'Soubory.Cleanup') -eq '<none>' -and (Val $runOnce 'FinderWin.DialogKit.Cleanup') -eq 'dialogkit pending')

    '=== a relative -InstallDir is taken from the PowerShell location'
    New-Item -ItemType Directory -Force "$Root\elsewhere" | Out-Null
    $cwd = [Environment]::CurrentDirectory; [Environment]::CurrentDirectory = "$Root\elsewhere"
    Push-Location $Root
    try {
        $a = $inst + @{ SourceDir = $src; NoStart = $true }; $a.InstallDir = 'inst\app'
        & "$scripts\install-fm.ps1" @a | Out-Null
        Check 'install-fm: relative folder resolved against the current location' ((Test-Path $exe) -and (Def "$reg\App Paths\fm.exe") -eq $exe -and -not (Test-Path "$Root\elsewhere\inst"))
        $a = $inst + $def; $a.InstallDir = '.\inst\app'
        & "$scripts\uninstall-fm.ps1" @a | Out-Null
        Check 'uninstall-fm: relative folder resolved the same way' (-not (Test-Path $dir) -and (Def "$reg\App Paths\fm.exe") -eq '<nokey>')
    } finally { Pop-Location; [Environment]::CurrentDirectory = $cwd }

    "=== another program's App Paths\fm.exe"
    $ap = "$reg\App Paths\fm.exe"
    New-Item -Force $ap | Out-Null; Set-ItemProperty $ap -Name '(default)' -Value 'C:\Other\fm.exe'; Set-ItemProperty $ap -Name 'Path' -Value 'C:\Other'
    $apDump = Dump $ap
    $w = @(& "$scripts\install-fm.ps1" @inst -SourceDir $src -NoStart 3>&1 | Where-Object { $_ -is [Management.Automation.WarningRecord] })
    Check 'install leaves it and warns' ((Dump $ap) -eq $apDump -and $w.Count -eq 1)
    & "$scripts\uninstall-fm.ps1" @inst @def | Out-Null
    Check 'uninstall leaves it' ((Dump $ap) -eq $apDump)
    Remove-Item $ap -Recurse -Force

    "=== -RemoveData keeps the other components' files"
    $data = Join-Path $Root 'data'
    New-Item -ItemType Directory -Force "$data\index" | Out-Null
    foreach ($f in 'prefs.txt', 'tags.tsv', 'fm.log', 'index\snapshot', 'disable-shim', 'shim.log', 'dialogkit.log') { Set-Content (Join-Path $data $f) 'x' }
    & "$scripts\uninstall-fm.ps1" @inst @def -DataDir $data -RemoveData | Out-Null
    Check "fm's data gone; disable-shim and the others' logs kept" (-not (Test-Path "$data\prefs.txt") -and -not (Test-Path "$data\tags.tsv") -and -not (Test-Path "$data\fm.log") -and
        -not (Test-Path "$data\index") -and (Test-Path "$data\disable-shim") -and (Test-Path "$data\shim.log") -and (Test-Path "$data\dialogkit.log"))
    Remove-Item "$data\disable-shim", "$data\shim.log", "$data\dialogkit.log"; Set-Content "$data\prefs.txt" 'x'
    & "$scripts\uninstall-fm.ps1" @inst @def -DataDir $data -RemoveData | Out-Null
    Check 'folder removed when only fm data was in it' (-not (Test-Path $data))

    '=== refusals'
    New-Item -ItemType Directory -Force "$Root\foreign" | Out-Null; Set-Content "$Root\foreign\keep.txt" 'x'
    New-Item -ItemType Directory -Force "$Root\fmdir", "$Root\dkdir", "$Root\dksrc", "$Root\shimsrc" | Out-Null
    Set-Content "$Root\fmdir\.fm-install" 'fm'; Set-Content "$Root\fmdir\keep.txt" 'x'
    Set-Content "$Root\dkdir\.fm-install" 'fm-dialogkit'; Set-Content "$Root\dkdir\keep.txt" 'x'
    Set-Content "$Root\dksrc\fm-dialogkit.exe" 'x'; Set-Content "$Root\shimsrc\fm_dialog_shim.dll" 'x'
    Set-ItemProperty $run -Name 'FinderWin.DialogKit' -Value 'dialogkit run'
    $files = { @(Get-ChildItem "$Root\foreign", "$Root\fmdir", "$Root\dkdir").Count }
    $nfiles = & $files
    $a = $inst + $def; $a.InstallDir = "$Root\foreign"
    Check 'uninstall-fm: folder without marker refused' (Refused 'uninstall-fm.ps1' $a '*značka*')
    $a = $inst + @{ SourceDir = $src; NoStart = $true }; $a.InstallDir = "$Root\foreign"
    Check 'install-fm: non-empty folder without marker refused' (Refused 'install-fm.ps1' $a '*značka*')
    $a.InstallDir = "$Root\dkdir"
    Check "install-fm: dialogkit's folder refused" (Refused 'install-fm.ps1' $a '*jiné části*')
    $a = $inst + $def; $a.InstallDir = "$Root\dkdir"
    Check "uninstall-fm: dialogkit's folder refused" (Refused 'uninstall-fm.ps1' $a '*jiné části*')
    Check "uninstall-dialogkit: fm's folder refused before its Run value goes" ((Refused 'uninstall-dialogkit.ps1' @{ InstallDir = "$Root\fmdir"; RegRoot = $reg } '*jiné části*') -and
        (Val $run 'FinderWin.DialogKit') -eq 'dialogkit run')
    Check 'install-dialogkit: non-empty folder without marker refused' (Refused 'install-dialogkit.ps1' @{ InstallDir = "$Root\foreign"; RegRoot = $reg; SourceDir = "$Root\dksrc"; NoStart = $true } '*značka*')
    Check "install-shim: fm's folder refused" (Refused 'install-shim.ps1' @{ InstallDir = "$Root\fmdir"; ClassesRoot = $cls; RegRoot = $reg; SourceDir = "$Root\shimsrc" } '*jiné části*')
    Check "uninstall-shim: fm's folder refused" (Refused 'uninstall-shim.ps1' @{ InstallDir = "$Root\fmdir"; ClassesRoot = $cls; RegRoot = $reg } '*jiné části*')
    Check 'every refused folder kept its files' ((& $files) -eq $nfiles)
    $a = $inst + @{ SourceDir = $src; NoStart = $true }; $a.InstallDir = "$Root\in[s]t\app"
    Check 'install-fm: folder with [ ] refused, nothing created' ((Refused 'install-fm.ps1' $a '*nesmí obsahovat*') -and -not (Test-Path -LiteralPath "$Root\in[s]t"))
    $a = $inst + $def; $a.InstallDir = "$Root\in[s]t\app"
    Check 'uninstall-fm: folder with [ ] refused' (Refused 'uninstall-fm.ps1' $a '*nesmí obsahovat*')
    Remove-ItemProperty $run -Name 'FinderWin.DialogKit'
    $p4 = Dump $t
    Check 'missing fm.exe refused, nothing written' ((Refused 'set-default-fm.ps1' ($def + @{ Exe = "$Root\nope\fm.exe" }) '*Chybí*') -and (Dump $t) -eq $p4)
    foreach ($k in 'ClassesRoot', 'FmRoot', 'BackupRoot') {
        $a = $def + @{ Exe = "$Root\foreign\keep.txt" }; $a[$k] = "HKLM:\Software\fm-test\$k"
        Check "non-HKCU $k refused" (Refused 'set-default-fm.ps1' $a '*musí být pod HKCU*')
    }
    $ps32 = Join-Path $env:WINDIR 'SysWOW64\WindowsPowerShell\v1.0\powershell.exe'
    if (Test-Path $ps32) {
        $p5 = Dump $t
        $runs = @{
            'set-default-fm.ps1' = @('-Exe', "$Root\foreign\keep.txt", '-ClassesRoot', $cls, '-FmRoot', $fmr, '-BackupRoot', $bak)
            'install-fm.ps1'     = @('-InstallDir', $dir, '-RegRoot', $reg, '-RunRoot', $run, '-StartMenuDir', $menu, '-SourceDir', $src, '-NoStart', '-Mutex', $testMutex)
        }
        foreach ($s in $runs.Keys) {
            $o = & { $ErrorActionPreference = 'Continue'; & $ps32 -NoProfile -ExecutionPolicy Bypass -File "$scripts\$s" @($runs[$s]) 2>&1 }
            Check "32-bit PowerShell refused ($s), nothing written" ($LASTEXITCODE -ne 0 -and "$o" -like '*64bit*' -and (Dump $t) -eq $p5 -and -not (Test-Path $dir))
        }
    }
    $a = $inst + @{ SourceDir = $src; NoStart = $true }; $a.RunRoot = 'HKLM:\Software\fm-test\Run'
    Check 'non-HKCU RunRoot refused' ((Refused 'install-fm.ps1' $a '*musí být pod HKCU*') -and -not (Test-Path $dir))
    $a = $inst + $def; $a.RunRoot = 'HKLM:\Software\fm-test\Run'
    Check 'non-HKCU RunRoot refused by uninstall' (Refused 'uninstall-fm.ps1' $a '*musí být pod HKCU*')

    '=== emergency lines in both headers'
    $lines = @('Directory\shell\open', 'Drive\shell\open', 'Folder\shell\explore', "CLSID\$winE\shell\opennewwindow", "CLSID\$winE\shell\open") +
        ($virt | ForEach-Object { "CLSID\$_\shell\open" }) | ForEach-Object { "reg delete `"HKCU\Software\Classes\$_`" /f" }
    $lines += @('Directory\shell', 'Drive\shell') + ($virt | ForEach-Object { "CLSID\$_\shell" }) | ForEach-Object { "reg delete `"HKCU\Software\Classes\$_`" /ve /f" }
    $lines += 'reg delete "HKCU\Software\fm" /v Redirect /f'
    foreach ($f in 'set-default-fm.ps1', 'restore-explorer.ps1') {
        $text = [IO.File]::ReadAllText("$scripts\$f")
        $missing = @($lines | Where-Object { $text.IndexOf($_, [StringComparison]::OrdinalIgnoreCase) -lt 0 })
        Check "$f lists all $($lines.Count) reg delete lines" ($missing.Count -eq 0)
        $missing | ForEach-Object { "        missing: $_" }
    }
} finally {
    FakeProcs | Stop-Process -Force
    Start-Sleep -Milliseconds 300
    Remove-Item -Recurse -Force $t -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $Root -ErrorAction SilentlyContinue
}
Check 'fake roots cleaned, no fake fm left running' (-not (Test-Path $t) -and -not (Test-Path $Root) -and -not (FakeProcs))
Check 'real keys unchanged (guard)' ((RealState) -eq $before)
"PASS $pass  FAIL $fails"
if ($fails) { 'FAILED'; exit 1 } else { 'ALL PASSED' }
