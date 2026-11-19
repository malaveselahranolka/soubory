# Shared by the install/uninstall scripts. HKCU only; every delete is limited to a directory
# that carries our marker file.
$Marker = '.fm-install'
$RunName = 'FinderWin.DialogKit'
$CleanupName = 'FinderWin.DialogKit.Cleanup'

function Assert-HkcuRoot([string]$root) {
    if ($root -notmatch '^HKCU:\\') { throw "RegRoot musí být pod HKCU:\ (zadáno: $root)" }
}

function Stop-FromDir([string]$dir, [string]$name) {
    $procs = Get-Process -Name $name -ErrorAction SilentlyContinue | Where-Object {
        $_.Path -and $_.Path.StartsWith($dir.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase)
    }
    foreach ($p in $procs) { Stop-Process -Id $p.Id -Force; $p.WaitForExit(3000) | Out-Null }
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
function Remove-InstallDir([string]$dir) {
    $clean = $true
    Get-ChildItem -LiteralPath $dir -Recurse -File -Force | ForEach-Object {
        if ($_.Name -like '*.del') { Remove-Item -LiteralPath $_.FullName -Force -ErrorAction SilentlyContinue }
        elseif (-not (Remove-OrSetAside $_.FullName)) { $clean = $false }
    }
    if (Get-ChildItem -LiteralPath $dir -Recurse -File -Force -Filter '*.del') { $clean = $false }
    if ($clean) { Remove-Item -LiteralPath $dir -Recurse -Force }
    return $clean
}

# One RunOnce value. Install passes no -WholeDir (delete *.del only); uninstall removes the directory.
# Re-installing replaces the uninstall variant, so a pending cleanup never deletes a fresh install.
function Register-DelCleanup([string]$root, [string]$dir, [switch]$WholeDir) {
    $runOnce = Join-Path $root 'RunOnce'
    New-Item -Force -ErrorAction SilentlyContinue $runOnce | Out-Null
    $cmd = if ($WholeDir) { "cmd.exe /d /c rd /s /q `"$dir`"" } else { "cmd.exe /d /c del /f /s /q `"$dir\*.del`"" }
    Set-ItemProperty -Path $runOnce -Name $CleanupName -Value $cmd
}

function Clear-DelCleanup([string]$root) {
    Remove-ItemProperty -Path (Join-Path $root 'RunOnce') -Name $CleanupName -ErrorAction SilentlyContinue
}
