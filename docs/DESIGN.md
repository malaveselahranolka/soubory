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
││ Oblíbené       ││ Název                  Změněno         Velikost  Druh   ││
││ ▌◷ Nedávné     ││ ▸ 📁 Faktury           Dnes 9:41          —     Složka   ││
││  ⌂ Domů        ││ ███ 📄 Smlouva.pdf ███ Včera 18:02    1,2 MB  PDF       ││  selection = accent capsule
││  ▭ Plocha      ││                                                          ││
││ Umístění       ││                                                          ││
││  ◫ Místní (C:) ││                                                          ││
││ Štítky         │├──────────────────────────────────────────────────────────┤│
││  ● Červená     ││ C: › Uživatelé › Dokumenty            12 položek, 34 GB volných ││  28 px path + status
│└────────────────┘╰──────────────────────────────────────────────────────────╯│
```
- L-shaped chrome on Mica (toolbar + sidebar); content sits on an opaque card with an 8 px top-left radius and a hairline edge — the Windows 11 Explorer material, Finder's proportions.
- Caption buttons stay Windows (46×32, Segoe Fluent glyphs, snap layouts on maximize).
- Tabs appear between toolbar and card only with ≥2 tabs (Finder).

## Signature
Finder's inset rounded selection capsule, combined with the Fluent accent indicator: the sidebar's selected row is a translucent pill with a 3×16 px accent bar at its leading edge; content rows select as a solid accent capsule with white text (grey when the window is inactive). Colour tags are Finder's seven colours as 8 px dots.

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

High contrast: system colours (`COLOR_WINDOW`, `COLOR_WINDOWTEXT`, `COLOR_HIGHLIGHT`, `COLOR_HIGHLIGHTTEXT`, `COLOR_BTNFACE`, `COLOR_GRAYTEXT`), no Mica, 2 px focus rings.

**Type:** Segoe UI Variable (Text ≤ 14 px, Display above), fallback Segoe UI; Segoe Fluent Icons for glyphs; Cascadia Mono for text previews. Scale (≈1.2): 11 caption · 12 small · 13 body · 15 title · 20 heading · 28 display. Weights 400 / 600 only. Grayscale antialiasing everywhere (Finder-like, works on Mica).

**Metrics (at 100 %):** spacing base 4. Toolbar 52. Sidebar 208 (180–320, resizable), rows 28, pill radius 6, inset 8. List rows 26, icon 16, capsule inset 8, radius 6. Column header 28. Path/status bar 28. Icon view tiles 64/96/128 px icons. Card radius 8. Buttons 32×28, radius 6. Hit areas ≥ 28 × 28.

**Depth:** borders-only plus tonal surfaces (Windows 11). No drop shadows inside the window; DWM supplies window and Quick Look shadows.

**Motion:** none on frequent actions (navigation, selection, typing). Quick Look opens with opacity 0→1 and scale .96→1 over 160 ms ease-out; off when the system disables client-area animations.

**Words:** Czech, sentence case, verbs on actions ("Nová složka", "Přesunout do koše"), Windows names for Windows things ("Koš", "Vlastnosti").
