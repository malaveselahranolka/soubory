# Renders throwaway dialogs with the button composited (normal | hover) into PNGs. No foreground needed.
param([string]$Out = "$env:TEMP\fm-dialogkit-preview", [switch]$Release)
Add-Type -AssemblyName System.Drawing
$cfg = if ($Release) { 'release' } else { 'debug' }
. (Join-Path $PSScriptRoot '..\lib\target-dir.ps1'); $target = Get-TargetDir
$kit = "$target\$cfg\fm-dialogkit.exe"
New-Item -ItemType Directory -Force $Out | Out-Null
$dir = Join-Path $Out 'folder'; New-Item -ItemType Directory -Force $dir | Out-Null
foreach ($kind in 'save', 'legacy') {
  $type = if ($kind -eq 'save') { 'SaveFileDialog' } else { 'OpenFileDialog' }
  $auto = if ($kind -eq 'legacy') { '$false' } else { '$true' }
  $cmd = "Add-Type -A System.Windows.Forms; `$d=New-Object System.Windows.Forms.$type; `$d.AutoUpgradeEnabled=$auto; `$d.InitialDirectory='$dir'; [void]`$d.ShowDialog()"
  $p = Start-Process powershell -ArgumentList '-NoProfile', '-STA', '-Command', $cmd -PassThru -WindowStyle Hidden
  $line = $null
  for ($i = 0; $i -lt 50 -and -not $line; $i++) {
    Start-Sleep -Milliseconds 200
    $line = (& $kit --dump | Out-String) -split "`r?`n" | Where-Object { $_ -match "pid=$($p.Id)`t" } | Select-Object -First 1
  }
  Start-Sleep -Milliseconds 500
  $h = ($line -split "`t")[0]
  $bmp = Join-Path $Out "$kind.bmp"
  (& $kit --act preview --hwnd $h --out $bmp | Out-String).Trim()
  $img = [System.Drawing.Image]::FromFile($bmp)
  # Crop to the top part (title, address and command bar) where the button lives.
  $crop = New-Object System.Drawing.Bitmap $img.Width, ([Math]::Min(300, $img.Height))
  $g = [System.Drawing.Graphics]::FromImage($crop); $g.DrawImage($img, 0, 0); $g.Dispose()
  if ($kind -eq 'legacy') { $crop.Dispose(); $crop = $img.Clone() }
  $crop.Save((Join-Path $Out "$kind.png"), [System.Drawing.Imaging.ImageFormat]::Png); $crop.Dispose(); $img.Dispose()
  Remove-Item $bmp
  Stop-Process -Id $p.Id -ErrorAction SilentlyContinue
}

