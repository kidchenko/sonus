//! The Windows panel: a borderless popup, painted with Direct2D.
//!
//! Direct2D rather than GDI, which Cota's panel uses: Cota draws bars, and GDI
//! is fine at bars, but a slider is circles, and GDI draws circles without
//! anti-aliasing. Direct2D and DirectWrite are what Windows' own flyouts are
//! drawn with, so the sliders and the text come out looking like theirs. Both
//! ship with Windows; nothing is bundled.
//!
//! Everything here runs on the UI thread. The window is created on it, painted
//! on it, and its messages are pumped by tao's event loop, so state lives in
//! `thread_local`s rather than behind locks.

use super::*;
use crate::icon;
use crate::log::{ldebug, lerror, lwarn};
use crate::util::{pcwstr, wide};
use std::cell::RefCell;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
    D2D_RECT_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_ESCAPE,
};
use windows::Win32::UI::WindowsAndMessaging::*;

const CLASS_NAME: &str = "SonusPanelWindow";
/// Lives in the Common Controls bindings, which this would be the only use of.
const WM_MOUSELEAVE: u32 = 0x02a3;
const EDGE_GAP: i32 = 12;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

struct WinState {
    hwnd: HWND,
    gfx: Option<Gfx>,
    target: Option<ID2D1HwndRenderTarget>,
    painter: Painter,
    /// Set by `--preview-panel`: holds the panel up even when it loses focus,
    /// because anything that looks at it takes the focus away.
    pinned: bool,
    /// When the panel last went away. See `Panel::toggle`.
    hidden_at: Option<Instant>,
    tracking_leave: bool,
    /// Whether the panel hangs from its bottom edge (it opened above the
    /// taskbar), so growing it grows it upwards.
    from_bottom: bool,
    /// Windows 11 rounds and outlines the window itself; Windows 10 does not,
    /// and the panel draws its own border there.
    dwm_frame: bool,
    on_action: Option<Box<dyn Fn(Action)>>,
}

thread_local! {
    static UI: RefCell<Ui> = RefCell::new(Ui::default());
    static WIN: RefCell<WinState> = RefCell::new(WinState {
        hwnd: HWND::default(),
        gfx: None,
        target: None,
        painter: Painter::default(),
        pinned: false,
        hidden_at: None,
        tracking_leave: false,
        from_bottom: true,
        dwm_frame: false,
        on_action: None,
    });
}

fn note_hidden() {
    WIN.with(|w| w.borrow_mut().hidden_at = Some(Instant::now()));
}

fn since_hidden() -> Option<Duration> {
    WIN.with(|w| w.borrow().hidden_at.map(|t| t.elapsed()))
}

fn hide_window(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
        let _ = ReleaseCapture();
    }
    note_hidden();
    // The render target and its bitmaps are only worth their memory while
    // the panel is up. Opening again rebuilds them in a few milliseconds.
    WIN.with(|w| {
        let mut w = w.borrow_mut();
        w.target = None;
        w.painter = Painter::default();
    });
}

// ---------------------------------------------------------------------------
// Panel
// ---------------------------------------------------------------------------

pub struct Panel {
    hwnd: HWND,
}

impl Panel {
    pub fn new(on_action: Box<dyn Fn(Action)>) -> Result<Self, String> {
        let gfx = Gfx::new()?;
        unsafe {
            let instance = GetModuleHandleW(None).map_err(|e| format!("GetModuleHandleW: {e}"))?;
            let class = wide(CLASS_NAME);
            let wc = WNDCLASSW {
                // CS_DROPSHADOW gives the popup a shadow on Windows 10, where
                // there is no DWM corner preference to ask for.
                style: CS_DROPSHADOW,
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                lpszClassName: pcwstr(&class),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                ..Default::default()
            };
            // A zero return means the class already exists, which is fine.
            let _ = RegisterClassW(&wc);

            let hwnd = CreateWindowExW(
                // TOOLWINDOW keeps it out of the taskbar and out of Alt+Tab;
                // TOPMOST keeps it above whatever the user was looking at.
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                pcwstr(&class),
                pcwstr(&wide("Sonus")),
                WS_POPUP,
                0,
                0,
                WIDTH as i32,
                200,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .map_err(|e| format!("CreateWindowExW: {e}"))?;

            // Windows 11 rounds the corners for us. Older builds return an
            // error, which is how we know to draw a border ourselves.
            let pref = DWMWCP_ROUND;
            let dwm_frame = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &pref as *const _ as *const _,
                std::mem::size_of_val(&pref) as u32,
            )
            .is_ok();

            WIN.with(|w| {
                let mut w = w.borrow_mut();
                w.hwnd = hwnd;
                w.gfx = Some(gfx);
                w.dwm_frame = dwm_frame;
                w.on_action = Some(on_action);
            });
            Ok(Self { hwnd })
        }
    }

    pub fn is_visible(&self) -> bool {
        unsafe { IsWindowVisible(self.hwnd).as_bool() }
    }

    pub fn hide(&self) {
        if self.is_visible() {
            hide_window(self.hwnd);
        }
    }

    /// Open the panel, or close it if it is already open.
    pub fn toggle(&self, model: Snapshot, look: Look, anchor: Option<Anchor>) {
        match toggle_action(self.is_visible(), since_hidden()) {
            Toggle::Close => self.hide(),
            Toggle::Open => self.show(model, look, anchor),
            Toggle::Ignore => ldebug!("panel dismissed a moment ago; not reopening"),
        }
    }

    pub fn set_wheel_step(&self, percent: u8) {
        UI.with(|u| u.borrow_mut().set_wheel_step(percent));
    }

    /// Show the panel beside the tray icon, or over the cursor when Windows
    /// will not say where the icon is.
    pub fn show(&self, model: Snapshot, look: Look, anchor: Option<Anchor>) {
        UI.with(|u| u.borrow_mut().open(model, look));
        self.set_border(look);
        unsafe {
            let dpi = GetDpiForWindow(self.hwnd).max(96);
            let scale = dpi as f32 / 96.0;
            let w = (WIDTH * scale).ceil() as i32;
            let h = (UI.with(|u| u.borrow().height()) * scale).ceil() as i32;

            let point = match anchor {
                Some(a) => POINT {
                    x: (a.x + a.w / 2.0) as i32,
                    y: a.y as i32,
                },
                None => {
                    let mut p = POINT::default();
                    let _ = GetCursorPos(&mut p);
                    p
                }
            };
            let anchor_h = anchor.map(|a| a.h as i32).unwrap_or(0);
            let (x, y, from_bottom) = place(point, anchor_h, w, h);
            WIN.with(|s| s.borrow_mut().from_bottom = from_bottom);

            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                x,
                y,
                w,
                h,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            // Focus is what makes WM_ACTIVATE fire when the user clicks away,
            // which is how the panel knows to dismiss itself.
            let _ = SetForegroundWindow(self.hwnd);
            let _ = InvalidateRect(Some(self.hwnd), None, false);
            ldebug!("panel shown at {x},{y} ({w}x{h} @ {dpi}dpi)");
        }
    }

    /// Replace the contents of an open panel, leaving it where it is.
    pub fn update(&self, model: Snapshot, look: Look) {
        if !self.is_visible() {
            return;
        }
        let resized = UI.with(|u| u.borrow_mut().set_model(model, look));
        self.set_border(look);
        if resized {
            fit(self.hwnd);
        }
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    pub fn set_peaks(&self, peaks: &Peaks) {
        if !self.is_visible() {
            return;
        }
        let dirty = UI.with(|u| u.borrow_mut().set_peaks(peaks));
        if !dirty.is_empty() {
            // Straight away and only there, rather than through WM_PAINT:
            // Windows would merge the strips into one rectangle around all of
            // them, which is most of the panel.
            paint(self.hwnd, &dirty);
        }
    }

    /// Whether a slider is held: the app holds off refreshing the model then.
    pub fn busy(&self) -> bool {
        UI.with(|u| u.borrow().dragging())
    }

    /// Windows 11 outlines the rounded window itself; it is told the colour so
    /// the outline matches the theme.
    fn set_border(&self, look: Look) {
        if !WIN.with(|w| w.borrow().dwm_frame) {
            return;
        }
        let c = Palette::new(look).border;
        let colour = COLORREF(
            ((c.0 * 255.0) as u32) | (((c.1 * 255.0) as u32) << 8) | (((c.2 * 255.0) as u32) << 16),
        );
        unsafe {
            let _ = DwmSetWindowAttribute(
                self.hwnd,
                DWMWA_BORDER_COLOR,
                &colour as *const _ as *const _,
                std::mem::size_of_val(&colour) as u32,
            );
        }
    }
}

impl Drop for Panel {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Resize to the content's height, keeping the edge the panel hangs from.
fn fit(hwnd: HWND) {
    unsafe {
        let dpi = GetDpiForWindow(hwnd).max(96);
        let h = (UI.with(|u| u.borrow().height()) * dpi as f32 / 96.0).ceil() as i32;
        let mut rc = RECT::default();
        let _ = GetWindowRect(hwnd, &mut rc);
        let from_bottom = WIN.with(|w| w.borrow().from_bottom);
        let y = if from_bottom { rc.bottom - h } else { rc.top };
        let _ = SetWindowPos(
            hwnd,
            None,
            rc.left,
            y,
            rc.right - rc.left,
            h,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// Above the tray icon (or the cursor) and centred on it, pulled back inside
/// the monitor's work area. Returns the origin and whether the panel hangs
/// from its bottom edge.
fn place(at: POINT, anchor_h: i32, w: i32, h: i32) -> (i32, i32, bool) {
    let mut area = RECT::default();
    unsafe {
        let monitor = MonitorFromPoint(at, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            area = info.rcWork;
        }
    }
    if area.right <= area.left {
        return (at.x - w / 2, at.y - h - EDGE_GAP, true);
    }
    place_in(at, anchor_h, w, h, area)
}

/// The placement arithmetic, apart from the monitor query so it can be tested
/// against work areas this machine does not have.
///
/// Every bound is ordered with `max` before it reaches `clamp`: `i32::clamp`
/// panics when `min > max`, which a work area narrower than the panel would
/// produce, and with `panic = "abort"` that would take the tray icon with it.
fn place_in(at: POINT, anchor_h: i32, w: i32, h: i32, area: RECT) -> (i32, i32, bool) {
    let min_x = area.left + EDGE_GAP;
    let max_x = (area.right - w - EDGE_GAP).max(min_x);
    let x = (at.x - w / 2).clamp(min_x, max_x);

    let min_y = area.top + EDGE_GAP;
    let max_y = (area.bottom - h - EDGE_GAP).max(min_y);
    // Above if there is room, below otherwise: the taskbar can be at the top.
    let above = at.y - h - EDGE_GAP >= area.top;
    let y = if above {
        // Level with Windows' own flyouts: just clear of the taskbar.
        area.bottom - h - EDGE_GAP
    } else {
        at.y + anchor_h + EDGE_GAP
    };
    (x, y.clamp(min_y, max_y), above)
}

// ---------------------------------------------------------------------------
// Window procedure
// ---------------------------------------------------------------------------

fn dispatch(hwnd: HWND, r: Response) {
    if r.resized {
        fit(hwnd);
    }
    if r.repaint || r.resized {
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }
    if r.actions.is_empty() {
        return;
    }
    // Taken out for the call, so the handler can never find the state
    // borrowed, then put back.
    let handler = WIN.with(|w| w.borrow_mut().on_action.take());
    if let Some(h) = &handler {
        for a in r.actions {
            h(a);
        }
    }
    WIN.with(|w| w.borrow_mut().on_action = handler);
}

fn logical(hwnd: HWND, lp: LPARAM) -> (f32, f32) {
    let x = (lp.0 & 0xffff) as i16 as f32;
    let y = ((lp.0 >> 16) & 0xffff) as i16 as f32;
    let scale = unsafe { GetDpiForWindow(hwnd).max(96) } as f32 / 96.0;
    (x / scale, y / scale)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            // Validate first: Direct2D draws, but does not tell Windows the
            // window has been painted, and an unvalidated window gets WM_PAINT
            // forever.
            let mut ps = PAINTSTRUCT::default();
            unsafe {
                let _ = BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
            }
            paint(hwnd, &[]);
            LRESULT(0)
        }
        // Painted entirely by Direct2D; letting Windows erase first would just
        // flash the background colour.
        WM_ERASEBKGND => LRESULT(1),
        WM_SIZE => {
            WIN.with(|w| {
                if let Some(t) = &w.borrow().target {
                    let size = D2D_SIZE_U {
                        width: (lp.0 & 0xffff) as u32,
                        height: ((lp.0 >> 16) & 0xffff) as u32,
                    };
                    let _ = unsafe { t.Resize(&size) };
                }
            });
            LRESULT(0)
        }
        WM_ACTIVATE => {
            let pinned = WIN.with(|w| w.borrow().pinned);
            if (wp.0 & 0xffff) as u32 == WA_INACTIVE && !pinned {
                hide_window(hwnd);
            }
            LRESULT(0)
        }
        WM_KEYDOWN if wp.0 as u16 == VK_ESCAPE.0 => {
            hide_window(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if !WIN.with(|w| w.borrow().tracking_leave) {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                if unsafe { TrackMouseEvent(&mut tme) }.is_ok() {
                    WIN.with(|w| w.borrow_mut().tracking_leave = true);
                }
            }
            let (x, y) = logical(hwnd, lp);
            let r = UI.with(|u| u.borrow_mut().mouse_move(x, y));
            dispatch(hwnd, r);
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            WIN.with(|w| w.borrow_mut().tracking_leave = false);
            if !UI.with(|u| u.borrow().dragging()) {
                let r = UI.with(|u| u.borrow_mut().mouse_leave());
                dispatch(hwnd, r);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            // Captured, so a drag that leaves the panel keeps the slider.
            unsafe {
                SetCapture(hwnd);
            }
            let (x, y) = logical(hwnd, lp);
            let r = UI.with(|u| u.borrow_mut().mouse_down(x, y));
            dispatch(hwnd, r);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            unsafe {
                let _ = ReleaseCapture();
            }
            let (x, y) = logical(hwnd, lp);
            let r = UI.with(|u| u.borrow_mut().mouse_up(x, y));
            dispatch(hwnd, r);
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wp.0 >> 16) & 0xffff) as i16 as f32 / WHEEL_DELTA as f32;
            // Wheel messages carry screen coordinates, unlike every other mouse
            // message.
            let mut pt = POINT {
                x: (lp.0 & 0xffff) as i16 as i32,
                y: ((lp.0 >> 16) & 0xffff) as i16 as i32,
            };
            unsafe {
                let _ = ScreenToClient(hwnd, &mut pt);
            }
            let scale = unsafe { GetDpiForWindow(hwnd).max(96) } as f32 / 96.0;
            let r = UI.with(|u| {
                u.borrow_mut()
                    .wheel(pt.x as f32 / scale, pt.y as f32 / scale, delta)
            });
            dispatch(hwnd, r);
            LRESULT(0)
        }
        WM_DPICHANGED => {
            fit(hwnd);
            LRESULT(0)
        }
        // The panel is reused for the life of the process; closing must hide it
        // rather than destroy the window out from under `Panel`.
        WM_CLOSE => {
            hide_window(hwnd);
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// Paint the panel, or just `clips` of it.
fn paint(hwnd: HWND, clips: &[Rect]) {
    WIN.with(|w| {
        let mut w = w.borrow_mut();
        let w = &mut *w;
        let Some(gfx) = &w.gfx else { return };
        if w.target.is_none() {
            match gfx.hwnd_target(hwnd) {
                Ok(t) => w.target = Some(t),
                Err(e) => {
                    lerror!("no render target: {e}");
                    return;
                }
            }
        }
        let Some(target) = w.target.clone() else {
            return;
        };
        let dpi = unsafe { GetDpiForWindow(hwnd).max(96) } as f32;
        unsafe {
            target.SetDpi(dpi, dpi);
        }
        let (scene, look, height) = UI.with(|u| {
            let u = u.borrow();
            (u.scene(), u.look(), u.height())
        });
        let own_border = !w.dwm_frame;
        let result = w.painter.paint(
            gfx,
            &target,
            &scene,
            look,
            height,
            dpi / 96.0,
            own_border,
            false,
            clips,
        );
        if let Err(e) = result {
            // The device went away (a driver update, a GPU reset, a remote
            // session): start again with a new target on the next paint.
            lwarn!("render target lost ({e}); recreating");
            w.target = None;
            w.painter = Painter::default();
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Direct2D
// ---------------------------------------------------------------------------

/// The factories and text formats: everything that does not belong to one
/// render target.
struct Gfx {
    d2d: ID2D1Factory,
    dw: IDWriteFactory,
    family: Vec<u16>,
    formats: RefCell<HashMap<(u32, Weight, Align), IDWriteTextFormat>>,
}

impl Gfx {
    fn new() -> Result<Self, String> {
        unsafe {
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)
                .map_err(|e| format!("D2D1CreateFactory: {e}"))?;
            let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)
                .map_err(|e| format!("DWriteCreateFactory: {e}"))?;
            let family = wide(&pick_family(&dw));
            Ok(Self {
                d2d,
                dw,
                family,
                formats: RefCell::new(HashMap::new()),
            })
        }
    }

    fn hwnd_target(&self, hwnd: HWND) -> windows::core::Result<ID2D1HwndRenderTarget> {
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(hwnd, &mut rc);
            self.d2d.CreateHwndRenderTarget(
                &D2D1_RENDER_TARGET_PROPERTIES {
                    // Software, on purpose. A hardware target brings up a
                    // Direct3D device, which loads the graphics driver: some
                    // 40 MB, and driver threads that keep waking long after
                    // the panel has closed. A 360 px panel redrawn thirty
                    // times a second is nothing for the CPU rasteriser.
                    r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: D2D1_ALPHA_MODE_IGNORE,
                    },
                    ..Default::default()
                },
                &D2D1_HWND_RENDER_TARGET_PROPERTIES {
                    hwnd,
                    pixelSize: D2D_SIZE_U {
                        width: rc.right.max(1) as u32,
                        height: rc.bottom.max(1) as u32,
                    },
                    // Kept between frames, so a meter tick can repaint just
                    // the meters and leave the rest of the panel as it was.
                    presentOptions: D2D1_PRESENT_OPTIONS_RETAIN_CONTENTS,
                },
            )
        }
    }

    fn format(&self, size: f32, weight: Weight, align: Align) -> Option<IDWriteTextFormat> {
        let key = ((size * 100.0) as u32, weight, align);
        if let Some(f) = self.formats.borrow().get(&key) {
            return Some(f.clone());
        }
        unsafe {
            let f = self
                .dw
                .CreateTextFormat(
                    pcwstr(&self.family),
                    None,
                    match weight {
                        Weight::Regular => DWRITE_FONT_WEIGHT_NORMAL,
                        Weight::Semibold => DWRITE_FONT_WEIGHT_SEMI_BOLD,
                    },
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    windows::core::w!("en-us"),
                )
                .ok()?;
            let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
            let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
            let _ = f.SetTextAlignment(match align {
                Align::Left => DWRITE_TEXT_ALIGNMENT_LEADING,
                Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
                Align::Right => DWRITE_TEXT_ALIGNMENT_TRAILING,
            });
            if let Ok(sign) = self.dw.CreateEllipsisTrimmingSign(&f) {
                let trim = DWRITE_TRIMMING {
                    granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                    delimiter: 0,
                    delimiterCount: 0,
                };
                let _ = f.SetTrimming(&trim, &sign);
            }
            self.formats.borrow_mut().insert(key, f.clone());
            Some(f)
        }
    }
}

/// Segoe UI Variable is Windows 11's UI face; Windows 10 has only Segoe UI.
fn pick_family(dw: &IDWriteFactory) -> String {
    const PREFERRED: &str = "Segoe UI Variable Text";
    unsafe {
        let mut fonts: Option<IDWriteFontCollection> = None;
        if dw.GetSystemFontCollection(&mut fonts, false).is_ok() {
            if let Some(fonts) = fonts {
                let name = wide(PREFERRED);
                let mut index = 0u32;
                let mut exists = windows::core::BOOL(0);
                if fonts
                    .FindFamilyName(pcwstr(&name), &mut index, &mut exists)
                    .is_ok()
                    && exists.as_bool()
                {
                    return PREFERRED.into();
                }
            }
        }
    }
    "Segoe UI".into()
}

/// The bitmaps and brush that belong to one render target. Thrown away with
/// it when the device is lost.
#[derive(Default)]
struct Painter {
    brush: Option<ID2D1SolidColorBrush>,
    glyphs: HashMap<(icon::Glyph, u32, [u8; 4]), ID2D1Bitmap>,
    /// `None` remembered too: an app with no icon is asked once, not thirty
    /// times a second.
    icons: HashMap<(String, u32), Option<ID2D1Bitmap>>,
}

fn color(c: Rgba) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: c.0,
        g: c.1,
        b: c.2,
        a: c.3,
    }
}

fn rect(r: Rect) -> D2D_RECT_F {
    D2D_RECT_F {
        left: r.x,
        top: r.y,
        right: r.right(),
        bottom: r.bottom(),
    }
}

impl Painter {
    /// Draw one frame. `opaque_corners` is for the offscreen render, which has
    /// no DWM to cut the corners round and so must leave them clear itself.
    #[allow(clippy::too_many_arguments)]
    fn paint(
        &mut self,
        gfx: &Gfx,
        target: &ID2D1RenderTarget,
        scene: &[Cmd],
        look: Look,
        height: f32,
        scale: f32,
        own_border: bool,
        clear_corners: bool,
        clips: &[Rect],
    ) -> windows::core::Result<()> {
        let p = Palette::new(look);
        unsafe {
            target.BeginDraw();
            if self.brush.is_none() {
                self.brush = target.CreateSolidColorBrush(&color(p.text), None).ok();
            }
            let Some(brush) = self.brush.clone() else {
                return target.EndDraw(None, None);
            };
            // The whole panel, or one pass per strip asked for.
            let passes: Vec<Option<Rect>> = if clips.is_empty() {
                vec![None]
            } else {
                clips.iter().copied().map(Some).collect()
            };
            for clip in passes {
                if let Some(c) = clip {
                    target.PushAxisAlignedClip(&rect(c), D2D1_ANTIALIAS_MODE_ALIASED);
                }
                target.SetTextAntialiasMode(if clear_corners {
                    // ClearType needs an opaque surface to blend against.
                    D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE
                } else {
                    D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE
                });
                let whole = Rect::new(0.0, 0.0, WIDTH, height);
                if clear_corners {
                    target.Clear(Some(&color(Rgba(0.0, 0.0, 0.0, 0.0))));
                    brush.SetColor(&color(p.bg));
                    target.FillRoundedRectangle(
                        &D2D1_ROUNDED_RECT {
                            rect: rect(whole),
                            radiusX: CORNER,
                            radiusY: CORNER,
                        },
                        &brush,
                    );
                } else {
                    target.Clear(Some(&color(p.bg)));
                }
                if own_border {
                    brush.SetColor(&color(p.border));
                    let r = Rect::new(0.5, 0.5, WIDTH - 1.0, height - 1.0);
                    let radius = if clear_corners { CORNER - 0.5 } else { 0.0 };
                    target.DrawRoundedRectangle(
                        &D2D1_ROUNDED_RECT {
                            rect: rect(r),
                            radiusX: radius,
                            radiusY: radius,
                        },
                        &brush,
                        1.0,
                        None,
                    );
                }

                for cmd in scene {
                    // Laying out text costs far more than clipping saves, so a
                    // command the repaint does not reach is not drawn at all.
                    if clip.is_some_and(|c| !c.intersects(&cmd.bounds())) {
                        continue;
                    }
                    match cmd {
                        Cmd::Fill {
                            r,
                            radius,
                            color: c,
                        } => {
                            brush.SetColor(&color(*c));
                            if *radius <= 0.0 {
                                target.FillRectangle(&rect(*r), &brush);
                            } else {
                                target.FillRoundedRectangle(
                                    &D2D1_ROUNDED_RECT {
                                        rect: rect(*r),
                                        radiusX: *radius,
                                        radiusY: *radius,
                                    },
                                    &brush,
                                );
                            }
                        }
                        Cmd::Stroke {
                            r,
                            radius,
                            width,
                            color: c,
                        } => {
                            brush.SetColor(&color(*c));
                            target.DrawRoundedRectangle(
                                &D2D1_ROUNDED_RECT {
                                    rect: rect(*r),
                                    radiusX: *radius,
                                    radiusY: *radius,
                                },
                                &brush,
                                *width,
                                None,
                            );
                        }
                        Cmd::Text {
                            r,
                            text,
                            size,
                            weight,
                            color: c,
                            align,
                        } => {
                            let Some(format) = gfx.format(*size, *weight, *align) else {
                                continue;
                            };
                            brush.SetColor(&color(*c));
                            let utf16: Vec<u16> = text.encode_utf16().collect();
                            target.DrawText(
                                &utf16,
                                &format,
                                &rect(*r),
                                &brush,
                                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                                DWRITE_MEASURING_MODE_NATURAL,
                            );
                        }
                        Cmd::Glyph { r, glyph, color: c } => {
                            if let Some(bmp) = self.glyph(target, *glyph, *c, r.w, scale) {
                                target.DrawBitmap(
                                    &bmp,
                                    Some(&rect(snap(*r, scale))),
                                    1.0,
                                    D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                                    None,
                                );
                            }
                        }
                        Cmd::AppIcon {
                            r,
                            path,
                            fallback,
                            opacity,
                        } => {
                            let bmp = path
                                .as_deref()
                                .and_then(|path| self.icon(target, path, r.w, scale));
                            match bmp {
                                Some(bmp) => target.DrawBitmap(
                                    &bmp,
                                    Some(&rect(snap(*r, scale))),
                                    *opacity,
                                    D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                                    None,
                                ),
                                None => {
                                    let (tile, letter) = fallback;
                                    brush.SetColor(&color(tile.alpha(*opacity)));
                                    target.FillRoundedRectangle(
                                        &D2D1_ROUNDED_RECT {
                                            rect: rect(*r),
                                            radiusX: r.w * 0.25,
                                            radiusY: r.w * 0.25,
                                        },
                                        &brush,
                                    );
                                    if let Some(format) =
                                        gfx.format(r.w * 0.5, Weight::Semibold, Align::Center)
                                    {
                                        brush.SetColor(&color(Rgba::white(*opacity)));
                                        let utf16: Vec<u16> = letter.encode_utf16().collect();
                                        target.DrawText(
                                            &utf16,
                                            &format,
                                            &rect(*r),
                                            &brush,
                                            D2D1_DRAW_TEXT_OPTIONS_CLIP,
                                            DWRITE_MEASURING_MODE_NATURAL,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                if clip.is_some() {
                    target.PopAxisAlignedClip();
                }
            }
            target.EndDraw(None, None)
        }
    }

    fn glyph(
        &mut self,
        target: &ID2D1RenderTarget,
        g: icon::Glyph,
        c: Rgba,
        logical: f32,
        scale: f32,
    ) -> Option<ID2D1Bitmap> {
        let px = (logical * scale).round().max(1.0) as u32;
        let tint = [
            (c.0 * 255.0) as u8,
            (c.1 * 255.0) as u8,
            (c.2 * 255.0) as u8,
            (c.3 * 255.0) as u8,
        ];
        let key = (g, px, tint);
        if let Some(b) = self.glyphs.get(&key) {
            return Some(b.clone());
        }
        let mut rgba = icon::glyph(g, px, 0);
        for p in rgba.chunks_mut(4) {
            p[0] = tint[0];
            p[1] = tint[1];
            p[2] = tint[2];
            p[3] = ((p[3] as u32 * tint[3] as u32) / 255) as u8;
        }
        let bmp = bitmap(target, &rgba, px)?;
        self.glyphs.insert(key, bmp.clone());
        Some(bmp)
    }

    fn icon(
        &mut self,
        target: &ID2D1RenderTarget,
        path: &str,
        logical: f32,
        scale: f32,
    ) -> Option<ID2D1Bitmap> {
        let px = (logical * scale).round().max(1.0) as u32;
        let key = (path.to_string(), px);
        if let Some(b) = self.icons.get(&key) {
            return b.clone();
        }
        let bmp = crate::program_icon::load(path, px).and_then(|rgba| bitmap(target, &rgba, px));
        self.icons.insert(key, bmp.clone());
        bmp
    }
}

/// Snap a rect to whole device pixels, so a bitmap drawn at its native size
/// lands 1:1 rather than smeared across a pixel boundary.
fn snap(r: Rect, scale: f32) -> Rect {
    let x = (r.x * scale).round() / scale;
    let y = (r.y * scale).round() / scale;
    let w = (r.w * scale).round() / scale;
    let h = (r.h * scale).round() / scale;
    Rect::new(x, y, w, h)
}

/// A Direct2D bitmap from straight-alpha RGBA.
fn bitmap(target: &ID2D1RenderTarget, rgba: &[u8], edge: u32) -> Option<ID2D1Bitmap> {
    // Premultiplied BGRA is the one format every render target accepts.
    let mut bgra = Vec::with_capacity(rgba.len());
    for p in rgba.chunks(4) {
        let a = p[3] as u32;
        let pre = |c: u8| ((c as u32 * a + 127) / 255) as u8;
        bgra.extend_from_slice(&[pre(p[2]), pre(p[1]), pre(p[0]), p[3]]);
    }
    unsafe {
        target
            .CreateBitmap(
                D2D_SIZE_U {
                    width: edge,
                    height: edge,
                },
                Some(bgra.as_ptr().cast()),
                edge * 4,
                &D2D1_BITMAP_PROPERTIES {
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                    },
                    dpiX: 96.0,
                    dpiY: 96.0,
                },
            )
            .ok()
    }
}

// ---------------------------------------------------------------------------
// Offscreen render and preview
// ---------------------------------------------------------------------------

/// Draw the panel straight into memory and hand back its pixels, as
/// `(width, height, straight-alpha RGBA)`. The same painter as the live panel,
/// onto a DIB nothing else has touched. A screenshot would carry whatever
/// was behind the window and whatever DPI the screen happens to be at.
pub fn render_to_rgba(
    (model, look, peaks, devices): (Snapshot, Look, Peaks, bool),
    scale: u32,
) -> Option<(u32, u32, Vec<u8>)> {
    let gfx = Gfx::new().map_err(|e| eprintln!("{e}")).ok()?;
    let mut ui = Ui::default();
    ui.open(model, look);
    if devices {
        ui.show_devices();
    }
    ui.set_peaks(&peaks);
    let height = ui.height();
    let scale = scale.max(1);
    let (w, h) = (
        (WIDTH * scale as f32).ceil() as i32,
        (height * scale as f32).ceil() as i32,
    );

    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(Some(mem), &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let old = SelectObject(mem, dib.into());

        let target = gfx
            .d2d
            .CreateDCRenderTarget(&D2D1_RENDER_TARGET_PROPERTIES {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                ..Default::default()
            })
            .ok()?;
        let dpi = 96.0 * scale as f32;
        target.SetDpi(dpi, dpi);
        target
            .BindDC(
                mem,
                &RECT {
                    left: 0,
                    top: 0,
                    right: w,
                    bottom: h,
                },
            )
            .ok()?;
        let mut painter = Painter::default();
        let scene = ui.scene();
        let result = painter.paint(
            &gfx,
            &target,
            &scene,
            look,
            height,
            scale as f32,
            true,
            true,
            &[],
        );
        let _ = GdiFlush();

        let len = (w * h * 4) as usize;
        let src = std::slice::from_raw_parts(bits as *const u8, len);
        let mut out = vec![0u8; len];
        for i in (0..len).step_by(4) {
            // Premultiplied BGRA back to straight RGBA.
            let a = src[i + 3] as u32;
            let un = |c: u8| {
                (c as u32 * 255 + a / 2)
                    .checked_div(a)
                    .map_or(0, |v| v.min(255) as u8)
            };
            out[i] = un(src[i + 2]);
            out[i + 1] = un(src[i + 1]);
            out[i + 2] = un(src[i]);
            out[i + 3] = a as u8;
        }

        drop(painter);
        drop(target);
        SelectObject(mem, old);
        let _ = DeleteObject(dib.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        result.map_err(|e| eprintln!("paint: {e}")).ok()?;
        Some((w as u32, h as u32, out))
    }
}

/// Show the panel with sample content and pump messages until the timeout, so
/// the rendering can actually be looked at, and moved, clicked and dragged.
pub fn preview(dark: bool, seconds: u64) {
    let (model, look, peaks, _) = sample(dark, false);
    let Ok(panel) = Panel::new(Box::new(|a| eprintln!("{a:?}"))) else {
        return;
    };
    WIN.with(|w| w.borrow_mut().pinned = true);
    panel.show(model, look, None);
    panel.set_peaks(&peaks);
    unsafe {
        let mut rc = RECT::default();
        let _ = GetWindowRect(panel.hwnd, &mut rc);
        eprintln!(
            "preview: visible={} at {},{} {}x{}",
            panel.is_visible(),
            rc.left,
            rc.top,
            rc.right - rc.left,
            rc.bottom - rc.top
        );
    }

    let deadline = Instant::now() + Duration::from_secs(seconds);
    unsafe {
        let mut msg = MSG::default();
        while Instant::now() < deadline {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(Duration::from_millis(16));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(l: i32, t: i32, r: i32, b: i32) -> RECT {
        RECT {
            left: l,
            top: t,
            right: r,
            bottom: b,
        }
    }

    #[test]
    fn the_panel_sits_inside_the_work_area_above_the_taskbar() {
        let work = area(0, 0, 1920, 1032);
        let (x, y, from_bottom) = place_in(POINT { x: 1890, y: 1040 }, 32, 360, 400, work);
        assert!(x >= 0 && x + 360 <= 1920, "x={x}");
        assert!(y >= 0 && y + 400 <= 1032, "y={y}");
        assert!(from_bottom);
    }

    #[test]
    fn it_drops_below_the_icon_when_the_taskbar_is_at_the_top() {
        let work = area(0, 48, 1920, 1080);
        let (_, y, from_bottom) = place_in(POINT { x: 960, y: 8 }, 32, 360, 400, work);
        assert!(y >= 48, "no room above, so it must open downwards");
        assert!(!from_bottom);
    }

    #[test]
    fn a_work_area_smaller_than_the_panel_does_not_panic() {
        for (w, h) in [(360, 400), (900, 1200), (4000, 4000)] {
            let tiny = area(0, 0, 320, 240);
            let _ = place_in(POINT { x: 160, y: 200 }, 0, w, h, tiny);
        }
    }

    #[test]
    fn snapping_lands_on_whole_pixels() {
        let r = snap(Rect::new(10.3, 7.7, 16.0, 16.0), 1.5);
        for v in [r.x, r.y, r.w, r.h] {
            let px = v * 1.5;
            assert!((px - px.round()).abs() < 1e-4, "{v}");
        }
    }
}
