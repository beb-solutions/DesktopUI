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

const ROW_H: f64 = 36.0;
const HEADER_H: f64 = 30.0;
const DOT_X: f64 = 18.0;
const DOT_R: f64 = 4.5;
const TEXT_X: f64 = 32.0;
const COL_GAP: f64 = 14.0;
const BUTTON_H: f64 = 24.0;
const BUTTON_GAP: f64 = 6.0;
const BUTTON_RADIUS: f64 = 6.0;
const BUTTON_WIDTHS: [f64; 2] = [72.0, 96.0];
const EDGE_W: f64 = 12.0;
/// How long a Copy button reads "Copied" after a click.
const COPIED_MS: i64 = 1_500;

#[cfg(target_os = "macos")]
const MONO_FAMILY: &str = "Menlo";
#[cfg(windows)]
const MONO_FAMILY: &str = "Consolas";
#[cfg(not(any(target_os = "macos", windows)))]
const MONO_FAMILY: &str = "Monospace";

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

type Rgb = (f64, f64, f64);

/// Colors for the network list, since libui drawing has no notion of the
/// system theme.
struct Palette {
    background: Rgb,
    row_hover: Rgb,
    separator: Rgb,
    text: Rgb,
    muted: Rgb,
    header: Rgb,
    action: Rgb,
    danger: Rgb,
    button: Rgb,
    button_hover: Rgb,
    status_ok: Rgb,
    status_pending: Rgb,
    status_error: Rgb,
    status_idle: Rgb,
}

const LIGHT_PALETTE: Palette = Palette {
    background: (1.0, 1.0, 1.0),
    row_hover: (0.96, 0.96, 0.97),
    separator: (0.91, 0.91, 0.92),
    text: (0.1, 0.1, 0.12),
    muted: (0.45, 0.45, 0.48),
    header: (0.5, 0.5, 0.53),
    action: (0.0, 0.45, 0.9),
    danger: (0.8, 0.2, 0.2),
    button: (0.94, 0.94, 0.95),
    button_hover: (0.87, 0.87, 0.89),
    status_ok: (0.2, 0.72, 0.35),
    status_pending: (0.93, 0.6, 0.1),
    status_error: (0.85, 0.25, 0.25),
    status_idle: (0.74, 0.74, 0.77),
};

#[cfg(target_os = "macos")]
const DARK_PALETTE: Palette = Palette {
    background: (0.12, 0.12, 0.13),
    row_hover: (0.17, 0.17, 0.18),
    separator: (0.2, 0.2, 0.21),
    text: (0.93, 0.93, 0.94),
    muted: (0.6, 0.6, 0.63),
    header: (0.55, 0.55, 0.58),
    action: (0.38, 0.66, 1.0),
    danger: (1.0, 0.45, 0.45),
    button: (0.2, 0.2, 0.21),
    button_hover: (0.28, 0.28, 0.3),
    status_ok: (0.3, 0.82, 0.45),
    status_pending: (1.0, 0.7, 0.2),
    status_error: (1.0, 0.4, 0.4),
    status_idle: (0.42, 0.42, 0.45),
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
    /// Vertical layout of the list (section headers and rows), rebuilt with
    /// the rows and shared by drawing and hit testing.
    lines: Vec<Line>,
    /// Row index under the mouse, and which of its buttons (if any).
    hover: Option<(usize, Option<usize>)>,
    /// Network ID whose Copy button was clicked, and when.
    copied: Option<(String, i64)>,
    /// Fitted row texts for the current rows and width. Measuring and
    /// shortening text is by far the most expensive part of drawing, so it is
    /// done once here instead of on every redraw (e.g. each hover change).
    text_cache: Option<TextCache>,
}

struct TextCache {
    content_w: f64,
    id_w: f64,
    rows: Vec<RowText>,
}

struct RowText {
    name: String,
    /// Shown in the muted style (placeholder for a network without a name).
    name_placeholder: bool,
    name_w: f64,
    show_id: bool,
    status: String,
}

enum LineKind {
    Header { title: &'static str, count: usize },
    Row(usize),
}

struct Line {
    y: f64,
    h: f64,
    kind: LineKind,
}

fn layout_lines(rows: &[Row]) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut y = 0.0;
    let joined = rows.iter().filter(|r| r.joined).count();
    for (i, r) in rows.iter().enumerate() {
        if i == 0 || r.joined != rows[i - 1].joined {
            let (title, count) = if r.joined {
                ("CONNECTED", joined)
            } else {
                ("SAVED", rows.len() - joined)
            };
            lines.push(Line {
                y,
                h: HEADER_H,
                kind: LineKind::Header { title, count },
            });
            y += HEADER_H;
        }
        lines.push(Line {
            y,
            h: ROW_H,
            kind: LineKind::Row(i),
        });
        y += ROW_H;
    }
    lines
}

/// Horizontal extents (x, width) of the two buttons at the end of a row.
fn button_rects(content_w: f64) -> [(f64, f64); 2] {
    let right = content_w - EDGE_W;
    let x1 = right - BUTTON_WIDTHS[1];
    let x0 = x1 - BUTTON_GAP - BUTTON_WIDTHS[0];
    [(x0, BUTTON_WIDTHS[0]), (x1, BUTTON_WIDTHS[1])]
}

/// What the status column says for a service status, and the dot color.
fn status_display<'a>(status: &str, palette: &'a Palette) -> (String, &'a Rgb) {
    match status {
        "OK" => ("Connected".into(), &palette.status_ok),
        "(not connected)" => ("Not connected".into(), &palette.status_idle),
        "(connecting...)" => ("Connecting…".into(), &palette.status_pending),
        "REQUESTING_CONFIGURATION" => ("Requesting configuration…".into(), &palette.status_pending),
        "AUTHENTICATION_REQUIRED" => ("Authentication required".into(), &palette.status_pending),
        "ACCESS_DENIED" => ("Access denied".into(), &palette.status_error),
        "NOT_FOUND" => ("Network not found".into(), &palette.status_error),
        "PORT_ERROR" => ("Port error".into(), &palette.status_error),
        "CLIENT_TOO_OLD" => ("Client too old".into(), &palette.status_error),
        "" => ("Unknown".into(), &palette.status_idle),
        other => {
            // Unknown future statuses: FOO_BAR -> "Foo bar".
            let mut t = other.replace('_', " ").to_lowercase();
            if let Some(c) = t.get(0..1) {
                let upper = c.to_uppercase();
                t.replace_range(0..1, &upper);
            }
            (t, &palette.status_error)
        }
    }
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

fn update_area(state: &mut State) {
    state.lines = layout_lines(&state.rows);
    state.text_cache = None;
    let h = state
        .lines
        .last()
        .map_or(60, |l| (l.y + l.h).ceil() as c_int + 8);
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

#[derive(Clone, Copy)]
struct TextStyle<'a> {
    color: &'a Rgb,
    weight: libui::uiTextWeight,
    mono: bool,
    /// Offset from the control font size in points.
    size_delta: f64,
}

impl<'a> TextStyle<'a> {
    fn plain(color: &'a Rgb) -> Self {
        TextStyle {
            color,
            weight: libui::uiTextWeightNormal,
            mono: false,
            size_delta: 0.0,
        }
    }
}

unsafe fn make_layout(
    font: &libui::uiFontDescriptor,
    text: &str,
    width: f64,
    style: TextStyle,
) -> (*mut libui::uiDrawTextLayout, *mut libui::uiAttributedString) {
    // Network names come from the controller and may contain NUL, which a
    // C string cannot hold; drop it rather than failing.
    let c = CString::new(text.replace('\0', "")).unwrap();
    let s = libui::uiNewAttributedString(c.as_ptr());
    let len = c.as_bytes().len();
    // The attributed string takes ownership of the attributes.
    let (r, g, b) = *style.color;
    libui::uiAttributedStringSetAttribute(s, libui::uiNewColorAttribute(r, g, b, 1.0), 0, len);
    if style.weight != libui::uiTextWeightNormal {
        libui::uiAttributedStringSetAttribute(s, libui::uiNewWeightAttribute(style.weight), 0, len);
    }
    if style.mono {
        let family = CString::new(MONO_FAMILY).unwrap();
        libui::uiAttributedStringSetAttribute(s, libui::uiNewFamilyAttribute(family.as_ptr()), 0, len);
    }
    if style.size_delta != 0.0 {
        let size = (font.Size + style.size_delta).max(6.0);
        libui::uiAttributedStringSetAttribute(s, libui::uiNewSizeAttribute(size), 0, len);
    }
    let mut p: libui::uiDrawTextLayoutParams = zeroed();
    p.String = s;
    p.DefaultFont = font as *const _ as *mut _;
    p.Width = width;
    p.Align = libui::uiDrawTextAlignLeft;
    let tl = libui::uiDrawNewTextLayout(&mut p);
    // The layout references the attributed string; it must outlive the layout.
    (tl, s)
}

unsafe fn text_size(font: &libui::uiFontDescriptor, text: &str, style: TextStyle) -> (f64, f64) {
    let (tl, s) = make_layout(font, text, 10_000.0, style);
    let mut w: f64 = 0.0;
    let mut h: f64 = 0.0;
    libui::uiDrawTextLayoutExtents(tl, &mut w, &mut h);
    libui::uiDrawFreeTextLayout(tl);
    libui::uiFreeAttributedString(s);
    (w, h)
}

/// Shorten text with an ellipsis until it fits into max_w on one line.
unsafe fn fit_text(font: &libui::uiFontDescriptor, text: &str, style: TextStyle, max_w: f64) -> String {
    if max_w <= 0.0 {
        return String::new();
    }
    if text_size(font, text, style).0 <= max_w {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        let candidate: String = chars[..mid].iter().collect::<String>() + "…";
        if text_size(font, &candidate, style).0 <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    if lo == 0 {
        String::new()
    } else {
        chars[..lo].iter().collect::<String>().trim_end().to_string() + "…"
    }
}

unsafe fn fill_path(ctx: *mut libui::uiDrawContext, path: *mut libui::uiDrawPath, (r, g, b): Rgb) {
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

unsafe fn fill_rect(ctx: *mut libui::uiDrawContext, x: f64, y: f64, w: f64, h: f64, color: Rgb) {
    let path = libui::uiDrawNewPath(libui::uiDrawFillModeWinding);
    libui::uiDrawPathAddRectangle(path, x, y, w, h);
    fill_path(ctx, path, color);
}

unsafe fn add_circle(path: *mut libui::uiDrawPath, cx: f64, cy: f64, r: f64) {
    libui::uiDrawPathNewFigureWithArc(path, cx, cy, r, 0.0, 2.0 * std::f64::consts::PI, 0);
    libui::uiDrawPathCloseFigure(path);
}

unsafe fn fill_circle(ctx: *mut libui::uiDrawContext, cx: f64, cy: f64, r: f64, color: Rgb) {
    let path = libui::uiDrawNewPath(libui::uiDrawFillModeWinding);
    add_circle(path, cx, cy, r);
    fill_path(ctx, path, color);
}

/// Rounded rectangle built from two rectangles and four corner circles, which
/// the winding fill merges into one shape.
unsafe fn fill_rounded_rect(
    ctx: *mut libui::uiDrawContext,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    r: f64,
    color: Rgb,
) {
    let r = r.min(w / 2.0).min(h / 2.0);
    let path = libui::uiDrawNewPath(libui::uiDrawFillModeWinding);
    libui::uiDrawPathAddRectangle(path, x + r, y, w - 2.0 * r, h);
    libui::uiDrawPathAddRectangle(path, x, y + r, w, h - 2.0 * r);
    add_circle(path, x + r, y + r, r);
    add_circle(path, x + w - r, y + r, r);
    add_circle(path, x + r, y + h - r, r);
    add_circle(path, x + w - r, y + h - r, r);
    fill_path(ctx, path, color);
}

/// A text layout kept across frames. Building a layout (font matching and
/// typesetting) costs about a millisecond, which added up to >100 ms per
/// frame and made hover highlighting lag; drawing an existing one is cheap.
struct CachedLayout {
    layout: *mut libui::uiDrawTextLayout,
    /// The layout references the attributed string, so it lives as long.
    string: *mut libui::uiAttributedString,
    w: f64,
    h: f64,
}

/// Everything that goes into a layout: text plus the style bits, colors
/// included since they are baked into the attributed string.
type LayoutKey = (String, i32, bool, u64, [u64; 3]);

/// Upper bound for the layout cache. Entries only pile up when texts change
/// (renames, "Copied" flips, theme switches), so a full flush is enough.
const LAYOUT_CACHE_MAX: usize = 512;

/// Only touched from the UI thread, like G_STATE.
static mut LAYOUT_CACHE: Option<HashMap<LayoutKey, CachedLayout>> = None;

unsafe fn flush_layout_cache() {
    if let Some(cache) = (*std::ptr::addr_of_mut!(LAYOUT_CACHE)).take() {
        for (_, c) in cache {
            libui::uiDrawFreeTextLayout(c.layout);
            libui::uiFreeAttributedString(c.string);
        }
    }
}

unsafe fn cached_layout(
    font: &libui::uiFontDescriptor,
    text: &str,
    style: TextStyle,
) -> &'static CachedLayout {
    let key: LayoutKey = (
        text.to_string(),
        style.weight as i32,
        style.mono,
        style.size_delta.to_bits(),
        [style.color.0.to_bits(), style.color.1.to_bits(), style.color.2.to_bits()],
    );
    if (*std::ptr::addr_of!(LAYOUT_CACHE)).as_ref().is_some_and(|c| c.len() >= LAYOUT_CACHE_MAX) {
        flush_layout_cache();
    }
    let cache = (*std::ptr::addr_of_mut!(LAYOUT_CACHE)).get_or_insert_with(HashMap::new);
    cache.entry(key).or_insert_with(|| {
        let (layout, string) = make_layout(font, text, 10_000.0, style);
        let (mut w, mut h) = (0.0, 0.0);
        libui::uiDrawTextLayoutExtents(layout, &mut w, &mut h);
        CachedLayout { layout, string, w, h }
    })
}

unsafe fn draw_text(
    ctx: *mut libui::uiDrawContext,
    font: &libui::uiFontDescriptor,
    text: &str,
    x: f64,
    y_center: f64,
    style: TextStyle,
) -> f64 {
    let l = cached_layout(font, text, style);
    libui::uiDrawText(ctx, l.layout, x, y_center - l.h / 2.0);
    l.w
}

unsafe fn draw_text_centered(
    ctx: *mut libui::uiDrawContext,
    font: &libui::uiFontDescriptor,
    text: &str,
    x_center: f64,
    y_center: f64,
    style: TextStyle,
) {
    let l = cached_layout(font, text, style);
    libui::uiDrawText(ctx, l.layout, x_center - l.w / 2.0, y_center - l.h / 2.0);
}

fn name_style(palette: &Palette) -> TextStyle<'_> {
    TextStyle {
        weight: libui::uiTextWeightSemiBold,
        ..TextStyle::plain(&palette.text)
    }
}

fn id_style(palette: &Palette) -> TextStyle<'_> {
    TextStyle {
        mono: true,
        size_delta: -1.0,
        ..TextStyle::plain(&palette.muted)
    }
}

/// Measure and shorten all row texts for the current width. Colors do not
/// affect sizes, so the light palette is used for measuring.
unsafe fn build_text_cache(font: &libui::uiFontDescriptor, state: &State) -> TextCache {
    let palette = &LIGHT_PALETTE;
    let buttons = button_rects(state.content_w);
    let text_right = buttons[0].0 - COL_GAP;
    let id_w = text_size(font, "0000000000000000", id_style(palette)).0;
    let avail = text_right - TEXT_X;
    let base_name_w = ((avail - id_w - 2.0 * COL_GAP) * 0.55).clamp(90.0, 280.0);
    let show_id = TEXT_X + base_name_w + COL_GAP + id_w <= text_right;
    // Too narrow for the ID column: give the name all the room.
    let name_w = if show_id { base_name_w } else { avail };
    let status_x = TEXT_X + name_w + COL_GAP + id_w + COL_GAP;

    let rows = state
        .rows
        .iter()
        .map(|row| {
            let (name, style, placeholder) = if row.name.is_empty() {
                ("Unnamed network", TextStyle::plain(&palette.muted), true)
            } else {
                (row.name.as_str(), name_style(palette), false)
            };
            let status = if show_id {
                let (text, _) = status_display(&row.status, palette);
                fit_text(font, &text, TextStyle::plain(&palette.muted), text_right - status_x)
            } else {
                String::new()
            };
            RowText {
                name: fit_text(font, name, style, name_w),
                name_placeholder: placeholder,
                name_w,
                show_id,
                status,
            }
        })
        .collect();
    TextCache {
        content_w: state.content_w,
        id_w,
        rows,
    }
}

unsafe fn draw_row(
    ctx: *mut libui::uiDrawContext,
    font: &libui::uiFontDescriptor,
    state: &State,
    cache: &TextCache,
    palette: &Palette,
    index: usize,
    y: f64,
    last_in_section: bool,
) {
    let row = &state.rows[index];
    let text = &cache.rows[index];
    let cy = y + ROW_H / 2.0;
    let hover = state.hover.filter(|(i, _)| *i == index);

    if hover.is_some() {
        fill_rect(ctx, 0.0, y, state.content_w, ROW_H, palette.row_hover);
    }
    if !last_in_section {
        fill_rect(ctx, TEXT_X, y + ROW_H - 1.0, state.content_w - TEXT_X, 1.0, palette.separator);
    }

    let (_, dot) = status_display(&row.status, palette);
    fill_circle(ctx, DOT_X, cy, DOT_R, *dot);

    // Columns: name | ID | status, all left of the buttons.
    let style = if text.name_placeholder {
        TextStyle::plain(&palette.muted)
    } else {
        name_style(palette)
    };
    draw_text(ctx, font, &text.name, TEXT_X, cy, style);
    if text.show_id {
        let id_x = TEXT_X + text.name_w + COL_GAP;
        draw_text(ctx, font, &row.id, id_x, cy, id_style(palette));
        if !text.status.is_empty() {
            let status_x = id_x + cache.id_w + COL_GAP;
            draw_text(ctx, font, &text.status, status_x, cy, TextStyle::plain(&palette.muted));
        }
    }

    // Buttons.
    let buttons = button_rects(state.content_w);
    let connecting = state.connecting.contains_key(&row.id);
    let copied = state
        .copied
        .as_ref()
        .is_some_and(|(id, _)| *id == row.id);
    let labels: [(&str, &Rgb, bool); 2] = [
        if copied {
            ("Copied", &palette.status_ok, true)
        } else {
            ("Copy ID", &palette.text, true)
        },
        if connecting {
            ("Connecting…", &palette.muted, false)
        } else if row.joined {
            ("Disconnect", &palette.danger, true)
        } else {
            ("Reconnect", &palette.action, true)
        },
    ];
    let by = cy - BUTTON_H / 2.0;
    for (j, ((bx, bw), (label, color, enabled))) in buttons.iter().zip(labels.iter()).enumerate() {
        let hovered = *enabled && hover.is_some_and(|(_, b)| b == Some(j));
        let bg = if hovered { palette.button_hover } else { palette.button };
        fill_rounded_rect(ctx, *bx, by, *bw, BUTTON_H, BUTTON_RADIUS, bg);
        let style = TextStyle {
            size_delta: -1.0,
            ..TextStyle::plain(color)
        };
        draw_text_centered(ctx, font, label, bx + bw / 2.0, cy, style);
    }
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
        let hint = if !state.search.is_empty() {
            "No networks match your search"
        } else if !state.online {
            "Waiting for ZeroTier system service..."
        } else {
            "No networks found"
        };
        draw_text_centered(
            ctx,
            &font,
            hint,
            state.content_w / 2.0,
            30.0,
            TextStyle::plain(&palette.muted),
        );
        libui::uiFreeFontDescriptor(&mut font);
        return;
    }

    if state
        .text_cache
        .as_ref()
        .is_none_or(|c| c.content_w != state.content_w)
    {
        state.text_cache = Some(build_text_cache(&font, state));
    }
    let state = &*state;
    let cache = state.text_cache.as_ref().unwrap();

    let clip_top = dp.ClipY;
    let clip_bottom = dp.ClipY + dp.ClipHeight;
    for (n, line) in state.lines.iter().enumerate() {
        if line.y + line.h < clip_top || line.y > clip_bottom {
            continue;
        }
        match line.kind {
            LineKind::Header { title, count } => {
                let style = TextStyle {
                    weight: libui::uiTextWeightSemiBold,
                    size_delta: -2.0,
                    ..TextStyle::plain(&palette.header)
                };
                let cy = line.y + line.h * 0.6;
                let w = draw_text(ctx, &font, title, DOT_X - DOT_R, cy, style);
                let count_style = TextStyle {
                    weight: libui::uiTextWeightNormal,
                    ..style
                };
                draw_text(ctx, &font, &count.to_string(), DOT_X - DOT_R + w + 6.0, cy, count_style);
            }
            LineKind::Row(index) => {
                let last_in_section = !matches!(
                    state.lines.get(n + 1).map(|l| &l.kind),
                    Some(LineKind::Row(_))
                );
                draw_row(ctx, &font, state, cache, palette, index, line.y, last_in_section);
            }
        }
    }

    libui::uiFreeFontDescriptor(&mut font);
}

/// Row and button (0 = copy, 1 = connect action) at a point in the area.
fn hit_test(state: &State, x: f64, y: f64) -> Option<(usize, Option<usize>)> {
    let line = state.lines.iter().find(|l| y >= l.y && y < l.y + l.h)?;
    let LineKind::Row(index) = line.kind else {
        return None;
    };
    let cy = line.y + ROW_H / 2.0;
    let button = button_rects(state.content_w)
        .iter()
        .position(|(bx, bw)| {
            x >= *bx && x < bx + bw && (y - cy).abs() <= BUTTON_H / 2.0
        });
    Some((index, button))
}

unsafe extern "C" fn area_mouse(
    _ah: *mut libui::uiAreaHandler,
    area: *mut libui::uiArea,
    me: *mut libui::uiAreaMouseEvent,
) {
    let state = &mut *G_STATE;
    let me = &*me;

    let hit = hit_test(state, me.X, me.Y);
    if me.Down == 0 {
        if hit != state.hover {
            state.hover = hit;
            libui::uiAreaQueueRedrawAll(area);
        }
        return;
    }
    if me.Down != 1 {
        return;
    }
    let Some((idx, Some(button))) = hit else {
        return;
    };
    let row = state.rows[idx].clone();

    // Button 0 copies the ID; button 1 is Disconnect/Reconnect. Row actions
    // are queued and applied from the timer tick so this handler never blocks
    // the UI thread on the service client lock (the background sync thread
    // may hold it for a while). No notifications are shown from this window:
    // the row list itself provides the feedback.
    if button == 0 {
        crate::copy_to_clipboard(row.id.as_str());
        state.copied = Some((row.id, ms_since_epoch()));
        libui::uiAreaQueueRedrawAll(area);
        return;
    }
    // While a reconnect is in progress the entry is shown as connecting;
    // further clicks on the action button are ignored.
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
        // showing its name with a "Connecting…" status until the service
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
    area: *mut libui::uiArea,
    left: c_int,
) {
    let state = &mut *G_STATE;
    if left != 0 && state.hover.is_some() {
        state.hover = None;
        libui::uiAreaQueueRedrawAll(area);
    }
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

    if state
        .copied
        .as_ref()
        .is_some_and(|(_, t)| ms_since_epoch() - t > COPIED_MS)
    {
        state.copied = None;
        libui::uiAreaQueueRedrawAll(state.area);
    }

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

        // Toolbar: search field stretching over the width, sort order on the right.
        let toolbar = libui::uiNewHorizontalBox();
        libui::uiBoxSetPadded(toolbar, 1);
        let search_entry = libui::uiNewSearchEntry();
        libui::uiBoxAppend(toolbar, search_entry.cast(), 1);
        let sort_combo = libui::uiNewCombobox();
        let by_id = CString::new("Sort by ID").unwrap();
        let by_name = CString::new("Sort by Name").unwrap();
        libui::uiComboboxAppend(sort_combo, by_id.as_ptr());
        libui::uiComboboxAppend(sort_combo, by_name.as_ptr());
        libui::uiComboboxSetSelected(sort_combo, 1); // default: sort by name
        libui::uiBoxAppend(toolbar, sort_combo.cast(), 0);
        libui::uiBoxAppend(vbox, toolbar.cast(), 0);

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
            lines: Vec::new(),
            hover: None,
            copied: None,
            text_cache: None,
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
