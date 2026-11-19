# Opens throwaway file dialogs (own PowerShell host processes) at several locations and runs the probe on each.
$probe = 'C:\Users\adame\AppData\Local\cargo-target\file-manager\debug\dialog-probe.exe'
$docs = [Environment]::GetFolderPath('MyDocuments')
$cases = @(
  @{ Name='Documents';      Dir=$docs },
  @{ Name='Documents\sub';  Dir=(Get-ChildItem $docs -Directory | Select-Object -First 1).FullName },
  @{ Name='Desktop';        Dir=[Environment]::GetFolderPath('Desktop') },
  @{ Name='Downloads';      Dir="$env:USERPROFILE\Downloads" },
  @{ Name='C root';         Dir='C:\' },
  @{ Name='Deep w/ space';  Dir='C:\Users\adame\OneDrive\Desktop\file manager' },
  @{ Name='OneDrive root';  Dir=$env:OneDrive },
  @{ Name='This PC';        Dir='::{20D04FE0-3AEA-1069-A2D8-08002B30309D}' },
  @{ Name='Home';           Dir='::{f874310e-b6b7-47dc-bc84-b9e6b38f5903}' },
  @{ Name='Open dialog';    Dir=$docs; Kind='open' },
  @{ Name='Legacy dialog';  Dir=$docs; Kind='legacy' },
  @{ Name='32-bit host';    Dir=$docs; X86=$true }
)
foreach ($c in $cases) {
  $kind = if ($c.Kind) { $c.Kind } else { 'save' }
  $type = if ($kind -eq 'save') { 'SaveFileDialog' } else { 'OpenFileDialog' }
  $auto = if ($kind -eq 'legacy') { '$false' } else { '$true' }
  $ps = if ($c.X86) { "$env:WINDIR\SysWOW64\WindowsPowerShell\v1.0\powershell.exe" } else { 'powershell.exe' }
  $cmd = "Add-Type -A System.Windows.Forms; `$d=New-Object System.Windows.Forms.$type; `$d.AutoUpgradeEnabled=$auto; `$d.InitialDirectory='$($c.Dir)'; [void]`$d.ShowDialog()"
  $p = Start-Process $ps -ArgumentList '-NoProfile','-STA','-Command',$cmd -PassThru -WindowStyle Hidden
  Start-Sleep -Milliseconds 3500
  "=== $($c.Name)  (InitialDirectory=$($c.Dir))"
  & $probe
  Stop-Process -Id $p.Id -ErrorAction SilentlyContinue
  Start-Sleep -Milliseconds 300
}
