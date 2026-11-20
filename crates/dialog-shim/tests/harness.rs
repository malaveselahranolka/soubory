//! Loads a temp copy of the built DLL (no registry) and drives its dialogs the way Electron does,
//! with a stand-in for fm: a door window of a test-only class that answers on the pipe. Covers
//! routing and logging, the recorded setters, single and multiple picks, cancel, a picker that
//! never connects and an fm that refuses (both: the system dialog), delegation on Advise, a
//! foreign QueryInterface and folder picking, and the kill switch.
//!
//! The cases that end in the system dialog show it on the desktop for a moment: a timer on this
//! thread posts IDCANCEL to it (a message to our own window; no input is synthesized). One test function,
//! because LOCALAPPDATA and the FM_SHIM_TEST_* variables are process-wide.
#![cfg(windows)]
use fm_dialog_shim::proto::{self, Msg, Request};
use std::{
    ffi::c_void,
    path::{Path, PathBuf},
    ptr::null_mut,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use windows::{
    Win32::{
        Foundation::*,
        Storage::FileSystem::*,
        System::{
            Com::*,
            DataExchange::COPYDATASTRUCT,
            LibraryLoader::*,
            Pipes::*,
        },
        UI::{
            Input::KeyboardAndMouse::IsWindowEnabled,
            Shell::{Common::COMDLG_FILTERSPEC, *},
            WindowsAndMessaging::*,
        },
    },
    core::*,
};
use windows_core::implement;

type GetClassObject = unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;

#[derive(Clone)]
enum Script {
    /// Take the request and answer with this after Ready.
    Answer(Msg),
    /// Take the request, never connect.
    Silent,
    /// Answer 0, as an older fm does.
    Refuse,
}

static SCRIPT: Mutex<Option<Script>> = Mutex::new(None);
/// Requests the door saw, and whether the owner was disabled while the fake picker ran.
static SEEN: Mutex<Vec<(Request, bool)>> = Mutex::new(Vec::new());
static DONE: AtomicBool = AtomicBool::new(false);

fn log_lines(fm: &Path) -> Vec<String> {
    std::fs::read_to_string(fm.join("shim.log")).unwrap_or_default().lines().map(str::to_string).collect()
}

/// The cdylib this test run built: uplifted into the profile dir, or (newer cargo build-dir
/// layout) in a sibling `build/fm-dialog-shim/<hash>/out`. The newest one wins.
fn built_dll(exe: &Path) -> Option<PathBuf> {
    let name = "fm_dialog_shim.dll";
    let mut found: Vec<PathBuf> = exe.ancestors().map(|a| a.join(name)).collect();
    if let Some(pkg) = exe.ancestors().find(|a| a.file_name().is_some_and(|n| n == "fm-dialog-shim")) {
        for d in std::fs::read_dir(pkg).into_iter().flatten().flatten() {
            found.push(d.path().join("out").join(name));
        }
    }
    found.into_iter().filter(|p| p.is_file()).max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
}

fn send(h: HANDLE, m: &Msg) {
    let b = proto::to_bytes(&m.encode());
    unsafe { WriteFile(h, Some(&b), None, None).unwrap() };
}

/// What fm does on the pipe: connect, read the request, Ready, then the answer.
fn fake_fm(pipe: String, answer: Msg) {
    unsafe {
        let h = CreateFileW(&HSTRING::from(pipe.as_str()), (GENERIC_READ | GENERIC_WRITE).0, FILE_SHARE_NONE, None, OPEN_EXISTING, FILE_FLAGS_AND_ATTRIBUTES(0), None).unwrap();
        SetNamedPipeHandleState(h, Some(&PIPE_READMODE_MESSAGE), None, None).unwrap();
        let mut buf = vec![0u8; 1 << 16];
        let mut n = 0u32;
        ReadFile(h, Some(&mut buf), Some(&mut n), None).unwrap();
        let req = Request::decode(&proto::from_bytes(&buf[..n as usize]).unwrap()).expect("the request is the pipe's first message");
        let owner_off = req.owner != 0 && !IsWindowEnabled(HWND(req.owner as *mut c_void)).as_bool();
        SEEN.lock().unwrap().push((req, owner_off));
        send(h, &Msg::Ready(0));
        send(h, &answer);
        let _ = CloseHandle(h);
    }
}

unsafe extern "system" fn door_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        if m != WM_COPYDATA {
            return DefWindowProcW(h, m, w, l);
        }
        let cds = &*(l.0 as *const COPYDATASTRUCT);
        assert_eq!(cds.dwData, proto::COPY_PICK);
        let words = std::slice::from_raw_parts(cds.lpData as *const u16, cds.cbData as usize / 2);
        let req = Request::decode(words).expect("a valid request");
        match SCRIPT.lock().unwrap().clone().unwrap() {
            Script::Refuse => LRESULT(0),
            Script::Silent => LRESULT(proto::ACK),
            Script::Answer(msg) => {
                std::thread::spawn(move || fake_fm(req.pipe, msg));
                LRESULT(proto::ACK)
            }
        }
    }
}

fn start_door(class: String) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || unsafe {
        let name = HSTRING::from(class.as_str());
        let hinst = GetModuleHandleW(None).unwrap();
        let wc = WNDCLASSEXW { cbSize: size_of::<WNDCLASSEXW>() as u32, lpfnWndProc: Some(door_proc), hInstance: hinst.into(), lpszClassName: PCWSTR(name.as_ptr()), ..Default::default() };
        RegisterClassExW(&wc);
        CreateWindowExW(WINDOW_EX_STYLE(0), PCWSTR(name.as_ptr()), None, WINDOW_STYLE(0), 0, 0, 0, 0, Some(HWND_MESSAGE), None, Some(hinst.into()), None).unwrap();
        tx.send(()).unwrap();
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
    });
    rx.recv().unwrap();
}

#[implement(IFileDialogEvents)]
struct Events;

impl IFileDialogEvents_Impl for Events_Impl {
    fn OnFileOk(&self, _: Ref<IFileDialog>) -> Result<()> {
        Ok(())
    }
    fn OnFolderChanging(&self, _: Ref<IFileDialog>, _: Ref<IShellItem>) -> Result<()> {
        Ok(())
    }
    fn OnFolderChange(&self, _: Ref<IFileDialog>) -> Result<()> {
        Ok(())
    }
    fn OnSelectionChange(&self, _: Ref<IFileDialog>) -> Result<()> {
        Ok(())
    }
    fn OnShareViolation(&self, _: Ref<IFileDialog>, _: Ref<IShellItem>) -> Result<FDE_SHAREVIOLATION_RESPONSE> {
        Ok(FDESVR_DEFAULT)
    }
    fn OnTypeChange(&self, _: Ref<IFileDialog>) -> Result<()> {
        Ok(())
    }
    fn OnOverwrite(&self, _: Ref<IFileDialog>, _: Ref<IShellItem>) -> Result<FDE_OVERWRITE_RESPONSE> {
        Ok(FDEOR_DEFAULT)
    }
}

/// Cancels the system dialog once it is up: IDCANCEL posted to this thread's dialog window
/// (IFileDialog::Close does nothing from outside the dialog's own callbacks).
unsafe extern "system" fn close_tick(_: HWND, _: u32, _: usize, _: u32) {
    unsafe extern "system" fn each(h: HWND, _: LPARAM) -> BOOL {
        let mut class = [0u16; 16];
        let n = unsafe { GetClassNameW(h, &mut class) } as usize;
        if String::from_utf16_lossy(&class[..n]) == "#32770" && unsafe { IsWindowVisible(h) }.as_bool() {
            let _ = unsafe { PostMessageW(Some(h), WM_COMMAND, WPARAM(IDCANCEL.0 as usize), LPARAM(0)) };
        }
        TRUE
    }
    unsafe {
        let _ = EnumThreadWindows(windows::Win32::System::Threading::GetCurrentThreadId(), Some(each), LPARAM(0));
    }
}

fn path_of(item: &IShellItem) -> String {
    unsafe {
        let p = item.GetDisplayName(SIGDN_FILESYSPATH).unwrap();
        let s = p.to_string().unwrap();
        CoTaskMemFree(Some(p.0 as *const c_void));
        s
    }
}

/// Show() as the host calls it; the system dialog, if it comes, is closed by the timer.
fn show(d: &IFileOpenDialog, owner: HWND) -> Result<()> {
    unsafe {
        let t = SetTimer(None, 0, 250, Some(close_tick));
        let r = d.Show(Some(owner));
        let _ = KillTimer(None, t);
        r
    }
}

fn last_show(fm: &Path) -> String {
    log_lines(fm).into_iter().rev().find(|l| l.contains("\tShow\t")).unwrap_or_default()
}

#[test]
fn picker_through_the_wrapper() {
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(90));
        if !DONE.load(Ordering::SeqCst) {
            let log = std::env::temp_dir().join(format!("fm-shim-test-{}", std::process::id())).join("fm").join("shim.log");
            eprintln!("watchdog: a Show() never returned; log:\n{}", std::fs::read_to_string(log).unwrap_or_default());
            std::process::exit(101);
        }
    });
    let exe = std::env::current_exe().unwrap();
    let built = built_dll(&exe).expect("the cdylib next to this test's build output");
    let tmp = std::env::temp_dir().join(format!("fm-shim-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let fm = tmp.join("fm");
    let files = tmp.join("soubory ž");
    for d in [&tmp.join("shim"), &fm, &files] {
        std::fs::create_dir_all(d).unwrap();
    }
    for f in ["a.png", "b.jpg", "c.txt"] {
        std::fs::write(files.join(f), "x").unwrap();
    }
    let (a, b) = (files.join("a.png").to_string_lossy().into_owned(), files.join("b.jpg").to_string_lossy().into_owned());
    let dll = tmp.join("shim").join("fm_dialog_shim.dll");
    std::fs::copy(&built, &dll).unwrap();
    let me = exe.file_name().unwrap().to_string_lossy().into_owned();
    std::fs::write(fm.join("shim-allow.txt"), format!("{me}\n")).unwrap();
    let class = format!("FmDoorTest-{}", std::process::id());
    // SAFETY: the only test in this binary; the shim reads these on each call.
    unsafe {
        std::env::set_var("LOCALAPPDATA", &tmp);
        std::env::set_var("FM_SHIM_TEST_DOOR", &class);
        std::env::set_var("FM_SHIM_TEST_EXE", tmp.join("missing").join("fm.exe"));
        std::env::set_var("FM_SHIM_TEST_CONNECT_MS", "800");
    }
    start_door(class);
    let set = |s: Script| *SCRIPT.lock().unwrap() = Some(s);

    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
        let m = LoadLibraryW(&HSTRING::from(dll.as_os_str())).unwrap();
        let f: GetClassObject = std::mem::transmute(GetProcAddress(m, s!("DllGetClassObject")).unwrap());
        let factory = || {
            let mut p = null_mut();
            f(&FileOpenDialog, &IClassFactory::IID, &mut p).ok().unwrap();
            IClassFactory::from_raw(p)
        };
        // An owner window of this thread, to see it disabled during the pick and enabled after.
        let owner = CreateWindowExW(WINDOW_EX_STYLE(0), w!("STATIC"), w!("owner"), WS_OVERLAPPED, 0, 0, 100, 100, None, None, None, None).unwrap();
        let folder: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(files.as_os_str()), None).unwrap();
        // Electron's sequence.
        let setters = |d: &IFileOpenDialog, opts: FILEOPENDIALOGOPTIONS| {
            d.SetOptions(opts).unwrap();
            d.SetFileName(w!("")).unwrap();
            d.SetTitle(w!("Vyberte soubory")).unwrap();
            d.SetOkButtonLabel(w!("Nahrát")).unwrap();
            let specs = [
                COMDLG_FILTERSPEC { pszName: w!("Obrázky"), pszSpec: w!("*.png;*.jpg") },
                COMDLG_FILTERSPEC { pszName: w!("Vše"), pszSpec: w!("*.*") },
            ];
            // A folder picker takes no file types (E_UNEXPECTED), so Electron gives none.
            if opts.0 & FOS_PICKFOLDERS.0 == 0 {
                d.SetFileTypes(&specs).unwrap();
                d.SetFileTypeIndex(1).unwrap();
            }
            d.SetFolder(&folder).unwrap();
        };
        let electron = |opts: FILEOPENDIALOGOPTIONS| -> IFileOpenDialog {
            let d: IFileOpenDialog = factory().CreateInstance(None).unwrap();
            setters(&d, opts);
            d
        };
        let base = FOS_FORCEFILESYSTEM | FOS_FILEMUSTEXIST;

        // Routing: one DllGetClassObject line, our factory.
        let _ = factory();
        let l = log_lines(&fm).pop().unwrap();
        assert!(l.contains(&format!("\t{me}\tFileOpenDialog\tIClassFactory\t")) && l.contains("\tpicker\t") && l.contains("hr=0x00000000"), "{l}");
        assert!(l.contains("harness"), "the caller shows on the stack: {l}");

        // 0. Through COM itself, as a host's CoCreateInstance gets it (whatever interfaces COM asks
        // the factory and the dialog for): our factory registered in-process under a throwaway class.
        let probe = GUID::from_u128(0x5f1d_0c2a_7e3b_4a19_8c6d_2b9e_0f41_a7d3);
        let cookie = CoRegisterClassObject(&probe, &factory().cast::<IUnknown>().unwrap(), CLSCTX_INPROC_SERVER, REGCLS_MULTIPLEUSE).unwrap();
        set(Script::Answer(Msg::Picked(vec![a.clone()])));
        let d: IFileOpenDialog = CoCreateInstance(&probe, None, CLSCTX_INPROC_SERVER).unwrap();
        setters(&d, base);
        show(&d, owner).unwrap();
        assert!(last_show(&fm).ends_with("\tpicker (resident)\tpicked 1"), "{}", last_show(&fm));
        assert_eq!(path_of(&d.GetResult().unwrap()), a);
        CoRevokeClassObject(cookie).unwrap();
        SEEN.lock().unwrap().clear();

        // 1. Single pick: the setters reach fm, GetResult(s) and GetFolder answer from the pick.
        set(Script::Answer(Msg::Picked(vec![a.clone()])));
        let d = electron(base);
        // A probe for nothing the real dialog has, and COM's marshaling probe, give nothing away.
        assert!(d.cast::<IFileSaveDialog>().is_err());
        let mut p = null_mut();
        assert_eq!(d.query(&GUID::from_u128(0x00000003_0000_0000_c000_000000000046), &mut p), E_NOINTERFACE);
        show(&d, owner).unwrap();
        assert!(last_show(&fm).ends_with("\tpicker (resident)\tpicked 1"), "{}", last_show(&fm));
        let (req, owner_off) = SEEN.lock().unwrap().pop().unwrap();
        assert!(owner_off, "owner disabled while the picker is up");
        assert!(IsWindowEnabled(owner).as_bool(), "owner enabled again");
        assert_eq!(
            (req.title.as_str(), req.ok.as_str(), req.folder.as_str(), req.name.as_str(), req.index, req.multi, req.owner),
            ("Vyberte soubory", "Nahrát", files.to_str().unwrap(), "", 1, false, owner.0 as isize)
        );
        assert_eq!(req.filters, [("Obrázky".to_string(), "*.png;*.jpg".to_string()), ("Vše".into(), "*.*".into())]);
        assert!(proto::valid_pipe(&req.pipe), "{}", req.pipe);
        assert_eq!(path_of(&d.GetResult().unwrap()), a);
        let all = d.GetResults().unwrap();
        assert_eq!(all.GetCount().unwrap(), 1);
        assert_eq!(path_of(&all.GetItemAt(0).unwrap()), a);
        assert_eq!(path_of(&d.GetFolder().unwrap()), files.to_string_lossy());

        // 2. Multiselect: every item, in order.
        set(Script::Answer(Msg::Picked(vec![a.clone(), b.clone()])));
        let d = electron(base | FOS_ALLOWMULTISELECT);
        show(&d, owner).unwrap();
        assert!(SEEN.lock().unwrap().pop().unwrap().0.multi);
        let all = d.GetResults().unwrap();
        assert_eq!((0..all.GetCount().unwrap()).map(|i| path_of(&all.GetItemAt(i).unwrap())).collect::<Vec<_>>(), [a.clone(), b.clone()]);
        assert!(last_show(&fm).ends_with("\tpicker (resident)\tpicked 2"), "{}", last_show(&fm));
        // An answer longer than the pipe's buffer arrives in pieces.
        let many = vec![a.clone(); 1000];
        set(Script::Answer(Msg::Picked(many.clone())));
        let d = electron(base | FOS_ALLOWMULTISELECT);
        show(&d, owner).unwrap();
        assert!(last_show(&fm).ends_with("\tpicker (resident)\tpicked 1000"), "{}", last_show(&fm));
        assert_eq!(d.GetResults().unwrap().GetCount().unwrap(), 1000);
        // Two files for a single-select dialog are not taken: the system dialog comes instead.
        set(Script::Answer(Msg::Picked(vec![a.clone(), b.clone()])));
        let d = electron(base);
        assert_eq!(show(&d, owner).unwrap_err().code(), ERROR_CANCELLED.to_hresult());
        assert!(last_show(&fm).ends_with("\tfallback: resident: result not usable\tsystem: cancelled"), "{}", last_show(&fm));

        // 3. Cancel in the picker.
        set(Script::Answer(Msg::Cancelled));
        let d = electron(base);
        assert_eq!(show(&d, owner).unwrap_err().code(), ERROR_CANCELLED.to_hresult());
        assert!(last_show(&fm).ends_with("\tpicker (resident)\tcancelled"), "{}", last_show(&fm));
        assert!(d.GetResult().is_err(), "nothing picked");
        assert!(IsWindowEnabled(owner).as_bool());

        // 4. fm takes the request but never shows up: the system dialog after the connect timeout.
        set(Script::Silent);
        let d = electron(base);
        assert_eq!(show(&d, owner).unwrap_err().code(), ERROR_CANCELLED.to_hresult());
        assert!(last_show(&fm).ends_with("\tfallback: fm never connected\tsystem: cancelled"), "{}", last_show(&fm));
        assert!(IsWindowEnabled(owner).as_bool());

        // 5. An older fm refuses and no fm.exe can be started.
        set(Script::Refuse);
        let d = electron(base);
        assert!(show(&d, owner).is_err());
        assert!(last_show(&fm).ends_with("\tfallback: fm.exe not found\tsystem: cancelled"), "{}", last_show(&fm));
        // An fm.exe without the --pick marker (the user's installed beta) is never started.
        std::env::set_var("FM_SHIM_TEST_EXE", &exe);
        let d = electron(base);
        assert!(show(&d, owner).is_err());
        assert!(last_show(&fm).ends_with("\tfallback: fm.exe too old for --pick\tsystem: cancelled"), "{}", last_show(&fm));

        // 6. Delegation: the picker never shows for what only the real dialog can do.
        SEEN.lock().unwrap().clear();
        set(Script::Answer(Msg::Picked(vec![a.clone()])));
        let d = electron(base);
        let events: IFileDialogEvents = Events.into();
        d.Advise(&events).unwrap();
        assert!(show(&d, owner).is_err());
        assert!(last_show(&fm).ends_with("\tdelegate: advise\tsystem: cancelled"), "{}", last_show(&fm));
        let d = electron(base);
        let _custom: IFileDialogCustomize = d.cast().unwrap();
        assert!(show(&d, owner).is_err());
        assert!(last_show(&fm).ends_with("\tdelegate: customize\tsystem: cancelled"), "{}", last_show(&fm));
        let d = electron(FOS_PICKFOLDERS);
        assert!(show(&d, owner).is_err());
        assert!(last_show(&fm).ends_with("\tdelegate: pick folders\tsystem: cancelled"), "{}", last_show(&fm));
        // What the picker cannot honour: a name to create, a shortcut picked as itself.
        for (o, why) in [(FOS_CREATEPROMPT, "create prompt"), (FOS_NODEREFERENCELINKS, "shortcuts as files")] {
            assert!(show(&electron(o), owner).is_err());
            assert!(last_show(&fm).ends_with(&format!("\tdelegate: {why}\tsystem: cancelled")), "{}", last_show(&fm));
        }
        assert!(SEEN.lock().unwrap().is_empty(), "fm was never asked");

        // 7. Kill switch: comdlg32's factory again, nothing logged.
        let before = log_lines(&fm).len();
        std::fs::write(fm.join("disable-shim"), "").unwrap();
        let d: IFileOpenDialog = factory().CreateInstance(None).unwrap();
        assert!(d.GetOptions().unwrap().0 & FOS_FILEMUSTEXIST.0 != 0, "a real Open dialog");
        assert_eq!(log_lines(&fm).len(), before, "kill switch: still served, nothing logged");

        // A class comdlg32 does not serve fails cleanly; bad pointers are refused.
        let mut p = null_mut();
        let bogus = GUID::from_u128(0x0f3e_8a54_6b2d_4c11_9a7e_31d5_c0ff_ee01);
        assert!(f(&bogus, &IClassFactory::IID, &mut p).is_err() && p.is_null());
        assert_eq!(f(&FileOpenDialog, &IClassFactory::IID, null_mut()), E_POINTER);
        // No path or file name ever reaches the log.
        let log = log_lines(&fm).join("\n");
        assert!(!log.contains("soubory") && !log.contains("a.png") && !log.contains("Vyberte"), "{log}");
        let _ = DestroyWindow(owner);
        CoUninitialize();
    }
    DONE.store(true, Ordering::SeqCst);
    let _ = std::fs::remove_dir_all(&tmp);
}
