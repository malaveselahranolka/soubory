//! `fm_dialog_shim.dll`: Option 2 (docs/ARCHITECTURE.md, M9 and M10).
//!
//! `scripts/install-shim.ps1` registers it in HKCU as the in-process server of
//! CLSID_FileOpenDialog. Outside the allowlist ([`route`]) every call hands back **comdlg32's own
//! class factory**, untouched. An allowlisted host (Claude.exe, plus `shim-allow.txt`) gets a
//! factory whose dialogs forward every call to the real one and, in `Show()`, open fm's picker
//! instead when the dialog asks nothing the picker cannot do (`wrap.rs`, `pick.rs`).
//!
//! `%LOCALAPPDATA%\fm\shim.log` gets one line per `DllGetClassObject` (process, integrity level,
//! AppContainer, interface, route, the modules on the call stack) and one per `Show()` (options,
//! decision and why, outcome). No paths, file names or dialog contents are logged.
//!
//! Rules for code inside other processes (§4.7): nothing runs in `DllMain`, every export and every
//! COM method is inside `catch_unwind`, state lives in one dialog object. The kill switch
//! `%LOCALAPPDATA%\fm\disable-shim` makes it hand out comdlg32's factory again and stops the log.
#![allow(clippy::missing_safety_doc)]

pub mod proto;
#[cfg(windows)]
pub mod pick;
#[cfg(windows)]
mod wrap;

/// Hosts that get the picker without being listed in `shim-allow.txt`: the M9 gate saw Claude's
/// dialog come through the override; nothing is known about other apps yet.
pub const DEFAULT_ALLOW: &[&str] = &["Claude.exe"];

/// Where a host's Open dialogs go.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    Pass(&'static str),
    Picker,
}

pub struct Host<'a> {
    /// Image file name, e.g. `notepad.exe`.
    pub exe: &'a str,
    pub elevated: bool,
    pub app_container: bool,
}

/// Only allowlisted, unelevated, non-sandboxed processes other than Explorer may get the picker.
pub fn route(h: &Host, allow: &[String]) -> Route {
    if h.elevated {
        Route::Pass("elevated")
    } else if h.app_container {
        // fm.exe could not be started from inside the sandbox.
        Route::Pass("appcontainer")
    } else if h.exe.eq_ignore_ascii_case("explorer.exe") {
        Route::Pass("explorer")
    } else if !DEFAULT_ALLOW.iter().copied().chain(allow.iter().map(String::as_str)).any(|a| a.trim().eq_ignore_ascii_case(h.exe)) {
        Route::Pass("not allowlisted")
    } else {
        Route::Picker
    }
}

/// One allowlisted image name per line; `#` starts a comment.
pub fn parse_allow(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(windows)]
pub(crate) mod imp {
    use super::*;
    use std::{ffi::c_void, io::Write, path::PathBuf};
    use windows::{
        Win32::{
            Foundation::*,
            Security::*,
            System::{
                Com::IClassFactory,
                LibraryLoader::*,
                Registry::*,
                SystemInformation::{GetLocalTime, GetSystemDirectoryW},
                Threading::*,
            },
            UI::Shell::{FileOpenDialog, FileSaveDialog},
        },
        core::*,
    };

    const LOG_CAP: u64 = 1 << 20;
    type GetClassObject =
        unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn RtlCaptureStackBackTrace(
            skip: u32,
            count: u32,
            frames: *mut *mut c_void,
            hash: *mut u32,
        ) -> u16;
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "system" fn DllGetClassObject(
        clsid: *const GUID,
        iid: *const GUID,
        out: *mut *mut c_void,
    ) -> HRESULT {
        if clsid.is_null() || iid.is_null() || out.is_null() {
            return E_POINTER;
        }
        unsafe { *out = std::ptr::null_mut() };
        let (clsid, iid) = unsafe { (&*clsid, &*iid) };
        // The kill switch: comdlg32's factory and nothing else, not even the log.
        if killed() {
            return std::panic::catch_unwind(|| unsafe { forward(clsid, iid, out) }).unwrap_or(E_UNEXPECTED);
        }
        let host = std::panic::catch_unwind(host_route).unwrap_or(Route::Pass("panic"));
        let wrap = host == Route::Picker && *clsid == FileOpenDialog && (*iid == IClassFactory::IID || *iid == IUnknown::IID);
        let hr = std::panic::catch_unwind(|| unsafe {
            if !wrap {
                return forward(clsid, iid, out);
            }
            let mut real = std::ptr::null_mut();
            let hr = forward(clsid, &IClassFactory::IID, &mut real);
            if hr.is_ok() {
                *out = crate::wrap::factory(IClassFactory::from_raw(real));
            }
            hr
        })
        .unwrap_or(E_UNEXPECTED);
        let _ = std::panic::catch_unwind(|| log(clsid, iid, hr, &host));
        hr
    }

    /// Never unloaded: cheaper than proving no caller still holds a pointer into us.
    #[unsafe(no_mangle)]
    pub extern "system" fn DllCanUnloadNow() -> HRESULT {
        S_FALSE
    }

    /// Calls the `DllGetClassObject` of the server HKLM registers for `clsid` (comdlg32). Never
    /// through COM: HKCR would hand the request straight back to us.
    unsafe fn forward(clsid: &GUID, iid: &GUID, out: *mut *mut c_void) -> HRESULT {
        let Some(path) = hklm_server(clsid).or_else(system_comdlg32) else {
            return CLASS_E_CLASSNOTAVAILABLE;
        };
        let w: Vec<u16> = path.as_os_str().encode_wide_z();
        let Ok(module) = (unsafe { LoadLibraryW(PCWSTR(w.as_ptr())) }) else {
            return CLASS_E_CLASSNOTAVAILABLE;
        };
        if Some(module) == own_module() {
            return CLASS_E_CLASSNOTAVAILABLE;
        }
        let Some(f) = (unsafe { GetProcAddress(module, s!("DllGetClassObject")) }) else {
            return CLASS_E_CLASSNOTAVAILABLE;
        };
        let f: GetClassObject = unsafe { std::mem::transmute(f) };
        unsafe { f(clsid, iid, out) }
    }

    fn hklm_server(clsid: &GUID) -> Option<PathBuf> {
        let s = reg_sz(HKEY_LOCAL_MACHINE, &format!("SOFTWARE\\Classes\\CLSID\\{{{clsid:?}}}\\InprocServer32"), "")?;
        let p = PathBuf::from(s.trim().trim_matches('"'));
        p.is_absolute().then_some(p)
    }

    /// A string value from the 64-bit view; `name` "" is the key's default value.
    pub(crate) fn reg_sz(root: HKEY, key: &str, name: &str) -> Option<String> {
        let key: Vec<u16> = key.encode_utf16().chain(Some(0)).collect();
        let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let mut buf = [0u16; 1040];
        let mut size = (buf.len() * 2) as u32;
        // RRF_RT_REG_SZ also accepts REG_EXPAND_SZ and expands it (%SystemRoot%).
        unsafe {
            RegGetValueW(
                root,
                PCWSTR(key.as_ptr()),
                if name.len() > 1 { PCWSTR(name.as_ptr()) } else { PCWSTR::null() },
                RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut size),
            )
        }
        .ok()
        .ok()?;
        Some(String::from_utf16_lossy(&buf[..(size as usize / 2).saturating_sub(1)]))
    }

    fn system_comdlg32() -> Option<PathBuf> {
        let mut buf = [0u16; 260];
        let n = unsafe { GetSystemDirectoryW(Some(&mut buf)) } as usize;
        (n > 0 && n < buf.len())
            .then(|| PathBuf::from(String::from_utf16_lossy(&buf[..n])).join("comdlg32.dll"))
    }

    fn own_module() -> Option<HMODULE> {
        let mut m = HMODULE::default();
        unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                    | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                PCWSTR(own_module as *const () as *const u16),
                &mut m,
            )
        }
        .ok()
        .map(|_| m)
    }

    fn fm_dir() -> Option<PathBuf> {
        std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("fm"))
    }

    pub(crate) fn killed() -> bool {
        fm_dir().is_some_and(|d| d.join("disable-shim").exists())
    }

    /// This process's route, from its image name, token and `shim-allow.txt`.
    fn host_route() -> Route {
        let exe = module_name(None).unwrap_or_default();
        let (elevated, app_container, _) = token_info();
        let allow = fm_dir()
            .and_then(|d| std::fs::read_to_string(d.join("shim-allow.txt")).ok())
            .map(|t| parse_allow(&t))
            .unwrap_or_default();
        route(&Host { exe: &exe, elevated, app_container }, &allow)
    }

    fn log(clsid: &GUID, iid: &GUID, hr: HRESULT, host: &Route) {
        let (_, app_container, integrity) = token_info();
        let r = match host {
            Route::Pass(why) => format!("pass ({why})"),
            Route::Picker => "picker".into(),
        };
        log_line(&format!(
            "{cls}\t{itf}\t{integrity}\tappcontainer={ac}\t{r}\thr={hr:#010x}\t{stack}",
            cls = guid_name(clsid),
            itf = guid_name(iid),
            ac = app_container as u8,
            hr = hr.0 as u32,
            stack = caller_modules(),
        ));
    }

    /// Appends `<time>\t<pid>\t<exe>\t<fields>` to shim.log, unless the kill switch is set or the
    /// log is over 1 MB.
    pub(crate) fn log_line(fields: &str) {
        let Some(dir) = fm_dir() else { return };
        if dir.join("disable-shim").exists() {
            return;
        }
        let path = dir.join("shim.log");
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_CAP) {
            return;
        }
        let t = unsafe { GetLocalTime() };
        let line = format!(
            "{y}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}\t{pid}\t{exe}\t{fields}\n",
            y = t.wYear,
            mo = t.wMonth,
            d = t.wDay,
            h = t.wHour,
            mi = t.wMinute,
            s = t.wSecond,
            pid = unsafe { GetCurrentProcessId() },
            exe = module_name(None).unwrap_or_default(),
        );
        let _ = std::fs::create_dir_all(&dir);
        // AppContainer hosts cannot write here; that is expected and silent.
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = f.write_all(line.as_bytes());
        }
    }

    pub(crate) fn guid_name(g: &GUID) -> String {
        match *g {
            x if x == FileOpenDialog => "FileOpenDialog".into(),
            x if x == FileSaveDialog => "FileSaveDialog".into(),
            x if x == IClassFactory::IID => "IClassFactory".into(),
            x if x == IUnknown::IID => "IUnknown".into(),
            x => format!("{{{x:?}}}"),
        }
    }

    /// (elevated, AppContainer, integrity level name).
    fn token_info() -> (bool, bool, &'static str) {
        unsafe {
            let mut token = HANDLE::default();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
                return (false, false, "?");
            }
            let mut len = 0u32;
            let mut elev = TOKEN_ELEVATION::default();
            let elevated = GetTokenInformation(
                token,
                TokenElevation,
                Some((&mut elev as *mut TOKEN_ELEVATION).cast()),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut len,
            )
            .is_ok()
                && elev.TokenIsElevated != 0;
            let mut ac = 0u32;
            let app_container = GetTokenInformation(
                token,
                TokenIsAppContainer,
                Some((&mut ac as *mut u32).cast()),
                4,
                &mut len,
            )
            .is_ok()
                && ac != 0;
            let mut buf = [0u64; 16]; // TOKEN_MANDATORY_LABEL + SID, 8-byte aligned
            let level = if GetTokenInformation(
                token,
                TokenIntegrityLevel,
                Some(buf.as_mut_ptr().cast()),
                size_of_val(&buf) as u32,
                &mut len,
            )
            .is_ok()
            {
                let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
                let sid = label.Label.Sid;
                let n = *GetSidSubAuthorityCount(sid);
                match if n > 0 {
                    *GetSidSubAuthority(sid, n as u32 - 1)
                } else {
                    0
                } {
                    0..0x1000 => "untrusted",
                    0x1000..0x2000 => "low",
                    0x2000..0x3000 => "medium",
                    0x3000..0x4000 => "high",
                    _ => "system",
                }
            } else {
                "?"
            };
            let _ = CloseHandle(token);
            (elevated, app_container, level)
        }
    }

    fn module_name(m: Option<HMODULE>) -> Option<String> {
        let mut buf = [0u16; 520];
        let n = unsafe { GetModuleFileNameW(m, &mut buf) } as usize;
        (n > 0).then(|| {
            let full = String::from_utf16_lossy(&buf[..n]);
            full.rsplit('\\').next().unwrap_or(&full).to_string()
        })
    }

    /// Modules on the call stack, innermost first, ours left out: `combase.dll<comdlg32.dll<notepad.exe`.
    fn caller_modules() -> String {
        let mut frames = [std::ptr::null_mut::<c_void>(); 32];
        let n = unsafe {
            RtlCaptureStackBackTrace(
                1,
                frames.len() as u32,
                frames.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        } as usize;
        let own = own_module();
        let mut out: Vec<String> = Vec::new();
        for &f in &frames[..n] {
            let mut m = HMODULE::default();
            let ok = unsafe {
                GetModuleHandleExW(
                    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                        | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                    PCWSTR(f as *const u16),
                    &mut m,
                )
            }
            .is_ok();
            if !ok || Some(m) == own {
                continue;
            }
            let name = module_name(Some(m)).unwrap_or_else(|| "?".into());
            if out.last() != Some(&name) {
                out.push(name);
            }
        }
        out.join("<")
    }

    trait EncodeWideZ {
        fn encode_wide_z(&self) -> Vec<u16>;
    }
    impl EncodeWideZ for std::ffi::OsStr {
        fn encode_wide_z(&self) -> Vec<u16> {
            use std::os::windows::ffi::OsStrExt;
            self.encode_wide().chain(Some(0)).collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(exe: &str) -> Host<'_> {
        Host {
            exe,
            elevated: false,
            app_container: false,
        }
    }

    #[test]
    fn routing() {
        let allow = parse_allow("Notepad.exe  # the gate's first target\n\n  mspaint.exe\n");
        assert_eq!(allow, ["Notepad.exe", "mspaint.exe"]);
        assert_eq!(route(&host("notepad.exe"), &allow), Route::Picker);
        assert_eq!(
            route(&host("chrome.exe"), &allow),
            Route::Pass("not allowlisted")
        );
        assert_eq!(
            route(&host("notepad.exe"), &[]),
            Route::Pass("not allowlisted")
        );
        // Claude.exe needs no line in shim-allow.txt.
        assert_eq!(route(&host("claude.exe"), &[]), Route::Picker);
        assert_eq!(
            route(&Host { elevated: true, ..host("Claude.exe") }, &[]),
            Route::Pass("elevated")
        );
        let explorer = vec!["explorer.exe".to_string()];
        assert_eq!(
            route(&host("Explorer.EXE"), &explorer),
            Route::Pass("explorer")
        );
        assert_eq!(
            route(
                &Host {
                    elevated: true,
                    ..host("notepad.exe")
                },
                &allow
            ),
            Route::Pass("elevated")
        );
        assert_eq!(
            route(
                &Host {
                    app_container: true,
                    ..host("notepad.exe")
                },
                &allow
            ),
            Route::Pass("appcontainer")
        );
    }
}
