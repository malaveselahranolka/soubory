# Design v1 — fm

Brief: "sleek, modern, mainly like macOS Finder, with a Windows touch". Finder gives the structure and calm; Windows gives the materials, glyphs, accent and conventions.

## Intent
- **Who:** one person on a desk PC, all day long: downloads, projects, photos, OneDrive. Opens it hundreds of times a day, mostly from the keyboard.
- **Task:** get to a file and act on it (open, rename, move, find). Content is the focus; chrome recedes.
- **Feel:** calm, precise, quiet. Finder's restraint on Windows 11 materials.

## Structure
```
┌──────────────── Mica (chrome) ─────────────────────────────────────── _ □ × ┐
│ [▤] [‹][›]  Dokumenty                 [▦ ☰ ▥ ▭] [⋯]   [⌕ Hledat        ]  │  52 px unified toolbar
│┌ sidebar (Mica) ┐╭─────────────── content card (opaque) ───────────────────╮│
││ ◷ Nedávné      ││ Název                  Změněno         Velikost  Druh   ││
││ ⌂ adame        ││ ▸ 📁 Faktury           Dnes 9:41          —     Složka   ││
││░▤ Dokumenty░░░░││ ███ 📄 Smlouva.pdf ███ Včera 18:02    1,2 MB  PDF       ││  selection = accent capsule
││ ───────────    ││                                                          ││
││ ☁ OneDrive     ││                                                          ││
││ ▭ Místní (C:)  ││                                                          ││
││   ▬▬▬▬▬▬▬▬──   ││                                                          ││
││ ───────────    ││                                                          ││
││ ● ● ● ● ● ● ●  │├──────────────────────────────────────────────────────────┤│
││ ───────────    ││ C: › Uživatelé › Dokumenty            12 položek, 34 GB volných ││  28 px path + status
││ 🗑 Koš          ││                                                          ││
│└────────────────┘╰──────────────────────────────────────────────────────────╯│
```
- L-shaped chrome on Mica (toolbar + sidebar); content sits on an opaque card with an 8 px top-left radius and a hairline edge — the Windows 11 Explorer material, Finder's proportions.
- Caption buttons stay Windows (46×32, Segoe Fluent glyphs, snap layouts on maximize).
- Tabs appear between toolbar and card only with ≥2 tabs (Finder).

## Signature
Colour moves from the chrome into the content ("Vlastní tvář", chosen 2026-11-20). The sidebar is neutral: Phosphor bold icons in `ink2`, no section headers, three groups (places · storage · tags) set apart by hairline dividers, Koš at the bottom. The selected place is a quiet accent tint (14 % light, 20 % dark, radius 6) with a semibold label and its icon switched to Phosphor fill in `accent_glyph`. Drives show how full they are as a 3 px bar (accent; #D13438 from 90 %). Tags are a row of 14 px dots (24 px targets, name on hover). Content rows select as a solid accent capsule with white text (grey when the window is inactive); small type glyphs turn white on it.

**File icons (fm draws them):** a type-coloured Phosphor bold glyph at ≤ 24 px; above that a page (96-unit template, 10 radius, folded corner) tinted with the type colour (fill 14 % light / 22 % dark, outline 34 %, fold 40 %) carrying the type glyph and, from 72 px, the extension. Folders are amber (light #DB9A26 back / #F4B740 front, dark #C08420 / #E3A93A); the user's known folders (home, Plocha, Dokumenty, Stažené, Obrázky, Hudba, Videa, OneDrive) carry their sidebar glyph pressed into the front. The shell keeps what only it knows: programs and shortcuts (exe, lnk, ico, url, msi…), folders whose desktop.ini gives them their own icon (a read-only or system folder whose shell icon is still the plain folder, like Program Files, is fm's), drives, items that exist only in the shell (network computers, phones), and real thumbnails (photos, videos, documents with a preview). The type comes from the full path when there is one, so Koš items listed without their extension still get their type from the $R… file. Selected tiles draw the page in the selection's text colour.

## Tokens (`crates/app/src/theme.rs` is the source of truth)
| Token | Light | Dark | Why |
|---|---|---|---|
| `chrome` (Mica fallback) | #EEF0F3 | #202020 | Mica tint when the backdrop is off |
| `card` (content) | #FFFFFF | #1C1C1C | content is the furthest layer in both modes, like Explorer |
| `ink` primary text | rgba(0,0,0,.894) | #FFFFFF | Fluent TextFillPrimary |
| `ink2` secondary | rgba(0,0,0,.62) | rgba(255,255,255,.786) | dates, sizes, kinds |
| `ink3` tertiary | rgba(0,0,0,.447) | rgba(255,255,255,.545) | section headers, status |
| `ink4` disabled | rgba(0,0,0,.36) | rgba(255,255,255,.363) | |
| `edge` hairline | rgba(0,0,0,.08) | rgba(255,255,255,.08) | card edge, column headers |
| `hover` | rgba(0,0,0,.04) | rgba(255,255,255,.06) | rows and buttons |
| `press` | rgba(0,0,0,.07) | rgba(255,255,255,.09) | |
| `pill` sidebar selection | rgba(0,0,0,.07) | rgba(255,255,255,.08) | |
| `accent` | system accent, darkened until white text ≥4.5:1 | system accent (light variant) | user's Windows colour |
| `sel_inactive` | #DCDCDC | #3A3A3A | Finder's unfocused selection |
| `field` search/rename | rgba(0,0,0,.05) + edge | rgba(255,255,255,.06) + edge | inset, darker than surround |
| tags | #FF453A #FF9F0A #FFD60A #32D74B #0A84FF #BF5AF2 #8E8E93 | same | Finder's tag colours |
| type colours (`fileicons.rs`) | text #5B6B82 · pdf #D93A40 · doc #2F6FED · sheet #1E8E4E · slides #D9591E · archive #A86D14 · image #0B8584 · audio #9B47EC · video #D63C7C · code/data #6E56CF · disk #475569 · other #7A808A | each mixed 22 % toward white | ≥ 3:1 on the card in both modes (tested) |

High contrast: system colours (`COLOR_WINDOW`, `COLOR_WINDOWTEXT`, `COLOR_HIGHLIGHT`, `COLOR_HIGHLIGHTTEXT`, `COLOR_BTNFACE`, `COLOR_GRAYTEXT`), no Mica, 2 px focus rings.

**Type:** Segoe UI Variable (Text ≤ 14 px, Display above), fallback Segoe UI; Phosphor Icons 2.1.1 (MIT, bold; fill for the selected place) for every glyph except the caption buttons, which stay Segoe Fluent; Cascadia Mono for text previews. Scale (≈1.2): 11 caption · 12 small · 13 body · 14 sidebar · 15 title · 20 heading · 28 display; page labels 12/96 of the icon, semibold. Weights 400 / 600 only. Grayscale antialiasing everywhere (Finder-like, works on Mica).

**Metrics (at 100 %):** spacing base 4. Toolbar 52. Sidebar 208 (160–320, resizable), rows 32 (drives 40), icons 18, text 14, dividers 17 (1 px line, 16 px side margins), pill radius 6, inset 8. Toolbar glyphs 16, search 14. List rows 26, icon 16, capsule inset 8, radius 6. Column header 28. Path/status bar 28. Icon view tiles 64/96/128 px icons. Card radius 8. Buttons 32×28, radius 6. Hit areas ≥ 28 × 28, tag dots 24 × 24.

**Depth:** borders-only plus tonal surfaces (Windows 11). No drop shadows inside the window; DWM supplies window and Quick Look shadows.

**Motion:** none on frequent actions (navigation, selection, typing). Quick Look opens with opacity 0→1 and scale .96→1 over 160 ms ease-out; off when the system disables client-area animations.

**Words:** Czech, sentence case, verbs on actions ("Nová složka", "Přesunout do koše"), Windows names for Windows things ("Koš", "Vlastnosti").
