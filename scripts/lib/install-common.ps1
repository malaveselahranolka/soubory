# Shared by the install/uninstall scripts. HKCU only; every delete is limited to a directory
# that carries our marker file.
$Marker = '.fm-install'
$RunName = 'FinderWin.DialogKit'
# Per component: fm and the shim set their own after dot-sourcing this file.
$CleanupName = 'FinderWin.DialogKit.Cleanup'

# A 32-bit PowerShell sees another view of HKCU\Software\Classes\CLSID and cannot read the Path of a
# 64-bit fm, so it would write keys Explorer never reads and miss a running fm.
if ([Environment]::Is64BitOperatingSystem -and -not [Environment]::Is64BitProcess) { throw 'Spusť 64bitový PowerShell.' }

function Assert-HkcuRoot([string]$root) {
    if ($root -notmatch '^HKCU:\\') { throw "RegRoot musí být pod HKCU:\ (zadáno: $root)" }
}

# An elevated run would start an elevated fm (a second resident one, files opened as admin).
# Only TokenElevationTypeFull (2) is refused, the elevated half of a split token, as fm.exe does:
# with UAC off or as the built-in Administrator (Default, 1) there is no normal token to run with.
function Assert-NotElevated {
    if (-not ('FmScripts.Token' -as [type])) {
        Add-Type -Namespace FmScripts -Name Token -MemberDefinition '[DllImport("advapi32.dll")] public static extern bool GetTokenInformation(IntPtr token, int cls, out int value, int size, out int returned);'
    }
    $type = 0; $len = 0
    if (-not [FmScripts.Token]::GetTokenInformation([Security.Principal.WindowsIdentity]::GetCurrent().Token, 18, [ref]$type, 4, [ref]$len)) { throw 'Nelze zjistit, zda PowerShell běží jako správce.' }
    if ($type -eq 2) { throw 'Spusť v obyčejném PowerShellu (bez práv správce): Soubory nesmí běžet jako správce.' }
}

# A relative path is taken from this PowerShell's location, not the folder the process started in.
# Brackets are wildcards for -Path; refusing them keeps every later use of the folder literal.
function Get-InstallDir([string]$dir) {
    $provider = $null; $drive = $null
    $full = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($dir, [ref]$provider, [ref]$drive)
    if ($provider.Name -ne 'FileSystem') { throw "Cesta $dir není složka na disku." }
    if ($full.IndexOfAny('[]'.ToCharArray()) -ge 0) { throw "Cesta $full nesmí obsahovat [ ]." }
    $full
}

function Test-Marker([string]$dir, [string]$product) {
    $m = Join-Path $dir $Marker
    (Test-Path -LiteralPath $m) -and (Get-Content -LiteralPath $m -TotalCount 1) -eq $product
}

# The marker names the product. Install takes a missing, empty or own folder; uninstall only its own.
function Assert-OwnDir([string]$dir, [string]$product, [switch]$Uninstall) {
    if (-not (Test-Path -LiteralPath $dir)) { return }
    $m = Join-Path $dir $Marker
    if (Test-Path -LiteralPath $m) {
        $owner = Get-Content -LiteralPath $m -TotalCount 1
        if ($owner -ne $product) { throw "Složka $dir patří jiné části ($owner), ne $product; nic neměním." }
    } elseif ($Uninstall -or (Get-ChildItem -LiteralPath $dir -Force | Select-Object -First 1)) {
        throw "Složka $dir není naše (chybí značka $Marker); nic neměním."
    }
}

# New-Item -Force on an existing key empties it (values and subkeys), so create only a missing one.
function New-KeyIfMissing([string]$key) {
    if (-not (Test-Path $key)) { New-Item -Force $key | Out-Null }
}

# Remove-ItemProperty cannot delete a key's default value; the registry API can (a missing one is fine).
function Remove-RegValue([string]$key, [string]$name) {
    $k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey(($key -replace '^HKCU:\\', ''), $true)
    if ($k) { try { $k.DeleteValue($name, $false) } finally { $k.Close() } }
}

# Copies a value keeping its kind and unexpanded text (GetValue expands REG_EXPAND_SZ by default).
function Copy-RegValue([string]$fromKey, [string]$fromName, [string]$toKey, [string]$toName) {
    $hk = [Microsoft.Win32.Registry]::CurrentUser
    $s = $hk.OpenSubKey(($fromKey -replace '^HKCU:\\', ''))
    $d = $hk.OpenSubKey(($toKey -replace '^HKCU:\\', ''), $true)
    try { $d.SetValue($toName, $s.GetValue($fromName, $null, 'DoNotExpandEnvironmentNames'), $s.GetValueKind($fromName)) }
    finally { $s.Close(); $d.Close() }
}

# Called only for a verb key with a verb: record. It stays ours only while its command is still
# fm's: the one recorded, or any quoted ...\fm.exe. Without a working command it is ours only when
# it holds nothing else: our interrupted run leaves an empty verb key, or an empty command key under
# it (fm writes no value on the verb key itself). Anything more is another app's.
function Test-FmVerb([string]$verbKey, [string]$recorded) {
    $v = Get-Item $verbKey
    $c = Get-Item "$verbKey\command" -ErrorAction SilentlyContinue
    if (-not $c -or $c.ValueCount -eq 0) {
        return $v.ValueCount -eq 0 -and $v.SubKeyCount -eq [int][bool]$c -and (-not $c -or $c.SubKeyCount -eq 0)
    }
    $cmd = $c.GetValue('', '', 'DoNotExpandEnvironmentNames')
    ($recorded -and $cmd -eq $recorded) -or $cmd -like '*\fm.exe"*'
}

function Get-FromDir([string]$dir, [string]$name) {
    Get-Process -Name $name -ErrorAction SilentlyContinue | Where-Object {
        $_.Path -and $_.Path.StartsWith($dir.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase)
    }
}

# Ends a process at once; nothing it holds in memory is saved.
function Stop-FromDir([string]$dir, [string]$name) {
    foreach ($p in @(Get-FromDir $dir $name)) { Stop-Process -Id $p.Id -Force; $p.WaitForExit(3000) | Out-Null }
}

# True while some fm owns the hub mutex, wherever it runs from (a dev build too). The handle is
# closed at once, or this PowerShell would keep the mutex alive. One we may not open is an fm too.
function Test-Instance([string]$name) {
    $m = $null
    try { if ([Threading.Mutex]::TryOpenExisting($name, [ref]$m)) { $m.Dispose(); return $true } }
    catch [UnauthorizedAccessException] { return $true }
    $false
}

# "fm.exe --quit" asks the running fm to close its windows and exit once running copies and open
# dialogs are done, saving its windows and index; it never starts one. That covers an fm in $dir
# and the resident that owns $Mutex from anywhere else, which would otherwise take over the new one.
# The installed fm.exe sends it, or $Quitter (the new one) on a first install. fm is never killed:
# one still running after $wait seconds stops the script here, so callers run this before they
# change anything.
function Stop-Fm([string]$dir, [int]$wait = 120, [string]$Mutex = 'Local\fm.main', [string]$Quitter) {
    if (-not (Get-FromDir $dir 'fm') -and -not (Test-Instance $Mutex)) { return }
    $q = @((Join-Path $dir 'fm.exe'), $Quitter) | Where-Object { $_ -and (Test-Path -LiteralPath $_) } | Select-Object -First 1
    if ($q) {
        (Start-Process -FilePath $q -ArgumentList '--quit' -WorkingDirectory "$env:SystemRoot\System32" -PassThru).WaitForExit(5000) | Out-Null
    }
    $sw = [Diagnostics.Stopwatch]::StartNew(); $next = 10
    # With nothing to send --quit (uninstall with no fm.exe left), only fm from $dir is waited for.
    while ((Get-FromDir $dir 'fm') -or ($q -and (Test-Instance $Mutex))) {
        $s = [int]$sw.Elapsed.TotalSeconds
        if ($s -ge $wait) { throw 'Soubory pořád běží: dokončují kopírování nebo přesun, nebo mají otevřený dialog. Dokonči nebo zruš operaci (případně Soubory zavři) a spusť skript znovu. Nic jsem nezměnil.' }
        if ($s -ge $next) { Write-Host "Soubory ještě dokončují kopírování nebo přesun, nebo mají otevřený dialog; čekám ($s s z $wait s)…"; $next += 10 }
        Start-Sleep -Milliseconds 200
    }
}

# Deletes a file, or renames it to *.N.del when it is mapped in another process. True when deleted.
function Remove-OrSetAside([string]$path) {
    if (-not (Test-Path -LiteralPath $path)) { return $true }
    try { Remove-Item -LiteralPath $path -Force; return $true } catch {}
    for ($i = 1; $i -lt 100; $i++) {
        $aside = "$path.$i.del"
        if (-not (Test-Path -LiteralPath $aside)) { Rename-Item -LiteralPath $path -NewName (Split-Path $aside -Leaf); return $false }
    }
    throw "Nelze odstranit ani přejmenovat $path"
}

function Grant-AppContainerRead([string]$dir) {
    # ALL APPLICATION PACKAGES: AppContainer dialog hosts must be able to load the hook DLL.
    & icacls.exe $dir /grant '*S-1-15-2-1:(OI)(CI)(RX)' /T /Q | Out-Null
    if ($LASTEXITCODE) { Write-Warning "icacls selhal ($LASTEXITCODE); dialogy v sandboxovaných aplikacích nemusí tlačítko ukázat." }
}

# Deletes the whole directory when it is gone, otherwise reports false (locked files set aside).
# The marker goes last, with the folder, so leftovers still read as ours.
function Remove-InstallDir([string]$dir) {
    $clean = $true
    Get-ChildItem -LiteralPath $dir -Recurse -File -Force | Where-Object Name -ne $Marker | ForEach-Object {
        if ($_.Name -like '*.del') { Remove-Item -LiteralPath $_.FullName -Force -ErrorAction SilentlyContinue }
        elseif (-not (Remove-OrSetAside $_.FullName)) { $clean = $false }
    }
    if (Get-ChildItem -LiteralPath $dir -Recurse -File -Force -Filter '*.del') { $clean = $false }
    if ($clean) { Remove-Item -LiteralPath $dir -Recurse -Force }
    return $clean
}

# One RunOnce value per component ($CleanupName). Install passes no -WholeDir (delete *.del only);
# uninstall removes the directory. Re-installing replaces the uninstall variant, so a pending
# cleanup never deletes a fresh install.
function Register-DelCleanup([string]$root, [string]$dir, [switch]$WholeDir) {
    $runOnce = Join-Path $root 'RunOnce'
    New-KeyIfMissing $runOnce
    $cmd = if ($WholeDir) { "cmd.exe /d /c rd /s /q `"$dir`"" } else { "cmd.exe /d /c del /f /s /q `"$dir\*.del`"" }
    Set-ItemProperty -Path $runOnce -Name $CleanupName -Value $cmd
}

function Clear-DelCleanup([string]$root) {
    Remove-ItemProperty -Path (Join-Path $root 'RunOnce') -Name $CleanupName -ErrorAction SilentlyContinue
}
