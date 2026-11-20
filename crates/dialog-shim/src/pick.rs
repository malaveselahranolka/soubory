//! The shim's side of a pick (the protocol is in proto.rs): the pipe, reaching fm, and a wait that
//! keeps the calling thread's messages flowing. Runs on the thread that called `Show()`; the owner
//! stays disabled while fm's window is up, as under the system dialog.
//!
//! Public so fm's own tests can drive a real `fm.exe --pick` through exactly this code.
use crate::proto::{self, Msg, Request};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::*,
        Storage::FileSystem::*,
        System::{DataExchange::COPYDATASTRUCT, IO::*, LibraryLoader::*, Pipes::*, Registry::HKEY_CURRENT_USER, Threading::*},
        UI::{Input::KeyboardAndMouse::{EnableWindow, IsWindowEnabled}, WindowsAndMessaging::*},
    },
    core::*,
};

/// Where fm is reached.
pub struct Hosts {
    /// Window class of the resident fm's door (win.rs `DOOR_CLASS`).
    pub door: String,
    /// fm.exe for `--pick` when no door takes the request. `None`: the installed one, if `installed`.
    pub exe: Option<PathBuf>,
    pub installed: bool,
    /// Appended to that command line (tests: `--dev-keys …`).
    pub args: String,
    /// How long fm has to connect and show its window.
    pub connect: Duration,
}

impl Hosts {
    /// The real fm. `FM_SHIM_TEST_DOOR` (with `FM_SHIM_TEST_EXE`, `FM_SHIM_TEST_CONNECT_MS`) points
    /// the shim at a test's stand-in instead, and then never at the user's installed fm.
    pub fn from_env() -> Hosts {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let door = var("FM_SHIM_TEST_DOOR");
        Hosts {
            installed: door.is_none(),
            door: door.unwrap_or_else(|| "FmDoor".into()),
            exe: var("FM_SHIM_TEST_EXE").map(PathBuf::from),
            args: String::new(),
            connect: Duration::from_millis(var("FM_SHIM_TEST_CONNECT_MS").and_then(|v| v.parse().ok()).unwrap_or(10_000)),
        }
    }
}

/// Shows fm's picker for `req` and returns its answer, and whether the resident fm or a started
/// one served it. `Err` (why) when no picker came up, or it went away without answering: the
/// caller shows the system dialog then.
pub fn run(req: &mut Request, owner: HWND, hosts: &Hosts) -> std::result::Result<(&'static str, Msg), String> {
    let mut pipe = Pipe::create()?;
    req.pipe = pipe.name.clone();
    pipe.listen()?;
    let payload = req.encode();
    let (via, pid, child) = match offer(&hosts.door, &payload) {
        Some(pid) => ("resident", pid, None),
        None => {
            let exe = hosts
                .exe
                .clone()
                .or_else(|| hosts.installed.then(installed_fm).flatten())
                .ok_or("no resident fm and no fm.exe")?;
            if !exe.is_file() {
                return Err("fm.exe not found".into());
            }
            if !understands_pick(&exe) {
                return Err("fm.exe too old for --pick".into());
            }
            let (pid, h) = launch(&exe, &pipe.name, &hosts.args)?;
            ("child", pid, Some(h))
        }
    };
    let r = {
        let _owner = OwnerOff::new(owner);
        pipe.wait(pid, child, &payload, hosts.connect, owner)
    };
    if let Some(h) = child {
        unsafe {
            // A picker that did not answer must not turn up after the system dialog.
            if r.is_err() {
                let _ = TerminateProcess(h, 1);
            }
            let _ = CloseHandle(h);
        }
    }
    r.map(|m| (via, m))
}

/// Offers the request to a running fm; its process id when it took it ([`proto::ACK`]).
fn offer(door: &str, payload: &[u16]) -> Option<u32> {
    unsafe {
        let door = FindWindowExW(Some(HWND_MESSAGE), None, &HSTRING::from(door), None).ok()?;
        let mut pid = 0u32;
        GetWindowThreadProcessId(door, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        // We are the foreground app (the user just asked for the dialog); fm may take it over.
        let _ = AllowSetForegroundWindow(pid);
        let cds = COPYDATASTRUCT { dwData: proto::COPY_PICK, cbData: (payload.len() * 2) as u32, lpData: payload.as_ptr() as *mut _ };
        let mut res = 0usize;
        let ok = SendMessageTimeoutW(door, WM_COPYDATA, WPARAM(0), LPARAM(&cds as *const COPYDATASTRUCT as isize), SMTO_ABORTIFHUNG, 3000, Some(&mut res));
        (ok.0 != 0 && res as isize == proto::ACK).then_some(pid)
    }
}

/// The installed fm.exe: the sign-in Run value "Soubory" (`"…\fm.exe" --background`), else App Paths.
fn installed_fm() -> Option<PathBuf> {
    let run = crate::imp::reg_sz(HKEY_CURRENT_USER, r"Software\Microsoft\Windows\CurrentVersion\Run", "Soubory").map(|v| {
        let v = v.trim();
        match v.strip_prefix('"') {
            Some(rest) => rest.split('"').next().unwrap_or("").to_string(),
            None => v.split(' ').next().unwrap_or("").to_string(),
        }
    });
    let app_paths = || crate::imp::reg_sz(HKEY_CURRENT_USER, r"Software\Microsoft\Windows\CurrentVersion\App Paths\fm.exe", "").map(|v| v.trim().trim_matches('"').to_string());
    [run, app_paths()]
        .into_iter()
        .flatten()
        .map(PathBuf::from)
        .find(|p| p.is_absolute() && p.is_file() && p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("fm.exe")))
}

/// Whether `exe` carries [`proto::MARKER`]. Read as data: none of its code runs here.
fn understands_pick(exe: &Path) -> bool {
    unsafe {
        let Ok(m) = LoadLibraryExW(&HSTRING::from(exe.as_os_str()), None, LOAD_LIBRARY_AS_DATAFILE | LOAD_LIBRARY_AS_IMAGE_RESOURCE) else {
            return false;
        };
        const RT_RCDATA: PCWSTR = PCWSTR(10 as _);
        let r = FindResourceW(Some(m), PCWSTR(proto::MARKER_ID as usize as *const u16), RT_RCDATA);
        let ok = !r.is_invalid()
            && LoadResource(Some(m), r).is_ok_and(|g| {
                let p = LockResource(g) as *const u8;
                let n = SizeofResource(Some(m), r) as usize;
                !p.is_null() && std::slice::from_raw_parts(p, n) == proto::MARKER.as_bytes()
            });
        let _ = FreeLibrary(m);
        ok
    }
}

fn launch(exe: &Path, pipe: &str, extra: &str) -> std::result::Result<(u32, HANDLE), String> {
    let mut cmd: Vec<u16> = format!("\"{}\" --pick {pipe} {extra}", exe.display()).trim_end().encode_utf16().chain(Some(0)).collect();
    let si = STARTUPINFOW { cb: size_of::<STARTUPINFOW>() as u32, ..Default::default() };
    let mut pi = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessW(&HSTRING::from(exe.as_os_str()), Some(PWSTR(cmd.as_mut_ptr())), None, None, false, PROCESS_CREATION_FLAGS(0), None, None, &si, &mut pi)
            .map_err(|e| format!("fm.exe did not start ({:#010x})", e.code().0 as u32))?;
        let _ = CloseHandle(pi.hThread);
        let _ = AllowSetForegroundWindow(pi.dwProcessId);
    }
    Ok((pi.dwProcessId, pi.hProcess))
}

/// Disables the owner while fm's window is up, if it was enabled; on drop enables it again and
/// gives it the foreground (fm allowed that before it closed).
struct OwnerOff(HWND, bool);

impl OwnerOff {
    fn new(owner: HWND) -> OwnerOff {
        unsafe {
            let off = !owner.is_invalid() && IsWindow(Some(owner)).as_bool() && IsWindowEnabled(owner).as_bool();
            if off {
                let _ = EnableWindow(owner, false);
            }
            OwnerOff(owner, off)
        }
    }
}

impl Drop for OwnerOff {
    fn drop(&mut self) {
        unsafe {
            if self.1 {
                let _ = EnableWindow(self.0, true);
            }
            if !self.0.is_invalid() && IsWindow(Some(self.0)).as_bool() {
                let _ = SetForegroundWindow(self.0);
            }
        }
    }
}

/// The picker has no owner and ours is disabled: when the user brings the host forward (its
/// taskbar button, Alt+Tab), the picker comes back on top, restored if minimized. Only while the
/// host is the foreground app, which then may hand the foreground on.
fn refocus(picker: HWND, owner: HWND) {
    unsafe {
        let fg = GetForegroundWindow();
        if picker.is_invalid() || fg.is_invalid() || fg == picker || !IsWindow(Some(picker)).as_bool() {
            return;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(fg, Some(&mut pid));
        if fg == owner || pid == GetCurrentProcessId() {
            if IsIconic(picker).as_bool() {
                let _ = ShowWindow(picker, SW_RESTORE);
            }
            let _ = SetForegroundWindow(picker);
        }
    }
}

/// The longest reply taken from fm (about 30 000 long paths); a longer one is an fm bug, not a pick.
const MAX_MSG: usize = 8 << 20;

/// One instance, message mode, local clients only, overlapped on our side.
struct Pipe {
    name: String,
    h: HANDLE,
    ev: HANDLE,
    ov: Box<OVERLAPPED>,
    /// An overlapped operation is in flight on `ov` and `chunk`.
    pending: bool,
    chunk: Vec<u8>,
    msg: Vec<u8>,
}

enum Read {
    /// A whole message.
    Done(Vec<u8>),
    /// More of a long message follows.
    More,
    Closed,
    /// Longer than [`MAX_MSG`].
    TooLong,
}

fn is(e: &Error, w: WIN32_ERROR) -> bool {
    e.code() == w.to_hresult()
}

impl Pipe {
    fn create() -> std::result::Result<Pipe, String> {
        use std::hash::BuildHasher;
        let token = std::collections::hash_map::RandomState::new().hash_one(Instant::now());
        let name = format!("{}{}-{token:016x}", proto::PIPE_PREFIX, unsafe { GetCurrentProcessId() });
        unsafe {
            let h = CreateNamedPipeW(
                &HSTRING::from(name.as_str()),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                1 << 16,
                1 << 16,
                0,
                None,
            );
            if h.is_invalid() {
                return Err("no pipe".into());
            }
            let ev = match CreateEventW(None, true, false, None) {
                Ok(ev) => ev,
                Err(_) => {
                    let _ = CloseHandle(h);
                    return Err("no event".into());
                }
            };
            Ok(Pipe { name, h, ev, ov: Box::default(), pending: false, chunk: vec![0; 1 << 16], msg: Vec::new() })
        }
    }

    fn fresh(&mut self) -> *mut OVERLAPPED {
        unsafe {
            let _ = ResetEvent(self.ev);
        }
        *self.ov = OVERLAPPED { hEvent: self.ev, ..Default::default() };
        &mut *self.ov
    }

    /// Starts waiting for fm to connect; the event fires when it has.
    fn listen(&mut self) -> std::result::Result<(), String> {
        let ov = self.fresh();
        match unsafe { ConnectNamedPipe(self.h, Some(ov)) } {
            Err(e) if is(&e, ERROR_IO_PENDING) => self.pending = true,
            // Connected before we asked (or at once): nothing in flight, fire the event ourselves.
            Ok(()) => unsafe { SetEvent(self.ev).map_err(|_| "event")? },
            Err(e) if is(&e, ERROR_PIPE_CONNECTED) => unsafe { SetEvent(self.ev).map_err(|_| "event")? },
            Err(_) => return Err("pipe not listening".into()),
        }
        Ok(())
    }

    fn start_read(&mut self) -> bool {
        let ov = self.fresh();
        match unsafe { ReadFile(self.h, Some(&mut self.chunk[..]), None, Some(ov)) } {
            // Done at once or later: either way the event fires.
            Ok(()) => self.pending = true,
            Err(e) if is(&e, ERROR_IO_PENDING) || is(&e, ERROR_MORE_DATA) => self.pending = true,
            Err(_) => return false,
        }
        true
    }

    fn finish_read(&mut self) -> Read {
        let mut n = 0u32;
        let r = unsafe { GetOverlappedResult(self.h, &*self.ov, &mut n, false) };
        if matches!(&r, Err(e) if is(e, ERROR_IO_INCOMPLETE)) {
            return Read::More;
        }
        self.pending = false;
        if self.msg.len() + n as usize > MAX_MSG {
            return Read::TooLong;
        }
        match r {
            Ok(()) => {
                self.msg.extend_from_slice(&self.chunk[..n as usize]);
                Read::Done(std::mem::take(&mut self.msg))
            }
            Err(e) if is(&e, ERROR_MORE_DATA) => {
                self.msg.extend_from_slice(&self.chunk[..n as usize]);
                Read::More
            }
            Err(_) => Read::Closed,
        }
    }

    /// After the connection: only the process we asked may be on the other end; it gets the
    /// request as the pipe's first message.
    fn accept(&mut self, pid: u32, request: &[u16]) -> std::result::Result<(), String> {
        let mut n = 0u32;
        if self.pending {
            self.pending = false;
            unsafe { GetOverlappedResult(self.h, &*self.ov, &mut n, false) }.map_err(|_| "connection failed")?;
        }
        let mut client = 0u32;
        unsafe { GetNamedPipeClientProcessId(self.h, &mut client) }.map_err(|_| "client unknown")?;
        if client != pid {
            return Err("another process connected".into());
        }
        let bytes = proto::to_bytes(request);
        let ov = self.fresh();
        match unsafe { WriteFile(self.h, Some(&bytes), None, Some(ov)) } {
            Ok(()) => {}
            Err(e) if is(&e, ERROR_IO_PENDING) => {}
            Err(_) => return Err("request not sent".into()),
        }
        self.pending = true;
        if unsafe { WaitForSingleObject(self.ev, 3000) } != WAIT_OBJECT_0 {
            return Err("request not taken".into());
        }
        self.pending = false;
        unsafe { GetOverlappedResult(self.h, &*self.ov, &mut n, false) }.map_err(|_| "request not sent")?;
        Ok(())
    }

    fn wait(&mut self, pid: u32, child: Option<HANDLE>, request: &[u16], connect: Duration, owner: HWND) -> std::result::Result<Msg, String> {
        const REFOCUS: Duration = Duration::from_millis(250);
        let start = Instant::now();
        let mut connected = false;
        // The picker's window, once fm said it is up.
        let mut ready = None;
        let mut child = child;
        let mut checked = Instant::now();
        loop {
            let mut handles = vec![self.ev];
            handles.extend(child);
            let timeout = if let Some(picker) = ready {
                // Checked by the clock, not on a timeout alone: a busy host's messages would starve it.
                if checked.elapsed() >= REFOCUS {
                    refocus(picker, owner);
                    checked = Instant::now();
                }
                REFOCUS.saturating_sub(checked.elapsed()).as_millis() as u32
            } else {
                let left = connect.saturating_sub(start.elapsed());
                if left.is_zero() {
                    return Err(if connected { "no window from fm" } else { "fm never connected" }.into());
                }
                left.as_millis() as u32
            };
            let w = unsafe { MsgWaitForMultipleObjectsEx(Some(&handles), timeout, QS_ALLINPUT, MWMO_INPUTAVAILABLE) };
            match w.0 {
                0 if !connected => {
                    self.accept(pid, request)?;
                    connected = true;
                    if !self.start_read() {
                        return Err("fm closed the pipe".into());
                    }
                }
                0 => match self.finish_read() {
                    Read::More => {
                        if !self.pending && !self.start_read() {
                            return Err("fm closed the pipe".into());
                        }
                    }
                    Read::Closed => return Err(if ready.is_some() { "picker closed without an answer" } else { "fm closed before its window" }.into()),
                    Read::TooLong => return Err("reply too long".into()),
                    Read::Done(bytes) => {
                        match proto::from_bytes(&bytes).and_then(|w| Msg::decode(&w)) {
                            Some(Msg::Ready(h)) => ready = Some(HWND(h as *mut _)),
                            Some(m) => return Ok(m),
                            None => return Err("unreadable message".into()),
                        }
                        if !self.start_read() {
                            return Err(if ready.is_some() { "picker closed without an answer" } else { "fm closed before its window" }.into());
                        }
                    }
                },
                // The started fm ended. Before it connected that is the end; after, the pipe tells.
                1 if child.is_some() => {
                    if !connected {
                        return Err("fm ended before connecting".into());
                    }
                    child = None;
                }
                x if x == handles.len() as u32 => {
                    if !pump() {
                        // The host is quitting: no dialog, nothing picked.
                        return Ok(Msg::Cancelled);
                    }
                }
                x if x == WAIT_TIMEOUT.0 => {}
                _ => return Err("wait failed".into()),
            }
        }
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        unsafe {
            // The kernel still owns `ov` and `chunk` while an operation is in flight.
            if self.pending {
                let _ = CancelIoEx(self.h, Some(&*self.ov));
                let mut n = 0u32;
                let _ = GetOverlappedResult(self.h, &*self.ov, &mut n, true);
            }
            let _ = CloseHandle(self.h);
            let _ = CloseHandle(self.ev);
        }
    }
}

/// Dispatches what is queued for this thread; false on WM_QUIT (posted again for the host's loop).
fn pump() -> bool {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_QUIT {
                PostQuitMessage(msg.wParam.0 as i32);
                return false;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        true
    }
}
