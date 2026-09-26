//! Drag a downloaded photo, video, or document out to a folder.
//!
//! The drop is a copy. The message and the original file stay. Windows uses
//! the shell drag-and-drop API. Other platforms keep Save as.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

static LEASES: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);
/// Idle export files older than this are residue. DoDragDrop does not report
/// when a target finishes reading a path, so this is not a transfer timeout.
const EXPORT_RETENTION: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
const EXPORT_DIR: &str = "drag-export";

/// How a shell drag ended. Cancel means no target accepted the drop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragEnd {
    Cancelled,
    Copied,
}

/// `IDropSource::QueryContinueDrag`. Escape cancels. Releasing the left
/// button asks the target to drop. Anything else keeps the drag going.
/// The shell refuses the drop when this always returns "continue".
pub fn drag_decision(escape: bool, key_state: u32) -> i32 {
    const MK_LBUTTON: u32 = 0x0001;
    const DROP: i32 = 0x0004_0100;
    const CANCEL: i32 = 0x0004_0101;
    if escape {
        CANCEL
    } else if key_state & MK_LBUTTON == 0 {
        DROP
    } else {
        0
    }
}

/// Paths the cache sweep must not delete while a drop target is reading them.
pub fn leased() -> Vec<PathBuf> {
    LEASES
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
        .unwrap_or_default()
        .into_iter()
        .collect()
}

fn lease(path: PathBuf) {
    let mut guard = LEASES.lock().unwrap_or_else(|poison| poison.into_inner());
    guard.get_or_insert_with(HashSet::new).insert(path);
}

fn release(path: &Path) {
    let mut guard = LEASES.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some(set) = guard.as_mut() {
        set.remove(path);
    }
}

/// A file Explorer can receive: on disk, non-empty, and not still downloading.
pub fn exportable(path: &Path, downloading: bool) -> bool {
    if downloading || !path.is_file() {
        return false;
    }
    std::fs::metadata(path).is_ok_and(|meta| meta.len() > 0)
}

/// A Windows file name that keeps letters such as accents and drops characters
/// Explorer rejects. Reserved device names gain a trailing underscore.
pub fn export_name(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let mut cleaned: String = base
        .chars()
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            control if control.is_control() => '_',
            other => other,
        })
        .collect();
    while cleaned.ends_with([' ', '.']) {
        cleaned.pop();
    }
    if cleaned.is_empty() {
        cleaned = "file".to_owned();
    }
    let stem = cleaned
        .split_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(cleaned.as_str());
    let reserved = matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "COM1" | "LPT1"
    );
    if reserved {
        cleaned.insert(stem.len(), '_');
    }
    cleaned
}

/// Hard-link a friendly name beside the original. Fails rather than reading
/// the bytes on the interface thread. The link is leased until `release`.
pub fn stage(source: &Path, display_name: &str) -> std::io::Result<PathBuf> {
    if !exportable(source, false) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "file is missing or incomplete",
        ));
    }
    let hold = source.parent().unwrap_or(Path::new(".")).join(EXPORT_DIR);
    std::fs::create_dir_all(&hold)?;
    let dest = unique_hold(&hold, &export_name(display_name))?;
    std::fs::hard_link(source, &dest)?;
    lease(source.to_path_buf());
    lease(dest.clone());
    Ok(dest)
}

pub fn unstage(source: &Path, staged: &Path) {
    release(source);
    release(staged);
}

fn unique_hold(dir: &Path, name: &str) -> std::io::Result<PathBuf> {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return Ok(candidate);
    }
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("file");
    let ext = path.extension().and_then(|ext| ext.to_str());
    for index in 2..100 {
        let next = match ext {
            Some(ext) => format!("{stem} ({index}).{ext}"),
            None => format!("{stem} ({index})"),
        };
        let candidate = dir.join(next);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "too many copies of this name",
    ))
}

/// The drop was cancelled or refused. No target accepted the path, so the
/// staged name goes away. The original stays.
pub fn discard_export(source: &Path, staged: &Path) {
    unstage(source, staged);
    let _ = std::fs::remove_file(staged);
}

/// The target accepted a copy. `DoDragDrop` has returned, which means `Drop`
/// returned. That is not a promise the target has finished reading the path
/// or will not open it again, so the staged file stays in the export area.
pub fn retain_export(source: &Path, staged: &Path) {
    unstage(source, staged);
}

/// Removes idle export files older than a day. A file that is open stays,
/// however old it is. Young files stay even when idle, because a target may
/// still open them. Never deletes originals outside this folder.
pub fn sweep_exports(dir: &Path) {
    sweep_exports_older_than(dir, EXPORT_RETENTION);
}

fn sweep_exports_older_than(dir: &Path, max_age: std::time::Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let age = meta
            .modified()
            .ok()
            .and_then(|when| when.elapsed().ok())
            .unwrap_or_default();
        if age < max_age || file_busy(&path) {
            continue;
        }
        let _ = std::fs::remove_file(&path);
    }
}

fn file_busy(path: &Path) -> bool {
    #[cfg(target_os = "windows")]
    {
        windows::sharing_violation(path)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
        false
    }
}

/// How long the pointer must stay down on a file before a drag can leave the
/// window. A quick flick stays a click or a scroll.
pub const HOLD_TO_DRAG: Duration = Duration::from_millis(400);
/// How far the pointer must travel, once held, before the shell drag starts.
const DRAG_DISTANCE: f32 = 10.0;

/// What a drag gesture did on this frame.
#[derive(Clone, Copy)]
pub enum Nudge {
    Idle,
    /// The file is held or dragged but the shell drag has not started, or a
    /// long press just ended. Do not also open the file.
    Began,
    /// The target accepted a copy. The shell does not report where it wrote it.
    Accepted,
    /// The drag was cancelled or the target refused the drop.
    Refused,
    /// Staging failed. The text names Save a copy.
    Failed(&'static str),
}

/// Hold the pointer on a ready file, then drag it a short way, to start a copy
/// drag once. Until both the hold and the distance are met the gesture is
/// `Began`, so callers suppress the click and can paint `paint_hint`.
pub fn nudge(response: &egui::Response, path: &Path, name: &str) -> Nudge {
    let once = response.id.with("export-drag");
    let travel_id = response.id.with("export-travel");
    let press_id = response.id.with("export-press");
    let ctx = &response.ctx;
    if !response.dragged() {
        if response.is_pointer_button_down_on() && exportable(path, false) {
            let held = ctx.data_mut(|data| {
                data.get_temp_mut_or_insert_with(press_id, Instant::now)
                    .elapsed()
            });
            if held >= HOLD_TO_DRAG {
                return Nudge::Began;
            }
            // Wake when the hold is met so the hint shows without motion.
            ctx.request_repaint_after(HOLD_TO_DRAG - held);
            return Nudge::Idle;
        }
        let held = ctx.data_mut(|data| {
            let held = data.get_temp::<Instant>(press_id).map(|at| at.elapsed());
            data.remove::<bool>(once);
            data.remove::<f32>(travel_id);
            data.remove::<Instant>(press_id);
            held
        });
        // A long press released in place is not a click either.
        return if held.is_some_and(|held| held >= HOLD_TO_DRAG) {
            Nudge::Began
        } else {
            Nudge::Idle
        };
    }
    // Read before locking: `drag_delta` takes the context read lock, which
    // deadlocks inside `data_mut`'s write lock.
    let delta = response.drag_delta().length();
    let (travel, held, ran) = ctx.data_mut(|data| {
        let travel = data.get_temp::<f32>(travel_id).unwrap_or(0.0) + delta;
        data.insert_temp(travel_id, travel);
        let held = data
            .get_temp_mut_or_insert_with(press_id, Instant::now)
            .elapsed();
        (travel, held, data.get_temp::<bool>(once).unwrap_or(false))
    });
    if ran {
        return Nudge::Idle;
    }
    if held < HOLD_TO_DRAG || travel < DRAG_DISTANCE {
        if held < HOLD_TO_DRAG {
            ctx.request_repaint_after(HOLD_TO_DRAG - held);
        }
        return Nudge::Began;
    }
    ctx.data_mut(|data| data.insert_temp(once, true));
    if !exportable(path, false) {
        return Nudge::Failed("This file is not ready to drag. Use Save a copy after it finishes.");
    }
    match begin(path, name) {
        Ok(DragEnd::Copied) => Nudge::Accepted,
        Ok(DragEnd::Cancelled) => Nudge::Refused,
        Err(_) => Nudge::Failed(
            "Could not start the drag. The original is unchanged. Use Save a copy instead.",
        ),
    }
}

/// A small card beside the pointer while a file is held or dragged, before
/// the shell drag takes over. Uses the current visuals, not the app theme.
pub fn paint_hint(ui: &mut egui::Ui, response: &egui::Response, display_name: &str, detail: &str) {
    if !response.dragged() && !response.is_pointer_button_down_on() {
        return;
    }
    let Some(pointer) = ui.ctx().pointer_latest_pos() else {
        return;
    };
    let arming = ui.ctx().data(|data| {
        data.get_temp::<Instant>(response.id.with("export-press"))
            .is_none_or(|at| at.elapsed() < HOLD_TO_DRAG)
    });
    ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
    let weak = ui.visuals().weak_text_color();
    egui::Area::new(response.id.with("export-hint"))
        .order(egui::Order::Tooltip)
        .fixed_pos(pointer + egui::vec2(16.0, 16.0))
        .interactable(false)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_max_width(260.0);
                ui.add(egui::Label::new(egui::RichText::new(display_name).strong()).truncate());
                ui.label(egui::RichText::new(detail).small().color(weak));
                let status = if arming {
                    "Hold a moment, then drop it on a folder."
                } else {
                    "Release over a folder to copy."
                };
                ui.label(egui::RichText::new(status).small());
            });
        });
}

/// Start a native copy drag. Returns false when this platform has no shell
/// drag, or the file cannot be staged. Save as stays available either way.
pub fn begin(source: &Path, display_name: &str) -> std::io::Result<DragEnd> {
    let staged = stage(source, display_name)?;
    let end = platform_drag(&staged);
    match end {
        DragEnd::Copied => retain_export(source, &staged),
        DragEnd::Cancelled => discard_export(source, &staged),
    }
    Ok(end)
}

#[cfg(not(target_os = "windows"))]
fn platform_drag(_staged: &Path) -> DragEnd {
    DragEnd::Cancelled
}

#[cfg(target_os = "windows")]
fn platform_drag(staged: &Path) -> DragEnd {
    windows::drag(staged)
}

#[cfg(target_os = "windows")]
#[allow(unsafe_op_in_unsafe_fn)] // COM callbacks are unsafe for their whole body.
mod windows {
    use std::path::Path;
    use std::sync::atomic::{AtomicI32, Ordering};

    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::Memory::{
        GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock,
    };
    use windows_sys::Win32::System::Ole::{
        DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, DoDragDrop, OleInitialize, OleUninitialize,
    };
    use windows_sys::Win32::UI::Shell::DROPFILES;

    const S_OK: i32 = 0;
    const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;
    const E_NOINTERFACE: i32 = 0x8000_4002_u32 as i32;
    const E_NOTIMPL: i32 = 0x8000_4001_u32 as i32;
    const DV_E_FORMATETC: i32 = 0x8004_0064_u32 as i32;
    const DRAGDROP_S_USEDEFAULTCURSORS: i32 = 0x0004_0102;
    const CF_HDROP: u16 = 15;
    const TYMED_HGLOBAL: u32 = 1;
    #[repr(C)]
    struct FormatEtc {
        format: u16,
        ptd: *mut std::ffi::c_void,
        aspect: u32,
        index: i32,
        tymed: u32,
    }

    #[repr(C)]
    struct Medium {
        tymed: u32,
        data: *mut std::ffi::c_void,
        unknown: *mut std::ffi::c_void,
    }

    #[repr(C)]
    struct Guid {
        data1: u32,
        data2: u16,
        data3: u16,
        data4: [u8; 8],
    }

    const IID_UNKNOWN: Guid = Guid {
        data1: 0,
        data2: 0,
        data3: 0,
        data4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46],
    };
    const IID_DATA: Guid = Guid {
        data1: 0x0000_010e,
        data2: 0,
        data3: 0,
        data4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46],
    };
    const IID_SOURCE: Guid = Guid {
        data1: 0x0000_0121,
        data2: 0,
        data3: 0,
        data4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46],
    };

    #[repr(C)]
    struct Object {
        source_vtbl: *const SourceVtbl,
        data_vtbl: *const DataVtbl,
        count: AtomicI32,
        path: Vec<u16>,
    }

    #[repr(C)]
    struct SourceVtbl {
        query:
            unsafe extern "system" fn(*mut Object, *const Guid, *mut *mut std::ffi::c_void) -> i32,
        add: unsafe extern "system" fn(*mut Object) -> u32,
        release: unsafe extern "system" fn(*mut Object) -> u32,
        query_continue: unsafe extern "system" fn(*mut Object, i32, u32) -> i32,
        feedback: unsafe extern "system" fn(*mut Object, u32) -> i32,
    }

    #[repr(C)]
    struct DataVtbl {
        query:
            unsafe extern "system" fn(*mut Object, *const Guid, *mut *mut std::ffi::c_void) -> i32,
        add: unsafe extern "system" fn(*mut Object) -> u32,
        release: unsafe extern "system" fn(*mut Object) -> u32,
        get_data: unsafe extern "system" fn(*mut Object, *const FormatEtc, *mut Medium) -> i32,
        get_here: unsafe extern "system" fn(*mut Object, *const FormatEtc, *mut Medium) -> i32,
        query_data: unsafe extern "system" fn(*mut Object, *const FormatEtc) -> i32,
        canonical: unsafe extern "system" fn(*mut Object, *const FormatEtc, *mut FormatEtc) -> i32,
        set_data: unsafe extern "system" fn(*mut Object, *const FormatEtc, *mut Medium, i32) -> i32,
        enum_formats:
            unsafe extern "system" fn(*mut Object, u32, *mut *mut std::ffi::c_void) -> i32,
        advise: unsafe extern "system" fn(
            *mut Object,
            *const FormatEtc,
            u32,
            *mut std::ffi::c_void,
            *mut u32,
        ) -> i32,
        unadvise: unsafe extern "system" fn(*mut Object, u32) -> i32,
        enum_advise: unsafe extern "system" fn(*mut Object, *mut *mut std::ffi::c_void) -> i32,
    }

    unsafe extern "system" fn query_source(
        this: *mut Object,
        iid: *const Guid,
        out: *mut *mut std::ffi::c_void,
    ) -> i32 {
        query(this, iid, out, true)
    }

    unsafe fn as_object(data_this: *mut Object) -> *mut Object {
        data_this
            .cast::<u8>()
            .sub(std::mem::size_of::<*const DataVtbl>())
            .cast()
    }

    unsafe extern "system" fn query_data(
        this: *mut Object,
        iid: *const Guid,
        out: *mut *mut std::ffi::c_void,
    ) -> i32 {
        query(as_object(this), iid, out, false)
    }

    unsafe extern "system" fn data_add(this: *mut Object) -> u32 {
        add_ref(as_object(this))
    }

    unsafe extern "system" fn data_release(this: *mut Object) -> u32 {
        release(as_object(this))
    }

    unsafe fn query(
        this: *mut Object,
        iid: *const Guid,
        out: *mut *mut std::ffi::c_void,
        source: bool,
    ) -> i32 {
        if iid.is_null() || out.is_null() {
            return E_NOINTERFACE;
        }
        let iid = &*iid;
        let known = guid_eq(iid, &IID_UNKNOWN)
            || (source && guid_eq(iid, &IID_SOURCE))
            || (!source && guid_eq(iid, &IID_DATA));
        if !known {
            *out = std::ptr::null_mut();
            return E_NOINTERFACE;
        }
        (*this).count.fetch_add(1, Ordering::Relaxed);
        *out = if source {
            this.cast()
        } else {
            std::ptr::addr_of_mut!((*this).data_vtbl).cast()
        };
        S_OK
    }

    fn guid_eq(left: &Guid, right: &Guid) -> bool {
        left.data1 == right.data1
            && left.data2 == right.data2
            && left.data3 == right.data3
            && left.data4 == right.data4
    }

    unsafe extern "system" fn add_ref(this: *mut Object) -> u32 {
        (*this).count.fetch_add(1, Ordering::Relaxed) as u32 + 1
    }

    unsafe extern "system" fn release(this: *mut Object) -> u32 {
        let left = (*this).count.fetch_sub(1, Ordering::Release) - 1;
        if left == 0 {
            std::sync::atomic::fence(Ordering::Acquire);
            drop(Box::from_raw(this));
        }
        left as u32
    }

    unsafe extern "system" fn query_continue(_this: *mut Object, escape: i32, keys: u32) -> i32 {
        super::drag_decision(escape != 0, keys)
    }

    unsafe extern "system" fn feedback(_this: *mut Object, _effect: u32) -> i32 {
        DRAGDROP_S_USEDEFAULTCURSORS
    }

    unsafe extern "system" fn get_here(
        _this: *mut Object,
        _format: *const FormatEtc,
        _medium: *mut Medium,
    ) -> i32 {
        E_NOTIMPL
    }

    unsafe extern "system" fn canonical(
        _this: *mut Object,
        _in: *const FormatEtc,
        _out: *mut FormatEtc,
    ) -> i32 {
        E_NOTIMPL
    }

    unsafe extern "system" fn set_data(
        _this: *mut Object,
        _format: *const FormatEtc,
        _medium: *mut Medium,
        _release: i32,
    ) -> i32 {
        E_NOTIMPL
    }

    #[repr(C)]
    struct FormatEnum {
        vtbl: *const EnumVtbl,
        count: AtomicI32,
        index: i32,
    }

    #[repr(C)]
    struct EnumVtbl {
        query: unsafe extern "system" fn(
            *mut FormatEnum,
            *const Guid,
            *mut *mut std::ffi::c_void,
        ) -> i32,
        add: unsafe extern "system" fn(*mut FormatEnum) -> u32,
        release: unsafe extern "system" fn(*mut FormatEnum) -> u32,
        next: unsafe extern "system" fn(*mut FormatEnum, u32, *mut FormatEtc, *mut u32) -> i32,
        skip: unsafe extern "system" fn(*mut FormatEnum, u32) -> i32,
        reset: unsafe extern "system" fn(*mut FormatEnum) -> i32,
        clone: unsafe extern "system" fn(*mut FormatEnum, *mut *mut std::ffi::c_void) -> i32,
    }

    fn hdrop_format() -> FormatEtc {
        FormatEtc {
            format: CF_HDROP,
            ptd: std::ptr::null_mut(),
            aspect: 1,
            index: -1,
            tymed: TYMED_HGLOBAL,
        }
    }

    unsafe extern "system" fn enum_query(
        this: *mut FormatEnum,
        iid: *const Guid,
        out: *mut *mut std::ffi::c_void,
    ) -> i32 {
        const IID_ENUM: Guid = Guid {
            data1: 0x0000_0103,
            data2: 0,
            data3: 0,
            data4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46],
        };
        if iid.is_null()
            || out.is_null()
            || !(guid_eq(&*iid, &IID_UNKNOWN) || guid_eq(&*iid, &IID_ENUM))
        {
            if !out.is_null() {
                *out = std::ptr::null_mut();
            }
            return E_NOINTERFACE;
        }
        (*this).count.fetch_add(1, Ordering::Relaxed);
        *out = this.cast();
        S_OK
    }

    unsafe extern "system" fn enum_add(this: *mut FormatEnum) -> u32 {
        (*this).count.fetch_add(1, Ordering::Relaxed) as u32 + 1
    }

    unsafe extern "system" fn enum_release(this: *mut FormatEnum) -> u32 {
        let left = (*this).count.fetch_sub(1, Ordering::Release) - 1;
        if left == 0 {
            drop(Box::from_raw(this));
        }
        left as u32
    }

    unsafe extern "system" fn enum_next(
        this: *mut FormatEnum,
        count: u32,
        formats: *mut FormatEtc,
        fetched: *mut u32,
    ) -> i32 {
        if formats.is_null() || count == 0 {
            return E_INVALIDARG;
        }
        let give = (*this).index == 0 && count >= 1;
        if give {
            formats.write(hdrop_format());
            (*this).index = 1;
        }
        if !fetched.is_null() {
            fetched.write(u32::from(give));
        }
        if give { S_OK } else { 1 }
    }

    unsafe extern "system" fn enum_skip(this: *mut FormatEnum, count: u32) -> i32 {
        if (*this).index == 0 && count > 0 {
            (*this).index = 1;
            if count == 1 { S_OK } else { 1 }
        } else {
            1
        }
    }

    unsafe extern "system" fn enum_reset(this: *mut FormatEnum) -> i32 {
        (*this).index = 0;
        S_OK
    }

    unsafe extern "system" fn enum_clone(
        this: *mut FormatEnum,
        out: *mut *mut std::ffi::c_void,
    ) -> i32 {
        if out.is_null() {
            return E_INVALIDARG;
        }
        let copy = Box::into_raw(Box::new(FormatEnum {
            vtbl: &ENUM_VTBL,
            count: AtomicI32::new(1),
            index: (*this).index,
        }));
        *out = copy.cast();
        S_OK
    }

    const ENUM_VTBL: EnumVtbl = EnumVtbl {
        query: enum_query,
        add: enum_add,
        release: enum_release,
        next: enum_next,
        skip: enum_skip,
        reset: enum_reset,
        clone: enum_clone,
    };

    unsafe extern "system" fn enum_formats(
        _this: *mut Object,
        direction: u32,
        out: *mut *mut std::ffi::c_void,
    ) -> i32 {
        if out.is_null() {
            return E_INVALIDARG;
        }
        *out = std::ptr::null_mut();
        // DATADIR_GET. Targets such as Explorer list formats before DragEnter.
        if direction != 1 {
            return E_NOTIMPL;
        }
        let enumerator = Box::into_raw(Box::new(FormatEnum {
            vtbl: &ENUM_VTBL,
            count: AtomicI32::new(1),
            index: 0,
        }));
        *out = enumerator.cast();
        S_OK
    }

    unsafe extern "system" fn advise(
        _this: *mut Object,
        _format: *const FormatEtc,
        _flags: u32,
        _sink: *mut std::ffi::c_void,
        _connection: *mut u32,
    ) -> i32 {
        E_NOTIMPL
    }

    unsafe extern "system" fn unadvise(_this: *mut Object, _connection: u32) -> i32 {
        E_NOTIMPL
    }

    unsafe extern "system" fn enum_advise(
        _this: *mut Object,
        _out: *mut *mut std::ffi::c_void,
    ) -> i32 {
        E_NOTIMPL
    }

    fn accepts(format: *const FormatEtc) -> bool {
        if format.is_null() {
            return false;
        }
        let format = unsafe { &*format };
        format.format == CF_HDROP && format.tymed & TYMED_HGLOBAL != 0
    }

    unsafe extern "system" fn query_format(_this: *mut Object, format: *const FormatEtc) -> i32 {
        if accepts(format) {
            S_OK
        } else {
            DV_E_FORMATETC
        }
    }

    unsafe extern "system" fn get_data(
        this: *mut Object,
        format: *const FormatEtc,
        medium: *mut Medium,
    ) -> i32 {
        if !accepts(format) || medium.is_null() {
            return DV_E_FORMATETC;
        }
        // COM passes the data vtable, which is the second field.
        let this = as_object(this);
        let Some(handle) = hdrop(&(*this).path) else {
            return E_NOTIMPL;
        };
        *medium = Medium {
            tymed: TYMED_HGLOBAL,
            data: handle,
            unknown: std::ptr::null_mut(),
        };
        S_OK
    }

    fn hdrop(path: &[u16]) -> Option<*mut std::ffi::c_void> {
        let header = std::mem::size_of::<DROPFILES>();
        let bytes = header + (path.len() + 1) * 2;
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) };
        if handle.is_null() {
            return None;
        }
        let locked = unsafe { GlobalLock(handle) };
        if locked.is_null() {
            // The block stays allocated after a failed lock; free it here
            // so a failed drop does not leak an HGLOBAL per attempt.
            unsafe {
                GlobalFree(handle);
            }
            return None;
        }
        unsafe {
            let drop = locked.cast::<DROPFILES>();
            drop.write(DROPFILES {
                pFiles: header as u32,
                pt: Default::default(),
                fNC: 0,
                fWide: 1,
            });
            let files = locked.cast::<u8>().add(header).cast::<u16>();
            std::ptr::copy_nonoverlapping(path.as_ptr(), files, path.len());
            files.add(path.len()).write(0);
            GlobalUnlock(handle);
        }
        Some(handle)
    }

    const SOURCE_VTBL: SourceVtbl = SourceVtbl {
        query: query_source,
        add: add_ref,
        release,
        query_continue,
        feedback,
    };
    const DATA_VTBL: DataVtbl = DataVtbl {
        query: query_data,
        add: data_add,
        release: data_release,
        get_data,
        get_here,
        query_data: query_format,
        canonical,
        set_data,
        enum_formats,
        advise,
        unadvise,
        enum_advise,
    };

    pub fn sharing_violation(path: &Path) -> bool {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_SHARE_NONE, OPEN_EXISTING,
        };
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_GENERIC_READ,
                FILE_SHARE_NONE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if handle.is_null() || handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return true;
        }
        unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
        false
    }

    pub fn drag(path: &Path) -> super::DragEnd {
        const DRAGDROP_S_DROP: i32 = 0x0004_0100;
        let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let shown = absolute.to_string_lossy();
        let shown = shown.strip_prefix(r"\\?\").unwrap_or(shown.as_ref());
        let mut wide: Vec<u16> = std::ffi::OsStr::new(shown).encode_wide().collect();
        wide.push(0);
        let object = Box::new(Object {
            source_vtbl: &SOURCE_VTBL,
            data_vtbl: &DATA_VTBL,
            count: AtomicI32::new(1),
            path: wide,
        });
        let object = Box::into_raw(object);
        unsafe {
            if OleInitialize(std::ptr::null_mut()) < 0 {
                drop(Box::from_raw(object));
                return super::DragEnd::Cancelled;
            }
            let mut effect: DROPEFFECT = DROPEFFECT_NONE;
            let data = std::ptr::addr_of_mut!((*object).data_vtbl).cast();
            let source = object.cast();
            let result = DoDragDrop(data, source, DROPEFFECT_COPY, &mut effect);
            release(object);
            OleUninitialize();
            // Drop has returned or the drag was cancelled. Neither code says
            // the target has finished reading the file.
            if (result == 0 || result == DRAGDROP_S_DROP) && effect & DROPEFFECT_COPY != 0 {
                return super::DragEnd::Copied;
            }
        }
        super::DragEnd::Cancelled
    }

    #[cfg(test)]
    pub fn exercise_contract(path: &std::path::Path) -> (u16, i32, i32, i32) {
        let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let shown = absolute.to_string_lossy();
        let shown = shown.strip_prefix(r"\\?\").unwrap_or(shown.as_ref());
        let mut wide: Vec<u16> = std::ffi::OsStr::new(shown).encode_wide().collect();
        wide.push(0);
        let object = Box::into_raw(Box::new(Object {
            source_vtbl: &SOURCE_VTBL,
            data_vtbl: &DATA_VTBL,
            count: AtomicI32::new(1),
            path: wide,
        }));
        let data = unsafe { std::ptr::addr_of_mut!((*object).data_vtbl).cast() };
        let held = super::drag_decision(false, 0x0001);
        let released = super::drag_decision(false, 0);
        let escaped = super::drag_decision(true, 0x0001);
        let mut enumerator = std::ptr::null_mut();
        let listed = unsafe { enum_formats(object, 1, &mut enumerator) };
        let mut format = FormatEtc {
            format: 0,
            ptd: std::ptr::null_mut(),
            aspect: 0,
            index: 0,
            tymed: 0,
        };
        let mut fetched = 0u32;
        unsafe {
            let next = (*enumerator.cast::<FormatEnum>()).vtbl;
            let _ = ((*next).next)(enumerator.cast(), 1, &mut format, &mut fetched);
            ((*next).release)(enumerator.cast());
        }
        let query = unsafe { query_format(data, &format) };
        let mut medium = Medium {
            tymed: 0,
            data: std::ptr::null_mut(),
            unknown: std::ptr::null_mut(),
        };
        let got = unsafe { get_data(data, &format, &mut medium) };
        unsafe { release(object) };
        assert_eq!(held, 0);
        assert_eq!(released, 0x0004_0100);
        assert_eq!(escaped, 0x0004_0101);
        assert_eq!(listed, S_OK);
        assert_eq!(format.format, CF_HDROP);
        assert_eq!(query, S_OK);
        assert_eq!(got, S_OK);
        assert!(!medium.data.is_null());
        (format.format, query, got, held)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs one frame with `events` and returns what `nudge` said about a
    /// file tile at the top left. The path does not exist, so a gesture that
    /// passes the gate ends in `Failed` instead of a native drag.
    fn frame(ctx: &egui::Context, events: Vec<egui::Event>) -> Nudge {
        let mut result = Nudge::Idle;
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 400.0),
            )),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            let (_, response) =
                ui.allocate_exact_size(egui::vec2(200.0, 200.0), egui::Sense::click_and_drag());
            result = nudge(&response, Path::new("missing-drag-file.bin"), "file.bin");
        });
        output.textures_delta.clear();
        result
    }

    fn press(pos: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        }
    }

    #[test]
    fn the_hold_gate_is_between_300_and_500_ms() {
        assert!(HOLD_TO_DRAG >= Duration::from_millis(300));
        assert!(HOLD_TO_DRAG <= Duration::from_millis(500));
    }

    #[test]
    fn a_quick_flick_arms_but_never_starts_the_shell_drag() {
        let ctx = egui::Context::default();
        let start = egui::pos2(50.0, 50.0);
        frame(&ctx, vec![egui::Event::PointerMoved(start)]);
        frame(&ctx, vec![press(start, true)]);
        let mut seen = Vec::new();
        for step in 1..6 {
            let at = start + egui::vec2(step as f32 * 12.0, 0.0);
            seen.push(frame(&ctx, vec![egui::Event::PointerMoved(at)]));
        }
        assert!(seen.iter().any(|nudge| matches!(nudge, Nudge::Began)));
        assert!(
            seen.iter()
                .all(|nudge| matches!(nudge, Nudge::Idle | Nudge::Began))
        );
        frame(&ctx, vec![press(start + egui::vec2(60.0, 0.0), false)]);
    }

    #[test]
    fn a_held_drag_that_travels_reaches_the_export() {
        let ctx = egui::Context::default();
        let start = egui::pos2(50.0, 50.0);
        frame(&ctx, vec![egui::Event::PointerMoved(start)]);
        frame(&ctx, vec![press(start, true)]);
        let first = frame(
            &ctx,
            vec![egui::Event::PointerMoved(start + egui::vec2(12.0, 0.0))],
        );
        assert!(matches!(first, Nudge::Began));
        std::thread::sleep(HOLD_TO_DRAG + Duration::from_millis(50));
        let mut ended = Nudge::Idle;
        for step in 2..5 {
            let at = start + egui::vec2(step as f32 * 12.0, 0.0);
            let nudge = frame(&ctx, vec![egui::Event::PointerMoved(at)]);
            if matches!(nudge, Nudge::Failed(_)) {
                ended = nudge;
            }
        }
        assert!(matches!(ended, Nudge::Failed(_)));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn the_drop_object_lists_hdrop_and_answers_get_data() {
        let dir = isolated("hdrop");
        let source = dir.join("relatório.bin");
        std::fs::write(&source, b"abc").unwrap();
        let staged = stage(&source, "relatório.bin").unwrap();
        let _ = windows::exercise_contract(&staged);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn releasing_the_button_drops_and_escape_cancels() {
        assert_eq!(drag_decision(true, 0x0001), 0x0004_0101);
        assert_eq!(drag_decision(false, 0), 0x0004_0100);
        assert_eq!(drag_decision(false, 0x0001), 0);
    }

    #[test]
    fn names_keep_accents_and_drop_characters_explorer_rejects() {
        assert_eq!(export_name("relatório.pdf"), "relatório.pdf");
        assert_eq!(export_name("a<b>:c?.pdf"), "a_b__c_.pdf");
        assert_eq!(export_name("CON.txt"), "CON_.txt");
        assert_eq!(export_name("   "), "file");
    }

    #[test]
    fn missing_and_empty_files_do_not_stage() {
        let dir = std::env::temp_dir().join(format!("zapext-drag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("gone.bin");
        assert!(!exportable(&missing, false));
        assert!(stage(&missing, "gone.bin").is_err());
        let empty = dir.join("empty.bin");
        std::fs::write(&empty, []).unwrap();
        assert!(!exportable(&empty, false));
        assert!(!exportable(&empty, true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_large_file_is_linked_without_changing_the_original() {
        let dir = std::env::temp_dir().join(format!("zapext-drag-big-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("vídeo grande.bin");
        let bytes = vec![7u8; 8 * 1024 * 1024];
        std::fs::write(&source, &bytes).unwrap();
        let staged = stage(&source, "vídeo grande.bin").unwrap();
        let again = stage(&source, "vídeo grande.bin").unwrap();
        assert!(
            staged.exists(),
            "a second name must not delete the first link"
        );
        assert_ne!(staged, again);
        assert!(leased().iter().any(|path| path == &source));
        assert_eq!(
            std::fs::metadata(&staged).unwrap().len(),
            bytes.len() as u64
        );
        assert_eq!(
            std::fs::metadata(&source).unwrap().len(),
            bytes.len() as u64
        );
        let mut head = [0u8; 1];
        use std::io::Read;
        std::fs::File::open(&source)
            .unwrap()
            .read_exact(&mut head)
            .unwrap();
        // The original's first byte is unchanged and the link is another name.
        assert_eq!(head[0], 7);
        assert_ne!(staged, source);
        unstage(&source, &staged);
        unstage(&source, &again);
        assert!(!leased().iter().any(|path| path == &source));
        let _ = std::fs::remove_file(&staged);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn isolated(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("zapext-drag-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn age(path: &Path, age: std::time::Duration) {
        let when = std::time::SystemTime::now() - age;
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }

    #[test]
    fn cancel_removes_the_export_and_keeps_the_original() {
        let dir = isolated("cancel");
        let source = dir.join("origem.bin");
        std::fs::write(&source, b"abc").unwrap();
        let staged = stage(&source, "origem.bin").unwrap();
        discard_export(&source, &staged);
        assert_eq!(std::fs::read(&source).unwrap(), b"abc");
        assert!(!staged.exists());
        assert!(std::fs::File::open(&staged).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_copy_stays_openable_late_and_again_until_later_cleanup() {
        let dir = isolated("late");
        let source = dir.join("origem.bin");
        std::fs::write(&source, b"payload-bytes").unwrap();
        let staged = stage(&source, "origem.bin").unwrap();
        retain_export(&source, &staged);
        let export_dir = staged.parent().unwrap();
        sweep_exports_older_than(export_dir, EXPORT_RETENTION);
        let mut first = std::fs::File::open(&staged).unwrap();
        let mut buf = Vec::new();
        use std::io::Read;
        first.read_to_end(&mut buf).unwrap();
        drop(first);
        assert_eq!(buf, b"payload-bytes");
        assert_eq!(std::fs::read(&staged).unwrap(), b"payload-bytes");
        assert_eq!(std::fs::read(&source).unwrap(), b"payload-bytes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_long_transfer_is_kept_and_idle_residue_is_removed_later() {
        let dir = isolated("long");
        let source = dir.join("origem.bin");
        std::fs::write(&source, b"payload-bytes").unwrap();
        let staged = stage(&source, "origem.bin").unwrap();
        retain_export(&source, &staged);
        age(
            &staged,
            EXPORT_RETENTION + std::time::Duration::from_secs(5),
        );
        let mut held = std::fs::File::open(&staged).unwrap();
        let export_dir = staged.parent().unwrap().to_path_buf();
        sweep_exports_older_than(&export_dir, EXPORT_RETENTION);
        assert!(staged.exists(), "an open transfer is not residue");
        let mut buf = Vec::new();
        use std::io::Read;
        held.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"payload-bytes");
        drop(held);
        sweep_exports_older_than(&export_dir, EXPORT_RETENTION);
        assert!(!staged.exists(), "idle residue goes on a later sweep");
        assert_eq!(std::fs::read(&source).unwrap(), b"payload-bytes");
        let fresh = dir.join("novo.bin");
        std::fs::write(&fresh, b"new").unwrap();
        let young = stage(&fresh, "outro.bin").unwrap();
        retain_export(&fresh, &young);
        sweep_exports_older_than(young.parent().unwrap(), EXPORT_RETENTION);
        assert!(young.exists(), "a fresh export is not residue");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
