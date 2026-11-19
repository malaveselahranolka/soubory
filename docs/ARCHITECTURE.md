# Architecture v1

Working name: `fm`. Written 2026-11-19 for Windows 11 x64, build 26300, Rust 1.100, edition 2024.

## 1. Goals and non-goals

Priorities, in order:
1. **Option 1 (must work).** Add "Nový soubor" / "Nová složka" buttons to every system Open/Save dialog, creating the item in the dialog's current folder.
2. **Instant filename search**, without Windows Search.
3. **Fast browsing in the style of Finder.** The design gets reworked later, so the UI layer stays swappable.
4. **Option 2 (experiment).** Replace the system dialog: opt-in, allowlisted, removable by script.

Coverage of "every dialog" in v1:

| Dialog | v1 |
|---|---|
| Common Item Dialog (IFileDialog): Notepad, Chrome/Edge/Electron, Firefox, most 32- and 64-bit apps | **Yes, target 100%** |
| Legacy `GetOpenFileName`, hook/template dialogs, packaged-app pickers | Decided by the M1a dump |
| Elevated apps | **No** (UIPI). Later via an explicit elevated instance |
| Private pickers (Office Backstage, Qt/GTK) | **No**. They are not system dialogs |
| Virtual locations (This PC, Home, Search) | Native new folder only. "New file" is disabled with a hint |

## 2. Decisions

**UI: Win32 plus comctl32 v6** through `windows` 0.62.2 (latest stable, 2025-10-06).
- A virtual `SysListView32` (`LVS_OWNERDATA`) handles 100k+ rows and includes rename, type-to-select, IME and UI Automation.
- The shell's context menu and drag and drop need a real WndProc, and the dialog helper uses the same toolkit.
- All logic sits in UI-free `app-core`, so a later reskin (Direct2D, or Slint 1.19.0) replaces only `app`.
- Rejected:
  - Tauri 2: WebView2 memory, weak native drag and context menu.
  - egui 0.37.0, iced 0.14.0.
  - gpui 0.2.2: pre-1.0.
- Other crates: `rayon` 1.12.0, `memchr` 2.8.3, `unicode-normalization` 0.1.26.
- Crates using `#[implement]` (`dialog-shim`, `app` for drag and drop) also need a direct `windows-core = "0.62.2"`, because the macro expands to `::windows_core::…`. **Never 0.100.0**: windows-core and windows-implement 0.100.0 are a separate 2026-09 line, and `windows` 0.62.2 depends on core 0.62.2. This corrects the research report.
- Sources: https://crates.io/api/v1/crates/windows/0.62.2/dependencies, https://crates.io/api/v1/crates/{rayon,memchr,unicode-normalization,slint}

**File operations: `IFileOperation`.**
- Covers the Recycle Bin (`FOFX_RECYCLEONDELETE|FOF_ALLOWUNDO`), the native conflict dialog, the shell's UAC elevation and the permanent-delete warning (`FOF_WANTNUKEWARNING`).
- Never `FOF_NOCONFIRMATION`. No `trash` crate.

**Search v1: a non-admin walk, a watcher and a snapshot.**
- **Walk.** Parallel `FindFirstFileExW` (`FindExInfoBasic`, `LARGE_FETCH`, `\\?\`) over fixed NTFS volumes.
- **Index layout:**
  - Structure-of-arrays: a folded name arena and a display name arena, plus `parent`, `attrs`, `size`, `mtime` and an extension id.
  - Paths are assembled lazily.
- **Matching:**
  - `fold()` = lowercase + strip diacritics (NFD).
  - Parallel `memchr::memmem` scan.
  - A refining keystroke filters the previous result.
  - A generation counter cancels stale queries.
- **Syntax:** AND, `"phrases"`, `ext:`, `path:`, `folder:`, `file:`. Hidden and system files are a query-time toggle.
- **Ranking:** exact, then prefix, then word boundary, then substring. Ties: shorter name, then newer.
- **Snapshot:**
  - `%LOCALAPPDATA%\fm\index-<serial>.bin`, with a header holding magic, version and serial.
  - Written via temp file plus rename, on exit and every 5 min.
  - A corrupt snapshot means a rebuild, never a panic.
- **Changes:**
  - One `ReadDirectoryChangesW` per volume root, coalesced over 50–100 ms.
  - Overflow or a restart triggers a dir-mtime reconcile.
  - A full re-walk runs daily.
- **Upgrades behind the same API:**
  - (a) Probe `FSCTL_READ_UNPRIVILEGED_USN_JOURNAL`. It is in the SDK 26100 `winioctl.h` but undocumented. It would give an exact catch-up after a restart.
  - (b) v2: an elevated USN service installed by script, with a named pipe for the user only.

**Dialog helper: a separate process, no injection.**
- **Why:** a helper crash cannot hit the host app, and there is no 32/64-bit DLL matrix.
- **Detection:**
  - Out-of-context `SetWinEventHook` (SHOW, FOREGROUND, DESTROY, LOCATIONCHANGE).
  - Watch `#32770` windows. About 150 ms after one appears (3 retries), classify its children: `SHELLDLL_DefView` plus either the file-name control (0x47C) or `DUIViewWndClassName`.
- **Current folder** is a hypothesis that M1 confirms. It is read at the moment of the action. The ladder:
  1. `WM_GETTEXT` on the breadcrumb `ToolbarWindow32`, keeping the text after the first `": "`.
  2. If that is not a path, map the first segment to a known folder (`SHGetKnownFolderPath`, display names), append the rest, and verify it is a directory.
  3. UIA: the address bar value, cross-checked against the item names in the list.
  4. Only if real folders fail: an in-process `WH_GETMESSAGE` hook with `CWM_GETISHELLBROWSER` (WM_USER+7, undocumented).
  - If the folder is unresolved, "New file" is refused.
- **New folder:** the dialog's native **Ctrl+Shift+N** (rename in place, and it works in virtual folders where the shell allows it). Fallbacks: UIA Invoke of "New folder", then `CreateDirectoryW`.
- **New file:**
  - A popup prefilled with "Nový soubor.txt".
  - `CreateFileW(CREATE_NEW)`, retrying with " (2)", " (3)"…, then `SHChangeNotify`.
  - `WM_SETTEXT` puts the name into the file-name box.
  - The helper never presses Open or Save. In a Save dialog the system asks about overwriting, which is expected.
- **Attaching to the dialog:**
  - Ctrl+Alt+N, registered only while a dialog is in the foreground.
  - A **button strip**: an unowned `WS_EX_TOPMOST|NOACTIVATE|TOOLWINDOW` popup, Per-Monitor-V2 DPI. It is visible only while the dialog or our popup is in the foreground, and follows LOCATIONCHANGE.
  - **No cross-process owner**: that would attach input queues, so a hang in one process would freeze the other ([Raymond Chen](https://devblogs.microsoft.com/oldnewthing/20130412-00/?p=4683)).
- **Higher-integrity targets:** skipped, logged once.

**Option 2: HKCU COM override plus a forwarding shim.**
- **Registration.** `fm_dialog_shim.dll` under `HKCU\Software\Classes\CLSID\{DC1C5A9C-E88A-4dde-A5A1-60F82A20AEF7}`, 64-bit view only, by script. This is [Files'](https://github.com/files-community/Files/tree/main/src/Files.App.OpenDialog) technique; see their failures in [#15275](https://github.com/files-community/Files/issues/15275) and [#6314](https://github.com/files-community/Files/issues/6314).
- **Default path.** `DllGetClassObject` returns **comdlg32's own factory** (path taken from HKLM `InprocServer32`) outside the allowlist, and also for elevated processes, `explorer.exe`, or when the kill switch is set.
- **Allowlisted processes** get a forwarding wrapper. Only `Show()` decides:
  - Use our picker only for Open, single-select, with no `IFileDialogCustomize` calls.
  - Run `fm.exe --pick … --result-file <random temp>` and wait on the process handle while pumping messages.
  - Re-enable the owner on every path.
  - If the launch fails or the picker crashes, fall back to the original `Show()`. Every export is wrapped in `catch_unwind`.

**Process model:**

| Process | Runs as | Lifetime | IPC |
|---|---|---|---|
| `fm.exe` | user | Single instance (`Local\fm.main`); a second launch hands over its path via `WM_COPYDATA`. Owns the index | none |
| `fm.exe --pick` | user | One per dialog call, no index | result file |
| `fm-dialogkit.exe` | user | HKCU `Run`, set by the install script | none |
| `fm_dialog_shim.dll` | allowlisted host | Experiment | starts the picker |
| `fm-indexd` (v2) | service | Elevated script | named pipe |

## 3. Cargo workspace

```
Cargo.toml            [workspace] members = ["crates/*"]; versions in [workspace.dependencies]
.cargo/config.toml    target-dir = "C:/Users/adame/AppData/Local/cargo-target/file-manager"  (exists)
crates/
  fm-fs/           lib    fold, name validation, unique_name, create_new_file, placeholder flags, IFileOperation (M6)
  fm-index/        lib    arena, query, ranking, snapshot (pure); win::{walker, watcher, reconcile}
  dialogkit-core/  lib    classify(&[Node]), resolve(text, &KnownFolders) (pure)
  dialogkit/       bin    fm-dialogkit.exe: hook, WM_GETTEXT/UIA, actions, strip, popup, tray, --dump, --act
  dialog-fixture/  bin    opens IFileOpenDialog / IFileSaveDialog / GetOpenFileNameW at a given folder
  app-core/        lib    history, tabs, listing, sorting, type-to-select, clipboard (no UI)
  app/             bin    fm.exe: Win32 views, shell integration, search, --pick
  dialog-shim/     cdylib depends only on `windows` (runs inside other processes)
scripts/  install-/uninstall-dialogkit.ps1, install-/uninstall-dialog-shim.ps1
docs/     ARCHITECTURE.md, search-bench.md, dialog-dumps/
```

Crate dependencies: `dialogkit` → `dialogkit-core`, `fm-fs`; `fm-index` → `fm-fs`; `app-core` → `fm-fs`; `app` → `app-core`, `fm-index`, `fm-fs`. Library tests run without a GUI. Tests that need a desktop are marked `#[ignore]`.

## 4. Safety rules

1. **No silent data loss.**
   - Delete goes to the Recycle Bin. Permanent deletion (Shift+Delete, or a file too large for the bin) always asks first.
   - New items use `CREATE_NEW` or a unique name.
   - Copy and move use the native conflict dialog.
   - Renaming onto an existing name is an error.
2. **Never hydrate OneDrive placeholders.**
   - Nothing reads the content of an item with `OFFLINE`, `RECALL_ON_OPEN` or `RECALL_ON_DATA_ACCESS`.
   - Icons come from `SHGFI_USEFILEATTRIBUTES`.
   - The walker only lists directories (checked by a mock test).
   - Hydration happens only on an explicit user action.
3. **No silent system changes.**
   - The registry and autostart are touched only by `scripts/*.ps1` run by the user, in HKCU only.
   - Uninstall removes exactly what install added.
   - The app never runs elevated. Elevation comes from the shell (IFileOperation).
4. **Stable install path.**
   - Binaries go to `%LOCALAPPDATA%\Programs\fm\`, never to the target dir or OneDrive. A dangling override would break dialogs everywhere.
   - Uninstall deletes the keys before the files.
   - Recovery: `reg delete "HKCU\Software\Classes\CLSID\{DC1C5A9C-E88A-4dde-A5A1-60F82A20AEF7}" /f`
5. **Shim kill switch.** `HKCU\Software\fm\Dialog\Enabled=0` or the file `%LOCALAPPDATA%\fm\disable-shim`, checked on every call.
6. **Data location.** Index and logs live only in `%LOCALAPPDATA%\fm\`.

## 5. Performance budgets

| Area | Budget | Measured by |
|---|---|---|
| Dialog shown → strip visible | ≤300 ms | dialogkit log |
| New-file action | ≤200 ms | M1b matrix test |
| dialogkit idle | ~0% CPU, <15 MB | `Get-Process` after 10 min (manual) |
| Query, 2M entries, 1–3 terms | p50 <15 ms, p99 <50 ms | `cargo test -p fm-index --release -- --ignored perf` |
| Refining keystroke | <5 ms | same |
| Snapshot load (2M entries) | ≤1 s, file ≤150 MB | same |
| Index memory | ≤100 B/entry | same |
| Live update | ≤1 ms/event | same |
| First full scan of C: | ≤90 s, UI usable meanwhile | M4 Phase 0 → `docs/search-bench.md` |
| fm.exe start → folder shown | ≤300 ms | startup log (manual) |
| Folder with 100k items | first rows ≤200 ms, all ≤1.5 s | lister test + manual scroll |
| fm.exe without index | ≤60 MB | `Get-Process` |

## 6. Milestones

Each milestone uses the `.ai/work/TASK.md` format.
- **[A]** = checked automatically (`cargo test` or a script).
- **[M]** = manual check by the lead from screenshots.

Preconditions:
- `cargo` is in `%USERPROFILE%\.cargo\bin`, which is **not on PATH**. Add it for the session only.
- `#[ignore]` dialog tests need an interactive desktop.

**M1a: dialog helper probe (Option 1 spike, part 1)**
- Scope:
  - Workspace skeleton and `fm-fs` (`unique_name`, `create_new_file`, name validation).
  - `dialog-fixture` with modes open, save, legacy and legacy-hook.
  - `fm-dialogkit --dump`: for each `#32770`, write the child tree, breadcrumb text, UIA name and process to `docs/dialog-dumps/`.
- Acceptance:
  - [A] `cargo test -p fm-fs`: `unique_name` gives " (2)" and " (3)", and `CREATE_NEW` leaves an existing file's bytes unchanged.
  - [A, ignored] `--dump` captures every fixture mode, and the dumps are committed.
  - [M] The lead runs `--dump` while Notepad Save As and a Chrome `<input type=file>` dialog are open, on a plain folder, Desktop, Documents, `Downloads\<subfolder>`, `D:\` and This PC. The dumps are committed.
  - [M] `docs/dialog-dumps/README.md` records the breadcrumb format for each dialog type, legacy-hook coverage and Chrome's dialog process.

**M1b: dialog helper actions (Option 1 spike, part 2)**
- Scope:
  - `dialogkit-core` (classify and resolve).
  - Run mode: the hook, Ctrl+Alt+N (create "Nový soubor.txt" and fill in the name) and the new-folder ladder.
  - `--act {resolve|new-file|new-folder} --hwnd H`, which prints JSON.
- Acceptance:
  - [A] `cargo test -p dialogkit-core`:
    - the resolver table passes on the committed dumps (CZ/EN, absolute, UNC, known folder plus subpath, This PC gives Unresolved);
    - `classify` accepts the Open, Save and legacy dumps and rejects Explorer and MessageBox.
  - [A, ignored] `dialog_matrix` over the modes open, save and legacy.
    - **Creation runs only in the test's own temp tree**: plain, `Žluťoučký kůň`, a decoy named `Dokumenty`, and a 240-character path.
    - **Real user folders are resolve-only**: Desktop, Documents (OneDrive), `D:\`, This PC.
    - Pass when:
      - resolve equals the actual folder;
      - This PC gives Unresolved and creates nothing;
      - new-file twice gives `Nový soubor.txt`, then `Nový soubor (2).txt`;
      - the file-name box equals the new name;
      - with focus in the file-name box, new-folder adds exactly one directory (log which rung worked);
      - every action takes ≤200 ms.
  - [M] In Chrome and in Notepad Save As, on Desktop and `D:\`, Ctrl+Alt+N creates the file and fills in the name. In Notepad, Save then shows the overwrite prompt.
  - [M] Elevated Notepad: no action, one line in the log, no hang. The idle budget holds.
- Escalation: if a real folder fails on rungs 1–3, add an in-process hook milestone before M2.
- Out of scope: the button strip, popup, tray and installer.

**M2: dialog helper (product)**
- Scope:
  - The strip and the name popup: Enter resolves the folder again and creates the file, Esc cancels, errors show inline.
  - Tray with Pause and Quit.
  - Single-instance mutex.
  - Scripts.
- Acceptance:
  - [M] In Chrome and Notepad, clicking "Nová složka" or "Nový soubor" creates the item in the dialog's current folder.
  - [M] The strip:
    - appears ≤300 ms after the dialog and follows it when moved or resized;
    - hides when the dialog is in the background or minimized, and stays while the popup is open;
    - is correct at 100% and 150% DPI;
    - never takes focus from the file-name box.
  - [A] Validation rejects CON, NUL, `<>:"/\|?*`, and a trailing dot or space.
  - [A] Install writes `Run` → `%LOCALAPPDATA%\Programs\fm\`. Uninstall removes the value and the folder.
  - [M] After signing in again, it works in Chrome and Notepad.

**M3: index core**
- Scope: the pure parts of `fm-index`:
  - fold, the arena, upsert/remove/rename/move, compaction;
  - the query parser, ranking, refinement;
  - the snapshot;
  - a seeded generator with Czech names.
- Acceptance:
  - [A] Semantics table: case, diacritics, AND, phrases, `ext:`/`path:`/`folder:`/`file:`, the hidden toggle, ranking.
  - [A] Snapshot round-trip. A truncated, corrupt or wrong-version snapshot returns `Err` with no panic.
  - [A, release, ignored] The search budgets in §5 hold on 2M entries.

**M4: scanner and watcher**
- Scope:
  - Walker: thread pool, no reparse points except cloud folders, access denied counted and skipped, depth cap 512.
  - RDCW with coalescing and overflow handling, plus the mtime reconcile.
  - A 2 h USN probe.
  - `examples/scan.rs`.
- Acceptance:
  - [A] The walker matches `std::fs::read_dir` on a tree with a junction loop, a path over 260 characters and diacritics.
  - [A] The mock records 0 content opens.
  - [A] Changes reach the index within ≤500 ms, and the index stays consistent after a 50k-file storm.
  - [M-run] Phase 0 on C: and D: puts the entry count, scan time, B/entry and 5 query times into `docs/search-bench.md`.
- Escalation: if the scan takes over 90 s, try `NtQueryDirectoryFile` first, then the USN service.

**M5: browsing (read-only)**
- Scope:
  - Sidebar: known folders, drives, pinned folders.
  - Address bar: Ctrl+L, `%env%`.
  - Back, forward and up (Alt+arrows, mouse buttons).
  - Virtual details view: Name, Modified, Type, Size, cloud. Sorted with `StrCmpLogicalW`.
  - Asynchronous, cancellable listing.
  - Icons per rule 2.
  - Ctrl+H, type-to-select, Enter.
- Acceptance:
  - [A] `app-core`: history, sorting, cancellation, and "zlu" finds "Žluťoučký".
  - [A] A 100k-item folder meets the §5 budget.
  - [M] Screenshots of C:, Desktop and OneDrive.
  - [M] A cloud-only file's attributes are unchanged after browsing (`attrib`).
- If it is too big for one run: M5a is the window, listing and details view; M5b is the sidebar, address bar and history.

**M6: file operations**
- Scope:
  - F2 (stem selected), Ctrl+Shift+N, Ctrl+Alt+N.
  - Delete and Shift+Delete.
  - Ctrl+C/X/V (`CF_HDROP` + `Preferred DropEffect`).
  - IContextMenu3.
  - Alt+Enter.
  - Auto-refresh.
- Acceptance:
  - [A] A deleted item is in the Recycle Bin.
  - [A] The flags never include `FOF_NOCONFIRMATION`.
  - [A] Renaming onto an existing name fails and both files remain.
  - [M] The conflict dialog appears, third-party menu items are present, and cut/paste works with Explorer in both directions.
- If it is too big for one run: M6a is rename, new, delete and refresh; M6b is the clipboard, context menu and Properties.

**M7: search in the app**
- Scope:
  - The index loads from the snapshot (or scans) in the background, then watches and saves.
  - Ctrl+F field with results: Name, Folder, Modified, Size.
  - Open the item or its containing folder.
  - Status in the status bar.
- Acceptance:
  - [A] An end-to-end query runs through the UI's own query function.
  - [M] No visible lag (p99 <50 ms in the log). With a 2M-entry snapshot the app still starts in ≤300 ms.

**M8: tabs and drag and drop**
- Scope: Ctrl+T/W/Tab. OLE `DoDragDrop` (`SHCreateDataObject`) and `IDropTarget` via IFileOperation.
- Acceptance:
  - [A] Tab model.
  - [M] Drag and drop to and from Explorer, the desktop and a Chrome upload.

**M9: Option 2 probe and pass-through shim**
- Scope: routing (§2), a wrapper that logs every method call, the kill switch, the scripts.
- Acceptance:
  - [A] Routing tests: allowlist, kill switch, elevated, explorer.exe.
  - [A] After install, `InprocServer32` points to an existing file under `%LOCALAPPDATA%\Programs\fm\shim\`. After uninstall, `reg query` finds nothing.
  - [M] Notepad, Chrome, Paint and Office are unchanged.
  - [M] The log shows the processes and methods, and whether legacy `GetOpenFileName` is routed through the shim.

**M10: Option 2 picker**
- Scope: `fm.exe --pick`, built on the M5 and M6 views including new file and new folder, plus the `Show()` integration.
- Acceptance:
  - [A] Delegation tests for customize, Save and multiselect.
  - [M] Notepad Open returns the file picked in our picker, and cancel works.
  - [M] Killing the picker brings up the system dialog with no hang.
  - [M] Uninstall leaves no trace.

**Later:** column view, Quick Look, Recents, view memory, the USN service, an elevated dialogkit, the redesign.

## 7. Risks and unknowns

| Risk | Resolved by |
|---|---|
| Breadcrumb format (Czech, build 26300, known folders) | M1 dump and matrix, then the ladder |
| Ctrl+Shift+N does nothing (focus in the name box, legacy dialog) | M1. Then UIA Invoke by AutomationId, then `CreateDirectoryW` |
| Limits on SendInput or foreground; Chrome dialog process and DLL blocking | M1b `--act`, M1a dump. Prefer UIA and messages |
| Topmost strip z-order; antivirus vs the global hook | M2 manual matrix with Defender on |
| Slow scan of C:; index stale after the app was closed | M4 escalation, reconcile, USN probe, v2 |
| Shell calls hydrating files (`.exe`/`.lnk` icons, menu handlers) | Rule 2, M5 attribute check |
| Option 2: legacy routing, antivirus vs the HKCU COM override, orphaned keys | M9 probe, stable path, kill switch |
| ListView does not feel like Finder | Accepted. The redesign keeps `app-core` |

## 8. Deliberately skipped

- **Items in the dialog's context menu:** cannot be added from outside the process.
- **Trigrams:** a memmem scan is enough. Revisit if the M4 numbers fail.
- **`trash`, `notify`, `walkdir`, `jwalk`, `rkyv`, `bincode`, `memmap2`, `criterion`:** IFileOperation, Win32, std and ignored tests cover these.
- **Content search, regex, `size:`/`date:`:** not requested.
- **ShellNew templates:** an empty file with any extension is enough.
- **Ctrl+Z:** the Recycle Bin covers deletes.
- **Dark mode, Mica, thumbnails, tags, icon/gallery views:** these come with the redesign.
- **Network drives:** RDCW is unreliable over SMB.
- **Option 2 Save, multiselect, 32-bit and ARM64:** outside the experiment.
- **`IExplorerBrowser`:** it brings Explorer's slowness and can't be reskinned.
