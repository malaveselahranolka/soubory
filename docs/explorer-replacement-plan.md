# Plan: make fm (Soubory) the only file manager on Windows 11, per user

The user asked to "replace everything; I only want to use ours".

Each claim carries a grade:
- [V] verified in this session (read-only registry queries, repo code, or a test exe in the scratchpad)
- [F] primary source fetched, by me or a research agent
- [S] search summary only
- [M] memory or inference only
- [U] unverified, needs a test

Nothing on the machine was changed.

## 0. Correction to the task's premise: the redirect is NOT applied [V]

- A read-only query in this session shows these are absent: `HKCU\Software\Classes\Drive\shell`, `HKCU\Software\Classes\Folder\shell`, `HKCU\Software\Classes\CLSID\{52205fd8-…}` and the backup key `HKCU\Software\fm`.
- `HKCU\Software\Classes\Directory\shell` exists but is empty. The merged HKCR still shows Directory and Drive default = `none`, and `Folder\shell\{open,explore}` comes from HKLM.
- Project memory (open-decisions.md) says install-fm.ps1 and set-default-fm.ps1 have only ever been run in sandboxes.
- So today everything still opens Explorer. The task's "still leaking" list is predicted, not observed.
- Caveat: this session runs inside the Claude MSIX container. Registry reads normally see the real HKCU, but the user should confirm with `reg query HKCU\Software\fm\Backup` from their own PowerShell.
- A useful side finding [V]: `HKCU\Software\Classes\Directory\shell` has no default value, yet HKCR reports HKLM's `none`. Values that are missing from an HKCU key leak through from HKLM. So every HKCU command override must also write `DelegateExecute=""` explicitly to hide HKLM's `{11dbb47c-a525-400b-9e80-a54615a090c0}`. Files and fm's own Win+E key already do this.

## 0b. Confirmed bug in the route that already exists (Drive verb) [V]

- Explorer passes a drive root as `C:\`. With the registered command `"fm.exe" "%1"`, the raw command line is `"…\fm.exe" "C:\"`.
- Rust `std::env::args` (MSVC rules) turns that into `C:"`. `start_loc` trims it to `C:`, and `is_dir()` is true for it, but on Windows `C:` means "the current directory on drive C".
- Test output from my scratchpad exe, with the working directory set to C:\Windows\System32:
  - `[C:"] -> trimmed [C:] is_dir=true`
  - `[D:"] -> [D:] is_dir=true`
  - `"C:\Program Files\" extra` arrives as one argument, `C:\Program Files" extra`, which is not a directory, so fm falls back to the profile folder.
- Test source: `C:\Users\adame\AppData\Local\Temp\claude\d5a2f667c153e111\e5657664-345a-43d3-b9f9-d2c727afc9c9\scratchpad\argtest\a.rs` and `C:\Users\adame\AppData\Local\Temp\claude\d5a2f667c153e111\e5657664-345a-43d3-b9f9-d2c727afc9c9\scratchpad\argtest\t.cmd`.
- Fix it in fm by tokenizing the raw `GetCommandLineW` tail. A registry trick such as `"%1\."` would break `::{GUID}` targets.
- set-default-fm.ps1 should not be run for real before this fix.

## 1. Every Explorer entry point and the per-user mechanism that redirects it

Notation:
- `CR` = `HKCU\Software\Classes`
- `CMD` = (Default) REG_SZ `"<fm.exe>" "%1"`
- `DE` = `DelegateExecute` REG_SZ `""` (empty)

All of these are HKCU only, need no admin, and are removed by the restore script.

| # | Entry point | How the caller reaches the shell | Per-user redirect (exact) | Confidence / proof |
|---|---|---|---|---|
| 1 | Folder or drive double-click: desktop, inside Explorer windows, Win+R with a path, `start .`, `ii .`, any ShellExecute with a NULL verb | The item's default verb. Association array: Directory→Folder, or Drive→Folder | **Existing**: `CR\Directory\shell` (Default)=`fm.open`, `…\fm.open\command` CMD. The same for `CR\Drive\shell`. | High. Explorer++, XYplorer and OneCommander ship the same thing [F]. A side effect is intended: a folder double-clicked inside an Explorer window also opens fm. |
| 2 | Explicit `open` verb on a file-system folder | `ShellExecute(…,"open",dir)`. Chrome uses it as the ShowItemInFolder fallback on ERROR_FILE_NOT_FOUND [V, Chromium platform_util_win.cc fetched]. Many other apps do too. Possibly also the cold path of SHOpenFolderAndSelectItems [U]. | **New**: `CR\Directory\shell\open\command` CMD+DE, and `CR\Drive\shell\open\command` CMD+DE. Keep default=`fm.open`. HKLM has no Directory or Drive `open` verb [V]. | Medium. It relies on Directory/Drive coming before Folder in the association array: documented, and probed by an agent, but not live-tested with the key in place. No product does exactly this; Files instead overrides `Folder\shell\open` broadly. Risk: a duplicate "Otevřít" item in the context menu [U]. |
| 3 | Explicit `explore` verb with class `folder` | Chromium/Edge `ui::win::OpenFolderViaShell`: lpVerb `explore`, lpClass `folder`, SEE_MASK_CLASSNAME [V, shell.cc fetched]. Electron `shell.openPath` on a folder probably does the same [U]. lpClass=folder skips the Directory and Drive keys. | **New**: `CR\Folder\shell\explore\command` CMD+DE | High. Files ships exactly this key [V, SetFilesAsDefault.reg fetched: `Folder\shell\open\command`, `Folder\shell\explore\command`, and the Win+E CLSID, each with `@` hex(2) plus DelegateExecute]. It is Folder-wide, so virtual folders opened with `explore` also reach fm. The passthrough (§2B) must exist first. Risk: if HKLM's ProgrammaticAccessOnly does not carry over, "explore" may appear in context menus [U]. |
| 4 | "Show in folder" / "Open containing folder" through SHOpenFolderAndSelectItems | **Chrome/Edge**: ShowItemInFolder → SHOpenFolderAndSelectItems [V, Chromium source]. **Firefox, Visual Studio** [F]. **Task Manager**: imports it [byte scan by an agent]. **GitHub Desktop** [F]. **VS Code/Electron** [U]. Warm path: IShellWindows::FindWindowSW(SWC_BROWSER, folder PIDL) → get_Document → QueryService(IID_IFolderView/IShellView) → `IShellView::SelectItem`. Cold path: the folder is launched through rows 1/2, then the shell waits for a pending registration. | **No registry. A COM change in fm**: register each window in IShellWindows (RegisterPending + Register + OnNavigate, SWC_BROWSER; Revoke on close). Implement IWebBrowserApp/IDispatch, IServiceProvider and IShellView::SelectItem, and route SelectItem to `App::go_select`. | High. **Files PR #18784**, merged 2026-08-13, was tested with Edge, Chrome, Task Manager and GitHub Desktop [V, PR fetched]. The follow-up issue #18818, "opening a folder from a shortcut no longer works", is a regression to test for. Explorer++ has done this since 2021 [F]. Limits: an Explorer window already open on that folder always wins. Without registration the folder opens without the file selected, after a 10–15 s stall [S]. The ShellWindows host is `LocalServer32 = rundll32 shell32.dll,SHCreateLocalServerRunDll` [V], so it exists even if explorer.exe is not the shell. |
| 5 | "Open file location" | **Task Manager**: row 4 (confirmed by the Files PR). **.lnk on the desktop or in Explorer**: verb `opencontaining` → shell32 OpenContainingFolderMenu `{37ea3a21-7493-4208-a011-7f9ea79ce9f5}` [reg query by an agent]. It probably ends in SHOpenFolderAndSelectItems, which would make it row 4 [U]. **Start menu, Search results** [U]. | Same as row 4 | Low to medium. Needs the tier-D tests. |
| 6 | Win+E | CLSID `{52205fd8-5dfb-447d-801a-d0b52f2e83e1}`, verb `opennewwindow` | **Existing**: `CR\CLSID\{52205fd8-…}\shell\opennewwindow\command` (Default) `"<fm.exe>"` + DE | High. Files uses the same key [F]. |
| 7 | Start "File Explorer" entry and the taskbar pin | Both shortcuts target `::{52205FD8-…}` [agent]. That CLSID has the verbs `open` and `OpenNewWindow` and no default value [V], so the default verb is `open`. | **New**: `CR\CLSID\{52205fd8-…}\shell\open\command` (Default) `"<fm.exe>"` + DE. For the taskbar, the user unpins Explorer and pins fm by hand; fm's windows group under fm.exe anyway. Never delete `Folder\shell\opennewwindow`, `opennewtab` or `opennewprocess`. | Medium/low. Which verb Start and the taskbar actually invoke is [U]. |
| 8 | This PC: desktop icon, `shell:MyComputerFolder`, `::{20D04FE0-…}` | The CLSID key is in the association array. HKLM shell default=`none`; verbs are `find` and `Manage` [V]. | **New**: `CR\CLSID\{20D04FE0-3AEA-1069-A2D8-08002B30309D}\shell` (Default)=`open`, plus `…\shell\open\command` CMD+DE. fm maps `::{20D04FE0…}` to `Loc::Computer`. Restore must delete the HKCU default value so HKLM's `none` returns. | Medium. Same pattern as the Win+E key. Files shows This PC itself, but through its broad Folder override [F]. |
| 9 | Recycle Bin {645FF040-5081-101B-9F08-00AA002F954E} | HKLM verbs are `empty` and `ModernProperties`, with no default [V] | **New**: `…\CLSID\{645FF040…}\shell` (Default)=`open`, plus `\open\command` CMD+DE. fm already renders it (`shellns::recycle_bin`). | Medium [U live] |
| 10 | Network {F02C1A0D-BE21-4350-88B0-7367FC96EF3C}; `\\server` | No shell key in HKCR [V]. `\\server` is the NetServer class, whose only verb is remotedesktop, so it falls through to Folder. `\\server\share` is a Directory, so row 1 covers it. | **New**: `CR\CLSID\{F02C1A0D…}\shell` (Default)=`open`, plus `\open\command` CMD+DE. Optional: `CR\NetServer\shell\open\command` CMD+DE. | Medium. The NetServer ordering in the association array is [U]. |
| 11 | `shell:` URIs (Win+R, ShellExecute) | There is no protocol key. The URI is parsed to a PIDL and then the item's default verb runs. | No extra key. File-system known folders resolve through row 1. RecycleBin, MyComputer and NetworkPlaces resolve through rows 8–10. Everything else stays with Explorer. | Medium [agent probe] |
| 12 | `file:///` folder URLs | Parsed to the folder item | No key; row 1 covers it. Do not touch `HKCR\file`. | Medium |
| 13 | Optional: User's Files `{59031a47-3f72-44a7-89c5-5595fe6b30ee}`, Home `{f874310e-b6b7-47dc-bc84-b9e6b38f5903}` | Folder verb | User's Files: `\shell\open\command` = `"<fm.exe>" "%USERPROFILE%"` (REG_EXPAND_SZ) + DE. Home: `"<fm.exe>"`. | Low value, low confidence |
| 14 | `explorer.exe` started directly: `/select,<path>`, `<path>`, `/root` (uTorrent, IDM, older and Electron apps) | A new explorer.exe opens its own window and never consults the folder verbs. Files relies on this for its passthrough [F]. Plain `/select` opens a new window even when one is already open [S]. | **No registry route** (§3). Per user, the only route is an Explorer-window watcher. Host it in **fm-dialogkit**, which is already resident through HKCU Run and already uses an out-of-context `SetWinEventHook` [V, crates/dialogkit/src/app.rs]. When a new CabinetWClass window appears, read its folder and selection through IShellWindows, call `Quit()` on it, and open fm with `/select`. | Medium. Proof: abdonkov/FileExplorerInterceptor [F]. Explorer flashes briefly. It needs an allow-list (Control Panel, fm's own escape hatch, the user's typed `explorer`). |
| 15 | Control Panel (`{26EE0668…}`, `{21EC2020…}`), `control.exe /name …`, Settings → legacy applet links, Libraries, Gallery, Home feed, AppsFolder | Folder\shell\open DelegateExecute | **Leave these to Explorer (recommended).** Taking them needs the broad `Folder\shell\open` override (the Files model), and then deep links break: Files #15344, BitLocker `/page` lost [F]. | User decision |
| 16 | Explorer's taskbar jump list and pinned folders | Owned by Explorer's button | Not interceptable [S, Opus staff 2015]. The replacement is an fm pin plus fm's own AUMID and jump list (ICustomDestinationList). fm has none today [V, grep]. | Low |

## 2. What fm.exe must accept or implement

Do this before any new registry keys go in.

**A. A real target parser.** Replace `start_loc`, which today accepts only `is_dir()` paths [V main.rs:37-43]. Proposed `parse_target(raw) -> Open{loc, select} | Passthrough(raw) | Error`.

Inputs:
- Tokenize the raw `GetCommandLineW` tail. This fixes §0b.
- Folder paths, drive roots with a trailing `\`, UNC `\\server\share`.
- Environment variables (`expand_env` already exists in win.rs but is not used on the command line).
- A **file path**: open its parent and select the file. Today this silently opens the profile folder.
- Explorer switches:
  - `/select,<path>`, and `/select` followed by the path as a separate token
  - `/root,<target>`
  - `/e,`, `/n`, `/separate`: ignore
  - commas as separators
- `::{GUID}[\…]`, `shell:Name`, `shell:::{GUID}`, resolved through SHParseDisplayName. Map the result:
  - a file-system directory → `Loc::Folder`
  - the This PC PIDL → `Loc::Computer`
  - Recycle Bin, Network, `\\server` and other browsable folders fm supports → `Loc::Shell` (`ShellId::from_raw`)
  - anything else → Passthrough
- `file:///…` through PathCreateFromUrlW.

Behaviour on bad input: show an error note instead of the silent profile-folder fallback. An empty argument still opens the default start view.

COM initialization: today `start_loc` runs in `main()` before `OleInitialize` in `win::run` [V]. Initialize COM first, or parse inside `run`.

**B. Passthrough, loop guard and escape hatch.**
- Unsupported targets go to `CreateProcess explorer.exe "<original>"`, with `shell:` prefixed to `::{…}`. This is Files' approach [F].
- fm must never ShellExecute a folder or virtual folder with a default, `open` or `explore` verb while the redirect is active; it navigates in-app instead.
- Add an "Otevřít v Průzkumníkovi" command as the user's escape hatch. None exists today [V, grep finds no explorer.exe in crates/app].

**C. Hand-over.** The Local\fm.main mutex plus WM_COPYDATA `COPY_OPEN` already sends the raw string, and the receiver re-parses it [V win.rs:222-257, 511-529]. Keep that, and have the receiver apply `select` through `go_select`. An older running fm ignores an unknown dwData, which is acceptable.

**D. IShellWindows registration.** This is the "Show in folder" fix.
- One registration per top-level window. Call OnNavigate on every navigation and on every change of the active tab.
- Revoke when a window closes. This must happen **before** the `TerminateProcess` exit in main.rs; otherwise ghost entries stay until explorer restarts.
- Build the objects with the windows crate `#[implement]`:
  - IWebBrowserApp: get_HWND, get_Document, get_LocationURL, Visible, Quit; everything else returns E_NOTIMPL.
  - IServiceProvider: answers IID_IFolderView and IShellView.
  - IShellView::SelectItem: SVSI_SELECT, FOCUSED, ENSUREVISIBLE and DESELECTOTHERS; SVSI_EDIT starts a rename.
- It must never crash under arbitrary `Shell.Application.Windows()` callers: Everything, AHK, the Files launcher, our own watcher.
- Code references:
  - Files `src/Files.App.Launcher/OpenInFolder.cpp` is MIT, so code can be adapted.
  - Explorer++ `BrowsingHandler.cpp` is GPL-3.0; read it for the pattern only.
  - dialog-hook already uses `IShellView::SelectItem` as a client [V].

**E. Cold start through hand-over [U].**
- The shell launches a second fm.exe through the verb. That process hands over to the hub and exits.
- It is unverified whether the hub's RegisterPending, matched by PIDL, satisfies the shell's wait.
- Fallback, the Files pattern: the launched process calls RegisterPending for the target PIDL itself and forwards the path to the hub. It keeps a cloaked registered window up to about 10 s to forward late SelectItem calls, then exits.

**F. Views fm must show.**
- This PC: `Loc::Computer`, exists.
- Recycle Bin and Network: `Loc::Shell`, exist.
- `\\server`: `Loc::Shell` through the parser, untested.
- Control Panel and the rest: passthrough.

**G. Later.**
- An AUMID and a jump list, for taskbar parity.
- A resident fm (open decision Q1): it makes the warm path normal and removes the cold-start stall.

Size: the parser is about 150–250 lines and the COM registration about 300–500, so both go to a Hydrogen worker under the user's rules.

**H. Script changes to set-default-fm.ps1 and restore-explorer.ps1.**
- The backup is currently all-or-nothing. It is written only when `$BackupRoot` is absent [V, line 33], so keys added in a later version never get their prior values saved. Change to a per-value backup that records each value once, the first time the script touches it.
- Restore deletes exactly what the script created, including the HKCU default on each `CLSID\{…}\shell`.
- Refuse to overwrite another app's existing `Folder\shell\explore` or CLSID override, the same way install-shim.ps1 already does.
- Put the emergency `reg delete` lines in the script header.
- uninstall-fm already restores before deleting; keep that.

## 3. What cannot be taken over per user

- **`explorer.exe` launched directly** (row 14). It ignores folder verbs [F]. explorer.exe is found in %WINDIR% before App Paths are consulted, and CreateProcess ignores App Paths entirely [M]. The admin-only options, both rejected:
  - an IFEO `Debugger` on explorer.exe (HKLM). It also intercepts the shell at logon and can leave the user with no desktop.
  - `ShellExecuteHooks` (HKLM plus the EnableShellExecuteHooks policy). Directory Opus uses it. Deprecated since Vista, it catches ShellExecute calls only, and it loads an in-process DLL into every caller.
  - One per-user option is technically possible but not recommended: hook CreateProcessW inside other processes through dialogkit's existing `WH_GETMESSAGE` injection. It injects into every GUI process, has bitness issues, can crash third-party apps, and antivirus may flag it.
  - The acceptable per-user answer is the watcher.
- **An Explorer window already open on the folder wins** SHOpenFolderAndSelectItems. This is inherent; only the watcher, or having no Explorer windows open, avoids it.
- **Control Panel and applets** could be taken per user, but only through the broad Folder override, which breaks deep links (Files #15344). This is a decision; I recommend not doing it.
- **Explorer's taskbar jump list**: there is no hook.
- **Pinning fm to the taskbar from code**: no supported API exists for an unpackaged app [M/U]. The user pins it by hand.
- **Elevated callers** (for example an admin cmd running `start .`): whether ShellExecute in an elevated process honours HKCU verbs is [U]. COM ignores some per-user registrations in elevated processes. If it does work, fm would then run elevated.

## 4. Test plan that never touches the real HKCU

**Tier A: unit tests, no registry.** Run `parse_target` over a table of inputs:
- raw `"C:\"`, `"D:\"`, `"C:\Program Files\" extra`
- a file path
- `/select,C:\Windows\notepad.exe` and `/select` followed by a separate token
- `/e,/root,…`
- `::{645FF040-…}`, `::{20D04FE0-…}`
- `shell:downloads`, `shell:RecycleBinFolder`, `shell:ControlPanelFolder` (expect Passthrough)
- `file:///C:/Users/x%20y`
- `\\localhost`, `\\localhost\C$`
- `%USERPROFILE%`
- garbage (expect Error, not the profile folder)

**Tier B: script logic against fake roots.** Use the existing `scripts\dev\test-default.ps1` with `HKCU\Software\fm-test\{Classes,Backup}`, guarded by `RealState`. Extend it to cover:
- every new key: Directory/Drive `shell\open`, `Folder\shell\explore`, CLSIDs `{20D04FE0}`, `{645FF040}`, `{F02C1A0D}`, `{52205fd8}\shell\open`, optional NetServer
- per-value backup
- restore removing the `{20D04FE0}\shell` default
- foreign pre-existing overrides kept
- an idempotent second run
- **`RealState` guarding every real key** the script now touches

The user must run it from their own PowerShell, because HKCU writes from this session may be virtualized. Fake roots prove only the script's logic; the shell never reads them.

**Tier C: runtime COM, no registry at all.**
1. Start `fm --new-instance C:\Windows`.
2. Check that `(New-Object -ComObject Shell.Application).Windows()` lists fm with the right LocationURL.
3. A PowerShell Add-Type harness calls SHParseDisplayName and SHOpenFolderAndSelectItems on `C:\Windows\notepad.exe`. Expect fm to select it and come to the front.
4. Repeat with two items, with OFASI_EDIT, with a second fm window or tab, after a close (the entry must be gone), and through the TerminateProcess exit path (no ghost entry).
5. Poke the entry through Shell.Application: Document, Navigate, Refresh, Quit. Nothing may crash.
6. Run `explorer.exe /select,C:\Windows\notepad.exe` and record whether fm or a new Explorer window gets it. This answers the open question.

At worst these steps open Explorer windows, which is harmless.

**Tier D: real verb resolution.** This needs a real Classes hive in a disposable profile. Options:
1. A second local standard user account. Creating it needs admin once; its HKCU is separate and it runs the real shell.
2. Windows Sandbox. It is **not installed** here (`WindowsSandbox.exe` absent [V]); enabling it needs admin and a reboot, and it may be flaky on Insider 26300.
3. A Hyper-V VM (admin).

In that profile, run install-fm.ps1 and the new set-default-fm.ps1, then check each case against its expected result:

| Case | Expected |
|---|---|
| Desktop folder; drive in an Explorer window (check it opens the root, not System32) | fm |
| Win+E; Start "File Explorer"; taskbar pin | fm |
| This PC, Recycle Bin, Network desktop icons | fm |
| Win+R: `C:\Users`, `\\localhost`, `shell:downloads`, `shell:RecycleBinFolder` | fm |
| `shell:ControlPanelFolder`, `control.exe`, `control /name Microsoft.BitLockerDriveEncryption`, Settings → related applet links | Explorer |
| Chrome, Edge and Firefox "Show in folder", warm and cold | fm with the file selected, no stall |
| Chrome folder-open (OpenFolderViaShell) | fm |
| VS Code "Reveal in File Explorer"; Task Manager, desktop .lnk, Start and Search "Open file location" | record results |
| `start .`, `ii .` | fm |
| `explorer .` | Explorer (escape hatch) |
| Open/Save dialog: double-click a folder | must still navigate inside the dialog |
| A .lnk pointing to a folder | fm (Files #18818 regression) |
| Context menus | no duplicate "Otevřít" and no visible "explore" |
| restore-explorer.ps1 | everything back to Explorer |
| uninstall-fm.ps1 while the redirect is active | everything back to Explorer |
| fm.exe deleted without uninstalling | measure the breakage, then confirm the emergency `reg delete` lines recover |
| An elevated cmd running `start .` | record |

An optional static probe [U]: resolve verbs with AssocCreateForClasses, with `hkClass` keys opened on the fake root, ordered as fake Directory → HKLM Directory → fake Folder → HKLM Folder. It only approximates the HKCR merge, so it cannot replace tier D.

## 5. Shell replacement option (desktop plus taskbar)

The shell-replacement agent worked from memory only, so this whole section is [M] unless marked.

- **Mechanism**, per user and without admin:
  - `HKCU\Software\Microsoft\Windows NT\CurrentVersion\Winlogon\Shell` (REG_SZ) = fm.exe, or better a guard stub. It takes effect at the next logon.
  - There is also a policy value, `HKCU\Software\Microsoft\Windows\CurrentVersion\Policies\System\Shell`. The rescue procedure must clear both places.
  - The official Shell Launcher requires Enterprise or Education; this machine runs Pro.
- **It fixes none of the leaks.** Show-in-folder, `explorer.exe /select`, shell: URIs and verbs all work independently of which program is the shell.
  - That agent also claimed SHOpenFolderAndSelectItems starts explorer.exe as a process. That contradicts the fetched sources, which show it looks up IShellWindows and then launches the folder's association.
  - ShellWindows has its own fallback host, rundll32 shell32 SHCreateLocalServerRunDll [V], so option 4 above would probably keep working under a replaced shell [M].
- **What would be lost and have to be rebuilt:**
  - Run, RunOnce and Startup-folder processing, so OneDrive and tray apps would not start
  - Win11 Start and Search
  - Win+X, Win+R, Win+A and Win+N
  - the system tray clock, volume, network and battery (private XAML, not Shell_NotifyIcon)
  - Action Center, Quick Settings and the notification center
  - virtual desktops (private COM whose GUIDs change every build)
  - Alt+Tab/Task View, Snap layouts, the wallpaper and desktop icons
- **What keeps working:** the lock screen, Settings (ms-settings:), UWP activation through AUMID, and drag-to-snap and Win+arrow keys.
- **Effort** (an estimate, not a measurement): about 2–3 months for a usable core; 9–18 months for something close to Win11 parity, plus maintenance after every major build.
- **Risk:** a black screen at logon. Rescue: Ctrl+Shift+Esc → Run new task → explorer.exe; a guard stub that deletes the Shell value after repeated crashes; only ever tried on a second account.
- **Recommendation:** do not do it. Keep explorer.exe as the desktop and Start host. If it is ever wanted, the Seelen UI style "hide Explorer's taskbar, draw our own" is a later opt-in experiment, never the default.

## 6. Where the research agents disagreed, and which wins

- **Chromium's fallback verb.** Both agents were right about different functions [V]: `ShowItemInFolder` falls back to ShellExecute `"open"` (row 2), while `OpenFolderViaShell` uses `explore` plus class `folder` (row 3).
- **How selection reaches fm.** The fm-code agent said only through argv. The source reading wins: selection arrives as a COM `IShellView::SelectItem` call. `/select` on argv is needed only for the watcher hand-off and for parity.
- **Desktop CLSID icons.** The fm-code agent said they run "explorer.exe /root,::{CLSID}". That was speculation; the virtual-folders agent measured that they resolve to Folder\shell\open DelegateExecute, and that probe wins.
- **SHOpenFolderAndSelectItems starting explorer.exe.** The shell-replacement agent's claim is contradicted (see §5).
- **The default verb versus `open` on the cold path** is still unknown. Rows 1 and 2 together cover both for file-system folders, so the answer does not change the plan.
- **Files elevates** (regedit with `runas`) even though it writes only HKCU keys. Do not copy that.

## 7. Decisions for the user

None of these is cut silently.

1. Should Control Panel and applets stay in Explorer (recommended), or should fm take them with the Files-style broad `Folder\shell\open`, at the cost of broken deep links?
2. Should the opt-in Explorer-window watcher (in dialogkit) catch `explorer.exe /select` from apps such as uTorrent and IDM? Explorer flashes briefly.
3. Should fm stay resident (open decision Q1)? It makes "Show in folder" instant and avoids the cold-start path.
4. Which test environment: a second Windows account (recommended) or enabling Windows Sandbox? Either needs admin once.
5. Shell replacement: I recommend not doing it.

## 8. Order of work

1. fm parser, drive-root fix, passthrough and escape hatch (tier A).
2. IShellWindows registration in fm (tier C).
3. set-default-fm.ps1 v2, restore-explorer.ps1 and test-default.ps1 (tier B).
4. Tier D in the second account.
5. The user runs the scripts on the real account.
6. Optionally, the watcher in dialogkit.

## Still unverified

- Which verbs Start, the taskbar and Search invoke.
- Whether a cold SHOpenFolderAndSelectItems uses the default verb or `open`.
- Whether the hub's registration satisfies the pending wait after a hand-over.
- Whether `explorer /select` consults fm's ShellWindows entry.
- NetServer and library-ms ordering in the association array.
- Context-menu duplicates.
- Elevated callers.
- The whole of §5.

## Sources

- Chromium platform_util_win.cc: https://raw.githubusercontent.com/chromium/chromium/main/chrome/browser/platform_util_win.cc
- Chromium ui/base/win/shell.cc: https://raw.githubusercontent.com/chromium/chromium/main/ui/base/win/shell.cc
- Files SetFilesAsDefault.reg: https://github.com/files-community/Files/blob/main/src/Files.App/Assets/FilesOpenDialog/SetFilesAsDefault.reg
- Files PR #18784: https://github.com/files-community/Files/pull/18784
- Files launcher: https://github.com/files-community/Files/blob/main/src/Files.App.Launcher/FilesLauncher.cpp
- Files OpenInFolder.cpp: https://github.com/files-community/Files/blob/main/src/Files.App.Launcher/OpenInFolder.cpp
- Files issue #15344: https://github.com/files-community/Files/issues/15344
- Explorer++: https://github.com/derceg/explorerplusplus
- FileExplorerInterceptor: https://github.com/abdonkov/FileExplorerInterceptor
- Directory Opus Explorer Replacement: https://www.gpsoft.com.au/help/opus12/Documents/Explorer_Replacement1.htm
- DosTips, `explorer /select` opens a new window: https://www.dostips.com/forum/viewtopic.php?p=52844
- voidtools, reusing Explorer windows: https://www.voidtools.com/forum/viewtopic.php?p=66104
- IShellWindows documentation: https://learn.microsoft.com/windows/win32/api/exdisp/nn-exdisp-ishellwindows

Not checked: I did not run test-default.ps1 (it writes to the registry), and I did not live-test any HKCU key or COM path.
Main risk: once the Folder or CLSID keys are in place, a missing or broken fm.exe makes those targets unopenable, so the restore path and the emergency `reg delete` lines must ship first.