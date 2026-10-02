//! Tray icon — fully procedural. The glyph (mountain + echo arcs) is computed
//! in `super::glyph`, supersampled 4× into a 32bpp alpha DIB, and turned into
//! an HICON via `CreateIconIndirect`. No `.ico` asset, no resource file, no
//! `build.rs`: the binary is the only artifact.
//!
//! A top-down DIB (negative `biHeight`) is used deliberately: it removes the
//! bottom-up scanline ambiguity that raw `CreateIcon` AND/XOR masks carry.

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, DestroyIcon, ICONINFO};

use super::glyph::glyph_mask_at;
use super::WM_APP_TRAY;
use crate::{log_info, log_warn};

const TRAY_ID: u32 = 1;
const TIP_LEN: usize = 128;
/// Echo-green, matching the PLAYING accent in render.rs.
const GLYPH_RGB: [u8; 3] = [0xA8, 0xE0, 0xBE];

pub struct Tray {
    nid: NOTIFYICONDATAW,
    hicon: windows::Win32::UI::WindowsAndMessaging::HICON,
    added: bool,
}

impl Tray {
    pub fn new() -> Self {
        let hicon = build_icon();
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            uID: TRAY_ID,
            uCallbackMessage: WM_APP_TRAY,
            hIcon: hicon,
            ..Default::default()
        };
        fill_tip(&mut nid.szTip, "kyouko — the mountain echo");
        Tray { nid, hicon, added: false }
    }

    /// (Re-)registers the icon. Called once at startup and again on
    /// `TaskbarCreated` (explorer restart) — NIM_ADD is idempotent for this.
    pub fn add(&mut self, hwnd: HWND) -> bool {
        self.nid.hWnd = hwnd;
        self.nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        let ok = unsafe { Shell_NotifyIconW(NIM_ADD, &self.nid) }.as_bool();
        if ok {
            log_info!("TRAY", "icon registered (procedural mountain+echo, no asset)");
            self.added = true;
        } else {
            log_warn!("TRAY", "NIM_ADD failed — no tray icon this session");
        }
        ok
    }

    pub fn set_tooltip(&mut self, text: &str) {
        if !self.added {
            return;
        }
        fill_tip(&mut self.nid.szTip, text);
        self.nid.uFlags = NIF_TIP;
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &self.nid);
        }
    }

    pub fn remove(&mut self) {
        if self.added {
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &self.nid);
            }
            self.added = false;
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.remove();
        unsafe {
            let _ = DestroyIcon(self.hicon);
        }
    }
}

fn fill_tip(slot: &mut [u16; TIP_LEN], text: &str) {
    let mut v = text.encode_utf16().take(TIP_LEN - 1).collect::<Vec<u16>>();
    v.push(0);
    slot[..v.len()].copy_from_slice(&v);
}

/// 4× supersampled coverage → straight-alpha ARGB icon. ~20 lines, zero
/// dependencies, and the edges don't look chewed at 16 px.
fn build_icon() -> windows::Win32::UI::WindowsAndMessaging::HICON {
    use windows::Win32::UI::WindowsAndMessaging::HICON;

    const S: usize = 4; // supersample factor
    let hi_res = glyph_mask_at(16 * S);
    let mut pixels = vec![0u8; 16 * 16 * 4]; // BGRA, top-down
    for y in 0..16usize {
        for x in 0..16usize {
            let mut lit = 0u32;
            for sy in 0..S {
                for sx in 0..S {
                    if hi_res[y * S + sy][x * S + sx] {
                        lit += 1;
                    }
                }
            }
            let a = (lit * 255 / (S * S) as u32) as u8;
            let i = (y * 16 + x) * 4;
            pixels[i] = ((u16::from(GLYPH_RGB[2]) * u16::from(a)) / 255) as u8; // B
            pixels[i + 1] = ((u16::from(GLYPH_RGB[1]) * u16::from(a)) / 255) as u8; // G
            pixels[i + 2] = ((u16::from(GLYPH_RGB[0]) * u16::from(a)) / 255) as u8; // R
            pixels[i + 3] = a; // A
        }
    }

    unsafe {
        let hdc = GetDC(None);
        let mem_dc = CreateCompatibleDC(Some(hdc));

        let mut bi = BITMAPINFO::default();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = 16;
        bi.bmiHeader.biHeight = -16; // top-down
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB.0;
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let color = match CreateDIBSection(Some(mem_dc), &bi, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(b) => {
                std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits as *mut u8, pixels.len());
                b
            }
            Err(_) => {
                ReleaseDC(None, hdc);
                let _ = DeleteDC(mem_dc);
                log_warn!("TRAY", "icon DIB failed");
                return HICON::default();
            }
        };
        // All-zero AND mask = "defer to the alpha channel".
        let mask_bmp = CreateBitmap(16, 16, 1, 1, Some(&[0u8; 32] as *const [u8] as *const _));

        let info = ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask_bmp,
            hbmColor: color,
        };
        let hicon = CreateIconIndirect(&info).unwrap_or_default();
        if hicon.is_invalid() {
            log_warn!("TRAY", "CreateIconIndirect failed");
        }

        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask_bmp.into());
        let _ = DeleteDC(mem_dc);
        ReleaseDC(None, hdc);
        hicon
    }
}
