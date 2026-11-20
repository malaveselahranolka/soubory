//! Loads a temp copy of the built DLL (no registry, no UI) and checks that it serves
//! CLSID_FileOpenDialog through comdlg32: one log line with the kill switch off, none with it on.
#![cfg(windows)]
use std::{ffi::c_void, path::PathBuf, ptr::null_mut};
use windows::{
    Win32::{
        Foundation::*,
        System::{Com::*, LibraryLoader::*},
        UI::Shell::*,
    },
    core::*,
};

type GetClassObject = unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;

fn log_lines(fm: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(fm.join("shim.log")).unwrap_or_default().lines().map(str::to_string).collect()
}

/// The cdylib this test run built: uplifted into the profile dir, or (newer cargo build-dir
/// layout) in a sibling `build/fm-dialog-shim/<hash>/out`. The newest one wins.
fn built_dll(exe: &std::path::Path) -> Option<PathBuf> {
    let name = "fm_dialog_shim.dll";
    let mut found: Vec<PathBuf> = exe.ancestors().map(|a| a.join(name)).collect();
    if let Some(pkg) = exe.ancestors().find(|a| a.file_name().is_some_and(|n| n == "fm-dialog-shim")) {
        for d in std::fs::read_dir(pkg).into_iter().flatten().flatten() {
            found.push(d.path().join("out").join(name));
        }
    }
    found.into_iter().filter(|p| p.is_file()).max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
}

#[test]
fn serves_file_open_dialog_through_comdlg32() {
    let exe = std::env::current_exe().unwrap();
    let built = built_dll(&exe).expect("the cdylib next to this test's build output");
    let tmp = std::env::temp_dir().join(format!("fm-shim-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let fm = tmp.join("fm");
    std::fs::create_dir_all(tmp.join("shim")).unwrap();
    std::fs::create_dir_all(&fm).unwrap();
    let dll = tmp.join("shim").join("fm_dialog_shim.dll");
    std::fs::copy(&built, &dll).unwrap();
    let me = exe.file_name().unwrap().to_string_lossy().into_owned();
    std::fs::write(fm.join("shim-allow.txt"), format!("{me}\n")).unwrap();
    // SAFETY: the only test in this binary; the shim reads %LOCALAPPDATA% on each call.
    unsafe { std::env::set_var("LOCALAPPDATA", &tmp) };

    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
        let m = LoadLibraryW(&HSTRING::from(dll.as_os_str())).unwrap();
        let f: GetClassObject = std::mem::transmute(GetProcAddress(m, s!("DllGetClassObject")).unwrap());
        let open = || {
            let mut p = null_mut();
            f(&FileOpenDialog, &IClassFactory::IID, &mut p).ok().unwrap();
            let cf = IClassFactory::from_raw(p);
            let d: IFileOpenDialog = cf.CreateInstance(None).unwrap();
            let opts = d.GetOptions().unwrap();
            assert!(opts.0 & FOS_FILEMUSTEXIST.0 != 0, "a real Open dialog: {:#x}", opts.0);
        };

        open();
        let lines = log_lines(&fm);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let l = &lines[0];
        eprintln!("log: {l}");
        assert!(l.contains(&format!("\t{me}\tFileOpenDialog\tIClassFactory\t")), "{l}");
        assert!(l.contains("appcontainer=0") && l.contains("picker (gate: passed through)") && l.contains("hr=0x00000000"), "{l}");
        assert!(l.contains("harness"), "the caller shows on the stack: {l}");

        std::fs::write(fm.join("disable-shim"), "").unwrap();
        open();
        assert_eq!(log_lines(&fm).len(), 1, "kill switch: still served, nothing logged");

        // A class comdlg32 does not serve fails cleanly; bad pointers are refused.
        let mut p = null_mut();
        let bogus = GUID::from_u128(0x0f3e_8a54_6b2d_4c11_9a7e_31d5_c0ff_ee01);
        assert!(f(&bogus, &IClassFactory::IID, &mut p).is_err() && p.is_null());
        assert_eq!(f(&FileOpenDialog, &IClassFactory::IID, null_mut()), E_POINTER);
        CoUninitialize();
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
