#![allow(unexpected_cfgs)]

use std::cmp::Ordering;
use std::ffi::{c_void, CStr, CString};
use std::mem::zeroed;
use std::os::raw::c_int;
use std::path::Path;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::libui;
use crate::serviceclient::ServiceClient;

const WINDOW_SIZE_X: c_int = 600;
const WINDOW_SIZE_Y: c_int = 480;

const ROW_H: f64 = 22.0;
const PAD_L: f64 = 8.0;
const BUTTON_W: f64 = 88.0;
const EDGE_W: f64 = 8.0;

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct NSPoint {
    x: f64,
    y: f64,
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct NSSize {
    width: f64,
    height: f64,
}

#[derive(Clone)]
struct Row {
    id: String,
    name: String,
    status: String,
    settings: String,
    joined: bool,
}

/// A per-row action that could not be run inline (because the service client
/// was busy) and is applied from the timer tick instead.
enum RowAction {
    /// Leave the service and keep the network remembered so it can be
    /// reconnected later. This is what the tray calls "Disconnect".
    Disconnect {
        id: String,
        name: String,
        settings: String,
    },
    Reconnect { id: String, settings: String },
}

struct State {
    client: Arc<Mutex<ServiceClient>>,
    dirty_flag: Arc<AtomicBool>,
    search: String,
    sort_by_name: bool,
    area: *mut libui::uiArea,
    rows: Vec<Row>,
    content_w: f64,
    content_h: i32,
    pending: Vec<RowAction>,
    rebuild_needed: bool,
}

static mut G_STATE: *mut State = null_mut();
static mut G_WINDOW: *mut libui::uiWindow = null_mut();

fn geometry_path() -> String {
    Path::new(crate::NETWORK_CACHE_PATH.as_str())
        .parent()
        .map(|d| d.join("networks_geometry.json"))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| crate::NETWORK_CACHE_PATH.to_string())
}

/// Actual visible width of the scrolling area's viewport. On macOS we read it
/// directly from the NSScrollView so the drawn content exactly matches the
/// usable width (scrollbars etc. included). Returns <= 0 when unknown.
#[cfg(target_os = "macos")]
unsafe fn area_viewport_width(area: *mut libui::uiArea) -> f64 {
    use objc::{msg_send, sel, sel_impl};
    let handle = libui::uiControlHandle(area.cast());
    if handle == 0 {
        return 0.0;
    }
    let sv = handle as *mut objc::runtime::Object;
    let sz: NSSize = msg_send![sv, contentSize];
    sz.width
}

#[cfg(not(target_os = "macos"))]
unsafe fn area_viewport_width(_area: *mut libui::uiArea) -> f64 {
    0.0
}

#[cfg(target_os = "macos")]
fn window_frame_origin(window: *mut libui::uiWindow) -> Option<(f64, f64)> {
    unsafe {
        use objc::{msg_send, sel, sel_impl};
        let handle = libui::uiControlHandle(window.cast());
        if handle == 0 {
            return None;
        }
        let win = handle as *mut objc::runtime::Object;
        let p: NSPoint = msg_send![win, frameOrigin];
        Some((p.x, p.y))
    }
}

#[cfg(target_os = "macos")]
fn window_set_frame_origin(window: *mut libui::uiWindow, x: f64, y: f64) {
    unsafe {
        use objc::{msg_send, sel, sel_impl};
        let handle = libui::uiControlHandle(window.cast());
        if handle == 0 {
            return;
        }
        let win = handle as *mut objc::runtime::Object;
        let _: () = msg_send![win, setFrameOrigin: NSPoint { x, y }];
    }
}

fn save_geometry(window: *mut libui::uiWindow) {
    unsafe {
        let mut w: c_int = 0;
        let mut h: c_int = 0;
        libui::uiWindowContentSize(window, &mut w, &mut h);
        #[cfg(target_os = "macos")]
        let (x, y) = window_frame_origin(window).unwrap_or((0.0, 0.0));
        #[cfg(not(target_os = "macos"))]
        let (x, y) = (0.0f64, 0.0f64);
        let geom = serde_json::json!({ "x": x, "y": y, "width": w, "height": h });
        let _ = std::fs::write(geometry_path(), serde_json::to_vec(&geom).unwrap());
    }
}

fn load_geometry() -> Option<(f64, f64, c_int, c_int)> {
    let data = std::fs::read(geometry_path()).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&data).ok()?;
    let x = v.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let y = v.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let w = v.get("width").and_then(|v| v.as_i64()).unwrap_or(0) as c_int;
    let h = v.get("height").and_then(|v| v.as_i64()).unwrap_or(0) as c_int;
    if w >= 200 && h >= 200 {
        Some((x, y, w, h))
    } else {
        None
    }
}

fn compute_rows(client: &ServiceClient, search: &str, sort_by_name: bool) -> Vec<Row> {
    let networks = client.networks();
    let saved = client.saved_networks();

    let query = search.trim().to_lowercase();
    let mut rows: Vec<Row> = Vec::new();

    for (id, obj) in networks.iter() {
        let name = obj
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let status = obj
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if !query.is_empty()
            && !id.to_lowercase().contains(&query)
            && !name.to_lowercase().contains(&query)
        {
            continue;
        }
        rows.push(Row {
            id: id.clone(),
            name,
            status,
            settings: serde_json::to_string(obj).unwrap_or_default(),
            joined: true,
        });
    }

    let joined_ids: Vec<&String> = networks.iter().map(|(id, _)| id).collect();
    for (id, name, settings) in saved.iter() {
        if joined_ids.iter().any(|j| *j == id) {
            continue;
        }
        if !query.is_empty()
            && !id.to_lowercase().contains(&query)
            && !name.to_lowercase().contains(&query)
        {
            continue;
        }
        rows.push(Row {
            id: id.clone(),
            name: name.clone(),
            status: "(not joined)".into(),
            settings: settings.clone(),
            joined: false,
        });
    }

    if sort_by_name {
        rows.sort_by(|a, b| {
            let an = &a.name;
            let bn = &b.name;
            match (an.is_empty(), bn.is_empty()) {
                (true, true) => a.id.cmp(&b.id),
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => an.cmp(bn).then_with(|| a.id.cmp(&b.id)),
            }
        });
    } else {
        rows.sort_by(|a, b| a.id.cmp(&b.id));
    }
    rows
}

fn truncate_name(name: &str, max_chars: usize) -> String {
    let mut out: String = name.chars().take(max_chars).collect();
    if name.chars().count() > max_chars {
        out.push('…');
    }
    out
}

fn row_label(r: &Row) -> String {
    let nm = if r.name.is_empty() {
        String::new()
    } else {
        truncate_name(&r.name, 40)
    };
    if nm.is_empty() {
        format!("{}   [{}]", r.id, r.status)
    } else {
        format!("{}   {}   [{}]", nm, r.id, r.status)
    }
}

fn update_area(state: &mut State) {
    let nrows = state.rows.len() as i32;
    let h = if nrows == 0 { 60 } else { nrows * (ROW_H as i32) };
    if state.content_h != h {
        state.content_h = h;
        unsafe {
            libui::uiAreaSetSize(state.area, state.content_w as c_int, h);
        }
    }
    unsafe {
        libui::uiAreaQueueRedrawAll(state.area);
    }
}

unsafe fn load_font() -> libui::uiFontDescriptor {
    let mut d: libui::uiFontDescriptor = zeroed();
    libui::uiLoadControlFont(&mut d);
    d
}

unsafe fn make_layout(
    font: &libui::uiFontDescriptor,
    text: &str,
    width: f64,
    r: f64,
    g: f64,
    b: f64,
) -> (*mut libui::uiDrawTextLayout, *mut libui::uiAttributedString) {
    let c = CString::new(text).unwrap();
    let s = libui::uiNewAttributedString(c.as_ptr());
    let len = text.len();
    let attr = libui::uiNewColorAttribute(r, g, b, 1.0);
    // The attributed string takes ownership of the attribute.
    libui::uiAttributedStringSetAttribute(s, attr, 0, len);
    let mut p: libui::uiDrawTextLayoutParams = zeroed();
    p.String = s;
    p.DefaultFont = font as *const _ as *mut _;
    p.Width = width;
    p.Align = libui::uiDrawTextAlignLeft;
    let tl = libui::uiDrawNewTextLayout(&mut p);
    // The layout references the attributed string; it must outlive the layout.
    (tl, s)
}

unsafe fn fill_rect(ctx: *mut libui::uiDrawContext, x: f64, y: f64, w: f64, h: f64, r: f64, g: f64, b: f64, a: f64) {
    let path = libui::uiDrawNewPath(libui::uiDrawFillModeWinding);
    libui::uiDrawPathAddRectangle(path, x, y, w, h);
    libui::uiDrawPathEnd(path);
    let mut brush: libui::uiDrawBrush = zeroed();
    brush.Type = libui::uiDrawBrushTypeSolid;
    brush.R = r;
    brush.G = g;
    brush.B = b;
    brush.A = a;
    libui::uiDrawFill(ctx, path, &mut brush);
    libui::uiDrawFreePath(path);
}

unsafe fn draw_text(
    ctx: *mut libui::uiDrawContext,
    font: &libui::uiFontDescriptor,
    text: &str,
    x: f64,
    y_center: f64,
    max_w: f64,
    r: f64,
    g: f64,
    b: f64,
) {
    let (tl, s) = make_layout(font, text, max_w.max(1.0), r, g, b);
    let mut tw: f64 = 0.0;
    let mut th: f64 = 0.0;
    libui::uiDrawTextLayoutExtents(tl, &mut tw, &mut th);
    let _ = tw;
    let top = y_center - th / 2.0;
    libui::uiDrawText(ctx, tl, x, top);
    libui::uiDrawFreeTextLayout(tl);
    libui::uiFreeAttributedString(s);
}

unsafe fn draw_text_right(
    ctx: *mut libui::uiDrawContext,
    font: &libui::uiFontDescriptor,
    text: &str,
    right_x: f64,
    y_center: f64,
    r: f64,
    g: f64,
    b: f64,
) {
    let (tl, s) = make_layout(font, text, 4000.0, r, g, b);
    let mut tw: f64 = 0.0;
    let mut th: f64 = 0.0;
    libui::uiDrawTextLayoutExtents(tl, &mut tw, &mut th);
    libui::uiDrawText(ctx, tl, right_x - tw, y_center - th / 2.0);
    libui::uiDrawFreeTextLayout(tl);
    libui::uiFreeAttributedString(s);
}

unsafe extern "C" fn area_draw(
    _ah: *mut libui::uiAreaHandler,
    _area: *mut libui::uiArea,
    dp: *mut libui::uiAreaDrawParams,
) {
    let state = &mut *G_STATE;
    let dp = &*dp;
    let ctx = dp.Context;

    // Keep the drawn content width matched to the actual viewport so the
    // rightmost action column always fits (no horizontal overflow).
    let vw = area_viewport_width(state.area);
    if vw >= 60.0 && (vw - state.content_w).abs() > 0.5 {
        state.content_w = vw;
        libui::uiAreaSetSize(state.area, state.content_w as c_int, state.content_h);
    }

    let mut font = load_font();

    // Background for the visible band.
    fill_rect(
        ctx,
        0.0,
        dp.ClipY,
        state.content_w.max(dp.ClipWidth),
        dp.ClipHeight,
        1.0,
        1.0,
        1.0,
        1.0,
    );

    if state.rows.is_empty() {
        draw_text(
            ctx,
            &font,
            if state.search.is_empty() {
                "No networks found"
            } else {
                "No networks match your search"
            },
            PAD_L,
            dp.ClipY + 12.0,
            state.content_w - PAD_L - EDGE_W,
            0.4,
            0.4,
            0.4,
        );
        libui::uiFreeFontDescriptor(&mut font);
        return;
    }

    let first = (dp.ClipY / ROW_H).floor().max(0.0) as usize;
    let last = (((dp.ClipY + dp.ClipHeight) / ROW_H).ceil() as usize).min(state.rows.len());

    let text_region_w = state.content_w - PAD_L - EDGE_W - 2.0 * BUTTON_W;

    for i in first..last {
        let row = &state.rows[i];
        let y = i as f64 * ROW_H;

        // Alternate row shading.
        if i % 2 == 0 {
            fill_rect(ctx, 0.0, y, state.content_w, ROW_H, 0.95, 0.95, 0.95, 1.0);
        }

        // Separator line.
        fill_rect(ctx, 0.0, y + ROW_H - 1.0, state.content_w, 1.0, 0.88, 0.88, 0.88, 1.0);

        // Main text.
        let label = row_label(row);
        draw_text(
            ctx,
            &font,
            &label,
            PAD_L,
            y + ROW_H / 2.0,
            text_region_w,
            0.0,
            0.0,
            0.0,
        );

        // Action buttons (right-aligned text, one cell each).
        let actions: [&str; 2] = if row.joined {
            ["Copy", "Disconnect"]
        } else {
            ["Copy", "Reconnect"]
        };
        for (j, act) in actions.iter().enumerate() {
            let cell_right = state.content_w - EDGE_W - (actions.len() - 1 - j) as f64 * BUTTON_W;
            draw_text_right(
                ctx,
                &font,
                act,
                cell_right - 4.0,
                y + ROW_H / 2.0,
                0.0,
                0.45,
                0.9,
            );
        }
    }

    libui::uiFreeFontDescriptor(&mut font);
}

unsafe extern "C" fn area_mouse(
    _ah: *mut libui::uiAreaHandler,
    _area: *mut libui::uiArea,
    me: *mut libui::uiAreaMouseEvent,
) {
    let state = &mut *G_STATE;
    let me = &*me;
    if me.Down != 1 || me.Y < 0.0 {
        return;
    }
    let idx = (me.Y / ROW_H) as usize;
    if idx >= state.rows.len() {
        return;
    }
    let x = me.X;
    let x0 = state.content_w - EDGE_W - 2.0 * BUTTON_W;
    let col = if x >= x0 && x < x0 + BUTTON_W {
        Some(0)
    } else if x >= x0 + BUTTON_W && x <= state.content_w - EDGE_W {
        Some(1)
    } else {
        None
    };
    let col = match col {
        Some(c) => c,
        None => return,
    };

    let row = state.rows[idx].clone();

    // col 0 is always "Copy"; col 1 is Disconnect/Reconnect. Row actions are
    // queued and applied from the timer tick so this handler never blocks the
    // UI thread on the service client lock (the background sync thread may
    // hold it for a while). No notifications are shown from this window: the
    // row list itself provides the feedback.
    if col == 0 {
        crate::copy_to_clipboard(row.id.as_str());
        return;
    }
    if row.joined {
        state.pending.push(RowAction::Disconnect {
            id: row.id,
            name: row.name,
            settings: row.settings,
        });
    } else {
        state.pending.push(RowAction::Reconnect {
            id: row.id,
            settings: row.settings,
        });
    }
    state.rebuild_needed = true;
}

unsafe extern "C" fn area_mouse_crossed(
    _ah: *mut libui::uiAreaHandler,
    _area: *mut libui::uiArea,
    _left: c_int,
) {
}

unsafe extern "C" fn area_drag_broken(_ah: *mut libui::uiAreaHandler, _area: *mut libui::uiArea) {}

unsafe extern "C" fn area_key(
    _ah: *mut libui::uiAreaHandler,
    _area: *mut libui::uiArea,
    _ke: *mut libui::uiAreaKeyEvent,
) -> c_int {
    0
}

/// Recompute the row list and repaint, but only if the service client lock is
/// free right now. Returns false when the background sync thread is holding
/// the lock; callers keep the rebuild flag set and retry on the next timer
/// tick. This keeps the UI thread responsive even while a (slow) service sync
/// is running.
fn try_rebuild(state: &mut State) -> bool {
    let Some(guard) = state.client.try_lock() else {
        return false;
    };
    let rows = compute_rows(&guard, &state.search, state.sort_by_name);
    drop(guard);
    state.rows = rows;
    state.rebuild_needed = false;
    update_area(state);
    true
}

unsafe extern "C" fn on_search_changed(e: *mut libui::uiEntry, data: *mut c_void) {
    let state = &mut *(data as *mut State);
    let text = libui::uiEntryText(e);
    state.search = if text.is_null() {
        String::new()
    } else {
        CStr::from_ptr(text.cast()).to_string_lossy().to_string()
    };
    state.rebuild_needed = true;
}

unsafe extern "C" fn on_sort_changed(c: *mut libui::uiCombobox, data: *mut c_void) {
    let state = &mut *(data as *mut State);
    state.sort_by_name = libui::uiComboboxSelected(c) == 1;
    state.rebuild_needed = true;
}

unsafe extern "C" fn on_timer(data: *mut c_void) -> c_int {
    let state = &mut *(data as *mut State);

    // Apply queued row actions as soon as the service client lock is free.
    if !state.pending.is_empty() {
        if let Some(mut guard) = state.client.try_lock() {
            let actions = std::mem::take(&mut state.pending);
            for action in actions {
                match action {
                    RowAction::Disconnect {
                        id,
                        name,
                        settings,
                    } => {
                        guard.enqueue_delete(format!("network/{}", id));
                        guard.remember_network(id, name, settings);
                    }
                    RowAction::Reconnect { id, settings } => {
                        guard.enqueue_post(format!("network/{}", id), settings);
                    }
                }
            }
            drop(guard);
            state.rebuild_needed = true;
        }
    }

    // Repaint whenever new data arrived or a queued action/control change
    // needs it, without ever blocking the UI thread on the client lock.
    if state.rebuild_needed || state.dirty_flag.load(AtomicOrdering::Relaxed) {
        if try_rebuild(state) {
            state.dirty_flag.store(false, AtomicOrdering::Relaxed);
        } else {
            state.rebuild_needed = true;
        }
    }
    1
}

unsafe extern "C" fn on_content_size_changed(w: *mut libui::uiWindow, data: *mut c_void) {
    let state = &mut *(data as *mut State);
    // Prefer the real viewport width (macOS). On other platforms fall back to
    // the window content width with a safety margin.
    let vw = area_viewport_width(state.area);
    if vw >= 60.0 {
        if (vw - state.content_w).abs() > 0.5 {
            state.content_w = vw;
            update_area(state);
        }
    } else {
        let mut cw: c_int = 0;
        let mut ch: c_int = 0;
        libui::uiWindowContentSize(w, &mut cw, &mut ch);
        let _ = ch;
        let new_w = ((cw as f64) - 24.0).max(280.0);
        if (new_w - state.content_w).abs() > 1.0 {
            state.content_w = new_w;
            update_area(state);
        }
    }
}

unsafe extern "C" fn on_should_quit(_: *mut c_void) -> c_int {
    if !G_WINDOW.is_null() {
        save_geometry(G_WINDOW);
    }
    std::process::exit(0);
}

unsafe extern "C" fn on_window_close(w: *mut libui::uiWindow, _: *mut c_void) -> c_int {
    save_geometry(w);
    on_should_quit(null_mut())
}

pub fn networks_main() {
    unsafe {
        let mut options: libui::uiInitOptions = zeroed();
        assert!(libui::uiInit(&mut options).is_null());

        #[cfg(target_os = "macos")]
        {
            let edit = libui::uiNewMenu("Edit\0".as_bytes().as_ptr().cast());
            libui::uiMenuAppendItem(edit, "@macCut\0".as_bytes().as_ptr().cast());
            libui::uiMenuAppendItem(edit, "@macCopy\0".as_bytes().as_ptr().cast());
            libui::uiMenuAppendItem(edit, "@macPaste\0".as_bytes().as_ptr().cast());
            libui::uiMenuAppendItem(edit, "@macSelectAll\0".as_bytes().as_ptr().cast());
        }

        libui::uiOnShouldQuit(Some(on_should_quit), null_mut());

        let title = CString::new("ZeroTier Networks").unwrap();
        let main_window = libui::uiNewWindow(title.as_ptr(), WINDOW_SIZE_X, WINDOW_SIZE_Y, 0);
        libui::uiWindowSetMargined(main_window, 1);
        libui::uiWindowSetResizeable(main_window, 1);
        libui::uiWindowOnClosing(main_window, Some(on_window_close), null_mut());

        let vbox = libui::uiNewVerticalBox();
        libui::uiBoxSetPadded(vbox, 1);

        let search_row = libui::uiNewHorizontalBox();
        libui::uiBoxSetPadded(search_row, 1);
        let search_label_text = CString::new("Search:").unwrap();
        let search_label = libui::uiNewLabel(search_label_text.as_ptr());
        libui::uiBoxAppend(search_row, search_label.cast(), 0);
        let search_entry = libui::uiNewSearchEntry();
        libui::uiBoxAppend(search_row, search_entry.cast(), 1);
        libui::uiBoxAppend(vbox, search_row.cast(), 0);

        let sort_row = libui::uiNewHorizontalBox();
        libui::uiBoxSetPadded(sort_row, 1);
        let sort_label_text = CString::new("Sort:").unwrap();
        let sort_label = libui::uiNewLabel(sort_label_text.as_ptr());
        libui::uiBoxAppend(sort_row, sort_label.cast(), 0);
        let sort_combo = libui::uiNewCombobox();
        let by_id = CString::new("Sort by ID").unwrap();
        let by_name = CString::new("Sort by Name").unwrap();
        libui::uiComboboxAppend(sort_combo, by_id.as_ptr());
        libui::uiComboboxAppend(sort_combo, by_name.as_ptr());
        libui::uiComboboxSetSelected(sort_combo, 0);
        libui::uiBoxAppend(sort_row, sort_combo.cast(), 0);
        libui::uiBoxAppend(vbox, sort_row.cast(), 0);

        let (client, dirty_flag) = crate::start_client_async(vec!["status", "network"], 250, 4);

        // Initialize drawing handler before creating the area. The handler is
        // leaked (Box::into_raw) so its address stays valid for the app lifetime.
        let mut ah: libui::uiAreaHandler = zeroed();
        ah.Draw = Some(area_draw);
        ah.MouseEvent = Some(area_mouse);
        ah.MouseCrossed = Some(area_mouse_crossed);
        ah.DragBroken = Some(area_drag_broken);
        ah.KeyEvent = Some(area_key);
        let ah_ptr = Box::into_raw(Box::new(ah));

        // Determine initial geometry.
        let (geom_x, geom_y, geom_w, geom_h) = load_geometry().unwrap_or((
            0.0,
            0.0,
            WINDOW_SIZE_X,
            WINDOW_SIZE_Y,
        ));
        let content_w = (geom_w as f64 - 12.0).max(280.0);

        let area = libui::uiNewScrollingArea(ah_ptr, content_w as c_int, 60);
        libui::uiBoxAppend(vbox, area.cast(), 1);

        libui::uiWindowSetChild(main_window, vbox.cast());

        // Restore content size and (on macOS) window position.
        libui::uiWindowSetContentSize(main_window, geom_w, geom_h);
        #[cfg(target_os = "macos")]
        window_set_frame_origin(main_window, geom_x, geom_y);

        let state = Box::new(State {
            client,
            dirty_flag,
            search: String::new(),
            sort_by_name: false,
            area,
            rows: Vec::new(),
            content_w,
            content_h: 60,
            pending: Vec::new(),
            rebuild_needed: true,
        });
        let state_ptr = Box::into_raw(state);
        G_STATE = state_ptr;
        G_WINDOW = main_window;

        libui::uiEntryOnChanged(search_entry, Some(on_search_changed), state_ptr.cast());
        libui::uiComboboxOnSelected(sort_combo, Some(on_sort_changed), state_ptr.cast());
        libui::uiWindowOnContentSizeChanged(main_window, Some(on_content_size_changed), state_ptr.cast());
        libui::uiTimer(250, Some(on_timer), state_ptr.cast());

        // Populate whatever is already available without blocking the UI
        // thread; if the client is busy the timer keeps retrying until the
        // first background sync lands.
        if !try_rebuild(&mut *state_ptr) {
            (*state_ptr).rebuild_needed = true;
        }

        libui::uiControlShow(main_window.cast());
        libui::uiMain();

        libui::uiUninit();
    }
}
