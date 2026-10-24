//! The macOS panel: a borderless popup over a vibrant material, painted with
//! AppKit.
//!
//! The window's content is an `NSVisualEffectView` with the menu material, the
//! same frosted glass Control Center and the menu bar's own menus are made of,
//! so the panel takes on the colour of whatever is behind it the way they do.
//! On top of it, one flipped view paints the commands [`Ui`] produces and
//! hands it the mouse.
//!
//! Everything here runs on the main thread (AppKit demands it), so state
//! lives in `thread_local`s, exactly as on Windows.

use super::*;
use crate::icon;
use crate::log::ldebug;

use std::cell::{Cell, RefCell};
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, AllocAnyThread, MainThreadOnly};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBackingStoreType, NSBezierPath, NSBitmapFormat, NSBitmapImageRep,
    NSColor, NSCompositingOperation, NSDeviceRGBColorSpace, NSEvent, NSEventMask, NSFont,
    NSFontAttributeName, NSForegroundColorAttributeName, NSGraphicsContext, NSImage,
    NSLineBreakMode, NSMutableParagraphStyle, NSPanel, NSParagraphStyleAttributeName, NSScreen,
    NSTextAlignment, NSTrackingArea, NSTrackingAreaOptions, NSView, NSVisualEffectBlendingMode,
    NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView, NSWindowCollectionBehavior,
    NSWindowStyleMask, NSWorkspace,
};
use objc2_foundation::{
    MainThreadMarker, NSDate, NSDefaultRunLoopMode, NSMutableDictionary, NSPoint, NSRect,
    NSRunLoop, NSSize, NSString,
};

// AppKit's `NSStatusWindowLevel`: level with menu-bar extras' own popups.
const STATUS_WINDOW_LEVEL: isize = 25;
const EDGE_GAP: f64 = 6.0;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

struct MacState {
    pinned: bool,
    hidden_at: Option<Instant>,
    on_action: Option<Box<dyn Fn(Action)>>,
    window: Option<Retained<NSPanel>>,
    glyphs: HashMap<(icon::Glyph, u32, [u8; 4]), Retained<NSImage>>,
    /// Bundle icons by path. NSWorkspace's are resolution-independent, so one
    /// image serves every size and scale.
    icons: HashMap<String, Retained<NSImage>>,
    /// Covers by id. A handful at most: one per player, and the last few
    /// tracks.
    art: HashMap<u64, Retained<NSImage>>,
}

thread_local! {
    static UI: RefCell<Ui> = RefCell::new(Ui::default());
    static MAC: RefCell<MacState> = RefCell::new(MacState {
        pinned: false,
        hidden_at: None,
        on_action: None,
        window: None,
        glyphs: HashMap::new(),
        icons: HashMap::new(),
        art: HashMap::new(),
    });
    /// Pixels per point for glyph bitmaps. `None` means "ask the window"; set
    /// while drawing offscreen, where there is no window to ask.
    static RENDER_SCALE: Cell<Option<f64>> = const { Cell::new(None) };
}

fn note_hidden() {
    MAC.with(|m| m.borrow_mut().hidden_at = Some(Instant::now()));
}

fn since_hidden() -> Option<Duration> {
    MAC.with(|m| m.borrow().hidden_at.map(|t| t.elapsed()))
}

fn pinned() -> bool {
    MAC.with(|m| m.borrow().pinned)
}

// ---------------------------------------------------------------------------
// The content view
// ---------------------------------------------------------------------------

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "SonusPanelView"]
    struct PanelView;

    impl PanelView {
        // Top-left origin, y down: the coordinate system the layout uses.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        // React to the very first click even though the app is never frontmost:
        // a menu-bar popup is clicked from wherever the user already was.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: *mut NSEvent) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, dirty: NSRect) {
            let scale = RENDER_SCALE
                .with(|r| r.get())
                .or_else(|| self.window().map(|w| w.backingScaleFactor()))
                .unwrap_or(2.0);
            let (scene, look, height) = UI.with(|u| {
                let u = u.borrow();
                (u.scene(), u.look(), u.height())
            });
            let dirty = Rect::new(
                dirty.origin.x as f32,
                dirty.origin.y as f32,
                dirty.size.width as f32,
                dirty.size.height as f32,
            );
            let offscreen = RENDER_SCALE.with(|r| r.get()).is_some();
            paint(&scene, look, height, scale, offscreen, dirty);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let (x, y) = self.point(event);
            let r = UI.with(|u| u.borrow_mut().mouse_down(x, y));
            self.dispatch(r);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let (x, y) = self.point(event);
            let r = UI.with(|u| u.borrow_mut().mouse_move(x, y));
            self.dispatch(r);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            let (x, y) = self.point(event);
            let r = UI.with(|u| u.borrow_mut().mouse_up(x, y));
            self.dispatch(r);
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            let (x, y) = self.point(event);
            let r = UI.with(|u| u.borrow_mut().mouse_move(x, y));
            self.dispatch(r);
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            if !UI.with(|u| u.borrow().dragging()) {
                let r = UI.with(|u| u.borrow_mut().mouse_leave());
                self.dispatch(r);
            }
        }

        /// A mouse wheel sends lines, a trackpad sends points: about a dozen
        /// points make one notch. Up is louder either way, whatever the
        /// natural-scrolling setting does to the content of other windows.
        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            let (x, y) = self.point(event);
            let mut dy = event.scrollingDeltaY();
            if event.hasPreciseScrollingDeltas() {
                dy /= 12.0;
            }
            if event.isDirectionInvertedFromDevice() {
                dy = -dy;
            }
            let r = UI.with(|u| u.borrow_mut().wheel(x, y, dy as f32));
            self.dispatch(r);
        }
    }
);

impl PanelView {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm);
        unsafe { msg_send![this, initWithFrame: frame] }
    }

    fn point(&self, event: &NSEvent) -> (f32, f32) {
        let p = self.convertPoint_fromView(event.locationInWindow(), None);
        (p.x as f32, p.y as f32)
    }

    fn dispatch(&self, r: Response) {
        if r.resized {
            if let Some(w) = self.window() {
                fit(&w);
            }
        }
        if r.repaint || r.resized {
            self.setNeedsDisplay(true);
        }
        if r.actions.is_empty() {
            return;
        }
        let handler = MAC.with(|m| m.borrow_mut().on_action.take());
        if let Some(h) = &handler {
            for a in r.actions {
                h(a);
            }
        }
        MAC.with(|m| m.borrow_mut().on_action = handler);
    }
}

/// Resize to the content, keeping the top edge under the menu bar.
fn fit(window: &objc2_app_kit::NSWindow) {
    let h = UI.with(|u| u.borrow().height()) as f64;
    let old = window.frame();
    let top = old.origin.y + old.size.height;
    window.setFrame_display(
        NSRect::new(
            NSPoint::new(old.origin.x, top - h),
            NSSize::new(WIDTH as f64, h),
        ),
        true,
    );
    window.invalidateShadow();
}

// ---------------------------------------------------------------------------
// Panel
// ---------------------------------------------------------------------------

pub struct Panel {
    window: Retained<NSPanel>,
    view: Retained<PanelView>,
    /// The global click monitor, kept alive for the life of the panel.
    _monitor: Option<Retained<AnyObject>>,
    mtm: MainThreadMarker,
}

impl Panel {
    pub fn new(on_action: Box<dyn Fn(Action)>) -> Result<Self, String> {
        let mtm = MainThreadMarker::new().ok_or("panel must be created on the main thread")?;
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH as f64, 200.0));

        let effect = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), frame);
        effect.setMaterial(NSVisualEffectMaterial::Menu);
        effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
        effect.setState(NSVisualEffectState::Active);
        effect.setWantsLayer(true);
        if let Some(layer) = effect.layer() {
            unsafe {
                let _: () = msg_send![&layer, setCornerRadius: CORNER as f64];
                let _: () = msg_send![&layer, setMasksToBounds: true];
            }
        }

        let view = PanelView::new(mtm, frame);
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        effect.addSubview(&view);
        let tracking = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                NSRect::ZERO,
                NSTrackingAreaOptions::MouseEnteredAndExited
                    | NSTrackingAreaOptions::MouseMoved
                    | NSTrackingAreaOptions::ActiveAlways
                    | NSTrackingAreaOptions::InVisibleRect,
                Some(&view),
                None,
            )
        };
        view.addTrackingArea(&tracking);

        let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
        let window: Retained<NSPanel> = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            frame,
            style,
            NSBackingStoreType::Buffered,
            false,
        );
        unsafe {
            window.setLevel(STATUS_WINDOW_LEVEL);
            window.setOpaque(false);
            window.setBackgroundColor(Some(&NSColor::clearColor()));
            window.setHasShadow(true);
            window.setHidesOnDeactivate(false);
            window.setReleasedWhenClosed(false);
            window.setAcceptsMouseMovedEvents(true);
            window.setContentView(Some(&effect));
            window.setCollectionBehavior(
                NSWindowCollectionBehavior::CanJoinAllSpaces
                    | NSWindowCollectionBehavior::FullScreenAuxiliary,
            );
        }

        // Click-away: a mouse-down in any other application hides the panel.
        // The shared reopen guard covers the race with a click on the status
        // item itself.
        let win_for_block = window.clone();
        let handler = RcBlock::new(move |_event: NonNull<NSEvent>| {
            if pinned() || UI.with(|u| u.borrow().dragging()) {
                return;
            }
            if win_for_block.isVisible() {
                win_for_block.orderOut(None);
                note_hidden();
            }
        });
        let monitor = NSEvent::addGlobalMonitorForEventsMatchingMask_handler(
            NSEventMask::LeftMouseDown | NSEventMask::RightMouseDown,
            &handler,
        );

        MAC.with(|m| {
            let mut m = m.borrow_mut();
            m.on_action = Some(on_action);
            m.window = Some(window.clone());
        });
        Ok(Self {
            window,
            view,
            _monitor: monitor,
            mtm,
        })
    }

    pub fn is_visible(&self) -> bool {
        self.window.isVisible()
    }

    pub fn hide(&self) {
        if self.is_visible() {
            self.window.orderOut(None);
            note_hidden();
        }
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

    /// Show the panel hanging from the menu bar, under the pointer, which is
    /// over the status item that was just clicked.
    pub fn show(&self, model: Snapshot, look: Look, _anchor: Option<Anchor>) {
        UI.with(|u| u.borrow_mut().open(model, look));
        let h = UI.with(|u| u.borrow().height()) as f64;
        let w = WIDTH as f64;
        let mouse = NSEvent::mouseLocation();
        let visible = self.visible_frame_under(mouse);
        let origin = place(mouse, w, h, visible);
        self.window
            .setFrame_display(NSRect::new(origin, NSSize::new(w, h)), true);
        self.view.setNeedsDisplay(true);
        self.window.orderFrontRegardless();
        self.window.invalidateShadow();
        ldebug!("panel shown at {},{} ({w}x{h})", origin.x, origin.y);
    }

    pub fn update(&self, model: Snapshot, look: Look) {
        if !self.is_visible() {
            return;
        }
        if UI.with(|u| u.borrow_mut().set_model(model, look)) {
            fit(&self.window);
        }
        self.view.setNeedsDisplay(true);
    }

    /// Only the strips whose meters moved are redrawn. AppKit keeps them
    /// apart, where Windows would merge them into one rectangle.
    pub fn set_peaks(&self, peaks: &Peaks) {
        if !self.is_visible() {
            return;
        }
        for r in UI.with(|u| u.borrow_mut().set_peaks(peaks)) {
            self.view.setNeedsDisplayInRect(ns_rect(r));
        }
    }

    pub fn busy(&self) -> bool {
        UI.with(|u| u.borrow().dragging())
    }

    fn visible_frame_under(&self, mouse: NSPoint) -> NSRect {
        let screens = NSScreen::screens(self.mtm);
        for i in 0..screens.count() {
            let s = screens.objectAtIndex(i);
            let f = s.frame();
            if mouse.x >= f.origin.x
                && mouse.x <= f.origin.x + f.size.width
                && mouse.y >= f.origin.y
                && mouse.y <= f.origin.y + f.size.height
            {
                return s.visibleFrame();
            }
        }
        NSScreen::mainScreen(self.mtm)
            .map(|s| s.visibleFrame())
            .unwrap_or(NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(1440.0, 900.0),
            ))
    }
}

impl Drop for Panel {
    fn drop(&mut self) {
        self.window.close();
    }
}

/// The panel's bottom-left origin (macOS screen coordinates, y up): its top
/// just under the menu bar, centred on the pointer, kept inside the visible
/// frame. Apart from the screen query so it can be tested.
fn place(mouse: NSPoint, w: f64, h: f64, visible: NSRect) -> NSPoint {
    let min_x = visible.origin.x + EDGE_GAP;
    let max_x = (visible.origin.x + visible.size.width - w - EDGE_GAP).max(min_x);
    let x = (mouse.x - w / 2.0).clamp(min_x, max_x);
    // `visibleFrame` already excludes the menu bar.
    let top = visible.origin.y + visible.size.height - EDGE_GAP;
    let y = (top - h).max(visible.origin.y + EDGE_GAP);
    NSPoint::new(x, y)
}

// ---------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------

fn paint(scene: &[Cmd], look: Look, height: f32, scale: f64, offscreen: bool, dirty: Rect) {
    let p = Palette::new(look);
    let whole = NSRect::new(
        NSPoint::new(0.5, 0.5),
        NSSize::new(WIDTH as f64 - 1.0, height as f64 - 1.0),
    );
    let outline = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
        whole,
        CORNER as f64 - 0.5,
        CORNER as f64 - 0.5,
    );
    if offscreen {
        // No material offscreen: stand in for it with the colour it
        // usually averages to.
        color(if look.dark {
            Rgba::hex(0x2a2a2c)
        } else {
            Rgba::hex(0xececec)
        })
        .setFill();
        outline.fill();
    }
    color(p.border).setStroke();
    outline.setLineWidth(1.0);
    outline.stroke();

    for cmd in scene {
        // A meter tick redraws a strip; text outside it is not laid out again.
        if !dirty.intersects(&cmd.bounds()) {
            continue;
        }
        match cmd {
            Cmd::Fill {
                r,
                radius,
                color: c,
            } => {
                color(*c).setFill();
                path(*r, *radius).fill();
            }
            Cmd::Stroke {
                r,
                radius,
                width,
                color: c,
            } => {
                color(*c).setStroke();
                let path = path(*r, *radius);
                path.setLineWidth(*width as f64);
                path.stroke();
            }
            Cmd::Text {
                r,
                text,
                size,
                weight,
                color: c,
                align,
            } => draw_text(text, *r, *size, *weight, *c, *align),
            Cmd::Glyph { r, glyph, color: c } => {
                if let Some(img) = glyph_image(*glyph, *c, r.w, scale) {
                    draw_image(&img, *r, 1.0);
                }
            }
            Cmd::AppIcon {
                r,
                path: icon_path,
                fallback,
                opacity,
            } => match icon_path.as_deref().map(app_icon) {
                Some(img) => draw_image(&img, *r, *opacity),
                None => {
                    let (tile, letter) = fallback;
                    color(tile.alpha(*opacity)).setFill();
                    path(*r, r.w * 0.25).fill();
                    draw_text(
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
                if let Some(img) = art_image(art) {
                    draw_image(&img, *r, 1.0);
                }
            }
        }
    }
}

fn art_image(art: &crate::media::Art) -> Option<Retained<NSImage>> {
    if let Some(img) = MAC.with(|m| m.borrow().art.get(&art.id).cloned()) {
        return Some(img);
    }
    let rep = rep_from_rgba(&art.rgba, art.edge, art.edge)?;
    let img = NSImage::initWithSize(NSImage::alloc(), NSSize::new(40.0, 40.0));
    img.addRepresentation(&rep);
    MAC.with(|m| {
        let mut m = m.borrow_mut();
        if m.art.len() >= 16 {
            m.art.clear();
        }
        m.art.insert(art.id, img.clone());
    });
    Some(img)
}

fn ns_rect(r: Rect) -> NSRect {
    NSRect::new(
        NSPoint::new(r.x as f64, r.y as f64),
        NSSize::new(r.w as f64, r.h as f64),
    )
}

fn path(r: Rect, radius: f32) -> Retained<NSBezierPath> {
    if radius <= 0.0 {
        NSBezierPath::bezierPathWithRect(ns_rect(r))
    } else {
        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
            ns_rect(r),
            radius as f64,
            radius as f64,
        )
    }
}

fn color(c: Rgba) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(c.0 as f64, c.1 as f64, c.2 as f64, c.3 as f64)
}

fn font(size: f32, weight: Weight, digits: bool) -> Retained<NSFont> {
    // NSFontWeightRegular and NSFontWeightSemibold.
    let w = match weight {
        Weight::Regular => 0.0,
        Weight::Semibold => 0.3,
    };
    if digits {
        // Numbers that change under a slider should not jiggle sideways.
        NSFont::monospacedDigitSystemFontOfSize_weight(size as f64, w)
    } else {
        NSFont::systemFontOfSize_weight(size as f64, w)
    }
}

/// One line, vertically centred in `r`, cut with an ellipsis if too long.
fn draw_text(text: &str, r: Rect, size: f32, weight: Weight, c: Rgba, align: Align) {
    let f = font(size, weight, align == Align::Right);
    let para = NSMutableParagraphStyle::new();
    para.setAlignment(match align {
        Align::Left => NSTextAlignment::Left,
        Align::Center => NSTextAlignment::Center,
        Align::Right => NSTextAlignment::Right,
    });
    para.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    let col = color(c);
    let attrs: Retained<NSMutableDictionary> = NSMutableDictionary::new();
    unsafe {
        let _: () = msg_send![&attrs, setObject: &*f, forKey: NSFontAttributeName];
        let _: () = msg_send![&attrs, setObject: &*col, forKey: NSForegroundColorAttributeName];
        let _: () = msg_send![&attrs, setObject: &*para, forKey: NSParagraphStyleAttributeName];
    }
    let line = f.ascender() - f.descender();
    let y = r.y as f64 + (r.h as f64 - line) / 2.0;
    let rect = NSRect::new(
        NSPoint::new(r.x as f64, y),
        NSSize::new(r.w.max(0.0) as f64, line + 2.0),
    );
    let ns = NSString::from_str(text);
    unsafe {
        let _: () = msg_send![&ns, drawInRect: rect, withAttributes: &*attrs];
    }
}

fn draw_image(img: &NSImage, r: Rect, opacity: f32) {
    // `respectFlipped` so the image is not drawn upside down in this view.
    unsafe {
        let _: () = msg_send![
            img,
            drawInRect: ns_rect(r),
            fromRect: NSRect::ZERO,
            operation: NSCompositingOperation::SourceOver,
            fraction: opacity as f64,
            respectFlipped: true,
            hints: std::ptr::null::<AnyObject>()
        ];
    }
}

fn glyph_image(g: icon::Glyph, c: Rgba, logical: f32, scale: f64) -> Option<Retained<NSImage>> {
    let px = (logical as f64 * scale).round().max(1.0) as u32;
    let tint = [
        (c.0 * 255.0) as u8,
        (c.1 * 255.0) as u8,
        (c.2 * 255.0) as u8,
        (c.3 * 255.0) as u8,
    ];
    let key = (g, px, tint);
    if let Some(img) = MAC.with(|m| m.borrow().glyphs.get(&key).cloned()) {
        return Some(img);
    }
    let mut rgba = icon::glyph(g, px, 0);
    for p in rgba.chunks_mut(4) {
        p[0] = tint[0];
        p[1] = tint[1];
        p[2] = tint[2];
        p[3] = ((p[3] as u32 * tint[3] as u32) / 255) as u8;
    }
    let rep = rep_from_rgba(&rgba, px, px)?;
    let size = NSSize::new(logical as f64, logical as f64);
    let img = NSImage::initWithSize(NSImage::alloc(), size);
    img.addRepresentation(&rep);
    MAC.with(|m| m.borrow_mut().glyphs.insert(key, img.clone()));
    Some(img)
}

fn app_icon(bundle: &str) -> Retained<NSImage> {
    if let Some(img) = MAC.with(|m| m.borrow().icons.get(bundle).cloned()) {
        return img;
    }
    let img = NSWorkspace::sharedWorkspace().iconForFile(&NSString::from_str(bundle));
    MAC.with(|m| m.borrow_mut().icons.insert(bundle.to_string(), img.clone()));
    img
}

/// An `NSBitmapImageRep` holding straight-alpha RGBA.
fn rep_from_rgba(rgba: &[u8], w: u32, h: u32) -> Option<Retained<NSBitmapImageRep>> {
    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bitmapFormat_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            w as isize,
            h as isize,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            NSBitmapFormat::AlphaNonpremultiplied,
            (w * 4) as isize,
            32,
        )
    }?;
    unsafe {
        let dst = rep.bitmapData();
        if dst.is_null() {
            return None;
        }
        let stride = rep.bytesPerRow() as usize;
        let row_bytes = (w * 4) as usize;
        for row in 0..h as usize {
            let src = &rgba[row * row_bytes..row * row_bytes + row_bytes];
            std::ptr::copy_nonoverlapping(src.as_ptr(), dst.add(row * stride), row_bytes);
        }
    }
    Some(rep)
}

// ---------------------------------------------------------------------------
// Offscreen render and preview
// ---------------------------------------------------------------------------

/// Draw the panel into a bitmap and hand back its pixels, never putting a
/// window on screen. The same `drawRect:` as the live panel, into a bitmap
/// context. Returns `(width, height, straight-alpha RGBA)`.
pub fn render_to_rgba(
    (model, look, peaks, devices): (Snapshot, Look, Peaks, bool),
    scale: u32,
) -> Option<(u32, u32, Vec<u8>)> {
    let scale = scale.max(1);
    let mtm = MainThreadMarker::new()?;
    let _ = objc2_app_kit::NSApplication::sharedApplication(mtm);

    UI.with(|u| {
        let mut u = u.borrow_mut();
        u.open(model, look);
        if devices {
            u.show_devices();
        }
        u.set_peaks(&peaks);
    });
    let hp = UI.with(|u| u.borrow().height()).ceil() as u32;
    let wp = WIDTH as u32;
    let (pw, ph) = (wp * scale, hp * scale);
    let bounds = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(wp as f64, hp as f64));
    let view = PanelView::new(mtm, bounds);

    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bitmapFormat_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            pw as isize,
            ph as isize,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            // Premultiplied: a bitmap context cannot be backed by anything
            // else, so it is un-premultiplied on the way out below.
            NSBitmapFormat::empty(),
            (pw * 4) as isize,
            32,
        )
    }?;
    rep.setSize(NSSize::new(wp as f64, hp as f64));
    let ctx = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep)?;
    RENDER_SCALE.with(|r| r.set(Some(scale as f64)));
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&ctx));
    view.displayRectIgnoringOpacity_inContext(bounds, &ctx);
    NSGraphicsContext::restoreGraphicsState_class();
    RENDER_SCALE.with(|r| r.set(None));

    let out = unsafe {
        let data = rep.bitmapData();
        if data.is_null() {
            return None;
        }
        let stride = rep.bytesPerRow() as usize;
        let row_bytes = (pw * 4) as usize;
        let mut out = vec![0u8; (pw * ph * 4) as usize];
        for row in 0..ph as usize {
            let src = std::slice::from_raw_parts(data.add(row * stride), row_bytes);
            let dst = &mut out[row * row_bytes..row * row_bytes + row_bytes];
            for px in 0..pw as usize {
                let i = px * 4;
                let a = src[i + 3] as u32;
                if a == 0 {
                    continue;
                }
                for c in 0..3 {
                    dst[i + c] = ((src[i + c] as u32 * 255 + a / 2) / a).min(255) as u8;
                }
                dst[i + 3] = a as u8;
            }
        }
        out
    };
    Some((pw, ph, out))
}

/// Show the panel with sample content and pump the run loop until the
/// timeout, so the rendering can be looked at, and moved, clicked, dragged.
pub fn preview(dark: bool, seconds: u64) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Accessory);

    let (model, look, peaks, _) = sample(dark, false);
    let Ok(panel) = Panel::new(Box::new(|a| eprintln!("{a:?}"))) else {
        return;
    };
    MAC.with(|m| m.borrow_mut().pinned = true);
    panel.show(model, look, None);
    panel.set_peaks(&peaks);

    let deadline = Instant::now() + Duration::from_secs(seconds);
    let run_loop = NSRunLoop::currentRunLoop();
    while Instant::now() < deadline {
        let until = NSDate::dateWithTimeIntervalSinceNow(0.05);
        unsafe {
            let _ = run_loop.runMode_beforeDate(NSDefaultRunLoopMode, &until);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vis(x: f64, y: f64, w: f64, h: f64) -> NSRect {
        NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
    }

    #[test]
    fn the_panel_hangs_from_the_top_of_the_visible_frame() {
        let visible = vis(0.0, 0.0, 1440.0, 875.0);
        let origin = place(NSPoint::new(1200.0, 880.0), 360.0, 400.0, visible);
        let top = origin.y + 400.0;
        assert!((860.0..=875.0).contains(&top), "top at {top}");
    }

    #[test]
    fn the_panel_stays_inside_the_visible_frame() {
        let visible = vis(0.0, 0.0, 1440.0, 875.0);
        let origin = place(NSPoint::new(1439.0, 880.0), 360.0, 400.0, visible);
        assert!(origin.x >= 0.0 && origin.x + 360.0 <= 1440.0);
    }
}
