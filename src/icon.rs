//! Everything Sonus draws as a picture: the tray icon, the app icon, and the
//! small glyphs in the panel and the menu.
//!
//! | what        | picture                                              |
//! |-------------|------------------------------------------------------|
//! | tray        | three mixer faders, at three different levels        |
//! | app icon    | the same faders, on a violet tile                    |
//! | glyphs      | speaker (with waves, or crossed out), chevron, check |
//!
//! Drawn from signed distance functions, like the other apps in this family:
//! crisp at any size and any DPI, no image decoder, no art files to keep in
//! step. The faders are not a speaker on purpose: the OS already has a speaker
//! in the tray, right next to this one.

/// What the tray asks for. Windows downsamples to the notification-area size
/// itself, and starting from 32 keeps 150% and 200% DPI sharp.
pub const EDGE: u32 = 32;

const SS: u32 = 4;

type Rgb = [f32; 3];

// ---------------------------------------------------------------------------
// Distance functions
// ---------------------------------------------------------------------------

fn len(x: f32, y: f32) -> f32 {
    (x * x + y * y).sqrt()
}

/// Distance to the segment a–b. Stroked with `- width / 2`, which gives round
/// caps for free.
fn seg(x: f32, y: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (px, py) = (x - a.0, y - a.1);
    let (bx, by) = (b.0 - a.0, b.1 - a.1);
    let h = ((px * bx + py * by) / (bx * bx + by * by)).clamp(0.0, 1.0);
    len(px - bx * h, py - by * h)
}

fn rounded_box(x: f32, y: f32, cx: f32, cy: f32, hx: f32, hy: f32, r: f32) -> f32 {
    let qx = (x - cx).abs() - (hx - r);
    let qy = (y - cy).abs() - (hy - r);
    len(qx.max(0.0), qy.max(0.0)) + qx.max(qy).min(0.0) - r
}

/// Signed distance to a closed polygon (negative inside).
fn polygon(x: f32, y: f32, pts: &[(f32, f32)]) -> f32 {
    let mut d = f32::MAX;
    let mut inside = false;
    let n = pts.len();
    for i in 0..n {
        let a = pts[i];
        let b = pts[(i + n - 1) % n];
        d = d.min(seg(x, y, a, b));
        if (a.1 > y) != (b.1 > y) && x < (b.0 - a.0) * (y - a.1) / (b.1 - a.1) + a.0 {
            inside = !inside;
        }
    }
    if inside {
        -d
    } else {
        d
    }
}

/// Distance to an arc of radius `r` about `c`, opening towards +x and
/// spanning `half` radians either side of it. Round ends.
fn arc(x: f32, y: f32, c: (f32, f32), r: f32, half: f32) -> f32 {
    let qx = x - c.0;
    let qy = (y - c.1).abs();
    let (s, co) = half.sin_cos();
    if qx * s > qy * co {
        (len(qx, qy) - r).abs()
    } else {
        len(qx - r * co, qy - r * s)
    }
}

// ---------------------------------------------------------------------------
// The faders (tray and app icon)
// ---------------------------------------------------------------------------

/// Fader positions on a 32 grid: the three channel strips and where each cap
/// sits. Different heights on purpose, so it reads as a mixer rather than as
/// three bars of a level meter.
const STRIPS: [(f32, f32); 3] = [(8.0, 20.0), (16.0, 10.5), (24.0, 16.5)];
const STRIP_TOP: f32 = 4.0;
const STRIP_BOTTOM: f32 = 28.0;
const CAP_HX: f32 = 3.9;
const CAP_HY: f32 = 2.4;

/// (line, cap) distances for the faders at `x, y` on the 32 grid. The line
/// stops short of each cap so the cap reads as a separate part even in one
/// colour.
fn faders(x: f32, y: f32, stroke: f32, gap: f32) -> (f32, f32) {
    let mut line = f32::MAX;
    let mut cap = f32::MAX;
    for (sx, sy) in STRIPS {
        let c = rounded_box(x, y, sx, sy, CAP_HX, CAP_HY, CAP_HY);
        cap = cap.min(c);
        let l = seg(x, y, (sx, STRIP_TOP), (sx, STRIP_BOTTOM)) - stroke / 2.0;
        // Carve the gap around this strip's own cap.
        line = line.min(l.max(-(c - gap)));
    }
    (line, cap)
}

/// RGBA8, `edge` x `edge`, straight alpha. On macOS it is a template image,
/// which AppKit tints for the menu bar, so the colour is irrelevant there.
pub fn render(dark_taskbar: bool, edge: u32) -> Vec<u8> {
    let ink: Rgb = if cfg!(target_os = "macos") {
        [0.0; 3]
    } else if dark_taskbar {
        [0.95, 0.95, 0.95]
    } else {
        [0.12, 0.12, 0.12]
    };
    supersample(edge, |x, y| {
        let (line, cap) = faders(x, y, 2.4, 1.6);
        (line <= 0.0 || cap <= 0.0).then_some(ink)
    })
}

/// The application icon: cream faders with white caps on a violet tile.
pub fn render_app(edge: u32) -> Vec<u8> {
    const TOP: Rgb = [0.62, 0.45, 1.0];
    const BOTTOM: Rgb = [0.30, 0.17, 0.78];
    const SHADOW: Rgb = [0.16, 0.07, 0.45];
    const GLYPH: f32 = 0.66;

    let tile = |y: f32| {
        let t = (y / 32.0).clamp(0.0, 1.0);
        [0, 1, 2].map(|i| TOP[i] + (BOTTOM[i] - TOP[i]) * t)
    };
    let mix = |a: Rgb, b: Rgb, t: f32| [0, 1, 2].map(|i| a[i] + (b[i] - a[i]) * t);
    supersample(edge, |x, y| {
        if rounded_box(x, y, 16.0, 16.0, 15.0, 15.0, 7.0) > 0.0 {
            return None;
        }
        let gx = 16.0 + (x - 16.0) / GLYPH;
        let gy = 16.0 + (y - 16.0) / GLYPH;
        let (line, cap) = faders(gx, gy, 2.0, 1.8);
        let base = tile(y);
        Some(if cap <= 0.0 {
            [1.0, 1.0, 1.0]
        } else if faders(gx, gy - 0.9, 2.0, 1.8).1 <= 0.0 {
            // A soft drop under each cap, so they sit on the tile.
            mix(base, SHADOW, 0.35)
        } else if line <= 0.0 {
            mix(base, [1.0, 0.97, 0.92], 0.55)
        } else {
            base
        })
    })
}

fn supersample(edge: u32, f: impl Fn(f32, f32) -> Option<Rgb>) -> Vec<u8> {
    let scale = 32.0 / edge as f32;
    let n = (SS * SS) as f32;
    let mut out = Vec::with_capacity((edge * edge * 4) as usize);
    for py in 0..edge {
        for px in 0..edge {
            let mut acc = [0.0f32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) * scale;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) * scale;
                    if let Some(c) = f(x, y) {
                        acc[0] += c[0];
                        acc[1] += c[1];
                        acc[2] += c[2];
                        acc[3] += 1.0;
                    }
                }
            }
            if acc[3] == 0.0 {
                out.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                out.extend_from_slice(&[
                    (acc[0] / acc[3] * 255.0).round() as u8,
                    (acc[1] / acc[3] * 255.0).round() as u8,
                    (acc[2] / acc[3] * 255.0).round() as u8,
                    (acc[3] / n * 255.0).round() as u8,
                ]);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Glyphs
// ---------------------------------------------------------------------------

/// The panel's and the menu's small pictures, drawn on Lucide's 24 grid with
/// its 2-unit stroke, so they sit happily beside the Lucide glyphs in the menu.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Glyph {
    /// A speaker with 0, 1 or 2 waves.
    Speaker(u8),
    SpeakerMuted,
    ChevronDown,
    ChevronUp,
    Check,
    Faders,
}

const SPEAKER: [(f32, f32); 6] = [
    (3.0, 9.0),
    (7.0, 9.0),
    (11.5, 5.0),
    (11.5, 19.0),
    (7.0, 15.0),
    (3.0, 15.0),
];

/// Distance to the glyph's ink at `x, y` on the 24 grid.
fn glyph_distance(g: Glyph, x: f32, y: f32) -> f32 {
    const W: f32 = 1.0; // half of Lucide's stroke
    let speaker = || polygon(x, y, &SPEAKER).abs() - W;
    match g {
        Glyph::Speaker(waves) => {
            let mut d = speaker();
            if waves >= 1 {
                d = d.min(arc(x, y, (12.0, 12.0), 4.0, 0.80) - W);
            }
            if waves >= 2 {
                d = d.min(arc(x, y, (12.0, 12.0), 8.0, 0.85) - W);
            }
            d
        }
        Glyph::SpeakerMuted => speaker()
            .min(seg(x, y, (15.5, 9.0), (21.5, 15.0)) - W)
            .min(seg(x, y, (21.5, 9.0), (15.5, 15.0)) - W),
        Glyph::ChevronDown => {
            seg(x, y, (7.0, 9.5), (12.0, 14.5)).min(seg(x, y, (12.0, 14.5), (17.0, 9.5))) - W
        }
        Glyph::ChevronUp => {
            seg(x, y, (7.0, 14.5), (12.0, 9.5)).min(seg(x, y, (12.0, 9.5), (17.0, 14.5))) - W
        }
        Glyph::Check => {
            seg(x, y, (5.0, 12.5), (9.5, 17.0)).min(seg(x, y, (9.5, 17.0), (19.0, 7.0))) - W
        }
        Glyph::Faders => {
            // The tray's faders, brought onto the 24 grid.
            let (line, cap) = faders(x * 32.0 / 24.0, y * 32.0 / 24.0, 2.6, 1.6);
            line.min(cap) * 24.0 / 32.0
        }
    }
}

/// A glyph as white-on-clear RGBA, `edge` px square, with `pad` px of the edge
/// left empty on every side. The caller tints it: the alpha is the shape.
///
/// Anti-aliased analytically, from the distance itself: at 14 px a glyph has
/// strokes barely more than a pixel wide, and coverage from the distance is
/// smoother there than any fixed supersampling grid.
pub fn glyph(g: Glyph, edge: u32, pad: u32) -> Vec<u8> {
    let inner = edge.saturating_sub(pad * 2).max(1) as f32;
    let per_px = 24.0 / inner;
    let mut out = Vec::with_capacity((edge * edge * 4) as usize);
    for py in 0..edge {
        for px in 0..edge {
            let x = (px as f32 + 0.5 - pad as f32) * per_px;
            let y = (py as f32 + 0.5 - pad as f32) * per_px;
            let d = glyph_distance(g, x, y) / per_px;
            let a = (0.5 - d).clamp(0.0, 1.0);
            out.extend_from_slice(&[255, 255, 255, (a * 255.0).round() as u8]);
        }
    }
    out
}

/// The speaker that says how loud something is: crossed out when muted or
/// silent, then one wave or two.
pub fn speaker_for(volume: f32, muted: bool) -> Glyph {
    if muted || volume <= 0.0 {
        Glyph::SpeakerMuted
    } else if volume < 0.34 {
        Glyph::Speaker(0)
    } else if volume < 0.67 {
        Glyph::Speaker(1)
    } else {
        Glyph::Speaker(2)
    }
}

/// Sizes the app icon is written at: everything an .ico, an .icns iconset
/// and the docs want, each drawn rather than resampled.
pub const APP_SIZES: [u32; 10] = [16, 20, 24, 32, 40, 48, 64, 128, 256, 1024];

/// `sonus --dump-icons <dir>`: the tray icon and every app icon size as raw
/// RGBA. `assets/make_icons.py` turns them into icon.ico and the PNGs.
pub fn dump(dir: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    for (theme, dark) in [("dark", true), ("light", false)] {
        for edge in [EDGE, 128] {
            std::fs::write(
                format!("{dir}/tray-faders-{theme}-{edge}.rgba"),
                render(dark, edge),
            )?;
        }
    }
    for edge in APP_SIZES {
        std::fs::write(format!("{dir}/app-{edge}.rgba"), render_app(edge))?;
    }
    for (name, g) in [
        ("speaker-0", Glyph::Speaker(0)),
        ("speaker-1", Glyph::Speaker(1)),
        ("speaker-2", Glyph::Speaker(2)),
        ("speaker-muted", Glyph::SpeakerMuted),
        ("chevron-down", Glyph::ChevronDown),
        ("check", Glyph::Check),
        ("faders", Glyph::Faders),
    ] {
        std::fs::write(format!("{dir}/glyph-{name}-96.rgba"), glyph(g, 96, 0))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ink(px: &[u8]) -> u32 {
        px.chunks(4).map(|p| p[3] as u32).sum()
    }

    #[test]
    fn the_tray_icon_has_ink_and_clear_corners() {
        let px = render(true, EDGE);
        assert!(ink(&px) > 0);
        assert_eq!(px[3], 0);
        assert_eq!(px[px.len() - 1], 0);
    }

    #[test]
    fn the_caps_are_drawn_apart_from_their_lines() {
        // Just above the middle strip's cap, inside the carved gap: empty.
        let (line, cap) = faders(16.0, 10.5 - CAP_HY - 0.8, 2.4, 1.6);
        assert!(line > 0.0 && cap > 0.0);
        // Well above it, on the line.
        let (line, _) = faders(16.0, 6.0, 2.4, 1.6);
        assert!(line <= 0.0);
    }

    #[test]
    fn the_app_icon_has_rounded_transparent_corners_and_an_opaque_centre() {
        let edge = 64;
        let px = render_app(edge);
        assert_eq!(px[3], 0);
        let centre = ((edge / 2 * edge + edge / 2) * 4 + 3) as usize;
        assert_eq!(px[centre], 255);
    }

    #[test]
    fn every_glyph_has_ink_and_more_waves_mean_more_ink() {
        for g in [
            Glyph::Speaker(0),
            Glyph::SpeakerMuted,
            Glyph::ChevronDown,
            Glyph::ChevronUp,
            Glyph::Check,
            Glyph::Faders,
        ] {
            assert!(ink(&glyph(g, 32, 2)) > 0, "{g:?}");
        }
        let w0 = ink(&glyph(Glyph::Speaker(0), 48, 0));
        let w1 = ink(&glyph(Glyph::Speaker(1), 48, 0));
        let w2 = ink(&glyph(Glyph::Speaker(2), 48, 0));
        assert!(w0 < w1 && w1 < w2);
    }

    #[test]
    fn the_speaker_says_how_loud() {
        assert_eq!(speaker_for(0.5, true), Glyph::SpeakerMuted);
        assert_eq!(speaker_for(0.0, false), Glyph::SpeakerMuted);
        assert_eq!(speaker_for(0.2, false), Glyph::Speaker(0));
        assert_eq!(speaker_for(0.5, false), Glyph::Speaker(1));
        assert_eq!(speaker_for(1.0, false), Glyph::Speaker(2));
    }

    #[test]
    fn polygons_know_inside_from_outside() {
        assert!(polygon(5.0, 12.0, &SPEAKER) < 0.0);
        assert!(polygon(20.0, 12.0, &SPEAKER) > 0.0);
    }
}
