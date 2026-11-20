//! An allowlisted host's Open dialogs: a class factory whose `IFileOpenDialog` forwards every call
//! to comdlg32's real dialog and records what fm's picker needs. `Show()` opens the picker
//! instead, unless the host asked for something only the real dialog does ("delegate"); after the
//! picker, `GetResult`, `GetResults` and `GetFolder` answer from its result.
//!
//! Written by hand rather than with `#[implement]`: a QueryInterface for anything else must hand
//! back the real dialog's interface, and every method must catch panics (one leaving an
//! `extern "system"` function would abort the host).
use crate::imp::{guid_name, killed, log_line};
use crate::proto::Msg;
use std::{
    ffi::c_void,
    panic::AssertUnwindSafe,
    path::Path,
    ptr::null_mut,
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicU32, Ordering},
    },
};
use windows::{
    Win32::{
        Foundation::*,
        System::Com::*,
        UI::Shell::{Common::*, *},
    },
    core::*,
};

fn guard(f: impl FnOnce() -> HRESULT) -> HRESULT {
    std::panic::catch_unwind(AssertUnwindSafe(f)).unwrap_or(E_UNEXPECTED)
}

// ---- class factory -------------------------------------------------------------------------------

#[repr(C)]
struct Factory {
    vtbl: *const IClassFactory_Vtbl,
    refs: AtomicU32,
    real: IClassFactory,
}

static FACTORY_VTBL: IClassFactory_Vtbl = IClassFactory_Vtbl {
    base__: IUnknown_Vtbl { QueryInterface: f_qi, AddRef: f_add, Release: f_release },
    CreateInstance: f_create,
    LockServer: f_lock,
};

/// Our factory around comdlg32's (one reference, handed to the caller).
pub fn factory(real: IClassFactory) -> *mut c_void {
    Box::into_raw(Box::new(Factory { vtbl: &FACTORY_VTBL, refs: AtomicU32::new(1), real })) as *mut c_void
}

unsafe extern "system" fn f_qi(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    guard(|| unsafe {
        if iid.is_null() || out.is_null() {
            return E_POINTER;
        }
        if *iid == IUnknown::IID || *iid == IClassFactory::IID {
            f_add(this);
            *out = this;
            return S_OK;
        }
        *out = null_mut();
        E_NOINTERFACE
    })
}

unsafe extern "system" fn f_add(this: *mut c_void) -> u32 {
    unsafe { (*(this as *const Factory)).refs.fetch_add(1, Ordering::SeqCst) + 1 }
}

unsafe extern "system" fn f_release(this: *mut c_void) -> u32 {
    std::panic::catch_unwind(|| unsafe {
        let n = (*(this as *const Factory)).refs.fetch_sub(1, Ordering::SeqCst) - 1;
        if n == 0 {
            drop(Box::from_raw(this as *mut Factory));
        }
        n
    })
    .unwrap_or(1)
}

unsafe extern "system" fn f_create(this: *mut c_void, outer: *mut c_void, riid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    guard(|| unsafe {
        if riid.is_null() || out.is_null() {
            return E_POINTER;
        }
        *out = null_mut();
        let f = &*(this as *const Factory);
        let create = f.real.vtable().CreateInstance;
        // Aggregation: the outer object would own identity and QueryInterface; the real one only.
        if !outer.is_null() {
            return create(f.real.as_raw(), outer, riid, out);
        }
        let mut p = null_mut();
        let hr = create(f.real.as_raw(), null_mut(), &IFileOpenDialog::IID, &mut p);
        if hr.is_err() {
            return hr;
        }
        let d = Box::into_raw(Box::new(Dialog {
            vtbl: &DIALOG_VTBL,
            refs: AtomicU32::new(1),
            real: IFileOpenDialog::from_raw(p),
            st: Mutex::new(State::default()),
        })) as *mut c_void;
        let hr = d_qi(d, riid, out);
        d_release(d);
        hr
    })
}

unsafe extern "system" fn f_lock(this: *mut c_void, lock: BOOL) -> HRESULT {
    guard(|| unsafe {
        let f = &*(this as *const Factory);
        (f.real.vtable().LockServer)(f.real.as_raw(), lock)
    })
}

// ---- the dialog ----------------------------------------------------------------------------------

#[repr(C)]
struct Dialog {
    vtbl: *const IFileOpenDialog_Vtbl,
    refs: AtomicU32,
    real: IFileOpenDialog,
    st: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Why this dialog is the system's: a QueryInterface or call the picker cannot serve.
    delegate: Option<String>,
    /// What the setters asked for (pipe, owner and options are filled in by Show).
    req: crate::proto::Request,
    /// Our picker's result, for GetResult(s) and GetFolder.
    picked: Option<Vec<String>>,
}

impl Dialog {
    fn st(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn vt(&self) -> &IFileOpenDialog_Vtbl {
        self.real.vtable()
    }
    fn raw(&self) -> *mut c_void {
        self.real.as_raw()
    }
    fn picked(&self) -> Option<Vec<String>> {
        self.st().picked.clone()
    }
}

unsafe fn me<'a>(this: *mut c_void) -> &'a Dialog {
    unsafe { &*(this as *const Dialog) }
}

static DIALOG_VTBL: IFileOpenDialog_Vtbl = IFileOpenDialog_Vtbl {
    base__: IFileDialog_Vtbl {
        base__: IModalWindow_Vtbl {
            base__: IUnknown_Vtbl { QueryInterface: d_qi, AddRef: d_add, Release: d_release },
            Show: show,
        },
        SetFileTypes: set_file_types,
        SetFileTypeIndex: set_file_type_index,
        GetFileTypeIndex: get_file_type_index,
        Advise: advise,
        Unadvise: unadvise,
        SetOptions: set_options,
        GetOptions: get_options,
        SetDefaultFolder: set_default_folder,
        SetFolder: set_folder,
        GetFolder: get_folder,
        GetCurrentSelection: get_current_selection,
        SetFileName: set_file_name,
        GetFileName: get_file_name,
        SetTitle: set_title,
        SetOkButtonLabel: set_ok_button_label,
        SetFileNameLabel: set_file_name_label,
        GetResult: get_result,
        AddPlace: add_place,
        SetDefaultExtension: set_default_extension,
        Close: close,
        SetClientGuid: set_client_guid,
        ClearClientData: clear_client_data,
        SetFilter: set_filter,
    },
    GetResults: get_results,
    GetSelectedItems: get_selected_items,
};

/// Interfaces of the wrapper itself.
const OWN: [GUID; 4] = [IUnknown::IID, IModalWindow::IID, IFileDialog::IID, IFileOpenDialog::IID];

/// COM's own probes (marshaling, agility): answered "no" without giving the dialog away, so COM
/// marshals the wrapper with the standard proxy like any apartment-bound object.
const INFRA: [GUID; 8] = [
    GUID::from_u128(0x00000003_0000_0000_c000_000000000046), // IMarshal
    GUID::from_u128(0x000001cf_0000_0000_c000_000000000046), // IMarshal2
    GUID::from_u128(0xecc8691b_c1db_4dc0_855e_65f6c551af49), // INoMarshal
    GUID::from_u128(0x94ea2b94_e9cc_49e0_c0ff_ee64ca8f5b90), // IAgileObject
    GUID::from_u128(0x00000018_0000_0000_c000_000000000046), // IStdMarshalInfo
    GUID::from_u128(0x00000019_0000_0000_c000_000000000046), // IExternalConnection
    GUID::from_u128(0x1c733a30_2a1c_11ce_ade5_00aa0044773d), // ICallFactory
    GUID::from_u128(0x00000040_0000_0000_c000_000000000046), // IFastRundown
];

fn delegate_name(iid: &GUID) -> String {
    match *iid {
        x if x == IFileDialogCustomize::IID => "customize".into(),
        x if x == IFileDialog2::IID => "IFileDialog2".into(),
        x if x == GUID::from_u128(0xfc4801a3_2ba9_11cf_a229_00aa003d7352) => "IObjectWithSite".into(),
        x => format!("interface {}", guid_name(&x)),
    }
}

unsafe extern "system" fn d_qi(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    guard(|| unsafe {
        if iid.is_null() || out.is_null() {
            return E_POINTER;
        }
        *out = null_mut();
        let iid = *iid;
        if OWN.contains(&iid) {
            d_add(this);
            *out = this;
            return S_OK;
        }
        if INFRA.contains(&iid) {
            return E_NOINTERFACE;
        }
        // Something the picker cannot serve: the real dialog's interface, and its Show() later.
        // Only when the real dialog has it, so a probe for nothing changes nothing.
        let d = me(this);
        let hr = d.real.query(&iid, out);
        if hr.is_ok() {
            d.st().delegate.get_or_insert_with(|| delegate_name(&iid));
        }
        hr
    })
}

unsafe extern "system" fn d_add(this: *mut c_void) -> u32 {
    unsafe { me(this).refs.fetch_add(1, Ordering::SeqCst) + 1 }
}

unsafe extern "system" fn d_release(this: *mut c_void) -> u32 {
    std::panic::catch_unwind(|| unsafe {
        let n = me(this).refs.fetch_sub(1, Ordering::SeqCst) - 1;
        if n == 0 {
            drop(Box::from_raw(this as *mut Dialog));
        }
        n
    })
    .unwrap_or(1)
}

/// A method that only passes the call on to the same slot of the real dialog's vtable.
macro_rules! pass {
    ($name:ident, [$($f:ident)+], ($($a:ident: $t:ty),*)) => {
        unsafe extern "system" fn $name(this: *mut c_void $(, $a: $t)*) -> HRESULT {
            guard(|| unsafe {
                let d = me(this);
                (d.vt()$(.$f)+)(d.raw() $(, $a)*)
            })
        }
    };
}

pass!(get_file_type_index, [base__ GetFileTypeIndex], (i: *mut u32));
pass!(unadvise, [base__ Unadvise], (cookie: u32));
pass!(set_options, [base__ SetOptions], (o: FILEOPENDIALOGOPTIONS));
pass!(get_options, [base__ GetOptions], (o: *mut FILEOPENDIALOGOPTIONS));
pass!(get_current_selection, [base__ GetCurrentSelection], (out: *mut *mut c_void));
// shortcut: after our picker GetFileName still answers what SetFileName set (Electron never asks).
pass!(get_file_name, [base__ GetFileName], (out: *mut PWSTR));
pass!(set_file_name_label, [base__ SetFileNameLabel], (s: PCWSTR));
pass!(add_place, [base__ AddPlace], (item: *mut c_void, at: FDAP));
pass!(set_default_extension, [base__ SetDefaultExtension], (s: PCWSTR));
pass!(close, [base__ Close], (hr: HRESULT));
pass!(set_client_guid, [base__ SetClientGuid], (g: *const GUID));
pass!(clear_client_data, [base__ ClearClientData], ());
pass!(get_selected_items, [GetSelectedItems], (out: *mut *mut c_void));

fn text(p: PCWSTR) -> String {
    if p.is_null() { String::new() } else { unsafe { p.to_string().unwrap_or_default() } }
}

/// A folder's file system path, "" for a place that has none.
fn item_path(item: *mut c_void) -> String {
    unsafe {
        let Some(item) = IShellItem::from_raw_borrowed(&item) else { return String::new() };
        let Ok(p) = item.GetDisplayName(SIGDN_FILESYSPATH) else { return String::new() };
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const c_void));
        s
    }
}

/// A setter: forwarded, and recorded once the real dialog took it.
macro_rules! record {
    ($name:ident, $slot:ident, ($($a:ident: $t:ty),*), |$st:ident| $rec:expr) => {
        unsafe extern "system" fn $name(this: *mut c_void $(, $a: $t)*) -> HRESULT {
            guard(|| unsafe {
                let d = me(this);
                let hr = (d.vt().base__.$slot)(d.raw() $(, $a)*);
                if hr.is_ok() {
                    let $st = &mut *d.st();
                    $rec;
                }
                hr
            })
        }
    };
}

record!(set_file_type_index, SetFileTypeIndex, (i: u32), |s| s.req.index = i);
record!(set_title, SetTitle, (p: PCWSTR), |s| s.req.title = text(p));
record!(set_ok_button_label, SetOkButtonLabel, (p: PCWSTR), |s| s.req.ok = text(p));
record!(set_file_name, SetFileName, (p: PCWSTR), |s| s.req.name = text(p));
record!(set_folder, SetFolder, (item: *mut c_void), |s| s.req.folder = item_path(item));
record!(set_default_folder, SetDefaultFolder, (item: *mut c_void), |s| s.req.default_folder = item_path(item));
record!(set_file_types, SetFileTypes, (n: u32, specs: *const COMDLG_FILTERSPEC), |s| {
    s.req.filters = if specs.is_null() { Vec::new() } else { std::slice::from_raw_parts(specs, n as usize).iter().map(|f| (text(f.pszName), text(f.pszSpec))).collect() }
});
// Events and item filters reach only the real dialog's own view: it shows instead.
record!(advise, Advise, (events: *mut c_void, cookie: *mut u32), |s| { s.delegate.get_or_insert_with(|| "advise".into()); });
record!(set_filter, SetFilter, (filter: *mut c_void), |s| { s.delegate.get_or_insert_with(|| "item filter".into()); });

/// Hands an interface to the caller.
fn put<T: Interface>(out: *mut *mut c_void, r: Result<T>) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    unsafe {
        match r {
            Ok(v) => {
                *out = v.into_raw();
                S_OK
            }
            Err(e) => {
                *out = null_mut();
                e.code()
            }
        }
    }
}

fn shell_item(path: &str) -> Result<IShellItem> {
    unsafe { SHCreateItemFromParsingName(&HSTRING::from(path), None) }
}

unsafe extern "system" fn get_result(this: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    guard(|| unsafe {
        let d = me(this);
        match d.picked() {
            Some(p) => put(out, shell_item(&p[0])),
            None => (d.vt().base__.GetResult)(d.raw(), out),
        }
    })
}

unsafe extern "system" fn get_folder(this: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    guard(|| unsafe {
        let d = me(this);
        match d.picked().and_then(|p| Path::new(&p[0]).parent().map(|f| f.to_string_lossy().into_owned())) {
            Some(f) => put(out, shell_item(&f)),
            None => (d.vt().base__.GetFolder)(d.raw(), out),
        }
    })
}

unsafe extern "system" fn get_results(this: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    guard(|| unsafe {
        let d = me(this);
        let Some(paths) = d.picked() else { return (d.vt().GetResults)(d.raw(), out) };
        let mut pidls: Vec<*mut ITEMIDLIST> = Vec::new();
        let mut r = Ok(());
        for p in &paths {
            let mut pidl = null_mut();
            r = SHParseDisplayName(&HSTRING::from(p.as_str()), None, &mut pidl, 0, None);
            if r.is_err() {
                break;
            }
            pidls.push(pidl);
        }
        let arr = r.and_then(|_| SHCreateShellItemArrayFromIDLists(&pidls.iter().map(|&p| p as *const ITEMIDLIST).collect::<Vec<_>>()));
        for p in pidls {
            CoTaskMemFree(Some(p as *const c_void));
        }
        put(out, arr)
    })
}

fn outcome(hr: HRESULT) -> String {
    match hr {
        S_OK => "system: ok".into(),
        h if h == ERROR_CANCELLED.to_hresult() => "system: cancelled".into(),
        h => format!("system: hr={:#010x}", h.0 as u32),
    }
}

/// What the picker returned is what the host asked for: existing files, one unless multiselect.
fn valid(paths: &[String], multi: bool) -> bool {
    (multi || paths.len() == 1) && paths.iter().all(|p| Path::new(p).is_absolute() && Path::new(p).is_file())
}

unsafe extern "system" fn show(this: *mut c_void, owner: HWND) -> HRESULT {
    guard(|| unsafe {
        let d = me(this);
        let real_show = || (d.vt().base__.base__.Show)(d.raw(), owner);
        // The real dialog took every setter, so it knows the options best (defaults included).
        let opts = d.real.GetOptions().unwrap_or_default().0 as u32;
        let line = |decision: &str, outcome: &str| log_line(&format!("Show\topts={opts:#010x}\t{decision}\t{outcome}"));
        let (why, mut req) = {
            let mut st = d.st();
            st.picked = None;
            let why = if killed() {
                Some("kill switch".to_string())
            } else if let Some(w) = &st.delegate {
                Some(w.clone())
            } else if opts & FOS_PICKFOLDERS.0 as u32 != 0 {
                Some("pick folders".into())
            } else if opts & FOS_ALLNONSTORAGEITEMS.0 as u32 != 0 {
                Some("non-storage items".into())
            } else {
                None
            };
            (why, st.req.clone())
        };
        if let Some(why) = why {
            let hr = real_show();
            line(&format!("delegate: {why}"), &outcome(hr));
            return hr;
        }
        req.owner = owner.0 as isize;
        req.multi = opts & FOS_ALLOWMULTISELECT.0 as u32 != 0;
        req.hidden = opts & FOS_FORCESHOWHIDDEN.0 as u32 != 0;
        let hosts = crate::pick::Hosts::from_env();
        let r = std::panic::catch_unwind(AssertUnwindSafe(|| crate::pick::run(&mut req, owner, &hosts))).unwrap_or_else(|_| Err("panic".into()));
        let why = match r {
            Ok((via, Msg::Picked(paths))) if valid(&paths, req.multi) => {
                let n = paths.len();
                d.st().picked = Some(paths);
                line(&format!("picker ({via})"), &format!("picked {n}"));
                return S_OK;
            }
            Ok((via, Msg::Cancelled)) => {
                line(&format!("picker ({via})"), "cancelled");
                return ERROR_CANCELLED.to_hresult();
            }
            Ok((via, _)) => format!("{via}: result not usable"),
            Err(why) => why,
        };
        let hr = real_show();
        line(&format!("fallback: {why}"), &outcome(hr));
        hr
    })
}
