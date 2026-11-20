# Architecture v1

Working name: `fm`. Written 2026-11-19 for Windows 11 x64, build 26300, Rust 1.100, edition 2024. Revised the same day after the review (Review log at the end): the dialog helper now follows the committed spikes `da5354d` (thread hook) and `e5970e8` (New menu).

## 1. Goals and non-goals

Priorities, in order:
1. **Option 1 (must work).** Add "Nová složka" / "Nový soubor" to every system Open/Save dialog, creating the item in the dialog's current folder.
2. **Instant filename search**, without Windows Search.
3. **Fast browsing in the style of Finder.** The design gets reworked later, so the UI layer stays swappable.
4. **Option 2 (experiment).** Replace the system dialog: opt-in, allowlisted, removable by script.

Coverage of "every dialog" in v1:

| Dialog | v1 |
|---|---|
| Common Item Dialog (IFileDialog): Notepad, Chrome/Edge/Electron, Firefox, most 32- and 64-bit apps | **Yes, target 100%** |
| Explorer-style legacy `GetOpenFileName` | **Yes** (same hook; spike `da5354d`) |
| Hook/template legacy dialogs, packaged-app pickers | Measured in the M1 matrix |
| Elevated apps | **No** (UIPI). Later via an explicit elevated instance |
| Private pickers (Office Backstage, Qt/GTK) | **No**. They are not system dialogs |
| Virtual locations (This PC, Home, Search) | Our button is disabled with a hint. The dialog's own "Nová složka" still works |

## 2. Decisions

**UI: one Win32 window drawn with Direct2D/DirectWrite on a DirectComposition swap chain** (revised 2026-11-20). The user made the Finder look a hard requirement (unified toolbar, translucent sidebar, inset rounded selection, icon/list/column/gallery views); a stock ListView cannot get there, and the earlier plan already named Direct2D as the reskin target.
- Native where Windows does it better: rename is a child `EDIT` (IME), context menus are the shell's `IContextMenu`, copy/paste/Properties are shell verbs, conflicts use the shell's dialogs, drag and drop is OLE with the folder's own `IDropTarget`.
- Mica (`DWMSBT_MAINWINDOW`) shows through transparent pixels of the composition swap chain (`WS_EX_NOREDIRECTIONBITMAP`). Nothing in the layout depends on it: the opaque fallback colour is a token.
- Paint code takes an `ID2D1RenderTarget`, so `fm.exe --render` draws the same frame into a WIC bitmap and writes a PNG. That is the visual test harness; no screen capture is used.
- Pure state (selection, layout rects, hit-testing, sorting, history, tabs, type-to-select) lives in `app-core` with unit tests; `fm.exe --act` drives the real window for E2E checks.
- Accessibility: full keyboard operation and high-contrast tokens; a UI Automation provider for the custom views is a known gap until built.
- Pivot rule: if the custom list is not navigable in time, a virtual ListView goes behind the same model.
- Rejected:
  - Tauri 2: WebView2 memory, weak native drag and context menu.
  - egui 0.37.0, iced 0.14.0.
  - gpui 0.2.2: pre-1.0.
- Other crates: `rayon` 1.12.0, `memchr` 2.8.3, `unicode-normalization` 0.1.26.
- Crates using `#[implement]` (`dialog-shim`, `app` for drag and drop) also need a direct `windows-core = "0.62.2"`, because the macro expands to `::windows_core::…`. **Never 0.100.0**: windows-core and windows-implement 0.100.0 are a separate 2026-09 line, and `windows` 0.62.2 depends on core 0.62.2. This corrects the research report.
- Sources: https://crates.io/api/v1/crates/windows/0.62.2/dependencies, https://crates.io/api/v1/crates/{rayon,memchr,unicode-normalization,slint}

**File operations: shell verbs, `IFileOperation` and `MoveFileExW`.**
- Copy, cut, paste and Properties: the canonical verbs `copy`, `cut`, `paste`, `properties` invoked on the shell's own `IContextMenu` for the selection. Paste uses the folder's background menu (`IShellFolder::CreateViewObject`). The Explorer-compatible clipboard and the native conflict dialog come with it; there is no own `CF_HDROP` code.
- Delete: `IFileOperation` with `FOFX_RECYCLEONDELETE|FOF_ALLOWUNDO` (Recycle Bin) and `FOF_WANTNUKEWARNING` for permanent deletion, plus the shell's UAC elevation. Explicit flags keep rule 1 testable and independent of the user's shell settings.
- Rename: `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING`, which fails with `ERROR_ALREADY_EXISTS` and never shows a prompt, then `SHChangeNotify`. A case-only rename (same file ID) is allowed.
- New: Ctrl+Shift+N creates a folder (`fm-fs`, then in-place rename). Other types come from the New submenu of the shell's context menu.
- Never `FOF_NOCONFIRMATION`. No `trash` crate.

**Search v1: a non-admin walk, a watcher and a snapshot.**
- **Walk.** Parallel `GetFileInformationByHandleEx(FileIdBothDirectoryInfo)` on each directory handle (`\\?\`, 64 KB buffer) over fixed NTFS volumes. One batch gives names, FileId, size, times and attributes without opening any file.
- **Index layout:**
  - Structure-of-arrays: a folded name arena and a display name arena, plus `parent`, `attrs`, `size`, `mtime`, the 64-bit `file_id` and an extension id.
  - Paths are assembled lazily.
- **Matching:**
  - `fold()` = lowercase + strip diacritics (NFD).
  - Parallel `memchr::memmem` scan.
  - A refining keystroke filters the previous result.
  - A generation counter cancels stale queries.
- **Syntax:** AND, `"phrases"`, `ext:`, `path:`, `folder:`, `file:`. Hidden and system files are a query-time toggle.
- **Ranking:** exact, then prefix, then word boundary, then substring. Ties: shorter name, then newer.
- **Snapshot:**
  - `%LOCALAPPDATA%\fm\index-<serial>.bin`. The header holds magic, version, serial, total length and a checksum. Every offset and parent index is validated on load.
  - Written to a temp file, `FlushFileBuffers`, then `MoveFileExW(REPLACE_EXISTING|WRITE_THROUGH)`. Written on exit, on `WM_ENDSESSION`, and hourly while dirty (every 5 min would be ~43 GB/day at 150 MB).
  - A corrupt snapshot means a rebuild, never a panic.
- **Changes:**
  - One `ReadDirectoryChangesExW` (`ReadDirectoryNotifyExtendedInformation`) per volume root, coalesced over 50–100 ms. Each record carries FileId, ParentFileId, size, times and attributes, so size and mtime stay exact.
  - Removes and renames match by FileId inside the parent found via ParentFileId, because an event may carry the 8.3 short name (which name is unspecified). An add whose name contains `~` goes through `GetLongPathNameW`.
  - Overflow (zero-byte completion or `ERROR_NOTIFY_ENUM_DIR`), app start and a daily timer all run the same thing: a background re-walk at low I/O priority, merged into the live index, one at a time per volume (a request during a running re-walk only marks it to run once more). There is no separate dir-mtime reconcile, because a directory's mtime does not change when a child's contents change.
- **Upgrade behind the same API:** v2, an elevated USN service installed by script, with a named pipe for the user only.

**Dialog helper: a separate process, plus a hook DLL loaded only for one request.** Proved on build 26300 by spikes `da5354d` and `e5970e8` (`spikes/dialog-probe/`): exact folder in ~2 ms in item and legacy dialogs, Chrome and Edge upload dialogs and 32-bit hosts.
- **Parts:**
  - `fm-dialogkit.exe` (64-bit): detection, the button, the New menu.
  - `fm_dialog_hook.dll`: loaded into the dialog's UI thread by a `WH_GETMESSAGE` *thread* hook for one request, then unhooked.
  - 32-bit hosts: `x86\fm-dialogkit.exe --inject` and the i686 DLL, because a hook DLL must match the host's bitness.
- **Owned risk:** a bug in the DLL can crash the host app. So the DLL only answers "query folder" and "select and rename", follows §4 rule 7, retries from `SetTimer` instead of blocking, and pins itself (`GET_MODULE_HANDLE_EX_FLAG_PIN`) so a pending timer never calls into an unloaded DLL. Consequence: once used, it stays mapped until the host exits (§4 rule 4).
- **Detection:**
  - Out-of-context `SetWinEventHook` for SHOW, DESTROY and FOREGROUND, global.
  - LOCATIONCHANGE is registered only for the tracked dialog's process and thread, filtered to `hwnd == dialog` and `OBJID_WINDOW`. A global one would wake on every caret and cursor move in any app.
  - Watch `#32770` windows. About 150 ms after one appears (3 retries), classify its children: `SHELLDLL_DefView`, plus `DUIViewWndClassName` for an item dialog, otherwise legacy.
- **Current folder**, read at the moment of the action:
  1. The hook DLL sends `CWM_GETISHELLBROWSER` (WM_USER+7, undocumented) to the dialog, then `IShellBrowser::QueryActiveShellView` → `IFolderView::GetFolder` → `SIGDN_FILESYSPATH`.
  2. Only when the hook cannot run (DLL blocked, x86 helper missing): the address bar text, used only if it is an absolute path to an existing directory.
  - Otherwise the action is refused. There is no breadcrumb or known-folder resolver.
- **Creating items:**
  - The button opens **Windows' own New menu**: `CLSID_NewMenu` hosted in dialogkit and initialized with the folder. It has Explorer's entries, templates, localized names and unique naming ("Nová složka", "Nový textový dokument.txt").
  - Then the hook DLL selects the new item in the dialog's view in rename mode (`IShellView::SelectItem(SVSI_EDIT)`), retrying from a timer until the view has it (≤2 s).
  - Fallback if the New menu is empty or fails: `fm-fs` `create_new_dir` / `create_new_file` (`CREATE_NEW`, " (2)", " (3)"…), `SHChangeNotify`, then the same select and rename.
  - Refused with an inline message, creating nothing, when the new path would reach 260 characters: the host app may not be long-path aware.
  - No input is ever synthesized: `SendInput` goes to whatever window is in the foreground.
  - The helper never presses Open or Save. In a Save dialog the system asks about overwriting, which is expected.
- **Never block on a host:**
  - The event thread waits at most 500 ms for a query reply. Select and rename is asynchronous: the reply is a `WM_COPYDATA` handled in the message loop, with a 2.5 s deadline. The hook is removed on reply or deadline.
  - Any cross-process `SendMessage` is `SendMessageTimeoutW(SMTO_ABORTIFHUNG)` with ≤500 ms.
  - UIA (button placement) runs on an MTA worker with connection and transaction timeouts of 1 s; the default transaction timeout is 20 s.
- **Attaching to the dialog:**
  - One **Nový** button (pending Q2 in §9): an unowned `WS_EX_TOPMOST|NOACTIVATE|TOOLWINDOW` popup, Per-Monitor-V2 DPI. It sits after the dialog's left command group (item dialog) or under the Places bar (legacy). It is visible only while the dialog or our menu is in the foreground, and follows LOCATIONCHANGE.
  - **No cross-process owner**: that would attach input queues, so a hang in one process would freeze the other ([Raymond Chen](https://devblogs.microsoft.com/oldnewthing/20130412-00/?p=4683)).
  - Ctrl+N opens the menu, registered only while a dialog is in the foreground. Never a Ctrl+Alt chord: Ctrl+Alt+N is AltGr+N, which types `}` on the Czech layout. The dialog's native Ctrl+Shift+N still creates folders.
- **Higher-integrity targets:** skipped, logged once.

**Option 2: HKCU COM override plus a forwarding shim** (built in M10, 2026-11-20; `crates/dialog-shim`, `crates/app/src/pick.rs`).
- **Registration.** `fm_dialog_shim.dll` under `HKCU\Software\Classes\CLSID\{DC1C5A9C-E88A-4dde-A5A1-60F82A20AEF7}`, 64-bit view only, by script. This is [Files'](https://github.com/files-community/Files/tree/main/src/Files.App.OpenDialog) technique; see their failures in [#15275](https://github.com/files-community/Files/issues/15275) and [#6314](https://github.com/files-community/Files/issues/6314).
- **Default path.** `DllGetClassObject` returns **comdlg32's own factory** (path taken from HKLM `InprocServer32`) outside the allowlist, and also for elevated processes, AppContainer processes, `explorer.exe`, or when the kill switch is set. The allowlist is `Claude.exe` (the M9 gate saw its dialog come through the override) plus `shim-allow.txt`.
- **Allowlisted processes** get our class factory. Its dialog is a hand-written COM object (`wrap.rs`, not `#[implement]`: an unknown `QueryInterface` must hand back the real dialog's interface) that forwards every call to comdlg32's real dialog and records what the picker needs: title, OK label, `SetFolder`, `SetDefaultFolder`, `SetFileName`, `SetFileTypes`/`SetFileTypeIndex`. Every method is inside `catch_unwind`.
- **Delegate** (the real `Show()`, logged with the reason) when the host called `Advise` or `SetFilter`, successfully asked for any other interface (`IFileDialogCustomize`, `IFileDialog2`, `IObjectWithSite`, …; COM's own marshaling and agility probes are answered "no" and change nothing), or the options (read from the real dialog) include `FOS_PICKFOLDERS` or `FOS_ALLNONSTORAGEITEMS`. Multiselect is supported.
- **Transport: a named pipe**, `\\.\pipe\fm-pick-<pid>-<random>`, created by the shim (message mode, one instance, local clients only, the client's process id checked). Not a file: an MSIX host such as Claude has its `%LOCALAPPDATA%` writes redirected into its package, and a process it starts inherits that view.
  1. **The resident fm** (preferred: it runs outside the package with the real prefs, tags and index). The shim sends its door window a `WM_COPYDATA` (`COPY_PICK`, a versioned request: pipe, owner, title, OK label, folders, file name, file types and index, multiselect, hidden files). Only the return value `ACK` counts; an older fm answers 0.
  2. Otherwise **`fm.exe --pick <pipe>`**: the exe from the Run value "Soubory" or App Paths, started only if it carries the `fm-pick=1` RCDATA marker (an older fm would read `--pick` as a folder to open). It never joins or becomes the resident fm, keeps no index (search filters the folder's listing) and saves nothing.
  3. Otherwise, or on any failure before the picker answers, the real `Show()`.
- **Protocol** (`proto.rs`, shared by both sides): fm connects and reads the request from the pipe (both ways in), sends `Ready` once its window is up, which ends the shim's 10 s connect clock, then exactly one `Picked(paths)` or `Cancelled`. A pipe that breaks without an answer means the picker is gone (killed, crashed): the real `Show()`. Cancel returns `HRESULT_FROM_WIN32(ERROR_CANCELLED)`.
- **Waiting** on the dialog's thread: `MsgWaitForMultipleObjectsEx` on the pipe and the started process, pumping messages (a `WM_QUIT` is posted again and counts as cancel). The owner is disabled meanwhile if it was enabled, enabled again on every path, and given the foreground back (fm allows that before it closes).
- **After `Show()`**, `GetResult`, `GetResults` (every picked item, `SHCreateShellItemArrayFromIDLists`) and `GetFolder` answer from our result; the shim checks first that each path is an existing file. `GetFileName` still answers what `SetFileName` set (Electron never asks).
- **The picker** is a normal top-level fm window (no cross-process owner, see dialog helper), centred over the owner and in the foreground, with the same sidebar, views and search, starting in `SetFolder`, else the last picker's folder, else `SetDefaultFolder`, else Documents. A bottom bar holds the file type (a native menu; non-matching files are hidden, folders and shortcuts always shown), "Zrušit" and the dialog's OK label ("Otevřít" by default). Double-click and Enter pick files (a shortcut gives its target) and open folders; a single-select dialog keeps one item selected; Esc and closing the window cancel. It saves no prefs, stays out of the shell's window list and closes by itself once a second check finds the host gone.
- **Log:** one `shim.log` line per `Show()`: options, decision with its reason (`picker (resident|child)`, `delegate: …`, `fallback: …`) and outcome (`picked N`, `cancelled`, the system dialog's result). Never a path or a title.

**Explorer replacement (2026-11-20, the user's answers):** folder windows only; the desktop, taskbar and Start stay Windows'. Control Panel and other places fm does not show stay Explorer's.
- Per-user verbs (`fm --set-default` / `fm --restore-explorer` in `defaultfm.rs`, the installer's way; `scripts/set-default-fm.ps1` / `restore-explorer.ps1` write the same keys and backup, so either undoes the other; HKCU only, per-entry backup): Directory and Drive `open`, Folder `explore`, Win+E (`{52205fd8…}`), This PC, Recycle Bin and Network. Every command key carries an empty `DelegateExecute`, or HKLM's leaks through the merged view.
- `target.rs` reads Explorer's command line from the raw `GetCommandLineW` tail (std's argv rules turn the shell's `"C:\"` into `C:"`). What fm cannot show goes to `explorer.exe` with the event `Local\fm.explorer-allow` signalled for 8 s, so nothing loops.
- Each window registers in the shell's window list (`shellwin.rs`, IShellWindows), which is where "Show in folder" looks first. Calls arrive from other processes, also inside modal loops, so they only post to the window.
- `explorer.exe` started directly by an app cannot be redirected per user: the fm-dialogkit watcher closes such a window once fm has been started for the same place and fm's UI thread answers (only while `HKCU\Software\fm\Redirect` exists).
- Resident like Explorer (old Q1: yes): instant opens and a warm index for ~450 MB. A hidden top-level window hears the session end and writes the index; "Ukončit Soubory" ends the process.
- Plan and research: `docs/explorer-replacement-plan.md`.

**Process model:**

| Process | Runs as | Lifetime | IPC |
|---|---|---|---|
| `fm.exe` | user | Single instance (`Local\fm.main`); a second launch hands over its command line and folder via `WM_COPYDATA`. Owns the index. Resident: `--background` at sign-in (HKCU Run), stays when the last window closes. Opens pickers for the shim | door window: `WM_COPYDATA` (handover, `--quit`, pick requests); named pipe per picker |
| `fm.exe --pick <pipe>` | user, inside the host's package view | One per dialog call when no resident fm takes it; never resident, no index, saves nothing | named pipe |
| `fm-dialogkit.exe` | user | HKCU `Run`, set by the install script | none |
| `x86\fm-dialogkit.exe --inject` | user | One per request to a 32-bit dialog | stdout |
| `fm_dialog_hook.dll` (x64, x86) | dialog host's UI thread | Loaded per request, pinned until the host exits | registered message → `WM_COPYDATA` |
| `fm_dialog_shim.dll` | every 64-bit host's Open dialog; the picker only for allowlisted ones (Claude.exe) | One dialog object per dialog | `WM_COPYDATA` to fm's door, or starts `fm.exe --pick`; named pipe |
| `fm-indexd` (v2) | service | Elevated script | named pipe |

## 3. Cargo workspace

```
Cargo.toml            [workspace] members = ["crates/*"]; versions in [workspace.dependencies]
.cargo/config.toml    target-dir outside OneDrive  (exists)
crates/
  fm-fs/           lib    fold, name validation, unique_name, create_new_file/dir, rename, placeholder flags
  fm-index/        lib    arena, query, ranking, snapshot (pure); win::{walker, watcher}
  dialog-hook/     cdylib+rlib  fm_dialog_hook.dll (x64 and i686): query folder, select and rename
  dialogkit/       bin    fm-dialogkit.exe: detect, inject, New menu, button, tray, --act, --inject; pure modules unit-tested in place
  app-core/        lib    history, listing, sorting, type-to-select (no UI)
  app/             bin    fm.exe: Win32 views, shell integration, search window, --pick
  dialog-shim/     cdylib depends only on `windows` (runs inside other processes)
scripts/  install-/uninstall-dialogkit.ps1, install-/uninstall-dialog-shim.ps1 (each takes -InstallDir and -RegRoot),
          dev/test-dialogkit.ps1 (fixture and matrix: WinForms dialogs in own processes, own temp folder)
docs/     ARCHITECTURE.md, search-bench.md, dialog-dumps/
```

Crate dependencies: `dialogkit` → `dialog-hook` (protocol constants), `fm-fs`; `fm-index` → `fm-fs`; `app-core` → `fm-fs`; `app` → `app-core`, `fm-index`, `fm-fs`. Library tests run without a GUI. Tests that need a desktop are marked `#[ignore]`.

## 4. Safety rules

1. **No silent data loss.**
   - Delete goes to the Recycle Bin. Permanent deletion (Shift+Delete, or a file too large for the bin) always asks first.
   - New items use `CREATE_NEW`, a unique name, or the shell's New menu.
   - Copy and move use the native conflict dialog.
   - Renaming onto an existing name fails (`MoveFileExW` without replace). A case-only rename is allowed.
2. **Never hydrate OneDrive placeholders.**
   - Nothing reads the content of an item with `OFFLINE`, `RECALL_ON_OPEN` or `RECALL_ON_DATA_ACCESS`.
   - Icons come from `SHGFI_USEFILEATTRIBUTES`.
   - The walker only opens directories (checked by a mock test).
   - Hydration happens only on an explicit user action.
3. **No silent system changes.**
   - The registry and autostart are touched only by `scripts/*.ps1` run by the user, in HKCU only.
   - Automated tests run the scripts only against a sandbox: `-RegRoot HKCU\Software\fm-test` and an `-InstallDir` in a temp dir. A guard test fails if the real `Run` value or CLSID key differs before and after the test run.
   - Uninstall removes exactly what install added.
   - The app never runs elevated. Elevation comes from the shell (IFileOperation).
4. **Install paths.**
   - One directory per component: `%LOCALAPPDATA%\Programs\fm\dialogkit\` (with `x86\`), `…\fm\shim\`, `…\fm\app\`. Never the target dir or OneDrive. A dangling override would break dialogs in every 64-bit process.
   - Each uninstaller deletes only its own directory. The dialogkit uninstaller stops `fm-dialogkit.exe` first. The shim uninstaller deletes the CLSID key and checks that it is gone before touching files.
   - Each installer grants read and execute to ALL APPLICATION PACKAGES (`*S-1-15-2-1`) on its directory, because `%LOCALAPPDATA%` has no such entry and an AppContainer host would otherwise fail to load the DLL.
   - A loaded DLL (the pinned hook DLL, the shim in any dialog host) cannot be deleted. Uninstall deletes the keys first, renames such files to `*.del` and registers an HKCU `RunOnce` cleanup, so removal finishes at the next sign-in. Install and upgrade rename a locked old DLL aside instead of overwriting it.
   - Recovery: `reg delete "HKCU\Software\Classes\CLSID\{DC1C5A9C-E88A-4dde-A5A1-60F82A20AEF7}" /f`
5. **Shim kill switch.** The file `%LOCALAPPDATA%\fm\disable-shim`, checked on every call.
6. **Data location.** Index and logs live only in `%LOCALAPPDATA%\fm\`.
7. **Code inside other processes** (hook DLL, shim): every export is wrapped in `catch_unwind`, it never blocks the host's thread, and it keeps no state beyond one request (the hook) or one dialog (the shim).

## 5. Performance budgets

Gated (a failure blocks the milestone):

| Area | Budget | Measured by |
|---|---|---|
| Dialog shown → button visible | ≤300 ms | dialogkit log |
| Folder query | ≤200 ms including the x86 helper (spike: ~2 ms in-process) | M1 matrix test |
| dialogkit idle | ~0% CPU, <15 MB, also while typing and moving the mouse in another app | `Get-Process` after 10 min (manual) |
| Query, 2M entries, 1–3 terms | p99 <50 ms | `cargo test -p fm-index --release -- --ignored perf` |
| Snapshot load (2M entries) | ≤1 s | same |
| First full scan of C: | ≤90 s, UI usable meanwhile | M4a → `docs/search-bench.md` |
| fm.exe start → folder shown | ≤300 ms | `fm.exe <folder> --new-instance --dev-startup <out>` (2026-11-20, release, 290-item folder: median ~220 ms, 160–355 ms; after `876f499`, 4-item folder: 98–109 ms in 4 of 6 runs, 438 and 517 ms in 2 — the phase trace puts all of the excess in creating the D3D11 device on the Intel iGPU, 330–420 ms instead of ~5 ms) |
| Folder with 100k items | first rows ≤200 ms, all ≤1.5 s | lister test + manual scroll |

Reported only, in `docs/search-bench.md`: query p50 (target <15 ms), refining keystroke (<5 ms), index memory (≤100 B/entry), live update (≤1 ms/event), snapshot size (≤150 MB), overflows per day, fm.exe memory without index (≤60 MB).

## 6. Milestones

Each milestone uses the `.ai/work/TASK.md` format.
- **[A]** = checked automatically (`cargo test` or a script).
- **[M]** = manual check by the lead from screenshots, or a step the user runs.

Tracks: dialog (M1–M2), search (M3–M5), browser (M6–M8), Option 2 (M9–M10). M3 and M4a can run in parallel with M1.

Preconditions:
- `cargo` is in `%USERPROFILE%\.cargo\bin`, which is **not on PATH**. Add it for the session only.
- The 32-bit hook needs `rustup target add i686-pc-windows-msvc`.
- `#[ignore]` dialog tests need an interactive desktop.

**M1: dialog helper core (Option 1 mechanism)**
- Built on spikes `da5354d` and `e5970e8`; `spikes/dialog-probe/` is the reference.
- Status: most of M1 and M2 landed in `5337897` before this review. That commit still has a global LOCATIONCHANGE hook, 3–4 s blocking waits, no `hung` mode, no 260-character refusal and no `matrix.md`. The next run closes those gaps against the acceptance below.
- Scope:
  - `fm-fs`: `unique_name`, `create_new_file`/`create_new_dir`, name validation.
  - `dialog-hook` for x64 and i686: query folder, select and rename (§2, §4 rule 7).
  - `dialogkit`: detection and classify, inject (with `x86\fm-dialogkit.exe --inject` for 32-bit hosts), the address-bar fallback, the New menu with the `fm-fs` fallback, select and rename, the 260-character refusal, the timeouts from §2.
  - `--act {query|anchor|list|new <index>} --hwnd H` and `--dump` (as in `5337897`). The query reply names the rung (hook or address bar) and the time taken.
  - The matrix and fixture script `scripts/dev/test-dialogkit.ps1`: WinForms dialogs in their own processes, modes open, save, legacy and hung, each x64 or x86. `hung` blocks the dialog's UI thread for 10 s after it shows (a WinForms timer that sleeps).
- Acceptance:
  - [A] `cargo test -p fm-fs -p fm-dialogkit`:
    - `unique_name` gives " (2)" and " (3)", and `CREATE_NEW` leaves an existing file's bytes unchanged;
    - validation rejects CON, NUL, `<>:"/\|?*`, and a trailing dot or space;
    - the address-bar fallback accepts only an absolute path to an existing directory, and rejects display names such as `Dokumenty` and `Tento počítač`;
    - the 260-character check.
  - [A, desktop] `scripts/dev/test-dialogkit.ps1` (needs an interactive desktop) over the modes open, save and legacy, each x64 and x86:
    - **Creation runs only in the test's own temp tree**: plain, `Žluťoučký kůň`, a decoy named `Dokumenty`, a deep folder where both new names still fit under 260 characters, and an over-limit folder.
    - **Real user folders are resolve-only**: Desktop, Documents (OneDrive), `D:\`, This PC.
    - Pass when:
      - resolve equals the actual folder, and the log shows which rung answered;
      - This PC gives Unresolved and creates nothing;
      - the New menu lists a Folder and a Text Document entry;
      - Folder twice gives the menu's default name, then its " (2)" variant (on this cs-CZ system `Nová složka`, `Nová složka (2)`); Text Document likewise (`Nový textový dokument.txt`, `Nový textový dokument (2).txt`);
      - the hook reports each new item selected in rename mode;
      - the over-limit folder is refused and nothing is created;
      - every resolve takes ≤200 ms;
      - the Desktop, Documents and Downloads listings are identical before and after the run.
  - [A, desktop] With a `hung` fixture open, query returns Err within 1 s, and a second fixture dialog opened meanwhile still resolves and creates.
  - [M] In Chrome and Edge upload dialogs and in Notepad Open and Save As, on Desktop and `D:\`, `--act new` with the Text Document entry creates the file in the dialog's folder and the dialog shows it in rename mode. After a name is typed, the Save dialog's file-name box holds it, and Save shows the overwrite prompt.
  - [M] Elevated Notepad: no action, one line in the log, no hang.
  - [M] `docs/dialog-dumps/matrix.md` is committed: one line per dialog with process, bitness, integrity level, AppContainer status, kind, folder, rung and ms. It also records Chrome's dialog process and whether hook/template legacy dialogs or packaged-app pickers occur.
- Escalation: if the hook fails in Chrome or Notepad, stop and decide with the lead before M2. The address-bar fallback alone does not cover known folders.
- Out of scope: the button, the menu UI, the tray and the installer.

**M2: dialog helper (product)**
- Scope:
  - The button, Ctrl+N and the New menu popup. Errors show inline.
  - Tray with Pause and Quit.
  - Single-instance mutex.
  - `install-/uninstall-dialogkit.ps1` per §4 rules 3 and 4.
- Acceptance:
  - [M] In Chrome and Notepad, the button's New menu creates a folder and a text file in the dialog's current folder, shown in rename mode.
  - [M] The button:
    - appears ≤300 ms after the dialog and follows it when moved or resized;
    - hides when the dialog is in the background or minimized, and stays while the menu is open;
    - is correct at 100% and 150% DPI;
    - never takes focus from the file-name box.
  - [M] With the Czech layout active, Ctrl+N opens the menu and AltGr+N still types `}` in the file-name box.
  - [M] The idle budget in §5 holds, including while typing and moving the mouse in another app.
  - [A] Sandbox only: install writes the `Run` value and the files with the ALL APPLICATION PACKAGES grant. Uninstall stops the process and removes the value and only its own directory. A locked DLL is renamed and a `RunOnce` cleanup is registered. The real-keys guard passes.
  - [M] The user runs the real install. After signing in again, it works in Chrome and Notepad. Uninstall removes the `Run` value at once and the files after the next sign-in.

**M3: index core**
- Scope: the pure parts of `fm-index`:
  - fold, the arena (with `file_id`), upsert/remove/rename/move, compaction;
  - the query parser, ranking, refinement;
  - the snapshot;
  - a seeded generator with Czech names.
- Acceptance:
  - [A] Semantics table: case, diacritics, AND, phrases, `ext:`/`path:`/`folder:`/`file:`, the hidden toggle, ranking.
  - [A] Snapshot round-trip. A truncated, corrupt or wrong-version snapshot, or one with an intact header and a zeroed tail, returns `Err` with no panic.
  - [A, release, ignored] The gated search budgets in §5 hold on 2M entries, and the reported ones are printed.

**M4a: scanner and bench**
- Scope:
  - Walker: thread pool, `FileIdBothDirectoryInfo` (§2), no reparse points except cloud folders, access denied counted and skipped, depth cap 512.
  - `examples/scan.rs`.
- Acceptance:
  - [A] The walker matches `std::fs::read_dir` on a tree with a junction loop, a path over 260 characters and diacritics.
  - [A] The mock records 0 file opens; only directories are opened.
  - [M-run] C: and D: put the entry count, cold and warm scan time, B/entry and 5 query times into `docs/search-bench.md`. The warm time is the cost of each re-walk (§2).
- Escalation: if C: takes over 90 s, tune threads and buffer size, then move the USN service up from Later.

**M4b: watcher**
- Scope: `ReadDirectoryChangesExW` with coalescing, FileId matching, `GetLongPathNameW` for names with `~`, and the background re-walk on overflow, start and daily (§2).
- Acceptance:
  - [A] Changes reach the index within ≤500 ms. After a 50k-file storm, the index equals a fresh walk.
  - [A] A forced overflow (test hook: tiny buffer) runs the re-walk. Afterwards added, removed and renamed entries, sizes and mtimes equal a fresh walk.
  - [A] A remove and a rename event carrying a short 8.3 name are applied by FileId.
  - [M-run] Overflows per day go into `docs/search-bench.md`.

**M5: search window**
- Scope:
  - The index loads from the snapshot (or scans) in the background, then re-walks, watches and saves (§2).
  - A standalone window: a query box and a virtual ListView with Name, Folder, Modified and Size.
  - Enter opens the item. Ctrl+Enter shows it in its folder (`SHOpenFolderAndSelectItems` until M6a exists).
  - Status line: entry count and scan progress.
  - Only if the user says yes to Q1: `fm.exe --background` at sign-in (a second `Run` value in the install script) and a global hotkey that opens the window.
- Acceptance:
  - [A] An end-to-end query runs through the window's own query function.
  - [M] No visible lag (p99 <50 ms in the log). With a 2M-entry snapshot, the window answers queries ≤1 s after start.

**M6a: browsing core (read-only)**
- Scope:
  - The window and an asynchronous, cancellable listing.
  - Virtual details view: Name, Modified, Type, Size, cloud. Sorted with `StrCmpLogicalW`.
  - Icons per rule 2.
  - Ctrl+H, type-to-select, Enter.
- Acceptance:
  - [A] `app-core`: sorting, cancellation, and "zlu" finds "Žluťoučký".
  - [A] A 100k-item folder meets the §5 budget.
  - [M] Screenshots of C:, Desktop and OneDrive.
  - [M] A cloud-only file's attributes are unchanged after browsing (`attrib`).

**M6b: navigation**
- Scope:
  - Sidebar: known folders, drives, pinned folders.
  - Address bar: Ctrl+L, `%env%`.
  - Back, forward and up (Alt+arrows, mouse buttons). The navigation state holds one tab, so a tab list can be added later.
  - Ctrl+F opens the M5 search window.
- Acceptance:
  - [A] `app-core`: history.
  - [M] Screenshots of the sidebar and address bar on C:, Desktop and OneDrive.

**M7: file operations**
- Scope (§2 File operations):
  - F2 (stem selected), Ctrl+Shift+N, the New submenu.
  - Delete and Shift+Delete.
  - Ctrl+C/X/V and Alt+Enter through the canonical verbs.
  - IContextMenu3.
  - Auto-refresh.
- Acceptance:
  - [A] A deleted item is in the Recycle Bin.
  - [A] The flags never include `FOF_NOCONFIRMATION`.
  - [A] Renaming onto an existing name fails and both files remain. Renaming `a.txt` to `A.txt` succeeds.
  - [M] The conflict dialog appears, third-party menu items are present, cut/paste works with Explorer in both directions, and Properties opens.
  - [M] A cloud-only file's attributes are unchanged after opening its context menu.

**M8: drag and drop**
- Scope: OLE `DoDragDrop` (`SHCreateDataObject`) and `IDropTarget` via IFileOperation.
- Acceptance:
  - [M] Drag and drop to and from Explorer, the desktop and a Chrome upload.

**M9: Option 2 probe and pass-through shim**
- Gate first, at most one day: a shim that only logs and returns comdlg32's factory. The user installs it for real. If the log does not show Notepad and Chrome creating the dialog through it, Option 2 stops here and M10 is not built.
- Scope: routing (§2), a wrapper that logs every method call, the kill switch, the scripts per §4 rules 3 and 4.
- Acceptance:
  - [A] Routing tests: allowlist, kill switch, elevated, explorer.exe.
  - [A] Sandbox only: after install, `InprocServer32` under the sandbox root points to an existing file in the temp `shim\` directory. After uninstall, nothing is left under the sandbox root. Installing dialogkit and the shim, then uninstalling dialogkit, leaves the shim DLL in place. The real-keys guard passes.
  - [M] Notepad, Chrome, Paint and Office are unchanged.
  - [M] The log shows the processes, methods, integrity level and AppContainer status, and whether legacy `GetOpenFileName` is routed through the shim.
- Status (gate build, 2026-11-20): `crates/dialog-shim`, `scripts/install-shim.ps1`, `scripts/uninstall-shim.ps1`.
  - The shim always returns comdlg32's own factory (from the HKLM `InprocServer32`, never through COM). It logs one line per `DllGetClassObject` to `%LOCALAPPDATA%\fm\shim.log`: process, interface, integrity level, AppContainer, the routing decision for `shim-allow.txt` (empty by default), and the modules on the call stack, which shows whether `comdlg32` (legacy `GetOpenFileName`) or the app itself created the dialog. No paths are logged.
  - The per-method wrapper is deliberately not built: it is the risky part and only makes sense if the gate passes.
  - Gate result (user's machine, 2026-11-20): `Claude.exe FileOpenDialog IClassFactory medium appcontainer=0 pass (not allowlisted) hr=0 combase.dll<Claude.exe`: Claude's dialog comes through the override, so M10 was built for it. Nothing is known yet about Notepad, Chrome, Paint or Office.
  - [A] passed: routing unit test; `tests/harness.rs` loads a temp copy of the DLL with no registry and creates a real `IFileOpenDialog` through it, with the kill switch off (one log line) and on (none); `scripts/dev/test-shim.ps1` (21 checks: key written last and removed first, ThreadingModel copied from HKLM, a locked DLL set aside with its own RunOnce cleanup, another app's override neither replaced nor removed, dialogkit uninstall leaves the shim, real-keys guard).
  - Gate run (user, own 64-bit PowerShell): `scripts\build-release.ps1 -SkipX86`, `scripts\install-shim.ps1`, open File → Open in Notepad, Chrome, Paint and Office, then read `%LOCALAPPDATA%\fm\shim.log` and `scripts\uninstall-shim.ps1`. Recovery: `reg delete "HKCU\Software\Classes\CLSID\{DC1C5A9C-E88A-4dde-A5A1-60F82A20AEF7}" /f`.

**M10: Option 2 picker**
- Scope: the forwarding wrapper and pipe transport in the shim, the picker in fm (the resident's door and `fm.exe --pick`), built on the M6 and M7 views; the `Show()` integration (§2). First host: Claude.exe (Electron's `file_dialog_win.cc`: raw `IFileOpenDialog`, `SetOptions` once, `SetFileName` always, `SetFolder` only for an absolute default path, no `Advise`, no `IFileDialogCustomize`, `GetResults` even for one item; `Show()` may run on a dialog thread).
- Acceptance:
  - [A] Delegation tests for customize, `FOS_PICKFOLDERS` and `Advise` (`FOS_ALLNONSTORAGEITEMS` and `SetFilter` take the same path). Save never reaches the shim (only CLSID_FileOpenDialog is registered).
  - [A] A filter round-trip: the setters reach the picker, and `GetResult`, `GetResults` (one and several items) and `GetFolder` return the pick; cancel returns `ERROR_CANCELLED`; a picker that never connects, an fm that refuses and no fm.exe all end in the system dialog; the owner is disabled while the picker is up and enabled after.
  - [A] A real `fm --pick` through the shim's transport: file types filter the folder, a single pick, a multiselect pick, Ctrl+A in a single-select dialog, Esc. A running fm (door under a test class, `--dev-door`) takes the request and answers when its picker closes.
  - [M] Claude's "attach file" returns the file(s) picked in our picker, and cancel works.
  - [M] Killing the picker brings up the system dialog with no hang.
  - [M] Uninstall leaves no registry keys at once and no files after the next sign-in.
- Status (2026-11-20, branch of `window-fixes`):
  - [A] passed: `cargo test -p fm-dialog-shim` (protocol round trips and refusals, routing, `tests/harness.rs` with a stand-in door: every case above, plus a dialog made by `CoCreateInstance` itself, an answer longer than the pipe buffer, an fm.exe without the marker), `cargo test -p fm-app` (file-type matching, start folder, the filtered listing and single-select clamp), `cargo test -p fm-app --test pick -- --ignored` (the real `fm --pick` and a real door), `scripts/dev/test-shim.ps1`.
  - Not covered automatically: anything inside the real Claude (does its packaged process see fm's door and `shim-allow.txt`, does the foreground come back, which thread and interface Electron really uses). The `shim.log` lines of the next real run answer these.
  - Try it (user, own 64-bit PowerShell, in the main checkout, whose `.cargo\config.toml` keeps the build out of OneDrive): `scripts\build-release.ps1 -SkipX86`; `& "$env:LOCALAPPDATA\Programs\Soubory\fm.exe" --quit`, then `Start-Process dist\app\fm.exe --background` (this build is the resident fm until the next sign-in; an older resident makes the shim show the system dialog); `scripts\install-shim.ps1`; quit Claude completely and start it again; attach a file. Log: `%LOCALAPPDATA%\fm\shim.log`, for Claude probably `%LOCALAPPDATA%\Packages\Claude_pzs8sxrjxfjjc\LocalCache\Local\fm\shim.log`. Off again: the file `%LOCALAPPDATA%\fm\disable-shim`, or `scripts\uninstall-shim.ps1`.

**Later:** column view, Quick Look, Recents, view memory, tabs, the USN service (and a probe of the undocumented `FSCTL_READ_UNPRIVILEGED_USN_JOURNAL` for an exact catch-up after restart), an elevated dialogkit, the redesign.

## 7. Risks and unknowns

| Risk | Resolved by |
|---|---|
| `CWM_GETISHELLBROWSER` is undocumented and may change in a future build | M1 matrix, re-run after feature updates; address-bar fallback for absolute paths |
| The hook DLL crashes or hangs a host app | §4 rule 7, asynchronous requests, `hung` fixture; it only runs on a user action |
| Chrome DLL blocking, antivirus vs the hook | M1 matrix (spike: Chrome and Edge work), M2 manual check with Defender on |
| Topmost button z-order | M2 manual check |
| Slow scan of C:; index stale after the app was closed | M4a escalation, re-walk at start, Q1, USN service later |
| Shell calls hydrating files (`.exe`/`.lnk` icons, menu handlers) | Rule 2, M6a and M7 attribute checks |
| Option 2: legacy routing, antivirus vs the HKCU COM override, orphaned keys | M9 gate, per-component directories, kill switch, `RunOnce` cleanup |
| ListView does not feel like Finder | Accepted. The redesign keeps `app-core` |

## 8. Deliberately skipped

- **Items in the dialog's own context menu:** the button opens the same New menu.
- **Breadcrumb and known-folder resolver:** the hook gives the exact folder.
- **Trigrams:** a memmem scan is enough. Revisit if the M4a numbers fail.
- **`trash`, `notify`, `walkdir`, `jwalk`, `rkyv`, `bincode`, `memmap2`, `criterion`:** IFileOperation, Win32, std and ignored tests cover these.
- **Content search, regex, `size:`/`date:`:** not requested.
- **Ctrl+Z:** the Recycle Bin covers deletes.
- **Dark mode, Mica, thumbnails, tags, icon/gallery views:** these come with the redesign.
- **Network drives:** RDCW is unreliable over SMB.
- **Option 2 Save, folder pickers, 32-bit and ARM64:** outside the experiment (multiselect is in since M10).
- **`IExplorerBrowser`:** it brings Explorer's slowness and can't be reskinned.

## 9. Open questions for the user

- **Q1.** Should `fm.exe` start hidden at sign-in, keeping the index live, with a global hotkey that opens search? It costs up to ~200 MB of RAM at 2M entries. Without it, search opens from the snapshot in ≤1 s and catches up in the background (up to ~90 s).
- **Q2.** One **Nový** button with Windows' New menu (Složka, Textový dokument, Word…), as built in `5337897`, or two separate buttons, "Nová složka" and "Nový soubor", as originally asked? The mechanism is the same either way.

## Review log

Review of 2026-11-19, 24 findings (1–14 feasibility, 15–24 scope). Findings cite the old milestone numbers. Old → new: M1a+M1b → M1; M2, M3 unchanged; M4 → M4a+M4b; M7 (search) → M5; M5 (browsing) → M6a+M6b; M6 (file operations) → M7; M8 → M8 without tabs; M9, M10 unchanged.

| # | Finding | Decision | Reason |
|---|---|---|---|
| 1 | [A] runs install scripts on the live hive | Accepted | Scripts take `-RegRoot`/`-InstallDir`; [A] uses the sandbox, the real install is [M]. The guard compares the real keys before and after, because they exist legitimately once the user installs. |
| 2 | Two uninstallers share one folder | Accepted | One directory per component; each uninstaller deletes only its own; sandboxed test in M9. |
| 3 | A mapped DLL cannot be deleted | Accepted, extended | Also hits the hook DLL, which pins itself. Rename to `*.del` plus HKCU `RunOnce`; M2 and M10 reworded. |
| 4 | No timeouts on cross-process calls | Accepted (modified) | Applied to the hook request (≤500 ms query, asynchronous select), `SendMessageTimeoutW` and UIA on a worker; `hung` fixture test in M1. |
| 5 | `SendInput` Ctrl+Shift+N can hit another app | Accepted (superseded by spike `da5354d`) | No synthesized input at all: the shell's New menu plus hook select and rename. The "nothing outside the temp tree" listing check is kept. |
| 6 | Option 2 routing ignores folder pickers, `Advise`, filters | Accepted | Routing excludes PICKFOLDERS, multiselect, ALLNONSTORAGEITEMS, Advise and customize; Set*/Get* honored; M10 tests added. |
| 7 | The reconcile cannot repair size and mtime | Accepted (modified) | `ReadDirectoryChangesExW` records carry size and mtime; overflow, start and daily run one re-walk instead of a dir-mtime reconcile; forced-overflow test in M4b. |
| 8 | 8.3 short names in events | Accepted | Extended records plus a stored FileId; the walker moves to `FileIdBothDirectoryInfo` so FileIds exist from the first scan. |
| 9 | Ctrl+Alt+N equals AltGr+N | Accepted (modified) | Verified on kbdlayout.info: AltGr+N gives `}` on KBDCZ. The dialog uses Ctrl+N (already in `5337897`), only while a dialog is in the foreground, with a Czech-layout check in M2; `fm.exe` uses Ctrl+Shift+N and the New submenu. |
| 10 | Global LOCATIONCHANGE wakes dialogkit constantly | Accepted | Scoped to the tracked dialog; CPU measured while typing and moving the mouse. |
| 11 | AppContainer hosts cannot load from `%LOCALAPPDATA%` | Accepted | `icacls` confirms no ALL APPLICATION PACKAGES entry; each installer grants it on its own directory, which may also let the hook DLL load into AppContainer hosts (not verified); AppContainer status is logged. |
| 12 | Rename collision through IFileOperation may prompt | Accepted | `MoveFileExW` without replace; collision and case-only tests in M7. |
| 13 | Snapshot writes ~43 GB/day, no integrity | Accepted | Hourly while dirty plus exit and end of session; flush and write-through rename; length and checksum; zeroed-tail test. |
| 14 | 240-character case collides with MAX_PATH | Accepted | Refuse at 260; a fitting case and an over-limit case. |
| 15 | Search arrives only at old M7 | Accepted | Standalone search window as new M5, before the browser (new M6a). |
| 16 | Nothing keeps the index resident | Accepted as a design point | `--background` and a hotkey are designed in; whether they ship is the user's call (Q1). |
| 17 | Use the dialog's own New menu | Accepted (modified, spike `e5970e8`) | `CLSID_NewMenu` is hosted in dialogkit instead of driving the dialog's menu across processes; the resolver table is gone. |
| 18 | Rust fixture crate | Accepted | The WinForms fixture in `scripts/dev/test-dialogkit.ps1` (from the spike scripts) gains a `hung` mode; a legacy-hook mode only if the matrix finds such dialogs. |
| 19 | Four change-tracking layers; split M4 | Partly accepted | USN probe moved to Later; reconcile and daily re-walk merged into one re-walk; M4 split into M4a/M4b. Rejected: walker before M3, because M3 is already being built in the `hydrogen/index` worktree; M4a runs in parallel instead. |
| 20 | Query features and gates exceed the request | Partly accepted | §5 split into gated and reported. Rejected: dropping `folder:`/`file:`, word boundary and the newer tie-break, which are already in the `hydrogen/index` task, cost a few lines, and now have exact mtimes. |
| 21 | Use shell verbs instead of own clipboard code | Partly accepted | Copy, cut, paste and Properties use the canonical verbs; the old optional file-operations split (M6a/M6b) is merged into one new M7. Rejected for delete: the verb depends on the user's settings and Shift state, so IFileOperation keeps rule 1 testable; rename uses `MoveFileExW` (#12). |
| 22 | Tabs are not requested | Accepted | Moved to Later; the navigation state holds one tab. |
| 23 | Extra crate, double kill switch, tray Pause | Partly accepted | No `dialogkit-core` crate, file-only kill switch. Rejected: removing tray Pause, which is already built in `5337897`, and dropping the 150% DPI check, which is one screenshot and catches positioning bugs over other processes' windows. |
| 24 | Old M5 too big; Option 2 needs a gate | Accepted | The browsing split (new M6a/M6b) is mandatory; M9 starts with a one-day go/no-go gate. |
