# End-to-end check of fm-dialogkit against throwaway dialogs (own PowerShell processes, own temp folder).
# Never creates anything outside $Root. Usage: scripts\dev\test-dialogkit.ps1 [-Release]
param([switch]$Release, [string]$Root = "$env:TEMP\fm-dialogkit-test")
$ErrorActionPreference = 'Stop'
$cfg = if ($Release) { 'release' } else { 'debug' }
$kit = "C:\Users\adame\.fm-target\$cfg\fm-dialogkit.exe"
$env:FM_X86_DIR = "C:\Users\adame\.fm-target\i686-pc-windows-msvc\$cfg"
$fail = 0

function Open-Dialog([string]$Dir, [string]$Kind, [switch]$X86) {
  $type = if ($Kind -eq 'save') { 'SaveFileDialog' } else { 'OpenFileDialog' }
  $auto = if ($Kind -eq 'legacy') { '$false' } else { '$true' }
  $ps = if ($X86) { "$env:WINDIR\SysWOW64\WindowsPowerShell\v1.0\powershell.exe" } else { 'powershell.exe' }
  $cmd = "Add-Type -A System.Windows.Forms; `$d=New-Object System.Windows.Forms.$type; `$d.AutoUpgradeEnabled=$auto; `$d.Filter='Vše|*.*'; `$d.InitialDirectory='$Dir'; [void]`$d.ShowDialog()"
  $p = Start-Process $ps -ArgumentList '-NoProfile', '-STA', '-Command', $cmd -PassThru -WindowStyle Hidden
  for ($i = 0; $i -lt 40; $i++) {
    Start-Sleep -Milliseconds 200
    foreach ($l in (Kit --dump)) {
      if ($l -match "pid=$($p.Id)`t") { return @{ Proc = $p; Hwnd = ($l -split "`t")[0] } }
    }
  }
  throw "dialog did not appear"
}

# The helper is a GUI-subsystem exe: piping makes PowerShell wait for it and capture stdout.
function Kit { (& $kit @args | Out-String).Trim() -split "`r?`n" }

function Check($name, $cond, $detail) {
  if ($cond) { "  PASS  $name" } else { "  FAIL  $name :: $detail"; $script:fail++ }
}

$cases = @(
  @{ Name = 'item save 64'; Kind = 'save' },
  @{ Name = 'item open 64'; Kind = 'open' },
  @{ Name = 'legacy open 64'; Kind = 'legacy' },
  @{ Name = 'item save 32'; Kind = 'save'; X86 = $true },
  @{ Name = 'diacritics dir'; Kind = 'save'; Sub = 'Žluťoučký kůň' }
)
foreach ($c in $cases) {
  $dir = Join-Path $Root ($c.Name -replace ' ', '_')
  if ($c.Sub) { $dir = Join-Path $dir $c.Sub }
  Remove-Item $dir -Recurse -Force -ErrorAction SilentlyContinue
  New-Item -ItemType Directory -Force $dir | Out-Null
  "=== $($c.Name)"
  $d = Open-Dialog $dir $c.Kind -X86:$c.X86
  try {
    $q = Kit --act query --hwnd $d.Hwnd
    Check 'query folder' ($q -match ('^OK	' + [regex]::Escape($dir) + '	')) $q
    $a = Kit --act anchor --hwnd $d.Hwnd
    Check 'anchor found' ($a -match 'Some\(RECT') $a
    $list = Kit --act list --hwnd $d.Hwnd
    $folderIdx = ($list | Where-Object { $_ -match "`tSložka|`tFolder" } | Select-Object -First 1) -split "`t" | Select-Object -First 1
    $txtIdx = ($list | Where-Object { $_ -match 'Textový dokument|Text Document' } | Select-Object -First 1) -split "`t" | Select-Object -First 1
    Check 'menu has folder + text' ($folderIdx -ne $null -and $txtIdx -ne $null) ($list -join ' | ')
    foreach ($idx in $folderIdx, $txtIdx, $txtIdx) {
      $r = Kit --act new $idx --hwnd $d.Hwnd
      Check "new #$idx selected" ($r -match 'selected=Some\(Ok\(\(\)\)\)') $r
      ($r -split "`t")[-1]
    }
    $names = (Get-ChildItem $dir -Force | Select-Object -ExpandProperty Name) -join ', '
    Check 'three items on disk' ((Get-ChildItem $dir -Force).Count -eq 3) $names
    "  on disk: $names"
  } finally {
    Stop-Process -Id $d.Proc.Id -ErrorAction SilentlyContinue
  }
}
if ($fail) { "FAILED: $fail"; exit 1 } else { "ALL PASSED" }
