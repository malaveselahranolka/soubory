# The cargo target directory: CARGO_TARGET_DIR, else target-dir in .cargo\config.toml, else <repo>\target.
function Get-TargetDir {
    if ($env:CARGO_TARGET_DIR) { return $env:CARGO_TARGET_DIR }
    $root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
    $cfg = Join-Path $root '.cargo\config.toml'
    if (Test-Path -LiteralPath $cfg) {
        $m = Select-String -LiteralPath $cfg -Pattern '^\s*target-dir\s*=\s*"([^"]+)"'
        if ($m) { return $m.Matches[0].Groups[1].Value -replace '/', '\' }
    }
    Join-Path $root 'target'
}
