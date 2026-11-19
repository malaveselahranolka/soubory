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
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindow(string c, string t);
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
$p = Start-Process $exe -ArgumentList (@("`"$Path`"") + $Dev) -PassThru
for ($i = 0; $i -lt 50 -and $p.MainWindowHandle -eq 0; $i++) { Start-Sleep -Milliseconds 100; $p.Refresh() }
Start-Sleep -Milliseconds $Wait
$h = if ($Class) { [Shot]::FindWindow($Class, [NullString]::Value) } else { $p.MainWindowHandle }
[Shot]::Save($h, $Out)
if (-not $Keep) { Stop-Process -Id $p.Id }
$Out
