//! Lists open file dialogs and reports their current folder two ways:
//! out-of-process (address bar text + known-folder lookup) and in-process (hook DLL).
use std::{
    cell::RefCell,
    ffi::c_void,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::*,
        System::{Com::*, DataExchange::*, LibraryLoader::*, SystemInformation::*, Threading::*},
        UI::{Shell::*, WindowsAndMessaging::*},
    },
    core::*,
};

const TAG: usize = 0x4657_4450;

thread_local! { static REPLY: RefCell<Option<String>> = const { RefCell::new(None) }; }

fn class_of(h: HWND) -> String {
    let mut b = [0u16; 256];
    let n = unsafe { GetClassNameW(h, &mut b) };
    String::from_utf16_lossy(&b[..n.max(0) as usize])
}

fn text_of(h: HWND) -> String {
    let mut b = [0u16; 1024];
    let n = unsafe { GetWindowTextW(h, &mut b) };
    String::from_utf16_lossy(&b[..n.max(0) as usize])
}

fn descendants(parent: HWND) -> Vec<HWND> {
    unsafe extern "system" fn cb(h: HWND, l: LPARAM) -> BOOL {
        unsafe { (*(l.0 as *mut Vec<HWND>)).push(h) };
        BOOL(1)
    }
    let mut v = Vec::new();
    unsafe {
        let _ = EnumChildWindows(Some(parent), Some(cb), LPARAM(&mut v as *mut _ as isize));
    }
    v
}

fn file_dialogs() -> Vec<HWND> {
    unsafe extern "system" fn cb(h: HWND, l: LPARAM) -> BOOL {
        unsafe {
            if IsWindowVisible(h).as_bool()
                && class_of(h) == "#32770"
                && descendants(h).iter().any(|&c| class_of(c) == "SHELLDLL_DefView")
            {
                (*(l.0 as *mut Vec<HWND>)).push(h);
            }
        }
        BOOL(1)
    }
    let mut v = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut v as *mut _ as isize));
    }
    v
}

fn address_text(dlg: HWND) -> Option<String> {
    let kids = descendants(dlg);
    let bp = kids.iter().copied().find(|&h| class_of(h) == "Breadcrumb Parent")?;
    descendants(bp).into_iter().find(|&h| class_of(h) == "ToolbarWindow32").map(text_of)
}

fn looks_like_path(s: &str) -> bool {
    let b = s.as_bytes();
    (b.len() >= 2 && b[1] == b':') || s.starts_with("\\\\")
}

fn disp(item: &IShellItem, sigdn: SIGDN) -> Option<String> {
    unsafe {
        let p = item.GetDisplayName(sigdn).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const c_void));
        s
    }
}

/// Out-of-process: address bar shows a path, or only a display name for known folders.
fn resolve_oop(addr: &str) -> String {
    let name = addr.split_once(": ").map(|x| x.1).unwrap_or(addr);
    if looks_like_path(name) {
        return format!("path {name}");
    }
    unsafe {
        let Ok(kfm) = CoCreateInstance::<_, IKnownFolderManager>(&KnownFolderManager, None, CLSCTX_INPROC_SERVER) else {
            return "kfm failed".into();
        };
        let mut ids = std::ptr::null_mut();
        let mut n = 0u32;
        if kfm.GetFolderIds(&mut ids, &mut n).is_err() {
            return "GetFolderIds failed".into();
        }
        let mut hits = vec![];
        for id in std::slice::from_raw_parts(ids, n as usize) {
            if let Ok(item) = SHGetKnownFolderItem::<IShellItem>(id, KF_FLAG_DEFAULT, None) {
                if disp(&item, SIGDN_NORMALDISPLAY).as_deref() == Some(name) {
                    hits.extend(disp(&item, SIGDN_DESKTOPABSOLUTEPARSING));
                }
            }
        }
        CoTaskMemFree(Some(ids as *const c_void));
        hits.sort();
        hits.dedup();
        format!("known[{}] {}", hits.len(), hits.join(" | "))
    }
}

unsafe extern "system" fn sink_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        if m == WM_COPYDATA {
            let cds = &*(l.0 as *const COPYDATASTRUCT);
            if cds.dwData == TAG {
                let s = std::slice::from_raw_parts(cds.lpData as *const u16, cds.cbData as usize / 2);
                REPLY.with(|r| *r.borrow_mut() = Some(String::from_utf16_lossy(s)));
                return LRESULT(1);
            }
        }
        DefWindowProcW(h, m, w, l)
    }
}

fn make_sink() -> HWND {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        let wc = WNDCLASSW { lpfnWndProc: Some(sink_proc), hInstance: hinst.into(), lpszClassName: w!("FinderWinProbeSink"), ..Default::default() };
        RegisterClassW(&wc);
        CreateWindowExW(Default::default(), w!("FinderWinProbeSink"), w!(""), Default::default(), 0, 0, 0, 0, Some(HWND_MESSAGE), None, Some(hinst.into()), None).unwrap()
    }
}

fn proc_info(pid: u32) -> (String, &'static str) {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return ("?".into(), "?") };
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let exe = if QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok() {
            String::from_utf16_lossy(&buf[..len as usize]).rsplit('\\').next().unwrap_or("").to_string()
        } else {
            "?".into()
        };
        let mut pm = IMAGE_FILE_MACHINE::default();
        let _ = IsWow64Process2(h, &mut pm, None);
        let _ = CloseHandle(h);
        (exe, if pm == IMAGE_FILE_MACHINE_UNKNOWN { "64" } else { "32" })
    }
}

fn inproc(dlg: HWND, sink: HWND, hook: HOOKPROC, hmod: HMODULE, qmsg: u32, lp: isize) -> String {
    unsafe {
        let tid = GetWindowThreadProcessId(dlg, None);
        let hh = match SetWindowsHookExW(WH_GETMESSAGE, hook, Some(HINSTANCE(hmod.0)), tid) {
            Ok(h) => h,
            Err(e) => return format!("hook failed: {e}"),
        };
        REPLY.with(|r| *r.borrow_mut() = None);
        let start = Instant::now();
        let mut out = "timeout".to_string();
        if let Err(e) = PostMessageW(Some(dlg), qmsg, WPARAM(sink.0 as usize), LPARAM(lp)) {
            out = format!("post failed: {e}");
        } else {
            while start.elapsed() < Duration::from_secs(3) {
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                if let Some(s) = REPLY.with(|r| r.borrow_mut().take()) {
                    out = s;
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        let _ = UnhookWindowsHookEx(hh);
        format!("{out} ({} ms)", start.elapsed().as_millis())
    }
}

/// "Nová složka", "Nová složka (2)", ... never overwrites (create_new / create_dir fail on existing).
fn create_unique(dir: &std::path::Path, folder: bool) -> std::io::Result<std::path::PathBuf> {
    let (stem, ext) = if folder { ("Nová složka", "") } else { ("Nový textový dokument", ".txt") };
    for i in 1..1000 {
        let name = if i == 1 { format!("{stem}{ext}") } else { format!("{stem} ({i}){ext}") };
        let p = dir.join(name);
        let r = if folder { std::fs::create_dir(&p) } else { std::fs::OpenOptions::new().write(true).create_new(true).open(&p).map(|_| ()) };
        match r {
            Ok(()) => return Ok(p),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no free name"))
}

fn main() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dll = std::env::current_exe().unwrap().with_file_name("dialog_hook.dll");
        let dll_w: Vec<u16> = dll.as_os_str().to_string_lossy().encode_utf16().chain([0]).collect();
        let hmod = LoadLibraryW(PCWSTR(dll_w.as_ptr())).expect("load dialog_hook.dll");
        let hook: HOOKPROC = std::mem::transmute(GetProcAddress(hmod, s!("GetMsgProc")));
        let qmsg = RegisterWindowMessageW(w!("FinderWin.DialogCmd"));
        let sink = make_sink();
        let dlgs = file_dialogs();
        if dlgs.is_empty() {
            println!("no file dialogs");
        }
        for dlg in dlgs {
            let mut pid = 0u32;
            GetWindowThreadProcessId(dlg, Some(&mut pid));
            let (exe, bits) = proc_info(pid);
            let kind = if descendants(dlg).iter().any(|&c| class_of(c) == "DUIViewWndClassName") { "item" } else { "legacy" };
            let addr = address_text(dlg).unwrap_or_default();
            let oop = if addr.is_empty() { "-".into() } else { resolve_oop(&addr) };
            let mine = if cfg!(target_pointer_width = "64") { "64" } else { "32" };
            if bits != mine {
                println!("[{kind}] {exe} ({bits}-bit) skipped by {mine}-bit probe");
                continue;
            }
            let inp = inproc(dlg, sink, hook, hmod, qmsg, 1);
            println!("[{kind}] {exe} ({bits}-bit) title='{}'\n  addr='{addr}'\n  oop: {oop}\n  inproc: {inp}", text_of(dlg));
            let Some(what) = std::env::args().nth(1) else { continue };
            let Some(dir) = inp.split("fs=").nth(1).and_then(|s| s.split(" | ").next()).filter(|s| !s.is_empty()) else {
                println!("  create: no filesystem folder");
                continue;
            };
            match create_unique(std::path::Path::new(dir), what == "new-folder") {
                Ok(p) => {
                    let name: Vec<u16> = p.file_name().unwrap().to_string_lossy().encode_utf16().chain([0]).collect();
                    let atom = GlobalAddAtomW(PCWSTR(name.as_ptr()));
                    let wpath: Vec<u16> = p.as_os_str().to_string_lossy().encode_utf16().chain([0]).collect();
                    let ev = if what == "new-folder" { SHCNE_MKDIR } else { SHCNE_CREATE };
                    SHChangeNotify(ev, SHCNF_PATHW | SHCNF_FLUSH, Some(wpath.as_ptr() as *const c_void), None);
                    let r = inproc(dlg, sink, hook, hmod, qmsg, 2 | ((atom as isize) << 8));
                    GlobalDeleteAtom(atom);
                    println!("  create: {} -> {r}", p.display());
                }
                Err(e) => println!("  create failed: {e}"),
            }
        }
    }
}
