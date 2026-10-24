//! The Linux panel: an undecorated GTK window, painted with cairo and pango.
//!
//! Desktops share no frosted material, so the panel is solid, in the
//! desktop's light or dark, with its corners rounded where a compositor can
//! show them. One drawing area paints the commands [`Ui`] produces and hands
//! it the mouse.
//!
//! On X11 the panel opens by the pointer, which has just clicked the tray's
//! menu. Wayland does not let a window choose its place, so there the
//! compositor puts it. Either way it closes when it loses focus, or on Esc.
//!
//! Everything here runs on the main thread (GTK demands it), so state lives
//! in `thread_local`s, as on the other two.

use super::*;
use crate::icon;
use crate::log::ldebug;

use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{cairo, gdk, gdk_pixbuf, glib, pango};

const EDGE_GAP: i32 = 6;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

struct LinuxState {
    pinned: bool,
    hidden_at: Option<Instant>,
    on_action: Option<Box<dyn Fn(Action)>>,
    window: Option<gtk::Window>,
    area: Option<gtk::DrawingArea>,
    glyphs: HashMap<(icon::Glyph, u32, [u8; 4]), cairo::ImageSurface>,
    /// App icons by name and pixel size; `None` for a name the icon theme
    /// does not have, so it is not looked up on every paint.
    icons: HashMap<(String, i32), Option<gdk_pixbuf::Pixbuf>>,
    /// Covers by id. A handful at most: one per player, and the last few
    /// tracks.
    art: HashMap<u64, cairo::ImageSurface>,
    /// The desktop's UI font, read once.
    family: Option<String>,
    /// X11, when the panel stands on a bottom panel: its bottom edge, so it
    /// grows upwards. `None` when it hangs from the top, or on Wayland.
    bottom: Option<i32>,
}

thread_local! {
    static UI: RefCell<Ui> = RefCell::new(Ui::default());
    static LX: RefCell<LinuxState> = RefCell::new(LinuxState {
        pinned: false,
        hidden_at: None,
        on_action: None,
        window: None,
        area: None,
        glyphs: HashMap::new(),
        icons: HashMap::new(),
        art: HashMap::new(),
        family: None,
        bottom: None,
    });
    /// Pixels per logical pixel for bitmaps. `None` means "ask the widget";
    /// set while drawing offscreen, where there is no widget to ask.
    static RENDER_SCALE: Cell<Option<i32>> = const { Cell::new(None) };
}

fn note_hidden() {
    LX.with(|l| l.borrow_mut().hidden_at = Some(Instant::now()));
}

fn since_hidden() -> Option<Duration> {
    LX.with(|l| l.borrow().hidden_at.map(|t| t.elapsed()))
}

fn pinned() -> bool {
    LX.with(|l| l.borrow().pinned)
}

fn dispatch(r: Response) {
    if r.resized {
        fit();
    }
    if r.repaint || r.resized {
        if let Some(area) = LX.with(|l| l.borrow().area.clone()) {
            area.queue_draw();
        }
    }
    if r.actions.is_empty() {
        return;
    }
    let handler = LX.with(|l| l.borrow_mut().on_action.take());
    if let Some(h) = &handler {
        for a in r.actions {
            h(a);
        }
    }
    LX.with(|l| l.borrow_mut().on_action = handler);
}

/// Resize to the content, keeping the edge it was placed against.
fn fit() {
    let h = UI.with(|u| u.borrow().height()).ceil() as i32;
    let (window, area, bottom) = LX.with(|l| {
        let l = l.borrow();
        (l.window.clone(), l.area.clone(), l.bottom)
    });
    if let (Some(window), Some(area)) = (window, area) {
        area.set_size_request(WIDTH as i32, h);
        window.resize(WIDTH as i32, h);
        if let Some(bottom) = bottom {
            let (x, _) = window.position();
            window.move_(x, bottom - h);
        }
    }
}

fn hide_window(window: &gtk::Window) {
    if window.is_visible() {
        window.hide();
        note_hidden();
    }
}

// ---------------------------------------------------------------------------
// Panel
// ---------------------------------------------------------------------------

pub struct Panel {
    window: gtk::Window,
    area: gtk::DrawingArea,
}

impl Panel {
    pub fn new(on_action: Box<dyn Fn(Action)>) -> Result<Self, String> {
        if !gtk::is_initialized_main_thread() {
            return Err("GTK is not running on this thread".into());
        }
        let window = gtk::Window::new(gtk::WindowType::Toplevel);
        window.set_title("Sonus");
        window.set_decorated(false);
        window.set_resizable(false);
        window.set_skip_taskbar_hint(true);
        window.set_skip_pager_hint(true);
        window.set_keep_above(true);
        window.set_type_hint(gdk::WindowTypeHint::Utility);
        window.set_icon_name(Some("sonus"));
        window.set_app_paintable(true);
        // Transparent corners where a compositor can show them; square ones
        // where it cannot.
        if let Some(visual) = WidgetExt::screen(&window).and_then(|s| s.rgba_visual()) {
            window.set_visual(Some(&visual));
        }

        let area = gtk::DrawingArea::new();
        area.set_size_request(WIDTH as i32, 200);
        area.add_events(
            gdk::EventMask::POINTER_MOTION_MASK
                | gdk::EventMask::BUTTON_PRESS_MASK
                | gdk::EventMask::BUTTON_RELEASE_MASK
                | gdk::EventMask::LEAVE_NOTIFY_MASK
                | gdk::EventMask::SCROLL_MASK
                | gdk::EventMask::SMOOTH_SCROLL_MASK,
        );
        area.connect_draw(|area, cr| {
            let scale = RENDER_SCALE
                .with(|r| r.get())
                .unwrap_or_else(|| area.scale_factor());
            let (scene, look, height) = UI.with(|u| {
                let u = u.borrow();
                (u.scene(), u.look(), u.height())
            });
            let dirty = cr
                .clip_extents()
                .map(|(x0, y0, x1, y1)| {
                    Rect::new(x0 as f32, y0 as f32, (x1 - x0) as f32, (y1 - y0) as f32)
                })
                .unwrap_or(Rect::new(0.0, 0.0, WIDTH, height));
            let composited = WidgetExt::screen(area).is_some_and(|s| s.is_composited());
            paint(cr, &scene, look, height, scale, composited, dirty);
            glib::Propagation::Stop
        });
        area.connect_button_press_event(|_, e| {
            if e.button() == 1 && e.event_type() == gdk::EventType::ButtonPress {
                let (x, y) = e.position();
                dispatch(UI.with(|u| u.borrow_mut().mouse_down(x as f32, y as f32)));
            }
            glib::Propagation::Stop
        });
        area.connect_button_release_event(|_, e| {
            if e.button() == 1 {
                let (x, y) = e.position();
                dispatch(UI.with(|u| u.borrow_mut().mouse_up(x as f32, y as f32)));
            }
            glib::Propagation::Stop
        });
        area.connect_motion_notify_event(|_, e| {
            let (x, y) = e.position();
            dispatch(UI.with(|u| u.borrow_mut().mouse_move(x as f32, y as f32)));
            glib::Propagation::Stop
        });
        area.connect_leave_notify_event(|_, _| {
            if !UI.with(|u| u.borrow().dragging()) {
                dispatch(UI.with(|u| u.borrow_mut().mouse_leave()));
            }
            glib::Propagation::Proceed
        });
        // A wheel sends whole steps; a touchpad sends fractions of one. Up is
        // louder either way.
        area.connect_scroll_event(|_, e| {
            let notches = match e.direction() {
                gdk::ScrollDirection::Up => 1.0,
                gdk::ScrollDirection::Down => -1.0,
                gdk::ScrollDirection::Smooth => -e.delta().1,
                _ => 0.0,
            };
            if notches != 0.0 {
                let (x, y) = e.position();
                dispatch(UI.with(|u| u.borrow_mut().wheel(x as f32, y as f32, notches as f32)));
            }
            glib::Propagation::Stop
        });
        window.add(&area);

        // Click-away: the panel closes when another window takes focus.
        window.connect_focus_out_event(|w, _| {
            if !pinned() && !UI.with(|u| u.borrow().dragging()) {
                hide_window(w);
            }
            glib::Propagation::Proceed
        });
        window.connect_key_press_event(|w, e| {
            if e.keyval() == gdk::keys::constants::Escape {
                hide_window(w);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        window.connect_delete_event(|w, _| {
            hide_window(w);
            glib::Propagation::Stop
        });
        area.show();

        LX.with(|l| {
            let mut l = l.borrow_mut();
            l.on_action = Some(on_action);
            l.window = Some(window.clone());
            l.area = Some(area.clone());
        });
        Ok(Self { window, area })
    }

    pub fn is_visible(&self) -> bool {
        self.window.is_visible()
    }

    pub fn hide(&self) {
        hide_window(&self.window);
    }

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

    pub fn show(&self, model: Snapshot, look: Look, _anchor: Option<Anchor>) {
        UI.with(|u| u.borrow_mut().open(model, look));
        let h = UI.with(|u| u.borrow().height()).ceil() as i32;
        let placed = place_by_pointer(WIDTH as i32, h);
        LX.with(|l| l.borrow_mut().bottom = placed.and_then(|(_, _, bottom)| bottom));
        fit();
        if let Some((x, y, _)) = placed {
            self.window.move_(x, y);
            ldebug!("panel shown at {x},{y} ({}x{h})", WIDTH);
        }
        self.area.queue_draw();
        self.window.present();
    }

    pub fn update(&self, model: Snapshot, look: Look) {
        if !self.is_visible() {
            return;
        }
        if UI.with(|u| u.borrow_mut().set_model(model, look)) {
            fit();
        }
        self.area.queue_draw();
    }

    /// Only the strips whose meters moved are redrawn.
    pub fn set_peaks(&self, peaks: &Peaks) {
        if !self.is_visible() {
            return;
        }
        for r in UI.with(|u| u.borrow_mut().set_peaks(peaks)) {
            self.area.queue_draw_area(
                r.x.floor() as i32,
                r.y.floor() as i32,
                r.w.ceil() as i32 + 1,
                r.h.ceil() as i32 + 1,
            );
        }
    }

    pub fn busy(&self) -> bool {
        UI.with(|u| u.borrow().dragging())
    }
}

impl Drop for Panel {
    fn drop(&mut self) {
        // SAFETY: the window is ours alone, and nothing touches it after this.
        unsafe { self.window.destroy() };
    }
}

/// Where the panel goes on X11: under the pointer, which has just clicked the
/// tray menu, hanging from the top of the work area or standing on its
/// bottom, whichever edge the pointer is nearer. The third value is the
/// bottom edge when it stands on the bottom. `None` on Wayland, where the
/// compositor decides.
fn place_by_pointer(w: i32, h: i32) -> Option<(i32, i32, Option<i32>)> {
    let display = gdk::Display::default()?;
    if display.type_().name().contains("Wayland") {
        return None;
    }
    let pointer = display.default_seat()?.pointer()?;
    let (_, px, py) = pointer.position();
    let monitor = display.monitor_at_point(px, py)?;
    let area = monitor.workarea();
    let (x, y) = place(
        (px, py),
        (w, h),
        (area.x(), area.y(), area.width(), area.height()),
    );
    let bottom = (y > area.y() + EDGE_GAP).then_some(y + h);
    Some((x, y, bottom))
}

/// Apart from the display query so it can be tested.
fn place(pointer: (i32, i32), size: (i32, i32), work: (i32, i32, i32, i32)) -> (i32, i32) {
    let (px, py) = pointer;
    let (w, h) = size;
    let (ax, ay, aw, ah) = work;
    let min_x = ax + EDGE_GAP;
    let max_x = (ax + aw - w - EDGE_GAP).max(min_x);
    let x = (px - w / 2).clamp(min_x, max_x);
    let y = if py < ay + ah / 2 {
        ay + EDGE_GAP
    } else {
        (ay + ah - h - EDGE_GAP).max(ay + EDGE_GAP)
    };
    (x, y)
}

// ---------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------

fn paint(
    cr: &cairo::Context,
    scene: &[Cmd],
    look: Look,
    height: f32,
    scale: i32,
    composited: bool,
    dirty: Rect,
) {
    let p = Palette::new(look);
    let radius = if composited { CORNER } else { 0.0 };
    cr.set_operator(cairo::Operator::Source);
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
    let _ = cr.paint();
    cr.set_operator(cairo::Operator::Over);

    rounded(cr, Rect::new(0.0, 0.0, WIDTH, height), radius);
    set_color(cr, p.bg);
    let _ = cr.fill();
    rounded(
        cr,
        Rect::new(0.5, 0.5, WIDTH - 1.0, height - 1.0),
        (radius - 0.5).max(0.0),
    );
    set_color(cr, p.border);
    cr.set_line_width(1.0);
    let _ = cr.stroke();

    for cmd in scene {
        // A meter tick redraws a strip; text outside it is not laid out again.
        if !dirty.intersects(&cmd.bounds()) {
            continue;
        }
        match cmd {
            Cmd::Fill { r, radius, color } => {
                rounded(cr, *r, *radius);
                set_color(cr, *color);
                let _ = cr.fill();
            }
            Cmd::Stroke {
                r,
                radius,
                width,
                color,
            } => {
                rounded(cr, *r, *radius);
                set_color(cr, *color);
                cr.set_line_width(*width as f64);
                let _ = cr.stroke();
            }
            Cmd::Text {
                r,
                text,
                size,
                weight,
                color,
                align,
            } => draw_text(cr, text, *r, *size, *weight, *color, *align),
            Cmd::Glyph { r, glyph, color } => {
                if let Some(s) = glyph_surface(*glyph, *color, r.w, scale) {
                    draw_surface(cr, &s, *r, scale, 1.0);
                }
            }
            Cmd::AppIcon {
                r,
                path,
                fallback,
                opacity,
            } => match path.as_deref().and_then(|p| app_icon(p, r.w, scale)) {
                Some(pixbuf) => draw_pixbuf(cr, &pixbuf, *r, *opacity),
                None => {
                    let (tile, letter) = fallback;
                    rounded(cr, *r, r.w * 0.25);
                    set_color(cr, tile.alpha(*opacity));
                    let _ = cr.fill();
                    draw_text(
                        cr,
                        letter,
                        *r,
                        r.w * 0.5,
                        Weight::Semibold,
                        Rgba::white(*opacity),
                        Align::Center,
                    );
                }
            },
            Cmd::Art { r, art } => {
                if let Some(s) = art_surface(art) {
                    let surface_scale = art.edge as f64 / r.w as f64;
                    s.set_device_scale(surface_scale, surface_scale);
                    let _ = cr.set_source_surface(&s, r.x as f64, r.y as f64);
                    let _ = cr.paint();
                }
            }
        }
    }
}

fn set_color(cr: &cairo::Context, c: Rgba) {
    cr.set_source_rgba(c.0 as f64, c.1 as f64, c.2 as f64, c.3 as f64);
}

fn rounded(cr: &cairo::Context, r: Rect, radius: f32) {
    let (x, y, w, h) = (r.x as f64, r.y as f64, r.w as f64, r.h as f64);
    let rad = (radius as f64).min(w / 2.0).min(h / 2.0);
    cr.new_path();
    if rad <= 0.0 {
        cr.rectangle(x, y, w, h);
        return;
    }
    use std::f64::consts::{FRAC_PI_2, PI};
    cr.arc(x + w - rad, y + rad, rad, -FRAC_PI_2, 0.0);
    cr.arc(x + w - rad, y + h - rad, rad, 0.0, FRAC_PI_2);
    cr.arc(x + rad, y + h - rad, rad, FRAC_PI_2, PI);
    cr.arc(x + rad, y + rad, rad, PI, PI + FRAC_PI_2);
    cr.close_path();
}

/// The desktop's UI font family (Cantarell, Ubuntu, Noto Sans...), or Sans.
fn family() -> String {
    if let Some(f) = LX.with(|l| l.borrow().family.clone()) {
        return f;
    }
    let family = gtk::is_initialized_main_thread()
        .then(gtk::Settings::default)
        .flatten()
        .and_then(|s| s.gtk_font_name())
        .map(|name| pango::FontDescription::from_string(&name))
        .and_then(|d| d.family().map(|f| f.to_string()))
        .unwrap_or_else(|| "Sans".into());
    LX.with(|l| l.borrow_mut().family = Some(family.clone()));
    family
}

/// One line, vertically centred in `r`, cut with an ellipsis if too long.
fn draw_text(
    cr: &cairo::Context,
    text: &str,
    r: Rect,
    size: f32,
    weight: Weight,
    c: Rgba,
    align: Align,
) {
    let layout = pangocairo::functions::create_layout(cr);
    let mut font = pango::FontDescription::new();
    font.set_family(&family());
    font.set_absolute_size(size as f64 * pango::SCALE as f64);
    font.set_weight(match weight {
        Weight::Regular => pango::Weight::Normal,
        Weight::Semibold => pango::Weight::Semibold,
    });
    layout.set_font_description(Some(&font));
    if align == Align::Right {
        // Numbers that change under a slider should not jiggle sideways.
        let attrs = pango::AttrList::new();
        attrs.insert(pango::AttrFontFeatures::new("tnum"));
        layout.set_attributes(Some(&attrs));
    }
    layout.set_text(text);
    layout.set_width((r.w.max(0.0) * pango::SCALE as f32) as i32);
    layout.set_ellipsize(pango::EllipsizeMode::End);
    layout.set_single_paragraph_mode(true);
    layout.set_alignment(match align {
        Align::Left => pango::Alignment::Left,
        Align::Center => pango::Alignment::Center,
        Align::Right => pango::Alignment::Right,
    });
    let (_, logical) = layout.pixel_extents();
    let y = r.y as f64 + (r.h as f64 - logical.height() as f64) / 2.0;
    set_color(cr, c);
    cr.move_to(r.x as f64, y);
    pangocairo::functions::show_layout(cr, &layout);
}

/// Straight-alpha RGBA as a cairo surface, which is premultiplied, in the
/// machine's byte order.
fn surface_from_rgba(rgba: &[u8], w: u32, h: u32) -> Option<cairo::ImageSurface> {
    let mut surface =
        cairo::ImageSurface::create(cairo::Format::ARgb32, w as i32, h as i32).ok()?;
    let stride = surface.stride() as usize;
    {
        let mut data = surface.data().ok()?;
        for y in 0..h as usize {
            for x in 0..w as usize {
                let s = &rgba[(y * w as usize + x) * 4..][..4];
                let a = s[3] as u32;
                let pre = |c: u8| ((c as u32 * a + 127) / 255) as u8;
                let px = (a << 24)
                    | (pre(s[0]) as u32) << 16
                    | (pre(s[1]) as u32) << 8
                    | pre(s[2]) as u32;
                data[y * stride + x * 4..][..4].copy_from_slice(&px.to_ne_bytes());
            }
        }
    }
    Some(surface)
}

/// A surface made at `scale` pixels per logical pixel, drawn into `r`.
fn draw_surface(cr: &cairo::Context, s: &cairo::ImageSurface, r: Rect, scale: i32, opacity: f32) {
    s.set_device_scale(scale as f64, scale as f64);
    let _ = cr.set_source_surface(s, r.x as f64, r.y as f64);
    let _ = cr.paint_with_alpha(opacity as f64);
}

fn draw_pixbuf(cr: &cairo::Context, pixbuf: &gdk_pixbuf::Pixbuf, r: Rect, opacity: f32) {
    let _ = cr.save();
    cr.translate(r.x as f64, r.y as f64);
    let k = r.w as f64 / pixbuf.width().max(1) as f64;
    cr.scale(k, k);
    cr.set_source_pixbuf(pixbuf, 0.0, 0.0);
    let _ = cr.paint_with_alpha(opacity as f64);
    let _ = cr.restore();
}

fn glyph_surface(g: icon::Glyph, c: Rgba, logical: f32, scale: i32) -> Option<cairo::ImageSurface> {
    let px = (logical * scale as f32).round().max(1.0) as u32;
    let tint = [
        (c.0 * 255.0) as u8,
        (c.1 * 255.0) as u8,
        (c.2 * 255.0) as u8,
        (c.3 * 255.0) as u8,
    ];
    let key = (g, px, tint);
    if let Some(s) = LX.with(|l| l.borrow().glyphs.get(&key).cloned()) {
        return Some(s);
    }
    let mut rgba = icon::glyph(g, px, 0);
    for p in rgba.chunks_mut(4) {
        p[0] = tint[0];
        p[1] = tint[1];
        p[2] = tint[2];
        p[3] = ((p[3] as u32 * tint[3] as u32) / 255) as u8;
    }
    let s = surface_from_rgba(&rgba, px, px)?;
    LX.with(|l| l.borrow_mut().glyphs.insert(key, s.clone()));
    Some(s)
}

/// An app's icon from the icon theme, by the name its sound stream gave, or
/// from a file when that name is a path.
fn app_icon(name: &str, logical: f32, scale: i32) -> Option<gdk_pixbuf::Pixbuf> {
    let px = (logical * scale as f32).round() as i32;
    let key = (name.to_string(), px);
    if let Some(found) = LX.with(|l| l.borrow().icons.get(&key).cloned()) {
        return found;
    }
    let found = if name.starts_with('/') {
        gdk_pixbuf::Pixbuf::from_file_at_size(name, px, px).ok()
    } else if gtk::is_initialized_main_thread() {
        gtk::IconTheme::default().and_then(|theme| {
            theme
                .load_icon(name, px, gtk::IconLookupFlags::FORCE_SIZE)
                .ok()
                .flatten()
        })
    } else {
        None
    };
    LX.with(|l| l.borrow_mut().icons.insert(key, found.clone()));
    found
}

fn art_surface(art: &crate::media::Art) -> Option<cairo::ImageSurface> {
    if let Some(s) = LX.with(|l| l.borrow().art.get(&art.id).cloned()) {
        return Some(s);
    }
    let s = surface_from_rgba(&art.rgba, art.edge, art.edge)?;
    LX.with(|l| {
        let mut l = l.borrow_mut();
        if l.art.len() >= 16 {
            l.art.clear();
        }
        l.art.insert(art.id, s.clone());
    });
    Some(s)
}

// ---------------------------------------------------------------------------
// Offscreen render and preview
// ---------------------------------------------------------------------------

/// Draw the panel into a bitmap and hand back its pixels, never putting a
/// window on screen: the same painter, into an image surface. Needs no
/// display; with one, it uses the desktop's font and icon theme. Returns
/// `(width, height, straight-alpha RGBA)`.
pub fn render_to_rgba(
    (model, look, peaks, devices): (Snapshot, Look, Peaks, bool),
    scale: u32,
) -> Option<(u32, u32, Vec<u8>)> {
    let scale = scale.max(1);
    let _ = gtk::init();
    UI.with(|u| {
        let mut u = u.borrow_mut();
        u.open(model, look);
        if devices {
            u.show_devices();
        }
        u.set_peaks(&peaks);
    });
    let (scene, height) = UI.with(|u| {
        let u = u.borrow();
        (u.scene(), u.height())
    });
    let (w, h) = (WIDTH as u32 * scale, height.ceil() as u32 * scale);
    let mut surface =
        cairo::ImageSurface::create(cairo::Format::ARgb32, w as i32, h as i32).ok()?;
    {
        let cr = cairo::Context::new(&surface).ok()?;
        cr.scale(scale as f64, scale as f64);
        RENDER_SCALE.with(|r| r.set(Some(scale as i32)));
        paint(
            &cr,
            &scene,
            look,
            height,
            scale as i32,
            true,
            Rect::new(0.0, 0.0, WIDTH, height),
        );
        RENDER_SCALE.with(|r| r.set(None));
    }
    surface.flush();
    let stride = surface.stride() as usize;
    let data = surface.data().ok()?;
    let mut out = vec![0u8; (w * h * 4) as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let px = u32::from_ne_bytes(data[y * stride + x * 4..][..4].try_into().ok()?);
            let a = px >> 24;
            if a == 0 {
                continue;
            }
            let un = |c: u32| ((c * 255 + a / 2) / a).min(255) as u8;
            let o = &mut out[(y * w as usize + x) * 4..][..4];
            o[0] = un((px >> 16) & 0xff);
            o[1] = un((px >> 8) & 0xff);
            o[2] = un(px & 0xff);
            o[3] = a as u8;
        }
    }
    Some((w, h, out))
}

/// Show the panel with sample content and run GTK until the timeout, so the
/// rendering can be looked at, and moved, clicked, dragged.
pub fn preview(dark: bool, seconds: u64) {
    if gtk::init().is_err() {
        eprintln!("no display");
        return;
    }
    let (model, look, peaks, _) = sample(dark, false);
    let Ok(panel) = Panel::new(Box::new(|a| eprintln!("{a:?}"))) else {
        return;
    };
    LX.with(|l| l.borrow_mut().pinned = true);
    panel.show(model, look, None);
    panel.set_peaks(&peaks);

    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        while gtk::events_pending() {
            gtk::main_iteration();
        }
        std::thread::sleep(Duration::from_millis(16));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORK: (i32, i32, i32, i32) = (0, 32, 1920, 1048);

    #[test]
    fn the_panel_hangs_under_a_top_bar() {
        assert_eq!(place((1800, 10), (360, 400), WORK), (1554, 38));
    }

    #[test]
    fn the_panel_stands_on_a_bottom_panel() {
        let (_, y) = place((900, 1070), (360, 400), WORK);
        assert_eq!(y + 400, 32 + 1048 - EDGE_GAP);
    }

    #[test]
    fn the_panel_stays_inside_the_work_area() {
        let (x, _) = place((2, 10), (360, 400), WORK);
        assert_eq!(x, EDGE_GAP);
    }

    #[test]
    fn surfaces_premultiply() {
        let s = surface_from_rgba(&[255, 0, 0, 128], 1, 1).unwrap();
        let mut s = s;
        let d = s.data().unwrap();
        let px = u32::from_ne_bytes(d[..4].try_into().unwrap());
        assert_eq!(px >> 24, 128);
        assert_eq!((px >> 16) & 0xff, 128, "red, premultiplied");
    }
}
