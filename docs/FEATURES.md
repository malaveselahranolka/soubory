# Feature matrix

Goal (user, 2026-11-20): replace Windows File Explorer completely — every Explorer feature, every Finder feature, plus our own. Status: ✅ done and verified (tests, `--dev-*` harnesses, window captures) · 🔶 built, not fully verified · ⬜ planned.

Nothing here has been tried by hand in daily use yet; see "Not verified" at the end.

## From Explorer
| Feature | How | Status |
|---|---|---|
| Navigation pane: Home, Desktop…Videos, OneDrive, This PC, drives, Network, Recycle Bin, tags; follows drives being plugged in, mapped or ejected | sidebar sections, shell drive notifications | ✅ (drive events not exercised) |
| Address bar: breadcrumb, typed path (Ctrl+L / Alt+D / Ctrl+Shift+G) | path bar edit | 🔶 |
| Folder menus on the path-bar chevrons (subfolders, drives) | `crumb_menu` | 🔶 |
| Back / Forward / Up, Alt+arrows, Backspace | history per tab | 🔶 |
| Tabs (Ctrl+T, Ctrl+W, Ctrl+Tab, middle-click) | tab strip | 🔶 |
| Several windows (Ctrl+N), one process; a second launch opens a window in it | hub window, `WM_COPYDATA` | ✅ |
| Views: details, icons (S–XL with slider), columns, gallery | Ctrl+1–4 | 🔶 |
| Group by: name, kind, date modified/created, size, tags; Finder-style headers that stay on top while scrolling; a header click selects its group | `groups.rs`, View menus, palette | ✅ (geometry round-trip tests, harness hit-test over the real window, captures) |
| Tiles view (Ctrl+5) | icon + name, kind, size in a grid; groups too | ✅ (harness hit-test, captures, UIA test) |
| Sort by any column, folders first | header click, More menu | 🔶 |
| Full shell context menu (Open with, Send to, Compress to ZIP, Extract all, 7-Zip, Git…) | `IContextMenu` of the folder | 🔶 |
| New folder / New ▸ any type | shell New menu (same as the dialog helper) | 🔶 |
| Cut / Copy / Paste / Delete / Rename | shell verbs, `IFileOperation`, in-place rename | 🔶 |
| Undo / Redo (Ctrl+Z / Ctrl+Y): rename, new folder, duplicate, paste, move, delete to Recycle Bin | own undo stack, bin restore via `$I` records | ✅ |
| Native copy progress and conflict dialogs | `IFileOperation` | 🔶 |
| Drag and drop into the window, tabs, path bar, sidebar; spring-loaded folders; drag out | target folder's own `IDropTarget`, `SHDoDragDrop` | ✅ in (harness), 🔶 out |
| Properties (Alt+Enter) | `SHObjectProperties` | 🔶 |
| Preview / details pane | inspector pane (Ctrl+Shift+P) | 🔶 |
| Show hidden items, file name extensions | More menu, Ctrl+Shift+. | 🔶 |
| Search | whole-disk index (fm-index), not Windows Search | ✅ |
| Open in Terminal, Copy as path (Ctrl+Shift+C) | | 🔶 |
| Recycle Bin: original location, date deleted, Restore, Empty | shell namespace folder | ✅ |
| Network | shell namespace folder (empty while network discovery is off) | 🔶 |
| Map / disconnect network drive | More menu → Windows' own dialogs | 🔶 |
| Open folders, drives, This PC, Recycle Bin and Network instead of Explorer; Win+E; Explorer windows other apps start with `explorer.exe` | `fm --set-default` (HKCU, undo with `fm --restore-explorer`; the scripts `set-default-fm.ps1` / `restore-explorer.ps1` do the same) and the fm-dialogkit watcher; Control Panel stays Explorer's | ✅ sandbox |
| "Show in folder" (browsers' downloads, Office, Start's "Open file location") selects the item in fm | fm windows join the shell's window list (IShellWindows) like Explorer's; scripts see them in `Shell.Application.Windows()` (LocationURL, Navigate, Quit) | ✅ (window already open; a cold start is the user's check) |
| Explorer's command line: a folder, a file (opens its folder, selects it), `/select,`, `/root,`, `::{CLSID}`, `shell:Name`, `file:///`, `\\server` | `target.rs`; what fm cannot show goes to Explorer | ✅ (unit tests, end-to-end runs) |
| Open in Explorer (escape hatch) | More menu, palette | 🔶 |
| Open/Save dialogs: New file / New folder button | fm-dialogkit | ✅ |
| Replace the system Open dialog (Option 2) | `fm_dialog_shim.dll`, gate build: logs only | 🔶 gate |

## From Finder
| Feature | Status |
|---|---|
| Unified toolbar, sidebar on Mica, path bar, status bar | 🔶 |
| Column and gallery views | 🔶 |
| Quick Look (Space): images, text, PDF/Office via preview handlers | 🔶 |
| Get Info (Ctrl+I) | 🔶 (Windows Properties) |
| Colour tags, tag dots, tag sidebar, tag by dropping on a tag | ✅ |
| fm's own icons (Phosphor, MIT): type-coloured glyphs and tinted pages for every ordinary file type, amber folders with emblems on known folders; the shell keeps programs, shortcuts, custom folders, drives and thumbnails. Sidebar with drive fill bars, tag dots, Koš at the bottom | ✅ (unit tests, offscreen renders at 100/125/150 % in light, dark, high contrast; UIA test; sidebar click and tag drop through the dev hooks) |
| Recents | 🔶 |
| Go to Folder (Ctrl+Shift+G) | 🔶 |
| Spring-loaded folders while dragging | ✅ (harness) |
| Type-to-select | 🔶 |
| Rename multiple items: replace, add text, number | ✅ |
| Duplicate (Ctrl+D), New Folder with Selection | ✅ |
| Search scope bar: whole computer / this folder | ✅ |
| Smart folders: saved searches in the sidebar (More → Uložit hledání) | ✅ open (harness), 🔶 save |
| Calculate folder sizes | ✅ always on, from the index (see Our own) |

## Our own
| Feature | Status |
|---|---|
| Instant whole-disk search with Czech diacritics folding (`kun` finds `kůň`), live as files change | ✅ (C: 4.2 M entries, 30 s walk, queries 3–8 ms: `docs/search-bench.md`; index reviewed twice, see `26ad376`) |
| New file/folder from any Open/Save dialog | ✅ |
| Cold start ≈ 200 ms (D3D device built in parallel, power-saving GPU) | ✅ (`--dev-startup`) |
| Folder sizes in list, tiles and column views, live from the index (an empty folder shows 0 bajtů; — while unknown: not indexed, a junction or link, access denied, a cloud placeholder, which is never opened); sort by size moves folders too; in the selection total and the inspector | ✅ (scratch-index harness and capture; the whole C: sums in 15 ms) |
| Runs in the background like Explorer: opening a folder is instant, the index stays warm; "Ukončit Soubory" (palette) or `fm --quit` ends it | ✅ (resident test: hand-over 45 ms in a debug build) |
| Folder shortcuts open in the same window | ✅ |
| Filter the current folder as you type | 🔶 (search scoped to the folder) |
| Command palette (Ctrl+K): every command plus every sidebar place, diacritics-insensitive | 🔶 (filtering tested and captured; running uses the menu handlers) |

## Not verified
- By hand: real drags from Explorer, the desktop and a browser; the Mica look on the user's machine; idle CPU over hours.
- Screen readers: the UI Automation tree (toolbar buttons, sidebar places, items with Select / Open / Scroll into view, status bar; focus, selection and structure events) passes `scripts/dev/uia-test.ps1` in list, icons, tiles and columns, but nobody has used Narrator or NVDA with it. Not exposed yet: group headers, tabs, the path bar, the column view's other columns, the preview pane.
- The switch to fm on the real account (the user's own run of the scripts): Win+E, double-clicked drives, the desktop's This PC and Recycle Bin icons, "Show in folder" when no fm window shows that folder yet, the watcher replacing a window an app opened with `explorer.exe`, "Otevřít v Průzkumníkovi" opening Explorer rather than coming back to fm, the index saved at logoff.
- Option 2 needs the user's gate run (`docs/ARCHITECTURE.md`, M9) before any picker is built.
- Busy rule (a row press, a drag in or out, an open rename): live search results, file-change and shell reloads, size re-sorts and UI Automation actions wait until it ends. Traced by four review rounds, not exercised with real mouse timing.
- Explorer replacement, known limits: fm tells the shell's window list about every navigation with a direct call into explorer.exe, so a hung Explorer can stall fm's navigation until it recovers; scripts get a window's folder (`.LocationURL`, `.Document.Folder`) but not its selection (`.Document.SelectedItems()` fails on fm windows); the watcher leaves alone Explorer windows open before it started, windows with several tabs, places it cannot read within about 3 s, Fonts and other virtual folders backed by a real folder, and any window while fm itself is not responding (a dropped network share).
- Known limits: a folder holding only empty folders shows — (its 0 is not trusted, because a folder of unreadable folders also sums to 0); a tag place lists its items again on every tab switch (a stat per tagged file on the UI thread; an offline share in a tag can stall that switch).
