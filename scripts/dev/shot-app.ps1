# Captures ONLY the fm window itself (PrintWindow, full content) into a PNG. Never the screen.
# Never synthesizes input (it could land in another window); drive fm with its --dev-* flags.
param([Parameter(Mandatory)][string]$Path, [Parameter(Mandatory)][string]$Out, [int]$Wait = 1500, [string[]]$Dev = @(), [string]$Class = '', [switch]$Keep)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System; using System.Runtime.InteropServices; using System.Drawing;
public static class Shot {
  [DllImport("user32.dll")] static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
  [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("dwmapi.dll")] static extern int DwmGetWindowAttribute(IntPtr h, int a, out RECT r, int s);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [DllImport("user32.dll")] static extern IntPtr SetThreadDpiAwarenessContext(IntPtr c);
  delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc p, IntPtr l);
  [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetClassName(IntPtr h, System.Text.StringBuilder s, int n);
  // Only a window of fm's own process: a class name alone could match another app's window.
  public static IntPtr Find(int pid, string cls) {
    IntPtr found = IntPtr.Zero;
    EnumWindows((h, l) => {
      uint p; GetWindowThreadProcessId(h, out p);
      var s = new System.Text.StringBuilder(256); GetClassName(h, s, 256);
      if (p == (uint)pid && s.ToString() == cls) { found = h; return false; }
      return true;
    }, IntPtr.Zero);
    return found;
  }
  public static void Save(IntPtr h, string path) {
    SetThreadDpiAwarenessContext(new IntPtr(-4)); // per-monitor v2: physical coordinates
    RECT r; GetWindowRect(h, out r);
    var bmp = new Bitmap(r.R - r.L, r.B - r.T);
    using (var g = Graphics.FromImage(bmp)) { var dc = g.GetHdc(); PrintWindow(h, dc, 2); g.ReleaseHdc(dc); }
    RECT f; DwmGetWindowAttribute(h, 9, out f, 16);
    var crop = new Rectangle(f.L - r.L, f.T - r.T, f.R - f.L, f.B - f.T);
    bmp.Clone(crop, bmp.PixelFormat).Save(path, System.Drawing.Imaging.ImageFormat.Png);
  }
}
'@ -ReferencedAssemblies System.Drawing
$exe = 'C:\Users\adame\.fm-target\debug\fm.exe'
$p = Start-Process $exe -ArgumentList (@("`"$Path`"", "--new-instance") + $Dev) -PassThru
try {
  for ($i = 0; $i -lt 50 -and $p.MainWindowHandle -eq 0; $i++) { Start-Sleep -Milliseconds 100; $p.Refresh() }
  Start-Sleep -Milliseconds $Wait
  $h = if ($Class) { [Shot]::Find($p.Id, $Class) } else { $p.MainWindowHandle }
  if ($h -eq [IntPtr]::Zero) { throw "No fm window to capture (class '$Class')." }
  [Shot]::Save($h, $Out)
  $Out
} finally {
  # A failed capture must not leave a test instance behind.
  if (-not $Keep -and -not $p.HasExited) { Stop-Process -Id $p.Id -Force }
}
