# Visual check: runs the helper, opens a throwaway dialog, brings it to the front and captures only
# the dialog's rectangle (plus the button) into PNG files. Usage: shot-dialogkit.ps1 -Out <dir>
param([string]$Out = "$env:TEMP\fm-dialogkit-shots", [string]$Kind = 'save', [switch]$X86, [switch]$Release)
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System; using System.Runtime.InteropServices; using System.Text;
public static class S {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, IntPtr pid);
  [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint a, uint b, bool f);
  [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowW(string c, string t);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
  // A harmless F24 tap makes this process the last input source, which Windows requires before
  // it lets a background process bring a window to the front.
  public static bool Front(IntPtr h) {
    keybd_event(0x87, 0, 0, UIntPtr.Zero); keybd_event(0x87, 0, 2, UIntPtr.Zero);
    return SetForegroundWindow(h);
  }
}
"@
$cfg = if ($Release) { 'release' } else { 'debug' }
$kit = "C:\Users\adame\.fm-target\$cfg\fm-dialogkit.exe"
$env:FM_X86_DIR = "C:\Users\adame\.fm-target\i686-pc-windows-msvc\$cfg"
New-Item -ItemType Directory -Force $Out | Out-Null
$dir = Join-Path $Out 'folder'; New-Item -ItemType Directory -Force $dir | Out-Null
$helper = Start-Process $kit -PassThru
$type = if ($Kind -eq 'save') { 'SaveFileDialog' } else { 'OpenFileDialog' }
$auto = if ($Kind -eq 'legacy') { '$false' } else { '$true' }
$ps = if ($X86) { "$env:WINDIR\SysWOW64\WindowsPowerShell\v1.0\powershell.exe" } else { 'powershell.exe' }
$cmd = "Add-Type -A System.Windows.Forms; `$d=New-Object System.Windows.Forms.$type; `$d.AutoUpgradeEnabled=$auto; `$d.InitialDirectory='$dir'; [void]`$d.ShowDialog()"
$p = Start-Process $ps -ArgumentList '-NoProfile', '-STA', '-Command', $cmd -PassThru -WindowStyle Hidden
Start-Sleep -Milliseconds 2500
$line = (& $kit --dump | Out-String) -split "`r?`n" | Where-Object { $_ -match "pid=$($p.Id)`t" } | Select-Object -First 1
$h = [IntPtr][long](($line -split "`t")[0])
"front: $([S]::Front($h))"
Start-Sleep -Milliseconds 900
$pill = [S]::FindWindowW('FmDialogKitPill', $null)
$pr = New-Object S+RECT; [void][S]::GetWindowRect($pill, [ref]$pr)
"pill visible=$([S]::IsWindowVisible($pill)) rect=$($pr.L),$($pr.T),$($pr.R),$($pr.B)"
function Shot($name, [int]$x, [int]$y, [int]$w, [int]$hh) {
  $bmp = New-Object System.Drawing.Bitmap $w, $hh
  $g = [System.Drawing.Graphics]::FromImage($bmp); $g.CopyFromScreen($x, $y, 0, 0, $bmp.Size)
  $bmp.Save((Join-Path $Out "$name.png")); $g.Dispose(); $bmp.Dispose(); "shot: $name.png"
}
if ([S]::GetForegroundWindow() -eq $h) {
  $r = New-Object S+RECT; [void][S]::GetWindowRect($h, [ref]$r)
  Shot "$Kind-dialog" $r.L $r.T ($r.R - $r.L) ([Math]::Min(260, $r.B - $r.T))
  # Hover state: move the mouse over the button.
  [void][S]::SetCursorPos([int](($pr.L + $pr.R) / 2), [int](($pr.T + $pr.B) / 2))
  Start-Sleep -Milliseconds 400
  Shot "$Kind-hover" ($pr.L - 260) ($pr.T - 14) 520 ($pr.B - $pr.T + 28)
} else { "dialog is not foreground (locked screen?) – no capture" }
Stop-Process -Id $p.Id -ErrorAction SilentlyContinue
Stop-Process -Id $helper.Id -ErrorAction SilentlyContinue
