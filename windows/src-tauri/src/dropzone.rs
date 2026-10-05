// Coucou's own OLE drop target for the island.
//
// wry registers a drop target on the WebView2 windows, but on this machine a
// file dragged from Explorer onto the island got the red "no drop" cursor and
// not one event reached the app. Rather than trust a target that stays silent,
// the island gets its own:
//
// - paths come from the shell (`SHCreateShellItemArrayFromDataObject`), which
//   understands every kind of Explorer drag, not only a plain CF_HDROP;
// - the formats the source offers are logged on every drag, so a drag that is
//   refused says why;
// - events go to the island as `file-drop`, with the same shape Tauri's
//   `onDragDropEvent` used to deliver.

use std::cell::Cell;
use std::path::PathBuf;

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use windows::core::{implement, Ref};
use windows::Win32::Foundation::{HWND, POINTL};
use windows::Win32::System::Com::{CoTaskMemFree, IDataObject, DATADIR_GET};
use windows::Win32::System::DataExchange::GetClipboardFormatNameW;
use windows::Win32::System::Ole::{
    IDropTarget, IDropTarget_Impl, RegisterDragDrop, RevokeDragDrop, DROPEFFECT, DROPEFFECT_COPY,
    DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::{
    IShellItemArray, SHCreateShellItemArrayFromDataObject, SIGDN_FILESYSPATH,
};

use crate::island::WINDOW_LABEL;
use crate::log;

#[derive(Serialize, Clone)]
struct FileDrop {
    #[serde(rename = "type")]
    kind: &'static str,
    paths: Vec<PathBuf>,
}

fn emit(app: &AppHandle, kind: &'static str, paths: Vec<PathBuf>) {
    let _ = app.emit_to(WINDOW_LABEL, "file-drop", FileDrop { kind, paths });
}

/// File-system paths carried by a drag, whatever shape the source packed them in.
fn paths_from(data: &IDataObject) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(items) = (unsafe { SHCreateShellItemArrayFromDataObject::<_, IShellItemArray>(data) })
    else {
        return out;
    };
    let count = unsafe { items.GetCount() }.unwrap_or(0);
    for i in 0..count {
        let Ok(item) = (unsafe { items.GetItemAt(i) }) else { continue };
        let Ok(name) = (unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }) else { continue };
        let path = unsafe { name.to_string() };
        unsafe { CoTaskMemFree(Some(name.0 as *const _)) };
        if let Ok(path) = path {
            out.push(PathBuf::from(path));
        }
    }
    out
}

/// "49161 Shell IDList Array, 15, …" — what the drag source offers.
fn formats_of(data: &IDataObject) -> String {
    let Ok(list) = (unsafe { data.EnumFormatEtc(DATADIR_GET.0 as u32) }) else {
        return "unreadable".into();
    };
    let mut names = Vec::new();
    loop {
        let mut fmt = [Default::default()];
        let mut fetched = 0u32;
        if unsafe { list.Next(&mut fmt, Some(&mut fetched)) }.is_err() || fetched == 0 {
            break;
        }
        let id = fmt[0].cfFormat as u32;
        let mut buf = [0u16; 64];
        let len = unsafe { GetClipboardFormatNameW(id, &mut buf) };
        names.push(if len > 0 {
            format!("{id} {}", String::from_utf16_lossy(&buf[..len as usize]))
        } else {
            id.to_string()
        });
    }
    names.join(", ")
}

#[implement(IDropTarget)]
struct Target {
    app: AppHandle,
    valid: Cell<bool>,
}

#[allow(non_snake_case)]
impl IDropTarget_Impl for Target_Impl {
    fn DragEnter(
        &self,
        data: Ref<'_, IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        // First line on purpose: if reading the data below stalls, this still shows.
        log::line("drop target: DragEnter called".to_string());
        let (paths, formats) = match data.as_ref() {
            Some(d) => (paths_from(d), formats_of(d)),
            None => (Vec::new(), "no data".into()),
        };
        log::line(format!("drop target: enter, {} path(s); offers: {formats}", paths.len()));
        let valid = !paths.is_empty();
        self.valid.set(valid);
        unsafe { *effect = if valid { DROPEFFECT_COPY } else { DROPEFFECT_NONE } };
        if valid {
            emit(&self.app, "enter", paths);
        }
        Ok(())
    }

    fn DragOver(
        &self,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        unsafe { *effect = if self.valid.get() { DROPEFFECT_COPY } else { DROPEFFECT_NONE } };
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        if self.valid.replace(false) {
            emit(&self.app, "leave", Vec::new());
        }
        Ok(())
    }

    fn Drop(
        &self,
        data: Ref<'_, IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let paths = data.as_ref().map(paths_from).unwrap_or_default();
        log::line(format!("drop target: drop, {} path(s)", paths.len()));
        self.valid.set(false);
        unsafe { *effect = if paths.is_empty() { DROPEFFECT_NONE } else { DROPEFFECT_COPY } };
        if !paths.is_empty() {
            emit(&self.app, "drop", paths);
        }
        Ok(())
    }
}

/// Puts our target on every window in `hwnds`, replacing whatever was there.
/// Must run on the thread that owns the windows (the main thread). Windows the
/// call is not allowed to touch are skipped — they are other processes' and do
/// not matter, because the lookup walks up to the first window with a target.
pub fn install(app: &AppHandle, hwnds: &[HWND]) {
    let target: IDropTarget = Target { app: app.clone(), valid: Cell::new(false) }.into();
    let mut ok = 0;
    let mut failed = Vec::new();
    for &hwnd in hwnds {
        let _ = unsafe { RevokeDragDrop(hwnd) };
        match unsafe { RegisterDragDrop(hwnd, &target) } {
            Ok(()) => ok += 1,
            Err(err) => failed.push(format!("{:#x}: {err}", hwnd.0 as usize)),
        }
    }
    log::line(format!(
        "drop target: registered on {ok}/{} window(s){}",
        hwnds.len(),
        if failed.is_empty() { String::new() } else { format!(", failed: {}", failed.join("; ")) }
    ));
}
