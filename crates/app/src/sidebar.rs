//! Sidebar model: places, then storage, then tags, with the Recycle Bin at the bottom. Groups are
//! set apart by dividers instead of headers.
use crate::app::Loc;
use crate::fsys;
use crate::phosphor::Ico;
use windows::Win32::UI::Shell::*;

#[derive(Clone, Debug)]
pub enum Row {
    /// A hairline between groups.
    Divider,
    /// Takes the height left over, so what follows sits at the bottom.
    Spacer,
    Item {
        label: String,
        ico: Ico,
        loc: Loc,
        eject: bool,
        /// A drive's used share (0..=1), shown as a bar under its name.
        usage: Option<f32>,
    },
    Tag(usize),
}

impl Row {
    fn item(label: impl Into<String>, ico: Ico, loc: Loc) -> Row {
        Row::Item { label: label.into(), ico, loc, eject: false, usage: None }
    }
}

fn usage(total: u64, free: u64) -> Option<f32> {
    (total > 0).then(|| (1.0 - free as f64 / total as f64).clamp(0.0, 1.0) as f32)
}

pub fn build(prefs: &crate::app::Prefs) -> Vec<Row> {
    let mut rows = vec![Row::item("Nedávné", Ico::ClockCounterClockwise, Loc::Recents)];
    let known = [
        (&FOLDERID_Profile, Ico::House),
        (&FOLDERID_Desktop, Ico::Monitor),
        (&FOLDERID_Documents, Ico::FileText),
        (&FOLDERID_Downloads, Ico::DownloadSimple),
        (&FOLDERID_Pictures, Ico::Image),
        (&FOLDERID_Music, Ico::MusicNotes),
        (&FOLDERID_Videos, Ico::FilmStrip),
    ];
    for (id, ico) in known {
        if let Some(p) = fsys::known_folder(id) {
            rows.push(Row::item(fsys::display_name(&p), ico, Loc::Folder(p)));
        }
    }
    for p in prefs.favorites.iter().filter(|p| p.is_dir()) {
        rows.push(Row::item(fsys::display_name(p), Ico::Folder, Loc::Folder(p.clone())));
    }
    for (text, scope) in &prefs.saved {
        let label = match scope {
            Some(f) => format!("{text} ({})", fsys::display_name(f)),
            None => text.clone(),
        };
        rows.push(Row::item(label, Ico::FileMagnifyingGlass, Loc::Search { text: text.clone(), scope: scope.clone() }));
    }
    rows.push(Row::Divider);
    if let Some(od) = fsys::onedrive() {
        rows.push(Row::item(fsys::display_name(&od), Ico::Cloud, Loc::Folder(od)));
    }
    rows.push(Row::item("Tento počítač", Ico::DesktopTower, Loc::Computer));
    for v in fsys::volumes() {
        let ico = if v.network {
            Ico::Network
        } else if v.removable {
            Ico::Usb
        } else {
            Ico::HardDrive
        };
        // volumes() leaves network drives at 0: their capacity is not read on the UI thread.
        let usage = usage(v.total, v.free);
        rows.push(Row::Item { label: v.label, ico, loc: Loc::Folder(v.root), eject: v.removable, usage });
    }
    if let Some(loc) = crate::shellns::network() {
        rows.push(Row::item("Síť", Ico::Network, loc));
    }
    rows.push(Row::Divider);
    rows.extend((0..crate::theme::TAGS.len()).map(Row::Tag));
    if let Some(loc) = crate::shellns::recycle_bin() {
        rows.push(Row::Spacer);
        rows.push(Row::Divider);
        rows.push(Row::item("Koš", Ico::Trash, loc));
    }
    rows
}

/// Re-reads the drives' fill after a file operation (the drives build() measured).
pub fn refresh_usage(rows: &mut [Row]) {
    for r in rows {
        if let Row::Item { loc: Loc::Folder(root), usage: u @ Some(_), .. } = r {
            if let Some((total, free)) = fsys::disk_space(root) {
                *u = usage(total, free).or(*u);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_is_the_used_share() {
        assert_eq!(usage(0, 0), None);
        assert_eq!(usage(100, 25), Some(0.75));
        assert_eq!(usage(100, 100), Some(0.0));
        assert_eq!(usage(100, 200), Some(0.0));
    }

    #[test]
    fn groups_are_divided_and_the_bin_is_last() {
        let prefs = crate::app::Prefs {
            show_hidden: false,
            folders_first: true,
            icon_size: 64.0,
            favorites: Vec::new(),
            saved: vec![("faktura".into(), None)],
            window: None,
        };
        let rows = build(&prefs);
        assert!(matches!(rows.first(), Some(Row::Item { loc: Loc::Recents, .. })));
        assert_eq!(rows.iter().filter(|r| matches!(r, Row::Tag(_))).count(), crate::theme::TAGS.len());
        assert!(rows.iter().any(|r| matches!(r, Row::Item { loc: Loc::Computer, .. })));
        assert!(rows.iter().any(|r| matches!(r, Row::Item { ico: Ico::FileMagnifyingGlass, .. })));
        if crate::shellns::recycle_bin().is_some() {
            let n = rows.len();
            assert!(matches!(&rows[n - 3..], [Row::Spacer, Row::Divider, Row::Item { label, .. }] if label == "Koš"));
        }
        assert!(!matches!(rows.first(), Some(Row::Divider)));
        assert!(rows.windows(2).all(|w| !matches!(w, [Row::Divider, Row::Divider])));
    }
}
