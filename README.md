# Soubory

Správce souborů pro Windows 11 ve stylu Finderu. Může nahradit Průzkumník souborů pro otevírání
složek, disků a Win+E. Rychlé vyhledávání v názvech všech souborů, náhledy (mezerník), karty,
štítky a vlastní ikony pro všechny běžné typy souborů.

*A Finder-style file manager for Windows 11 that can take over from File Explorer for folders,
drives and Win+E. The interface is in Czech. Licensed under GPL-3.0-or-later.*

> **Testovací verze.** Soubory jsou ve vývoji a zatím je používá jen pár lidí. Instalace mění jen
> nastavení tvého uživatelského účtu a jde kdykoli vrátit (viz níže).

## Stažení a instalace

**[Stáhnout Soubory 0.1.0-beta.3](https://github.com/malaveselahranolka/soubory/releases/download/v0.1.0-beta.3/Soubory-0.1.0-beta.3-setup.exe)**
(instalátor pro 64bitová Windows 10 a 11; další verze najdeš v
[Releases](https://github.com/malaveselahranolka/soubory/releases)).

Stažený soubor spusť a projdi průvodce. Instaluje se jen pro tvůj účet, bez práv správce, a nabídne,
aby se složky, disky a Win+E otevíraly v Souborech místo Průzkumníka.

- Instalátor zatím není digitálně podepsaný, takže Windows SmartScreen může varovat. Klikni na
  **Další informace** a pak **Přesto spustit**.
- Zpět na Průzkumník souborů: odinstaluj Soubory v Nastavení › Aplikace, nebo spusť instalátor znovu
  a volbu zruš.

## Co umí

- Zobrazení ikony, seznam, sloupce, galerie a dlaždice; karty; postranní panel s disky a štítky.
- Vyhledávání v názvech souborů na všech discích (vlastní index), uložená hledání.
- Rychlý náhled (mezerník) obrázků, textu, PDF a dokumentů Office.
- Barevné štítky, kopírování a přesun s průběhem, Koš, zpět/znovu (Ctrl+Z / Ctrl+Y).
- Volitelně tlačítko „Nový ▾“ v oknech Otevřít/Uložit ostatních aplikací (dialogkit).
- Světlý, tmavý a vysoce kontrastní režim; čtečky obrazovky přes UI Automation.

Podrobný přehled: [docs/FEATURES.md](docs/FEATURES.md). Návrh vzhledu: [docs/DESIGN.md](docs/DESIGN.md).

## Sestavení ze zdrojů

Potřebuješ Rust (stable, toolchain `x86_64-pc-windows-msvc`) a Visual Studio Build Tools.

```powershell
cargo build --release -p fm-app
.\target\release\fm.exe
```

`scripts\build-release.ps1` sestaví všechny části (včetně 32bitového dialogkitu) do `dist\` a
`iscc installer\Soubory.iss` z nich udělá instalátor ([Inno Setup](https://jrsoftware.org/isinfo.php) 6.7).
Pokud máš repozitář v synchronizované složce (OneDrive), zkopíruj `.cargo\config.example.toml`
na `.cargo\config.toml` a nastav v něm `target-dir` mimo ni.

## Licence

Copyright © 2026 malaveselahranolka.

Soubory jsou svobodný software: můžeš je šířit a upravovat podle licence GNU General Public License
verze 3 nebo novější ([LICENSE](LICENSE)). Ikony [Phosphor](https://phosphoricons.com) a použité
knihovny jazyka Rust mají vlastní licence, viz [THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt).
