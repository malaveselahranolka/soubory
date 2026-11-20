// Embeds the application manifest (Common Controls v6, Windows 10/11 compatibility, needed for
// layered child windows over the composition surface, per-monitor DPI v2, long paths), the app
// icon (assets\fm.ico, drawn by `fm --render assets --brand`) and the `--pick` marker.
use std::path::Path;

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let manifest = Path::new(&dir).join("fm.manifest");
    let ico = Path::new(&dir).join("../../assets/fm.ico");
    println!("cargo:rerun-if-changed={}", manifest.display());
    println!("cargo:rerun-if-changed={}", ico.display());
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}", manifest.display());
        // Missing only before the first `fm --render assets --brand`: fm then uses the folder icon.
        let icon = std::fs::read(&ico).map_err(|e| println!("cargo:warning=no app icon ({}: {e})", ico.display())).ok();
        let res = Path::new(&std::env::var("OUT_DIR").unwrap()).join("fm.res");
        std::fs::write(&res, resources(icon.as_deref())).unwrap();
        println!("cargo:rustc-link-arg-bins={}", res.display());
    }
}

fn entry(res: &mut Vec<u8>, ty: u16, id: u16, data: &[u8]) {
    res.extend((data.len() as u32).to_le_bytes());
    res.extend(32u32.to_le_bytes());
    for v in [0xFFFF, ty, 0xFFFF, id] {
        res.extend(u16::to_le_bytes(v));
    }
    res.extend([0u8; 4]); // DataVersion
    res.extend(if ty == 0 { 0u16 } else { 0x1010 }.to_le_bytes()); // MOVEABLE | DISCARDABLE
    res.extend([0u8; 10]); // LanguageId (neutral), Version, Characteristics
    res.extend(data);
    while res.len() % 4 != 0 {
        res.push(0);
    }
}

/// A compiled resource file, which link.exe takes as is (no resource compiler needed): the .ico's
/// frames (RT_ICON 1..n) and their directory (RT_GROUP_ICON 1), and the RT_RCDATA marker that
/// tells fm_dialog_shim this exe understands `--pick` (proto::MARKER_ID, proto::MARKER).
fn resources(ico: Option<&[u8]>) -> Vec<u8> {
    let mut res = Vec::new();
    entry(&mut res, 0, 0, &[]);
    entry(&mut res, 10, 0x666D, b"fm-pick=1");
    if let Some(ico) = ico {
        icon(&mut res, ico);
    }
    res
}

fn icon(res: &mut Vec<u8>, ico: &[u8]) {
    let n = u16::from_le_bytes([ico[4], ico[5]]);
    let mut group = vec![0, 0, 1, 0];
    group.extend(n.to_le_bytes());
    for i in 0..n {
        let e = &ico[6 + 16 * i as usize..][..16];
        let len = u32::from_le_bytes(e[8..12].try_into().unwrap()) as usize;
        let at = u32::from_le_bytes(e[12..16].try_into().unwrap()) as usize;
        entry(res, 3, i + 1, &ico[at..at + len]);
        group.extend(&e[..12]);
        group.extend((i + 1).to_le_bytes());
    }
    entry(res, 14, 1, &group);
}
