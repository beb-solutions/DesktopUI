use std::cmp::Ordering;
use std::collections::HashMap;
use std::ffi::{c_void, CStr, CString};
use std::mem::zeroed;
use std::os::raw::c_int;
use std::path::Path;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::libui;
use crate::serviceclient::{ms_since_epoch, ServiceClient};

const WINDOW_SIZE_X: c_int = 600;
const WINDOW_SIZE_Y: c_int = 480;

const ROW_H: f64 = 22.0;
const PAD_L: f64 = 8.0;
const BUTTON_W: f64 = 88.0;
const EDGE_W: f64 = 8.0;

/// How long a reconnecting network is shown as "(connecting...)" before it
/// falls back to whatever status the service reports. Guards against a join
/// that never completes leaving the entry stuck in the connecting state.
const CONNECT_TIMEOUT_MS: i64 = 30_000;

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct NSPoint {
    x: f64,
    y: f64,
}

#[cfg(target_os = "macos")]
unsafe impl objc2::Encode for NSPoint {
    const ENCODING: objc2::Encoding =
        objc2::Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct NSSize {
    width: f64,
    height: f64,
}

#[cfg(target_os = "macos")]
unsafe impl objc2::Encode for NSSize {
    const ENCODING: objc2::Encoding =
        objc2::Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
}

/// Colors for the network list, since libui drawing has no notion of the
/// system theme.
struct Palette {
    background: (f64, f64, f64),
    stripe: (f64, f64, f64),
    joined: (f64, f64, f64),
    separator: (f64, f64, f64),
    text: (f64, f64, f64),
    muted: (f64, f64, f64),
    action: (f64, f64, f64),
}

const LIGHT_PALETTE: Palette = Palette {
    background: (1.0, 1.0, 1.0),
    stripe: (0.95, 0.95, 0.95),
    joined: (0.88, 0.97, 0.89),
    separator: (0.88, 0.88, 0.88),
    text: (0.0, 0.0, 0.0),
    muted: (0.4, 0.4, 0.4),
    action: (0.0, 0.45, 0.9),
};

#[cfg(target_os = "macos")]
const DARK_PALETTE: Palette = Palette {
    background: (0.12, 0.12, 0.12),
    stripe: (0.16, 0.16, 0.16),
    joined: (0.12, 0.26, 0.15),
    separator: (0.24, 0.24, 0.24),
    text: (0.92, 0.92, 0.92),
    muted: (0.6, 0.6, 0.6),
    action: (0.3, 0.62, 1.0),
};

/// Palette for the appearance the area is currently being drawn in. Only
/// valid inside the draw handler: AppKit sets the current appearance to the
/// view's effective appearance while it draws.
#[cfg(target_os = "macos")]
unsafe fn current_palette() -> &'static Palette {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    let appearance: *mut AnyObject = msg_send![class!(NSAppearance), currentAppearance];
    if appearance.is_null() {
        return &LIGHT_PALETTE;
    }
    let name: *mut AnyObject = msg_send![appearance, name];
    if name.is_null() {
        return &LIGHT_PALETTE;
    }
    let name: *const std::os::raw::c_char = msg_send![name, UTF8String];
    if !name.is_null() && CStr::from_ptr(name).to_string_lossy().contains("Dark") {
        &DARK_PALETTE
    } else {
        &LIGHT_PALETTE
    }
}

#[cfg(not(target_os = "macos"))]
unsafe fn current_palette() -> &'static Palette {
    &LIGHT_PALETTE
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
    /// Size last passed to uiAreaSetSize.
    area_size: (c_int, c_int),
    pending: Vec<RowAction>,
    rebuild_needed: bool,
    /// Network IDs currently being reconnected. Value is the name as last
    /// seen plus the time (ms since epoch) the reconnect was started, so the
    /// entry can keep showing its name with a "(connecting...)" status until
    /// the service reports a definitive state.
    connecting: HashMap<String, (String, i64)>,
    /// Whether the service answered the last sync; drives the empty-list hint.
    online: bool,
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
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    let handle = libui::uiControlHandle(area.cast());
    if handle == 0 {
        return 0.0;
    }
    let sv = handle as *mut AnyObject;
    let sz: NSSize = msg_send![sv, contentSize];
    sz.width
}

#[cfg(not(target_os = "macos"))]
unsafe fn area_viewport_width(_area: *mut libui::uiArea) -> f64 {
    0.0
}

/// Window position as the platform reports it (macOS: bottom-left frame
/// origin, Windows: top-left corner, both in screen coordinates).
#[cfg(target_os = "macos")]
fn window_position(window: *mut libui::uiWindow) -> Option<(f64, f64)> {
    unsafe {
        use objc2::msg_send;
        use objc2::runtime::AnyObject;
        let handle = libui::uiControlHandle(window.cast());
        if handle == 0 {
            return None;
        }
        let win = handle as *mut AnyObject;
        let p: NSPoint = msg_send![win, frameOrigin];
        Some((p.x, p.y))
    }
}

/// Move the window to a saved position. If that spot is no longer on any
/// screen (monitor unplugged, resolution changed) the window is centered.
#[cfg(target_os = "macos")]
fn window_restore_position(window: *mut libui::uiWindow, x: f64, y: f64) {
    unsafe {
        use objc2::msg_send;
        use objc2::runtime::AnyObject;
        let handle = libui::uiControlHandle(window.cast());
        if handle == 0 {
            return;
        }
        let win = handle as *mut AnyObject;
        let _: () = msg_send![win, setFrameOrigin: NSPoint { x, y }];
        let screen: *mut AnyObject = msg_send![win, screen];
        if screen.is_null() {
            let _: () = msg_send![win, center];
        }
    }
}

#[cfg(windows)]
fn window_position(window: *mut libui::uiWindow) -> Option<(f64, f64)> {
    use winapi::shared::windef::{HWND, RECT};
    use winapi::um::winuser::GetWindowRect;
    unsafe {
        let hwnd = libui::uiControlHandle(window.cast()) as HWND;
        if hwnd.is_null() {
            return None;
        }
        let mut r: RECT = zeroed();
        if GetWindowRect(hwnd, &mut r) == 0 {
            return None;
        }
        Some((r.left as f64, r.top as f64))
    }
}

/// Move the window to a saved position, unless that spot is no longer on any
/// monitor; then the default placement is kept.
#[cfg(windows)]
fn window_restore_position(window: *mut libui::uiWindow, x: f64, y: f64) {
    use winapi::shared::windef::{HWND, RECT};
    use winapi::um::winuser::{
        GetWindowRect, MonitorFromRect, SetWindowPos, MONITOR_DEFAULTTONULL, SWP_NOACTIVATE,
        SWP_NOSIZE, SWP_NOZORDER,
    };
    unsafe {
        let hwnd = libui::uiControlHandle(window.cast()) as HWND;
        if hwnd.is_null() {
            return;
        }
        let mut r: RECT = zeroed();
        if GetWindowRect(hwnd, &mut r) == 0 {
            return;
        }
        let (x, y) = (x as i32, y as i32);
        let target = RECT {
            left: x,
            top: y,
            right: x + (r.right - r.left),
            bottom: y + (r.bottom - r.top),
        };
        if MonitorFromRect(&target, MONITOR_DEFAULTTONULL).is_null() {
            return;
        }
        SetWindowPos(
            hwnd,
            null_mut(),
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// Positioning is not supported here (and Wayland does not allow it at all),
/// so only the size is restored.
#[cfg(not(any(target_os = "macos", windows)))]
fn window_position(_window: *mut libui::uiWindow) -> Option<(f64, f64)> {
    None
}

#[cfg(not(any(target_os = "macos", windows)))]
fn window_restore_position(_window: *mut libui::uiWindow, _x: f64, _y: f64) {}

struct Geometry {
    position: Option<(f64, f64)>,
    width: c_int,
    height: c_int,
}

fn save_geometry(window: *mut libui::uiWindow) {
    unsafe {
        let mut w: c_int = 0;
        let mut h: c_int = 0;
        libui::uiWindowContentSize(window, &mut w, &mut h);
        let mut geom = serde_json::json!({ "width": w, "height": h });
        if let Some((x, y)) = window_position(window) {
            geom["x"] = x.into();
            geom["y"] = y.into();
        }
        let _ = std::fs::write(geometry_path(), serde_json::to_vec(&geom).unwrap());
    }
}

fn load_geometry() -> Option<Geometry> {
    let data = std::fs::read(geometry_path()).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&data).ok()?;
    let x = v.get("x").and_then(|v| v.as_f64());
    let y = v.get("y").and_then(|v| v.as_f64());
    let w = v.get("width").and_then(|v| v.as_i64()).unwrap_or(0) as c_int;
    let h = v.get("height").and_then(|v| v.as_i64()).unwrap_or(0) as c_int;
    if w >= 200 && h >= 200 {
        Some(Geometry {
            position: x.zip(y),
            width: w,
            height: h,
        })
    } else {
        None
    }
}

fn compute_rows(
    client: &ServiceClient,
    search: &str,
    sort_by_name: bool,
    connecting: &HashMap<String, (String, i64)>,
) -> Vec<Row> {
    let networks = client.networks();
    let saved = client.saved_networks();

    let query = search.trim().to_lowercase();
    let mut rows: Vec<Row> = Vec::new();

    for (id, obj) in networks.iter() {
        // A reconnecting network is rendered below from the remembered entry
        // (so its name stays visible) until the service reports a definitive
        // status; showing the raw REQUESTING_CONFIGURATION object here would
        // make the row look empty.
        if connecting.contains_key(id) {
            continue;
        }
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
        let is_connecting = connecting.contains_key(id);
        // Skip networks already shown as joined -- unless they are reconnecting,
        // in which case the remembered entry is used so the row can display the
        // name with a "(connecting...)" status the whole time.
        if joined_ids.iter().any(|j| *j == id) && !is_connecting {
            continue;
        }
        if !query.is_empty()
            && !id.to_lowercase().contains(&query)
            && !name.to_lowercase().contains(&query)
        {
            continue;
        }
        let display_name = connecting
            .get(id)
            .map(|(n, _)| n.clone())
            .unwrap_or_else(|| name.clone());
        let status = if is_connecting {
            "(connecting...)".into()
        } else {
            "(not connected)".into()
        };
        rows.push(Row {
            id: id.clone(),
            name: display_name,
            status,
            settings: settings.clone(),
            joined: false,
        });
    }

    // Currently connected networks always come first; within each group the
    // rows are ordered by the selected sort key (name by default, or ID).
    rows.sort_by(|a, b| {
        match (a.joined, b.joined) {
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
        if sort_by_name {
            let an = &a.name;
            let bn = &b.name;
            match (an.is_empty(), bn.is_empty()) {
                (true, true) => a.id.cmp(&b.id),
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => an.cmp(bn).then_with(|| a.id.cmp(&b.id)),
            }
        } else {
            a.id.cmp(&b.id)
        }
    });
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
    let size = (state.content_w as c_int, h);
    if state.area_size != size {
        state.area_size = size;
        unsafe {
            libui::uiAreaSetSize(state.area, size.0, size.1);
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
    // Network names come from the controller and may contain NUL, which a
    // C string cannot hold; drop it rather than failing.
    let c = CString::new(text.replace('\0', "")).unwrap();
    let s = libui::uiNewAttributedString(c.as_ptr());
    let len = c.as_bytes().len();
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

unsafe fn fill_rect(
    ctx: *mut libui::uiDrawContext,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    (r, g, b): (f64, f64, f64),
) {
    let path = libui::uiDrawNewPath(libui::uiDrawFillModeWinding);
    libui::uiDrawPathAddRectangle(path, x, y, w, h);
    libui::uiDrawPathEnd(path);
    let mut brush: libui::uiDrawBrush = zeroed();
    brush.Type = libui::uiDrawBrushTypeSolid;
    brush.R = r;
    brush.G = g;
    brush.B = b;
    brush.A = 1.0;
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
    (r, g, b): (f64, f64, f64),
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
    (r, g, b): (f64, f64, f64),
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
    let palette = current_palette();

    let mut font = load_font();

    // Background for the visible band.
    fill_rect(
        ctx,
        0.0,
        dp.ClipY,
        state.content_w.max(dp.ClipWidth),
        dp.ClipHeight,
        palette.background,
    );

    if state.rows.is_empty() {
        draw_text(
            ctx,
            &font,
            if !state.search.is_empty() {
                "No networks match your search"
            } else if !state.online {
                "Waiting for ZeroTier system service..."
            } else {
                "No networks found"
            },
            PAD_L,
            dp.ClipY + 12.0,
            state.content_w - PAD_L - EDGE_W,
            palette.muted,
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

        // Currently connected networks get a light green background so they
        // clearly stand out from the "(not connected)" entries, which keep
        // the alternate row shading.
        if row.joined {
            fill_rect(ctx, 0.0, y, state.content_w, ROW_H, palette.joined);
        } else if i % 2 == 0 {
            fill_rect(ctx, 0.0, y, state.content_w, ROW_H, palette.stripe);
        }

        // Separator line.
        fill_rect(ctx, 0.0, y + ROW_H - 1.0, state.content_w, 1.0, palette.separator);

        // Main text.
        let label = row_label(row);
        draw_text(
            ctx,
            &font,
            &label,
            PAD_L,
            y + ROW_H / 2.0,
            text_region_w,
            palette.text,
        );

        // Action buttons (right-aligned text, one cell each). A reconnecting
        // entry has no usable action yet (the status column already reads
        // "(connecting...)"), so its second cell is just a busy indicator and
        // clicks there are ignored in the mouse handler.
        let actions: [&str; 2] = if state.connecting.contains_key(&row.id) {
            ["Copy", "…"]
        } else if row.joined {
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
                palette.action,
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
    // While a reconnect is in progress the entry is shown as connecting;
    // further clicks on the action column are ignored.
    if state.connecting.contains_key(&row.id) {
        return;
    }
    if row.joined {
        state.pending.push(RowAction::Disconnect {
            id: row.id,
            name: row.name,
            settings: row.settings,
        });
    } else {
        // Remember that this network is being reconnected so the row keeps
        // showing its name with a "(connecting...)" status until the service
        // reports the new state.
        state.connecting.insert(
            row.id.clone(),
            (row.name.clone(), ms_since_epoch()),
        );
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

/// Drop reconnecting entries that have reached a definitive state (the service
/// now reports a real status such as OK / AUTHENTICATION_REQUIRED) or that have
/// been trying for longer than [CONNECT_TIMEOUT_MS]. The row then shows the
/// actual state reported by the service again.
fn prune_connecting(client: &ServiceClient, connecting: &mut HashMap<String, (String, i64)>) {
    let now = ms_since_epoch();
    let networks = client.networks();
    connecting.retain(|id, (_, start)| {
        if now - *start > CONNECT_TIMEOUT_MS {
            return false;
        }
        let status = networks
            .iter()
            .find(|(n, _)| n == id)
            .and_then(|(_, o)| o.get("status"))
            .and_then(|s| s.as_str())
            .unwrap_or("");
        match status {
            // Not (yet) reported, or still being configured: keep connecting.
            "" | "REQUESTING_CONFIGURATION" => true,
            _ => false,
        }
    });
}

/// Recompute the row list and repaint, but only if the service client lock is
/// free right now. Returns false when the background sync thread is holding
/// the lock; callers keep the rebuild flag set and retry on the next timer
/// tick. This keeps the UI thread responsive even while a (slow) service sync
/// is running.
fn try_rebuild(state: &mut State) -> bool {
    // Clone the Arc so the guard doesn't hold a borrow of `state` and we can
    // also touch `state.connecting` below.
    let client = state.client.clone();
    let Some(guard) = client.try_lock() else {
        return false;
    };
    prune_connecting(&guard, &mut state.connecting);
    state.online = guard.is_online();
    let rows = compute_rows(
        &guard,
        &state.search,
        state.sort_by_name,
        &state.connecting,
    );
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

/// Keep the drawn content width matched to the actual viewport so the
/// rightmost action column always fits (no horizontal overflow). The viewport
/// also narrows without a window resize when a scrollbar appears, hence this
/// also runs from the timer. Returns false if the viewport width is unknown
/// on this platform.
unsafe fn sync_content_width(state: &mut State) -> bool {
    let vw = area_viewport_width(state.area);
    if vw < 60.0 {
        return false;
    }
    if (vw - state.content_w).abs() > 0.5 {
        state.content_w = vw;
        update_area(state);
    }
    true
}

unsafe extern "C" fn on_timer(data: *mut c_void) -> c_int {
    let state = &mut *(data as *mut State);
    sync_content_width(state);

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
    if !sync_content_width(state) {
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
        libui::uiComboboxSetSelected(sort_combo, 1); // default: sort by name
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
        let geom = load_geometry();
        let (geom_w, geom_h) = geom
            .as_ref()
            .map_or((WINDOW_SIZE_X, WINDOW_SIZE_Y), |g| (g.width, g.height));
        let content_w = (geom_w as f64 - 12.0).max(280.0);

        let area = libui::uiNewScrollingArea(ah_ptr, content_w as c_int, 60);
        libui::uiBoxAppend(vbox, area.cast(), 1);

        libui::uiWindowSetChild(main_window, vbox.cast());

        // Restore content size and window position. Without a saved position
        // the window keeps libui's default placement.
        libui::uiWindowSetContentSize(main_window, geom_w, geom_h);
        if let Some((x, y)) = geom.and_then(|g| g.position) {
            window_restore_position(main_window, x, y);
        }

        let state = Box::new(State {
            client,
            dirty_flag,
            search: String::new(),
            sort_by_name: true,
            area,
            rows: Vec::new(),
            content_w,
            area_size: (content_w as c_int, 60),
            pending: Vec::new(),
            rebuild_needed: true,
            connecting: HashMap::new(),
            online: false,
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
