<#
.SYNOPSIS
  Builds release binaries for x64 and x86 and stages them in dist\.
.DESCRIPTION
  dist\dialogkit\            fm-dialogkit.exe, fm_dialog_hook.dll
  dist\dialogkit\x86\        32-bit helper and hook (reaches 32-bit Open/Save dialogs)
  dist\app\                  fm.exe (when the app crate exists)
#>
param([switch]$SkipX86)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$target = 'C:\Users\adame\.fm-target'
if (Test-Path "$root\.cargo\config.toml") {
    $m = Select-String -Path "$root\.cargo\config.toml" -Pattern 'target-dir\s*=\s*"([^"]+)"'
    if ($m) { $target = $m.Matches[0].Groups[1].Value -replace '/', '\' }
}

Push-Location $root
# cargo writes progress to stderr; with 'Stop' Windows PowerShell would treat that as a failure.
$ErrorActionPreference = 'Continue'
try {
    cargo build --release -p fm-dialogkit -p fm-dialog-hook
    if ($LASTEXITCODE) { throw 'x64 build failed' }
    if (-not $SkipX86) {
        cargo build --release --target i686-pc-windows-msvc -p fm-dialogkit -p fm-dialog-hook
        if ($LASTEXITCODE) { throw 'x86 build failed' }
    }
    if (Test-Path "$root\crates\app\Cargo.toml") {
        cargo build --release -p fm-app
        if ($LASTEXITCODE) { throw 'app build failed' }
    }
} finally { Pop-Location; $ErrorActionPreference = 'Stop' }

$dk = Join-Path $root 'dist\dialogkit'
New-Item -ItemType Directory -Force $dk | Out-Null
Copy-Item "$target\release\fm-dialogkit.exe", "$target\release\fm_dialog_hook.dll" $dk -Force
if (-not $SkipX86) {
    New-Item -ItemType Directory -Force "$dk\x86" | Out-Null
    Copy-Item "$target\i686-pc-windows-msvc\release\fm-dialogkit.exe", "$target\i686-pc-windows-msvc\release\fm_dialog_hook.dll" "$dk\x86" -Force
}
if (Test-Path "$target\release\fm.exe") {
    $app = Join-Path $root 'dist\app'
    New-Item -ItemType Directory -Force $app | Out-Null
    Copy-Item "$target\release\fm.exe" $app -Force
}
Get-ChildItem -Recurse (Join-Path $root 'dist') -File | ForEach-Object { '{0,10:N0}  {1}' -f $_.Length, $_.FullName.Substring($root.Length + 1) }
