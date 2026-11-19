//! Talks to `fm_dialog_hook.dll` inside the dialog's process.
//!
//! The hook is installed only for the dialog's UI thread and only for the few milliseconds of one
//! request, then removed. 32-bit dialogs need a 32-bit hook, which the 32-bit build of this exe
//! installs (`--inject`), because SetWindowsHookEx only loads DLLs of the caller's bitness.
use crate::util::*;
use fm_dialog_hook::{CMD_MESSAGE_NAME, HOOK_PROC_NAME, REPLY_TAG};
use std::{
    cell::RefCell,
    ffi::c_void,
    os::windows::process::CommandExt,
    path::PathBuf,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::*,
        System::{DataExchange::COPYDATASTRUCT, LibraryLoader::*, Threading::*},
        UI::WindowsAndMessaging::*,
    },
    core::*,
};

thread_local! { static REPLY: RefCell<Option<String>> = const { RefCell::new(None) }; }

/// Call from the sink window's WndProc. Returns true when the message was a hook reply.
pub fn on_copydata(lparam: LPARAM) -> bool {
    unsafe {
        let cds = &*(lparam.0 as *const COPYDATASTRUCT);
        if cds.dwData != REPLY_TAG || cds.lpData.is_null() {
            return false;
        }
        let text = std::slice::from_raw_parts(cds.lpData as *const u16, cds.cbData as usize / 2);
        REPLY.with(|r| *r.borrow_mut() = Some(String::from_utf16_lossy(text)));
        true
    }
}

/// Takes a reply that arrived outside a synchronous wait.
pub fn take_reply() -> Option<String> {
    REPLY.with(|r| r.borrow_mut().take())
}

pub struct Injector {
    module: HMODULE,
    proc_: HOOKPROC,
    message: u32,
    sink: HWND,
    x86_exe: Option<PathBuf>,
}

impl Injector {
    pub fn new(sink: HWND) -> std::result::Result<Self, String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let dll = exe.with_file_name("fm_dialog_hook.dll");
        let dll_w = wide(&dll.to_string_lossy());
        unsafe {
            let module = LoadLibraryW(PCWSTR(dll_w.as_ptr())).map_err(|e| format!("load {}: {e}", dll.display()))?;
            let name: Vec<u8> = HOOK_PROC_NAME.bytes().chain([0]).collect();
            let addr = GetProcAddress(module, PCSTR(name.as_ptr())).ok_or("hook proc missing")?;
            let proc_: HOOKPROC = std::mem::transmute(addr);
            let message = RegisterWindowMessageW(PCWSTR(wide(CMD_MESSAGE_NAME).as_ptr()));
            let x86_exe = [exe.with_file_name("x86").join("fm-dialogkit.exe")]
                .into_iter()
                .chain(std::env::var_os("FM_X86_DIR").map(|d| PathBuf::from(d).join("fm-dialogkit.exe")))
                .find(|p| p.is_file());
            Ok(Self { module, proc_, message, sink, x86_exe })
        }
    }

    /// Sends one command to the dialog and waits for the reply.
    pub fn call(&self, dlg: HWND, is32: bool, lparam: isize, timeout: Duration) -> std::result::Result<String, String> {
        if is32 != cfg!(target_pointer_width = "32") {
            return self.call_other_bitness(dlg, lparam, timeout);
        }
        unsafe {
            let tid = GetWindowThreadProcessId(dlg, None);
            if tid == 0 {
                return Err("dialog is gone".into());
            }
            let hook = SetWindowsHookExW(WH_GETMESSAGE, self.proc_, Some(HINSTANCE(self.module.0)), tid)
                .map_err(|e| format!("hook: {e}"))?;
            REPLY.with(|r| *r.borrow_mut() = None);
            let result = match PostMessageW(Some(dlg), self.message, WPARAM(self.sink.0 as usize), LPARAM(lparam)) {
                Err(e) => Err(format!("post: {e}")),
                Ok(()) => wait_reply(timeout),
            };
            let _ = UnhookWindowsHookEx(hook);
            result
        }
    }

    /// Starts a request without waiting; the reply arrives later as WM_COPYDATA on the sink window.
    /// Returns the hook to remove once the reply (or a timeout) arrived.
    pub fn start(&self, dlg: HWND, lparam: isize) -> std::result::Result<HHOOK, String> {
        unsafe {
            let tid = GetWindowThreadProcessId(dlg, None);
            if tid == 0 {
                return Err("dialog is gone".into());
            }
            let hook = SetWindowsHookExW(WH_GETMESSAGE, self.proc_, Some(HINSTANCE(self.module.0)), tid).map_err(|e| format!("hook: {e}"))?;
            if let Err(e) = PostMessageW(Some(dlg), self.message, WPARAM(self.sink.0 as usize), LPARAM(lparam)) {
                let _ = UnhookWindowsHookEx(hook);
                return Err(format!("post: {e}"));
            }
            Ok(hook)
        }
    }

    /// 32-bit variant of [`Injector::start`]: the x86 helper runs on a worker thread and its reply
    /// is posted to `notify` as `msg` with a `Box<String>` in lParam.
    pub fn start_other_bitness(&self, dlg: HWND, lparam: isize, timeout: Duration, notify: HWND, msg: u32) -> std::result::Result<(), String> {
        let exe = self.x86_exe.clone().ok_or("32-bit helper not installed")?;
        let (dlg_v, notify_v) = (dlg.0 as usize, notify.0 as usize);
        std::thread::spawn(move || {
            let out = std::process::Command::new(exe)
                .args(["--inject", &dlg_v.to_string(), &lparam.to_string(), &timeout.as_millis().to_string()])
                .creation_flags(CREATE_NO_WINDOW.0)
                .output();
            let text = match out {
                Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
                Err(e) => format!("ERR\tx86 helper: {e}"),
            };
            let boxed = Box::into_raw(Box::new(text));
            unsafe {
                if PostMessageW(Some(HWND(notify_v as *mut c_void)), msg, WPARAM(0), LPARAM(boxed as isize)).is_err() {
                    drop(Box::from_raw(boxed));
                }
            }
        });
        Ok(())
    }

    fn call_other_bitness(&self, dlg: HWND, lparam: isize, timeout: Duration) -> std::result::Result<String, String> {
        let exe = self.x86_exe.as_ref().ok_or("32-bit helper not installed")?;
        let out = std::process::Command::new(exe)
            .args(["--inject", &(dlg.0 as usize).to_string(), &lparam.to_string(), &timeout.as_millis().to_string()])
            .creation_flags(CREATE_NO_WINDOW.0)
            .output()
            .map_err(|e| format!("x86 helper: {e}"))?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if text.is_empty() { Err("x86 helper gave no reply".into()) } else { Ok(text) }
    }
}

/// Waits for the WM_COPYDATA reply. Only *sent* messages are dispatched meanwhile, so posted input
/// and timers of the helper cannot re-enter its state.
fn wait_reply(timeout: Duration) -> std::result::Result<String, String> {
    let start = Instant::now();
    loop {
        unsafe {
            let mut msg = MSG::default();
            let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE | PM_QS_SENDMESSAGE);
        }
        if let Some(r) = REPLY.with(|r| r.borrow_mut().take()) {
            return Ok(r);
        }
        let left = timeout.saturating_sub(start.elapsed());
        if left.is_zero() {
            return Err("timeout".into());
        }
        unsafe {
            MsgWaitForMultipleObjects(None, false, left.as_millis().min(50) as u32, QS_SENDMESSAGE);
        }
    }
}

unsafe extern "system" fn sink_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if m == WM_COPYDATA && on_copydata(l) {
        return LRESULT(1);
    }
    unsafe { DefWindowProcW(h, m, w, l) }
}

/// `--inject <hwnd> <lparam> [timeout-ms]`: used by the 64-bit helper to reach 32-bit dialogs.
pub fn cli(args: &[String]) -> i32 {
    let parse = |i: usize| args.get(i).and_then(|s| s.parse::<i64>().ok());
    let (Some(hwnd), Some(lparam)) = (parse(0), parse(1)) else {
        println!("ERR\tusage");
        return 2;
    };
    let timeout = Duration::from_millis(parse(2).unwrap_or(3000).clamp(100, 10_000) as u64);
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let class = wide("FmDialogKitInjectSink");
        let wc = WNDCLASSW { lpfnWndProc: Some(sink_proc), hInstance: hinst.into(), lpszClassName: PCWSTR(class.as_ptr()), ..Default::default() };
        RegisterClassW(&wc);
        let sink = CreateWindowExW(Default::default(), PCWSTR(class.as_ptr()), w!(""), Default::default(), 0, 0, 0, 0, Some(HWND_MESSAGE), None, Some(hinst.into()), None);
        let Ok(sink) = sink else {
            println!("ERR\tsink");
            return 1;
        };
        let reply = Injector::new(sink).and_then(|inj| inj.call(HWND(hwnd as usize as *mut c_void), cfg!(target_pointer_width = "32"), lparam as isize, timeout));
        match reply {
            Ok(r) => {
                println!("{r}");
                0
            }
            Err(e) => {
                println!("ERR\t{e}");
                1
            }
        }
    }
}
