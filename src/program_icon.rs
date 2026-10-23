//! An app's own icon, for its row in the panel (Windows).
//!
//! Asked for at the exact pixel size it will be drawn at, so a 32 px row at
//! 150% gets the 48 px image the app ships rather than its 32 px one scaled up.
//! macOS needs nothing like this: its painter asks NSWorkspace for the bundle's
//! icon, which is resolution-independent already.

use crate::util::{pcwstr, wide};
use windows::Win32::Foundation::S_OK;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
};
use windows::Win32::UI::Shell::SHDefExtractIconW;
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

/// The executable's icon as straight-alpha RGBA, `edge` px square. `None` when
/// it has no icon of its own: the panel draws a lettered tile then, which
/// reads better than Windows' generic program icon.
pub fn load(exe: &str, edge: u32) -> Option<Vec<u8>> {
    if exe.is_empty() || edge == 0 {
        return None;
    }
    let path = wide(exe);
    let mut icon = HICON::default();
    let rc = unsafe { SHDefExtractIconW(pcwstr(&path), 0, 0, Some(&mut icon), None, edge) };
    if rc != S_OK || icon.is_invalid() {
        return None;
    }
    let pixels = unsafe { rgba(icon) };
    unsafe {
        let _ = DestroyIcon(icon);
    }
    pixels.filter(|(_, e)| *e == edge).map(|(px, _)| px)
}

unsafe fn rgba(icon: HICON) -> Option<(Vec<u8>, u32)> {
    let mut ii = ICONINFO::default();
    GetIconInfo(icon, &mut ii).ok()?;
    // Both bitmaps are ours to free once GetIconInfo succeeds.
    let free = |ii: &ICONINFO| {
        let _ = DeleteObject(HGDIOBJ(ii.hbmColor.0));
        let _ = DeleteObject(HGDIOBJ(ii.hbmMask.0));
    };
    if ii.hbmColor.is_invalid() {
        free(&ii);
        return None;
    }
    let mut bm = BITMAP::default();
    if GetObjectW(
        HGDIOBJ(ii.hbmColor.0),
        std::mem::size_of::<BITMAP>() as i32,
        Some((&mut bm as *mut BITMAP).cast()),
    ) == 0
        || bm.bmWidth <= 0
        || bm.bmWidth != bm.bmHeight
    {
        free(&ii);
        return None;
    }
    let edge = bm.bmWidth as u32;
    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: bm.bmWidth,
            // Negative: top-down rows.
            biHeight: -bm.bmHeight,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut px = vec![0u8; (edge * edge * 4) as usize];
    let dc = CreateCompatibleDC(None);
    let lines = GetDIBits(
        dc,
        ii.hbmColor,
        0,
        edge,
        Some(px.as_mut_ptr().cast()),
        &mut bmi,
        DIB_RGB_COLORS,
    );
    let _ = DeleteDC(dc);
    free(&ii);
    if lines == 0 {
        return None;
    }
    // BGRA to RGBA. Icons from before alpha channels have none at all; for
    // those, any coloured pixel is opaque.
    let has_alpha = px.chunks(4).any(|p| p[3] != 0);
    for p in px.chunks_mut(4) {
        p.swap(0, 2);
        if !has_alpha {
            p[3] = if p[0] | p[1] | p[2] != 0 { 255 } else { 0 };
        }
    }
    Some((px, edge))
}

#[cfg(test)]
mod tests {
    #[test]
    fn explorer_has_an_icon_at_any_size() {
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
        let exe = format!(r"{windir}\explorer.exe");
        for edge in [24, 32, 48] {
            let px = super::load(&exe, edge).expect("an icon");
            assert_eq!(px.len(), (edge * edge * 4) as usize);
            assert!(px.chunks(4).any(|p| p[3] == 255), "drawn, not blank");
        }
    }

    #[test]
    fn nothing_is_not_an_icon() {
        assert!(super::load("", 32).is_none());
        assert!(super::load(r"C:\definitely\not\here.exe", 32).is_none());
    }
}
