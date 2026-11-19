//! Sidebar model: Finder's sections with Windows locations.
use crate::app::Loc;
use crate::fsys;
use std::path::PathBuf;
use windows::Win32::UI::Shell::*;

#[derive(Clone, Debug)]
pub enum Row {
    Header(&'static str),
    Item {
        label: String,
        glyph: char,
        loc: Loc,
        eject: bool,
    },
    Tag(usize),
}

pub mod glyph {
    pub const RECENT: char = '\u{E81C}';
    pub const HOME: char = '\u{E80F}';
    pub const DESKTOP: char = '\u{E7F4}';
    pub const DOCUMENTS: char = '\u{E8A5}';
    pub const DOWNLOADS: char = '\u{E896}';
    pub const PICTURES: char = '\u{EB9F}';
    pub const MUSIC: char = '\u{EC4F}';
    pub const VIDEOS: char = '\u{E714}';
    pub const CLOUD: char = '\u{E753}';
    pub const DRIVE: char = '\u{EDA2}';
    pub const USB: char = '\u{E88E}';
    pub const NETWORK: char = '\u{E968}';
    pub const COMPUTER: char = '\u{E7F8}';
    pub const FOLDER: char = '\u{E8B7}';
    pub const EJECT: char = '\u{F847}';
}

pub fn build(favorites: &[PathBuf]) -> Vec<Row> {
    let mut rows = vec![Row::Header("Oblíbené")];
    rows.push(Row::Item {
        label: "Nedávné".into(),
        glyph: glyph::RECENT,
        loc: Loc::Recents,
        eject: false,
    });
    let known: [(&windows::core::GUID, char); 7] = [
        (&FOLDERID_Profile, glyph::HOME),
        (&FOLDERID_Desktop, glyph::DESKTOP),
        (&FOLDERID_Documents, glyph::DOCUMENTS),
        (&FOLDERID_Downloads, glyph::DOWNLOADS),
        (&FOLDERID_Pictures, glyph::PICTURES),
        (&FOLDERID_Music, glyph::MUSIC),
        (&FOLDERID_Videos, glyph::VIDEOS),
    ];
    for (id, g) in known {
        if let Some(p) = fsys::known_folder(id) {
            rows.push(Row::Item {
                label: fsys::display_name(&p),
                glyph: g,
                loc: Loc::Folder(p),
                eject: false,
            });
        }
    }
    for p in favorites.iter().filter(|p| p.is_dir()) {
        rows.push(Row::Item {
            label: fsys::display_name(p),
            glyph: glyph::FOLDER,
            loc: Loc::Folder(p.clone()),
            eject: false,
        });
    }
    if let Some(od) = fsys::onedrive() {
        rows.push(Row::Header("Cloud"));
        rows.push(Row::Item {
            label: fsys::display_name(&od),
            glyph: glyph::CLOUD,
            loc: Loc::Folder(od),
            eject: false,
        });
    }
    rows.push(Row::Header("Umístění"));
    rows.push(Row::Item {
        label: "Tento počítač".into(),
        glyph: glyph::COMPUTER,
        loc: Loc::Computer,
        eject: false,
    });
    for v in fsys::volumes() {
        let g = if v.network {
            glyph::NETWORK
        } else if v.removable {
            glyph::USB
        } else {
            glyph::DRIVE
        };
        rows.push(Row::Item {
            label: v.label,
            glyph: g,
            loc: Loc::Folder(v.root),
            eject: v.removable,
        });
    }
    rows.push(Row::Header("Štítky"));
    for i in 0..crate::theme::TAGS.len() {
        rows.push(Row::Tag(i));
    }
    rows
}
