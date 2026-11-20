//! UI Automation: screen readers (Narrator, NVDA, JAWS) and automation tools see the window as
//! a toolbar of buttons, the sidebar's places, the list of items and the status bar, with names,
//! selection and the actions Open / Select / Scroll into view.
//!
//! Providers keep only the window handle and an identity (an item's path, a sidebar row's
//! label), and resolve it against the live App on every call: a provider a client still holds
//! after the list changed finds its item again or reports "not available"; after the window is
//! gone it always does. Calls arrive on the UI thread (COM threading, STA) and may arrive while
//! the thread pumps inside a menu or a drag, so they only read; anything that changes state is
//! posted to the window (WM_APP_UIA) and done there.
// UIA constants are named like the SDK (UIA_NamePropertyId) and used as match patterns.
#![allow(non_upper_case_globals)]
use crate::app::{App, Focus, Hit};
use crate::geom::Rect;
use crate::sidebar::Row;
use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::c_void,
    path::{Path, PathBuf},
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, POINT, WPARAM},
    Graphics::Gdi::{ClientToScreen, ScreenToClient},
    System::{
        Com::SAFEARRAY,
        Ole::{SafeArrayCreateVector, SafeArrayPutElement},
        Variant::{VARIANT, VT_I4, VT_UNKNOWN},
    },
    UI::{Accessibility::*, Input::KeyboardAndMouse::SetFocus, WindowsAndMessaging::PostMessageW},
};
use windows_core::{BOOL, Error, HRESULT, IUnknown, Interface, Result, implement};

/// UI Automation request for the window's own tree (lParam of WM_GETOBJECT).
pub const ROOT_OBJECT_ID: i32 = UiaRootObjectId;

fn gone() -> Error {
    Error::from_hresult(HRESULT(UIA_E_ELEMENTNOTAVAILABLE as i32))
}

fn not_now() -> Error {
    Error::from_hresult(HRESULT(UIA_E_INVALIDOPERATION as i32))
}

/// "Nothing here": S_OK with a null result (an end of navigation, an unsupported pattern).
fn none<T>() -> Result<T> {
    Err(Error::empty())
}

fn app_at(hwnd: isize) -> Result<&'static App> {
    let p = crate::win::app_of(HWND(hwnd as *mut c_void));
    if p.is_null() { Err(gone()) } else { Ok(unsafe { &*p }) }
}

const BUTTONS: [Hit; 8] = [Hit::SidebarBtn, Hit::Back, Hit::Fwd, Hit::View(0), Hit::View(1), Hit::View(2), Hit::View(3), Hit::More];

#[derive(Clone, Debug, PartialEq)]
enum Kind {
    Toolbar,
    Button(Hit),
    Sidebar,
    /// A sidebar row: index (a hint) and its label (the identity).
    Place(usize, String),
    /// The item list of the current place.
    Content,
    /// An item: index (a hint) and its path (the identity).
    Item(usize, PathBuf),
    Status,
}

// ---- resolving identities against the live window ------------------------------------------------------

fn place_label(row: &Row) -> Option<String> {
    match row {
        Row::Item { label, .. } => Some(label.clone()),
        Row::Tag(i) => Some(crate::theme::TAGS[*i].1.to_string()),
        Row::Header(_) => None,
    }
}

fn place_index(app: &App, hint: usize, label: &str) -> Option<usize> {
    if !app.sidebar_on {
        return None;
    }
    let is = |k: usize| app.side_rows.get(k).and_then(place_label).as_deref() == Some(label);
    if is(hint) { Some(hint) } else { (0..app.side_rows.len()).find(|&k| is(k)) }
}

fn places(app: &App) -> Vec<usize> {
    if !app.sidebar_on {
        return Vec::new();
    }
    (0..app.side_rows.len()).filter(|&k| place_label(&app.side_rows[k]).is_some()).collect()
}

fn item_index(app: &App, hint: usize, path: &Path) -> Option<usize> {
    let t = app.tab();
    let dir = t.dir();
    let is = |i: usize| {
        t.entries.get(i).is_some_and(|e| match &e.full {
            Some(f) => f == path,
            None => path.parent() == Some(dir.as_path()) && path.file_name().is_some_and(|n| n == e.name.as_str()),
        })
    };
    if is(hint) { Some(hint) } else { (0..t.entries.len()).find(|&i| is(i)) }
}

fn item(app: &App, i: usize) -> Kind {
    Kind::Item(i, app.tab().path_of(i))
}

fn top_level(app: &App) -> Vec<Kind> {
    let mut v = vec![Kind::Toolbar];
    if app.sidebar_on {
        v.push(Kind::Sidebar);
    }
    v.push(Kind::Content);
    v.push(Kind::Status);
    v
}

/// The element still exists; an item or place may have moved (its index is refreshed).
fn resolve(app: &App, k: &Kind) -> Option<Kind> {
    match k {
        Kind::Place(hint, label) => place_index(app, *hint, label).map(|i| Kind::Place(i, label.clone())),
        Kind::Item(hint, path) => item_index(app, *hint, path).map(|i| Kind::Item(i, path.clone())),
        Kind::Sidebar if !app.sidebar_on => None,
        other => Some(other.clone()),
    }
}

fn first_child(app: &App, k: &Kind, last: bool) -> Option<Kind> {
    match k {
        Kind::Toolbar => Some(Kind::Button(if last { BUTTONS[BUTTONS.len() - 1] } else { BUTTONS[0] })),
        Kind::Sidebar => {
            let p = places(app);
            let k = if last { p.last() } else { p.first() };
            k.map(|&k| Kind::Place(k, place_label(&app.side_rows[k]).unwrap_or_default()))
        }
        Kind::Content => {
            let n = app.tab().entries.len();
            (n > 0).then(|| item(app, if last { n - 1 } else { 0 }))
        }
        _ => None,
    }
}

fn sibling(app: &App, k: &Kind, next: bool) -> Option<Kind> {
    let step = |i: usize, n: usize| if next { (i + 1 < n).then(|| i + 1) } else { i.checked_sub(1) };
    match k {
        Kind::Button(h) => {
            let i = BUTTONS.iter().position(|b| b == h)?;
            step(i, BUTTONS.len()).map(|j| Kind::Button(BUTTONS[j]))
        }
        Kind::Place(i, _) => {
            let p = places(app);
            let at = p.iter().position(|x| x == i)?;
            step(at, p.len()).map(|j| Kind::Place(p[j], place_label(&app.side_rows[p[j]]).unwrap_or_default()))
        }
        Kind::Item(i, _) => step(*i, app.tab().entries.len()).map(|j| item(app, j)),
        top => {
            let v = top_level(app);
            let at = v.iter().position(|x| x == top)?;
            step(at, v.len()).map(|j| v[j].clone())
        }
    }
}

fn parent(k: &Kind) -> Option<Kind> {
    match k {
        Kind::Button(_) => Some(Kind::Toolbar),
        Kind::Place(..) => Some(Kind::Sidebar),
        Kind::Item(..) => Some(Kind::Content),
        _ => None, // the root
    }
}

fn button_name(app: &App, h: Hit) -> &'static str {
    match h {
        Hit::SidebarBtn if app.sidebar_on => "Skrýt postranní panel",
        Hit::SidebarBtn => "Zobrazit postranní panel",
        Hit::Back => "Zpět",
        Hit::Fwd => "Vpřed",
        Hit::View(0) => "Ikony",
        Hit::View(1) => "Seznam",
        Hit::View(2) => "Sloupce",
        Hit::View(3) => "Galerie",
        _ => "Další akce",
    }
}

fn button_enabled(app: &App, h: Hit) -> bool {
    match h {
        Hit::Back => app.tab().hist.can_back(),
        Hit::Fwd => app.tab().hist.can_forward(),
        _ => true,
    }
}

fn rect_of(app: &App, k: &Kind) -> Rect {
    let l = &app.lay;
    match k {
        Kind::Toolbar => l.toolbar,
        Kind::Button(h) => match h {
            Hit::SidebarBtn => l.sidebar_btn,
            Hit::Back => l.back,
            Hit::Fwd => l.fwd,
            Hit::View(v) => l.views[(*v).min(3)],
            _ => l.more,
        },
        Kind::Sidebar => l.sidebar,
        Kind::Place(i, _) => crate::paint::side_row_rect(app, *i),
        Kind::Content => l.body,
        Kind::Item(i, _) => app.item_rect(*i),
        Kind::Status => l.status,
    }
}

fn screen_rect(app: &App, r: Rect) -> UiaRect {
    let s = app.scale;
    let mut p = POINT { x: (r.x * s).round() as i32, y: (r.y * s).round() as i32 };
    unsafe {
        let _ = ClientToScreen(app.hwnd, &mut p);
    }
    UiaRect { left: p.x as f64, top: p.y as f64, width: (r.w * s).max(0.0) as f64, height: (r.h * s).max(0.0) as f64 }
}

/// The part of `r` inside `clip` (items scrolled partly out of the list).
fn clipped(r: Rect, clip: Rect) -> Rect {
    let (x0, y0) = (r.x.max(clip.x), r.y.max(clip.y));
    let (x1, y1) = (r.r().min(clip.r()), r.b().min(clip.b()));
    Rect::new(x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0))
}

fn offscreen(app: &App, k: &Kind) -> bool {
    match k {
        Kind::Item(..) => !rect_of(app, k).intersects(&app.lay.body),
        Kind::Place(..) => !rect_of(app, k).intersects(&app.lay.sidebar),
        _ => false,
    }
}

/// The element with keyboard focus inside the window (None: an edit field or nothing).
fn focused(app: &App) -> Option<Kind> {
    match app.focus {
        Focus::Content => match app.tab().sel.focus.filter(|&f| f < app.tab().entries.len()) {
            Some(f) => Some(item(app, f)),
            None => Some(Kind::Content),
        },
        Focus::Sidebar if app.sidebar_on => Some(Kind::Sidebar),
        _ => None,
    }
}

fn place_selected(app: &App, k: usize) -> bool {
    match app.side_rows.get(k) {
        Some(Row::Item { loc, .. }) => &app.tab().loc == loc,
        Some(Row::Tag(i)) => app.tab().loc == crate::app::Loc::Tag(*i),
        _ => false,
    }
}

fn runtime_id(k: &Kind) -> [i32; 3] {
    let hash = |s: &str| {
        // FNV-1a folded to 31 bits: stable for the same path within a session.
        let mut h: u32 = 0x811c9dc5;
        for b in s.bytes() {
            h = (h ^ b as u32).wrapping_mul(0x01000193);
        }
        (h & 0x7fff_ffff) as i32
    };
    let (tag, id) = match k {
        Kind::Toolbar => (1, 0),
        Kind::Button(h) => (2, BUTTONS.iter().position(|b| b == h).unwrap_or(0) as i32),
        Kind::Sidebar => (3, 0),
        Kind::Place(_, label) => (4, hash(label)),
        Kind::Content => (5, 0),
        Kind::Item(_, p) => (6, hash(&p.to_string_lossy())),
        Kind::Status => (7, 0),
    };
    [UiaAppendRuntimeId as i32, tag, id]
}

fn int_array(v: &[i32]) -> *mut SAFEARRAY {
    unsafe {
        let sa = SafeArrayCreateVector(VT_I4, 0, v.len() as u32);
        if !sa.is_null() {
            for (i, x) in v.iter().enumerate() {
                let at = i as i32;
                let _ = SafeArrayPutElement(sa, &at, (x as *const i32).cast());
            }
        }
        sa
    }
}

fn provider_array(v: &[IRawElementProviderSimple]) -> *mut SAFEARRAY {
    unsafe {
        let sa = SafeArrayCreateVector(VT_UNKNOWN, 0, v.len() as u32);
        if !sa.is_null() {
            for (i, p) in v.iter().enumerate() {
                let at = i as i32;
                // The array takes its own reference.
                let _ = SafeArrayPutElement(sa, &at, p.as_raw());
            }
        }
        sa
    }
}

// ---- providers ----------------------------------------------------------------------------------------------

#[implement(IRawElementProviderSimple, IRawElementProviderFragment, IRawElementProviderFragmentRoot)]
struct Root {
    hwnd: isize,
}

#[implement(
    IRawElementProviderSimple,
    IRawElementProviderFragment,
    ISelectionProvider,
    ISelectionItemProvider,
    IInvokeProvider,
    IScrollItemProvider
)]
struct Node {
    hwnd: isize,
    kind: Kind,
}

fn node(hwnd: isize, kind: Kind) -> Node {
    Node { hwnd, kind }
}

fn root(hwnd: isize) -> IRawElementProviderSimple {
    Root { hwnd }.into()
}

fn fragment(hwnd: isize, k: Option<Kind>) -> Result<IRawElementProviderFragment> {
    match k {
        Some(k) => Ok(node(hwnd, k).into()),
        None => none(),
    }
}

const OPTIONS: ProviderOptions = ProviderOptions(ProviderOptions_ServerSideProvider.0 | ProviderOptions_UseComThreading.0);

impl IRawElementProviderSimple_Impl for Root_Impl {
    fn ProviderOptions(&self) -> Result<ProviderOptions> {
        Ok(OPTIONS)
    }
    fn GetPatternProvider(&self, _: UIA_PATTERN_ID) -> Result<IUnknown> {
        none()
    }
    fn GetPropertyValue(&self, id: UIA_PROPERTY_ID) -> Result<VARIANT> {
        // The window's own name and type come from the host (its title, "okno").
        Ok(match id {
            UIA_ProviderDescriptionPropertyId => VARIANT::from("fm"),
            _ => VARIANT::default(),
        })
    }
    fn HostRawElementProvider(&self) -> Result<IRawElementProviderSimple> {
        unsafe { UiaHostProviderFromHwnd(HWND(self.hwnd as *mut c_void)) }
    }
}

impl IRawElementProviderFragment_Impl for Root_Impl {
    fn Navigate(&self, direction: NavigateDirection) -> Result<IRawElementProviderFragment> {
        let app = app_at(self.hwnd)?;
        let v = top_level(app);
        match direction {
            NavigateDirection_FirstChild => fragment(self.hwnd, v.first().cloned()),
            NavigateDirection_LastChild => fragment(self.hwnd, v.last().cloned()),
            _ => none(), // parent and siblings: the host window's
        }
    }
    fn GetRuntimeId(&self) -> Result<*mut SAFEARRAY> {
        Ok(std::ptr::null_mut()) // the host window's
    }
    fn BoundingRectangle(&self) -> Result<UiaRect> {
        Ok(UiaRect::default()) // the host window's
    }
    fn GetEmbeddedFragmentRoots(&self) -> Result<*mut SAFEARRAY> {
        Ok(std::ptr::null_mut())
    }
    fn SetFocus(&self) -> Result<()> {
        Ok(())
    }
    fn FragmentRoot(&self) -> Result<IRawElementProviderFragmentRoot> {
        Ok(Root { hwnd: self.hwnd }.into())
    }
}

impl IRawElementProviderFragmentRoot_Impl for Root_Impl {
    fn ElementProviderFromPoint(&self, x: f64, y: f64) -> Result<IRawElementProviderFragment> {
        let app = app_at(self.hwnd)?;
        let mut p = POINT { x: x as i32, y: y as i32 };
        unsafe {
            let _ = ScreenToClient(app.hwnd, &mut p);
        }
        let (cx, cy) = (p.x as f32 / app.scale, p.y as f32 / app.scale);
        let l = &app.lay;
        let k = match app.hit(cx, cy) {
            Hit::Row(i) => Some(item(app, i)),
            Hit::SideRow(k) => app.side_rows.get(k).and_then(place_label).map(|label| Kind::Place(k, label)),
            h @ (Hit::SidebarBtn | Hit::Back | Hit::Fwd | Hit::View(_) | Hit::More) => Some(Kind::Button(h)),
            _ if l.body.contains(cx, cy) => Some(Kind::Content),
            _ if app.sidebar_on && l.sidebar.contains(cx, cy) => Some(Kind::Sidebar),
            _ if l.toolbar.contains(cx, cy) => Some(Kind::Toolbar),
            _ if l.status.contains(cx, cy) => Some(Kind::Status),
            _ => None,
        };
        fragment(self.hwnd, k)
    }
    fn GetFocus(&self) -> Result<IRawElementProviderFragment> {
        let app = app_at(self.hwnd)?;
        fragment(self.hwnd, focused(app))
    }
}

impl Node_Impl {
    /// The live element, or "not available" when it is gone.
    fn live(&self) -> Result<(&'static App, Kind)> {
        let app = app_at(self.hwnd)?;
        let k = resolve(app, &self.kind).ok_or_else(gone)?;
        Ok((app, k))
    }
    fn simple(&self, k: Kind) -> IRawElementProviderSimple {
        node(self.hwnd, k).into()
    }
}

impl IRawElementProviderSimple_Impl for Node_Impl {
    fn ProviderOptions(&self) -> Result<ProviderOptions> {
        Ok(OPTIONS)
    }
    fn GetPatternProvider(&self, pattern: UIA_PATTERN_ID) -> Result<IUnknown> {
        let (_, k) = self.live()?;
        let has = match k {
            Kind::Content | Kind::Sidebar => pattern == UIA_SelectionPatternId,
            Kind::Item(..) => [UIA_SelectionItemPatternId, UIA_InvokePatternId, UIA_ScrollItemPatternId].contains(&pattern),
            Kind::Place(..) => [UIA_SelectionItemPatternId, UIA_InvokePatternId].contains(&pattern),
            Kind::Button(_) => pattern == UIA_InvokePatternId,
            _ => false,
        };
        if has { Ok(node(self.hwnd, k).into()) } else { none() }
    }
    fn GetPropertyValue(&self, id: UIA_PROPERTY_ID) -> Result<VARIANT> {
        let (app, k) = self.live()?;
        let control = |c: UIA_CONTROLTYPE_ID| VARIANT::from(c.0);
        let v = match (&k, id) {
            (_, UIA_ProviderDescriptionPropertyId) => VARIANT::from("fm"),
            (_, UIA_IsEnabledPropertyId) => VARIANT::from(match k {
                Kind::Button(h) => button_enabled(app, h),
                _ => true,
            }),
            (_, UIA_IsOffscreenPropertyId) => VARIANT::from(offscreen(app, &k)),
            (Kind::Toolbar, UIA_ControlTypePropertyId) => control(UIA_ToolBarControlTypeId),
            (Kind::Toolbar, UIA_NamePropertyId) => VARIANT::from("Panel nástrojů"),
            (Kind::Button(_), UIA_ControlTypePropertyId) => control(UIA_ButtonControlTypeId),
            (Kind::Button(h), UIA_NamePropertyId) => VARIANT::from(button_name(app, *h)),
            (Kind::Button(_), UIA_IsKeyboardFocusablePropertyId) => VARIANT::from(false),
            (Kind::Sidebar, UIA_ControlTypePropertyId) => control(UIA_ListControlTypeId),
            (Kind::Sidebar, UIA_NamePropertyId) => VARIANT::from("Postranní panel"),
            (Kind::Place(..), UIA_ControlTypePropertyId) => control(UIA_ListItemControlTypeId),
            (Kind::Place(_, label), UIA_NamePropertyId) => VARIANT::from(label.as_str()),
            (Kind::Content, UIA_ControlTypePropertyId) => control(UIA_ListControlTypeId),
            (Kind::Content, UIA_NamePropertyId) => VARIANT::from(app.tab().loc.title().as_str()),
            (Kind::Content, UIA_IsKeyboardFocusablePropertyId) => VARIANT::from(true),
            (Kind::Content, UIA_HasKeyboardFocusPropertyId) => VARIANT::from(has_focus(app) && focused(app) == Some(Kind::Content)),
            (Kind::Item(..), UIA_ControlTypePropertyId) => control(UIA_ListItemControlTypeId),
            (Kind::Item(i, _), UIA_NamePropertyId) => VARIANT::from(app.tab().entries[*i].name.as_str()),
            (Kind::Item(i, _), UIA_ItemTypePropertyId) => VARIANT::from(crate::fsys::kind_name(&app.tab().entries[*i]).as_str()),
            (Kind::Item(i, _), UIA_HelpTextPropertyId) => VARIANT::from(describe(&app.tab().entries[*i]).as_str()),
            (Kind::Item(..), UIA_IsKeyboardFocusablePropertyId) => VARIANT::from(true),
            (Kind::Item(..), UIA_HasKeyboardFocusPropertyId) => VARIANT::from(has_focus(app) && focused(app).as_ref() == Some(&k)),
            (Kind::Status, UIA_ControlTypePropertyId) => control(UIA_StatusBarControlTypeId),
            (Kind::Status, UIA_NamePropertyId) => VARIANT::from(crate::paint::status_text(app).as_str()),
            _ => VARIANT::default(),
        };
        Ok(v)
    }
    fn HostRawElementProvider(&self) -> Result<IRawElementProviderSimple> {
        none()
    }
}

/// "Dokument PDF, 48,0 kB, změněno Dnes 4:35" (what a reader adds after the name).
fn describe(e: &crate::fsys::Entry) -> String {
    let mut parts = vec![crate::fsys::kind_name(e)];
    if e.size_known() {
        parts.push(crate::fsys::format_size(e.size));
    }
    let date = crate::fsys::format_date(e.modified);
    if !date.is_empty() {
        parts.push(format!("změněno {date}"));
    }
    parts.join(", ")
}

fn has_focus(app: &App) -> bool {
    unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetFocus() == app.hwnd }
}

impl IRawElementProviderFragment_Impl for Node_Impl {
    fn Navigate(&self, direction: NavigateDirection) -> Result<IRawElementProviderFragment> {
        let (app, k) = self.live()?;
        match direction {
            NavigateDirection_Parent => match parent(&k) {
                Some(p) => fragment(self.hwnd, Some(p)),
                None => Ok(Root { hwnd: self.hwnd }.into()),
            },
            NavigateDirection_NextSibling => fragment(self.hwnd, sibling(app, &k, true)),
            NavigateDirection_PreviousSibling => fragment(self.hwnd, sibling(app, &k, false)),
            NavigateDirection_FirstChild => fragment(self.hwnd, first_child(app, &k, false)),
            NavigateDirection_LastChild => fragment(self.hwnd, first_child(app, &k, true)),
            _ => none(),
        }
    }
    fn GetRuntimeId(&self) -> Result<*mut SAFEARRAY> {
        Ok(int_array(&runtime_id(&self.kind)))
    }
    fn BoundingRectangle(&self) -> Result<UiaRect> {
        let (app, k) = self.live()?;
        let r = rect_of(app, &k);
        let r = match k {
            Kind::Item(..) => clipped(r, app.lay.body),
            Kind::Place(..) => clipped(r, app.lay.sidebar),
            _ => r,
        };
        Ok(screen_rect(app, r))
    }
    fn GetEmbeddedFragmentRoots(&self) -> Result<*mut SAFEARRAY> {
        Ok(std::ptr::null_mut())
    }
    fn SetFocus(&self) -> Result<()> {
        let (_, k) = self.live()?;
        post(self.hwnd, Action::Focus(k));
        Ok(())
    }
    fn FragmentRoot(&self) -> Result<IRawElementProviderFragmentRoot> {
        Ok(Root { hwnd: self.hwnd }.into())
    }
}

impl ISelectionProvider_Impl for Node_Impl {
    fn GetSelection(&self) -> Result<*mut SAFEARRAY> {
        let (app, k) = self.live()?;
        let v: Vec<IRawElementProviderSimple> = match k {
            // shortcut: at most 5000 selected items are reported; Ctrl+A in a huge folder stays fast.
            Kind::Content => app.tab().sel.indices().into_iter().take(5000).map(|i| self.simple(item(app, i))).collect(),
            Kind::Sidebar => places(app)
                .into_iter()
                .filter(|&p| place_selected(app, p))
                .map(|p| self.simple(Kind::Place(p, place_label(&app.side_rows[p]).unwrap_or_default())))
                .collect(),
            _ => return Err(not_now()),
        };
        Ok(provider_array(&v))
    }
    fn CanSelectMultiple(&self) -> Result<BOOL> {
        let (_, k) = self.live()?;
        Ok((k == Kind::Content).into())
    }
    fn IsSelectionRequired(&self) -> Result<BOOL> {
        Ok(false.into())
    }
}

impl ISelectionItemProvider_Impl for Node_Impl {
    fn Select(&self) -> Result<()> {
        let (_, k) = self.live()?;
        post(self.hwnd, Action::Select(k, SelOp::Only));
        Ok(())
    }
    fn AddToSelection(&self) -> Result<()> {
        match self.live()? {
            (_, k @ Kind::Item(..)) => post(self.hwnd, Action::Select(k, SelOp::Add)),
            (app, Kind::Place(i, _)) if place_selected(app, i) => {}
            _ => return Err(not_now()), // one place at a time
        }
        Ok(())
    }
    fn RemoveFromSelection(&self) -> Result<()> {
        match self.live()? {
            (_, k @ Kind::Item(..)) => post(self.hwnd, Action::Select(k, SelOp::Remove)),
            (app, Kind::Place(i, _)) if !place_selected(app, i) => {}
            _ => return Err(not_now()),
        }
        Ok(())
    }
    fn IsSelected(&self) -> Result<BOOL> {
        let (app, k) = self.live()?;
        Ok(match k {
            Kind::Item(i, _) => app.tab().sel.is(i),
            Kind::Place(i, _) => place_selected(app, i),
            _ => false,
        }
        .into())
    }
    fn SelectionContainer(&self) -> Result<IRawElementProviderSimple> {
        let (_, k) = self.live()?;
        match k {
            Kind::Item(..) => Ok(self.simple(Kind::Content)),
            Kind::Place(..) => Ok(self.simple(Kind::Sidebar)),
            _ => none(),
        }
    }
}

impl IInvokeProvider_Impl for Node_Impl {
    fn Invoke(&self) -> Result<()> {
        let (app, k) = self.live()?;
        if let Kind::Button(h) = k {
            if !button_enabled(app, h) {
                return Err(Error::from_hresult(HRESULT(UIA_E_ELEMENTNOTENABLED as i32)));
            }
        }
        post(self.hwnd, Action::Invoke(k));
        Ok(())
    }
}

impl IScrollItemProvider_Impl for Node_Impl {
    fn ScrollIntoView(&self) -> Result<()> {
        let (_, k) = self.live()?;
        post(self.hwnd, Action::Reveal(k));
        Ok(())
    }
}

// ---- actions, done on the window's own turn ---------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum SelOp {
    Only,
    Add,
    Remove,
}

enum Action {
    Invoke(Kind),
    Select(Kind, SelOp),
    Focus(Kind),
    Reveal(Kind),
}

fn post(hwnd: isize, a: Action) {
    let b = Box::into_raw(Box::new(a));
    unsafe {
        if PostMessageW(Some(HWND(hwnd as *mut c_void)), crate::win::WM_APP_UIA, WPARAM(0), LPARAM(b as isize)).is_err() {
            drop(Box::from_raw(b));
        }
    }
}

/// WM_APP_UIA: a client asked for an action; the element is looked up again first.
pub fn act(app: &mut App, l: LPARAM) {
    let a = unsafe { Box::from_raw(l.0 as *mut Action) };
    let k = match &*a {
        Action::Invoke(k) | Action::Select(k, _) | Action::Focus(k) | Action::Reveal(k) => resolve(app, k),
    };
    let Some(k) = k else { return };
    match (*a, k) {
        (Action::Invoke(_), Kind::Item(i, _)) => app.open(i, false),
        (Action::Invoke(_) | Action::Select(_, SelOp::Only), Kind::Place(i, _)) => app.activate(Hit::SideRow(i)),
        (Action::Invoke(_), Kind::Button(h)) => app.activate(h),
        (Action::Select(_, op), Kind::Item(i, _)) => {
            app.focus = Focus::Content;
            let sel = &mut app.tab_mut().sel;
            match op {
                SelOp::Only => sel.select_only(i),
                SelOp::Add if !sel.is(i) => sel.toggle(i),
                SelOp::Remove if sel.is(i) => sel.toggle(i),
                _ => {}
            }
            app.reveal(i);
            crate::views::sync_columns(app);
        }
        (Action::Focus(_), Kind::Item(i, _)) => {
            app.focus = Focus::Content;
            app.tab_mut().sel.focus = Some(i);
            app.reveal(i);
            unsafe {
                let _ = SetFocus(Some(app.hwnd));
            }
        }
        (Action::Focus(_), Kind::Content) => {
            app.focus = Focus::Content;
            unsafe {
                let _ = SetFocus(Some(app.hwnd));
            }
        }
        (Action::Reveal(_), Kind::Item(i, _)) => app.reveal(i),
        _ => {}
    }
    app.invalidate();
}

// ---- the window's side: WM_GETOBJECT and events ------------------------------------------------------------

/// WM_GETOBJECT for UiaRootObjectId.
pub fn get_object(hwnd: HWND, w: WPARAM, l: LPARAM) -> windows::Win32::Foundation::LRESULT {
    let r = root(hwnd.0 as isize);
    unsafe { UiaReturnRawElementProvider(hwnd, w, l, &r) }
}

/// WM_DESTROY: clients drop their providers for this window.
pub fn forget(hwnd: HWND) {
    SEEN.with(|s| s.borrow_mut().remove(&(hwnd.0 as isize)));
    unsafe {
        UiaReturnRawElementProvider(hwnd, WPARAM(0), LPARAM(0), None::<&IRawElementProviderSimple>);
    }
}

#[derive(Clone, PartialEq, Default)]
struct Seen {
    /// The place and its list: a change re-reads the children.
    list: (String, usize, Option<PathBuf>, Option<PathBuf>),
    focus: Option<Kind>,
    selection: (usize, Option<usize>),
}

thread_local! {
    static SEEN: RefCell<HashMap<isize, Seen>> = RefCell::new(HashMap::new());
}

fn seen(app: &App) -> Seen {
    let t = app.tab();
    let n = t.entries.len();
    Seen {
        list: (t.loc.title(), n, (n > 0).then(|| t.path_of(0)), (n > 0).then(|| t.path_of(n - 1))),
        focus: if has_focus(app) { focused(app) } else { None },
        selection: (t.sel.count(), t.sel.first()),
    }
}

/// After every paint (every state change paints): tells listening clients what changed.
/// Runs on the window's own turn, never inside a provider call.
pub fn after_paint(app: &App) {
    if !unsafe { UiaClientsAreListening() }.as_bool() {
        return;
    }
    let now = seen(app);
    let key = app.hwnd.0 as isize;
    let Some(prev) = SEEN.with(|s| s.borrow_mut().insert(key, now.clone())) else { return };
    let hwnd = key;
    unsafe {
        if prev.list != now.list {
            let content: IRawElementProviderSimple = node(hwnd, Kind::Content).into();
            let mut id = runtime_id(&Kind::Content);
            let _ = UiaRaiseStructureChangedEvent(&content, StructureChangeType_ChildrenInvalidated, id.as_mut_ptr(), id.len() as i32);
        }
        if prev.focus != now.focus {
            if let Some(k) = &now.focus {
                let p: IRawElementProviderSimple = node(hwnd, k.clone()).into();
                let _ = UiaRaiseAutomationEvent(&p, UIA_AutomationFocusChangedEventId);
            }
        }
        if prev.selection != now.selection {
            match now.selection {
                (1, Some(i)) => {
                    let p: IRawElementProviderSimple = node(hwnd, item(app, i)).into();
                    let _ = UiaRaiseAutomationEvent(&p, UIA_SelectionItem_ElementSelectedEventId);
                }
                _ => {
                    let p: IRawElementProviderSimple = node(hwnd, Kind::Content).into();
                    let _ = UiaRaiseAutomationEvent(&p, UIA_Selection_InvalidatedEventId);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_ids_are_stable_and_distinct() {
        let a = runtime_id(&Kind::Item(0, PathBuf::from(r"C:\a\b.txt")));
        let b = runtime_id(&Kind::Item(7, PathBuf::from(r"C:\a\b.txt")));
        let c = runtime_id(&Kind::Item(0, PathBuf::from(r"C:\a\c.txt")));
        assert_eq!(a, b, "the index is only a hint");
        assert_ne!(a, c);
        assert_ne!(runtime_id(&Kind::Content), runtime_id(&Kind::Toolbar));
        assert_eq!(a[0], UiaAppendRuntimeId as i32);
    }

    #[test]
    fn clipping_keeps_the_visible_part() {
        let r = clipped(Rect::new(0.0, 90.0, 100.0, 26.0), Rect::new(0.0, 100.0, 100.0, 300.0));
        assert_eq!((r.y, r.h), (100.0, 16.0));
        let r = clipped(Rect::new(0.0, 0.0, 10.0, 10.0), Rect::new(50.0, 50.0, 10.0, 10.0));
        assert_eq!((r.w, r.h), (0.0, 0.0));
    }
}
