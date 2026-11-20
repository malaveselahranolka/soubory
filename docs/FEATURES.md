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
| Open folders and drives instead of Explorer; Win+E | `scripts/set-default-fm.ps1` (HKCU, undo with `restore-explorer.ps1`) | ✅ sandbox |
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
| Folder sizes in list, tiles and column views, live from the index (an empty folder shows 0 bajtů, an unindexed one —); sort by size moves folders too; in the selection total and the inspector | ✅ (scratch-index harness and capture; the whole C: sums in 15 ms) |
| Filter the current folder as you type | 🔶 (search scoped to the folder) |
| Command palette (Ctrl+K): every command plus every sidebar place, diacritics-insensitive | 🔶 (filtering tested and captured; running uses the menu handlers) |

## Not verified
- By hand: real drags from Explorer, the desktop and a browser; the Mica look on the user's machine; idle CPU over hours.
- Screen readers: the UI Automation tree (toolbar buttons, sidebar places, items with Select / Open / Scroll into view, status bar; focus, selection and structure events) passes `scripts/dev/uia-test.ps1` in list, icons, tiles and columns, but nobody has used Narrator or NVDA with it. Not exposed yet: group headers, tabs, the path bar, the column view's other columns, the preview pane.
- "Show in folder" from other apps and virtual folders opened from the desktop (Control Panel, Recycle Bin icon) still open Explorer; Windows offers no per-user hook for those.
- Option 2 needs the user's gate run (`docs/ARCHITECTURE.md`, M9) before any picker is built.
