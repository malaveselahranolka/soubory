# Opens a throwaway Save/Open dialog in a temp folder, creates a folder and a file through the hook,
# and captures a screenshot of the dialog after each step. Never touches real user folders.
param([string]$Out = "$env:TEMP\finderwin-dlgtest", [string]$Kind = 'save', [switch]$X86)
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System; using System.Runtime.InteropServices;
public static class R { [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, Rr, B; }
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
  public delegate bool EP(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] static extern bool EnumWindows(EP f, IntPtr l);
  [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
  public static IntPtr Top(uint pid) { IntPtr r = IntPtr.Zero; EnumWindows((h, l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == pid && IsWindowVisible(h)) { r = h; return false; } return true; }, IntPtr.Zero); return r; } }
"@
$t = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $PSScriptRoot 'target' }
$probe = if ($X86) { "$t\i686-pc-windows-msvc\debug\dialog-probe.exe" } else { "$t\debug\dialog-probe.exe" }
$dir = Join-Path $Out 'folder'
Remove-Item $Out -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $dir | Out-Null
'existing' | Set-Content (Join-Path $dir 'existing.txt')
$type = if ($Kind -eq 'save') { 'SaveFileDialog' } else { 'OpenFileDialog' }
$auto = if ($Kind -eq 'legacy') { '$false' } else { '$true' }
$ps = if ($X86) { "$env:WINDIR\SysWOW64\WindowsPowerShell\v1.0\powershell.exe" } else { 'powershell.exe' }
$cmd = "Add-Type -A System.Windows.Forms; `$d=New-Object System.Windows.Forms.$type; `$d.AutoUpgradeEnabled=$auto; `$d.InitialDirectory='$dir'; [void]`$d.ShowDialog()"
$p = Start-Process $ps -ArgumentList '-NoProfile','-STA','-Command',$cmd -PassThru -WindowStyle Hidden
Start-Sleep -Milliseconds 3000
function Shot($name) {
  # PrintWindow renders only the dialog itself, never whatever else is on screen.
  $h = [R]::Top([uint32]$p.Id)
  $r = New-Object R+RECT; [void][R]::GetWindowRect($h, [ref]$r)
  $bmp = New-Object System.Drawing.Bitmap ($r.Rr - $r.L), ($r.B - $r.T)
  $g = [System.Drawing.Graphics]::FromImage($bmp); $hdc = $g.GetHdc(); [void][R]::PrintWindow($h, $hdc, 2); $g.ReleaseHdc($hdc)
  $bmp.Save((Join-Path $Out "$name.png")); $g.Dispose(); $bmp.Dispose(); "  shot: $name.png"
}
foreach ($step in 'new-folder', 'new-file') {
  & $probe $step | Select-String 'create'
  Start-Sleep -Milliseconds 700
  Shot $step
}
Get-ChildItem $dir | ForEach-Object { "  on disk: $($_.Name)" }
Stop-Process -Id $p.Id -ErrorAction SilentlyContinue
