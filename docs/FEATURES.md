# Feature matrix

Goal (user, 2026-11-20): replace Windows File Explorer completely — every Explorer feature, every Finder feature, plus our own. Status: ✅ done and verified · 🔶 built, not fully verified · ⬜ planned.

## From Explorer
| Feature | How | Status |
|---|---|---|
| Navigation pane: Home, Desktop…Videos, OneDrive, This PC, drives, network | sidebar sections | 🔶 |
| Address bar: breadcrumb, typed path (Ctrl+L / Alt+D), sibling-folder dropdowns | path bar + Go to Folder | ⬜ |
| Back / Forward / Up, Alt+arrows, Backspace | history per tab | 🔶 |
| Tabs (Ctrl+T, Ctrl+W, Ctrl+Tab, middle-click) | tab strip | 🔶 |
| Views: details, icons (S–XL), tiles | list + icon view with size slider | 🔶 |
| Sort by any column, folders first, group by | header click, More menu | 🔶 |
| Full shell context menu (Open with, Send to, 7-Zip, Git…) | `IContextMenu` | 🔶 |
| New folder / New ▸ any type | shell New menu (same as the dialog helper) | 🔶 |
| Cut / Copy / Paste / Delete / Rename / Undo | shell verbs, `IFileOperation`, in-place rename | 🔶 |
| Native copy progress and conflict dialogs | `IFileOperation` | 🔶 |
| Drag and drop in/out, with Explorer semantics | OLE, folder's own `IDropTarget` | ⬜ |
| Properties (Alt+Enter) | `SHObjectProperties` | 🔶 |
| Preview pane / details pane | right inspector pane (Ctrl+Shift+P) | ⬜ |
| Show hidden items, file name extensions | More menu, Ctrl+Shift+. | 🔶 |
| Search | instant index (fm-index), not Windows Search | ⬜ |
| Compress to ZIP / Extract all | shell verbs | ⬜ |
| Open in Terminal, Copy as path (Ctrl+Shift+C) | | 🔶 |
| Recycle Bin, restore | shell namespace folder | ⬜ |
| Network, map drive | shell | ⬜ |
| Open/Save dialogs: New file / New folder button | fm-dialogkit | ✅ |

## From Finder
| Feature | Status |
|---|---|
| Unified toolbar, translucent sidebar, path bar, status bar | 🔶 |
| Icon, list, column and gallery views (Ctrl+1–4) | 🔶 list/icon, ⬜ column/gallery |
| Quick Look (Space) | ⬜ |
| Get Info (Ctrl+I) | 🔶 (Windows Properties) |
| Colour tags + tag sidebar | ⬜ |
| Recents | 🔶 |
| Smart folders (saved searches) | ⬜ |
| Go to Folder (Ctrl+Shift+G) | ⬜ |
| Spring-loaded folders while dragging | ⬜ |
| Type-to-select | 🔶 |
| Rename multiple items (batch rename) | ⬜ |
| Duplicate (Ctrl+D), New Folder with Selection | ⬜ |
| Calculate folder sizes | ⬜ (from the index) |

## Our own
| Feature | Status |
|---|---|
| Instant whole-disk search with Czech diacritics folding (`kun` finds `kůň`) | 🔶 core done |
| New file/folder from any Open/Save dialog | ✅ |
| Folder sizes in list view, live from the index | ⬜ |
| Filter the current folder as you type (Ctrl+F in folder scope) | ⬜ |
| Command palette (Ctrl+K) | ⬜ |
