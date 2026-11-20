# Usage: uia-test.ps1 -Path <a scratch folder with several items, at least one of them a folder>
#        [-Exe fm.exe] [-View list|icons|tiles|columns] [-Group none|kind|modified|...]
param([Parameter(Mandatory)][string]$Path, [string]$Exe = 'C:\Users\adame\.fm-target\release\fm.exe', [string]$View = 'list', [string]$Group = 'none')
# UI Automation client test against fm's OWN test window only (FromHandle on its window; never RootElement).
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
# Event handlers in C#: UIA calls them on its own threads, where PowerShell script blocks cannot run.
Add-Type -ReferencedAssemblies UIAutomationClient, UIAutomationTypes, WindowsBase -TypeDefinition @'
using System; using System.Collections.Generic; using System.Windows.Automation;
public static class Ev {
  public static List<string> Sel = new List<string>(); public static List<string> Focus = new List<string>();
  public static int Struct = 0; public static int Pid = 0;
  public static void OnSel(object s, AutomationEventArgs e) { try { var n = ((AutomationElement)s).Current.Name; lock (Sel) Sel.Add(n); } catch { } }
  public static void OnStruct(object s, StructureChangedEventArgs e) { System.Threading.Interlocked.Increment(ref Struct); }
  // Desktop-wide by nature: anything not from fm's process is dropped before it is read.
  public static void OnFocus(object s, AutomationFocusChangedEventArgs e) { try { var el = (AutomationElement)s; if (el.Current.ProcessId != Pid) return; var n = el.Current.Name; lock (Focus) Focus.Add(n); } catch { } }
}
'@
$d = (Resolve-Path $Path).Path
$leaf = Split-Path $d -Leaf
$p = Start-Process $Exe -ArgumentList @("`"$d`"", '--new-instance', '--dev-view', $View, '--dev-group', $Group) -PassThru
try {
  for ($i = 0; $i -lt 60 -and $p.MainWindowHandle -eq 0; $i++) { Start-Sleep -Milliseconds 100; $p.Refresh() }
  Start-Sleep -Milliseconds 800
  $A = [System.Windows.Automation.AutomationElement]
  $W = [System.Windows.Automation.TreeWalker]::RawViewWalker
  $root = $A::FromHandle($p.MainWindowHandle)
  $ok = 0; $bad = @()
  function Check($name, $cond) { if ($cond) { $script:ok++ } else { $script:bad += $name } }
  function Kids($e) { $v = @(); $c = $W.GetFirstChild($e); while ($c) { $v += $c; $c = $W.GetNextSibling($c) }; $v }

  "root: '$($root.Current.Name)' type=$($root.Current.ControlType.ProgrammaticName)"
  $top = Kids $root
  foreach ($t in $top) { "  $($t.Current.ControlType.ProgrammaticName) '$($t.Current.Name)' children=$((Kids $t).Count)" }
  $toolbar = $top | Where-Object { $_.Current.ControlType -eq [System.Windows.Automation.ControlType]::ToolBar } | Select-Object -First 1
  $list = $top | Where-Object { $_.Current.ControlType -eq [System.Windows.Automation.ControlType]::List -and $_.Current.Name -eq $leaf } | Select-Object -First 1
  $status = $top | Where-Object { $_.Current.ControlType -eq [System.Windows.Automation.ControlType]::StatusBar } | Select-Object -First 1
  Check 'toolbar' ($null -ne $toolbar); Check 'content list' ($null -ne $list); Check 'status' ($null -ne $status)
  "  buttons: " + ((Kids $toolbar | ForEach-Object { $_.Current.Name }) -join ', ')

  # 1. Names match the folder.
  $items = Kids $list
  $names = $items | ForEach-Object { $_.Current.Name }
  $disk = Get-ChildItem $d | ForEach-Object { $_.Name }
  Check 'names match' ((Compare-Object ($names | Sort-Object) ($disk | Sort-Object)) -eq $null)
  "  items: $($items.Count) (disk $($disk.Count)); first: $($names[0]); help: $($items[1].Current.HelpText)"
  # 2. Last item has no next sibling; first has no previous.
  Check 'last next null' ($null -eq $W.GetNextSibling($items[-1]))
  Check 'first prev null' ($null -eq $W.GetPreviousSibling($items[0]))
  Check 'parent of item is list' ($W.GetParent($items[0]) -eq $list)
  # Events scoped to fm's own window subtree.
  [Ev]::Pid = $p.Id
  $h = [Delegate]::CreateDelegate([System.Windows.Automation.AutomationEventHandler], [Ev].GetMethod('OnSel'))
  [System.Windows.Automation.Automation]::AddAutomationEventHandler([System.Windows.Automation.SelectionItemPattern]::ElementSelectedEvent, $list, [System.Windows.Automation.TreeScope]::Subtree, $h)
  $hs = [Delegate]::CreateDelegate([System.Windows.Automation.StructureChangedEventHandler], [Ev].GetMethod('OnStruct'))
  [System.Windows.Automation.Automation]::AddStructureChangedEventHandler($root, [System.Windows.Automation.TreeScope]::Subtree, $hs)
  $hf = [Delegate]::CreateDelegate([System.Windows.Automation.AutomationFocusChangedEventHandler], [Ev].GetMethod('OnFocus'))
  [System.Windows.Automation.Automation]::AddAutomationFocusChangedEventHandler($hf)
  # 3. Select -> IsSelected, status text follows.
  $target = $items[3]
  $sip = $target.GetCurrentPattern([System.Windows.Automation.SelectionItemPattern]::Pattern)
  $sip.Select(); Start-Sleep -Milliseconds 400
  Check 'is selected' ($sip.Current.IsSelected)
  $sp = $list.GetCurrentPattern([System.Windows.Automation.SelectionPattern]::Pattern)
  $selNames = $sp.Current.GetSelection() | ForEach-Object { $_.Current.Name }
  Check 'selection is the item' (@($selNames).Count -eq 1 -and $selNames -eq $target.Current.Name)
  "  status after select: $($status.Current.Name)"
  Check 'status says 1 selected' ($status.Current.Name -like 'Vybráno 1 z*')
  $sip2 = $items[5].GetCurrentPattern([System.Windows.Automation.SelectionItemPattern]::Pattern)
  $sip2.AddToSelection(); Start-Sleep -Milliseconds 300
  Check 'add to selection' ((@($sp.Current.GetSelection())).Count -eq 2)
  Start-Sleep -Milliseconds 300
  "  element-selected events: " + ([Ev]::Sel -join ' | ')
  Check 'element selected event' ([Ev]::Sel -contains $target.Current.Name)
  "  focus events from fm: " + ([Ev]::Focus -join ' | ')
  Check 'focus follows the selection' ([Ev]::Focus -contains $target.Current.Name)
  # 4. Bounding rect of a visible item lies inside the window.
  $wr = $root.Current.BoundingRectangle; $ir = $items[0].Current.BoundingRectangle
  Check 'item rect inside window' ($ir.Left -ge $wr.Left -and $ir.Right -le $wr.Right -and $ir.Top -ge $wr.Top -and $ir.Bottom -le $wr.Bottom -and $ir.Width -gt 0)
  # FromPoint at the item's centre returns that item.
  $pt = New-Object System.Windows.Point (($ir.Left + 40), (($ir.Top + $ir.Bottom) / 2))
  $hit = $A::FromPoint($pt)
  Check 'from point' ($hit.Current.Name -eq $items[0].Current.Name)
  # 5. Invoke a folder -> the window goes there; a held item from the old list is not available.
  $folder = $items | Where-Object { $_.Current.ItemType -like 'Složka*' } | Select-Object -First 1
  $held = $items[-1]; $heldName = $held.Current.Name
  $folderName = $folder.Current.Name
  $folder.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke(); Start-Sleep -Milliseconds 600
  "  title after invoke: '$($root.Current.Name)'"
  Check 'invoke opens folder' ($root.Current.Name -eq $folderName)
  "  structure events after navigation: $([Ev]::Struct)"
  Check 'structure changed on navigation' ([Ev]::Struct -gt 0)
  $gone = $false; $what = ''
  try { $what = "name='" + $held.Current.Name + "' rect=" + $held.Current.BoundingRectangle } catch { $gone = $true; $what = $_.Exception.GetType().Name }
  "  held after navigation: $what"
  try { $x = $W.GetNextSibling($held); "  held next sibling: " + $(if ($x) { $x.Current.Name } else { 'null' }) } catch { "  held next sibling: " + $_.Exception.GetType().Name }
  # The core turns a gone element into empty values: it must never answer as another file.
  Check 'held item reports nothing' ($gone -or $what -eq "name='' rect=")
  # 6. Back button (Invoke) returns; the held item is there again.
  $back = Kids $toolbar | Where-Object { $_.Current.Name -eq 'Zpět' }
  Check 'back enabled' ($back.Current.IsEnabled)
  $back.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke(); Start-Sleep -Milliseconds 600
  Check 'back returns' ($root.Current.Name -eq $leaf)
  $again = $false; try { $again = $held.Current.Name -eq $heldName } catch { }
  Check 'held item back' $again
  # 7. Sidebar places.
  $side = $top | Where-Object { $_.Current.Name -eq 'Postranní panel' -and $_.Current.ControlType -eq [System.Windows.Automation.ControlType]::List }
  "  places: " + ((Kids $side | Select-Object -First 6 | ForEach-Object { $_.Current.Name }) -join ', ')
  Check 'places' ((Kids $side).Count -ge 5)
  # 8. Focus: the focused element is an item of the list.
  [System.Windows.Automation.Automation]::RemoveAllEventHandlers()
  "RESULT ok=$ok bad=[$($bad -join ', ')]"
} finally {
  if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force }
}
