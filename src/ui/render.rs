//! Layered-window renderer: panel text → 32bpp ARGB DIB → `UpdateLayeredWindow`.
//!
//! GDI text output never writes the alpha byte, so the trick here is: render
//! white text onto a zeroed (fully transparent) DIB, then post-process once —
//! `alpha = luminance`, `rgb = accent × alpha` (premultiplied, as
//! `UpdateLayeredWindow` requires). That yields per-pixel-alpha antialiased
//! text with a single pass over ~90 KB. Redraws happen only on state changes
//! and at 1 Hz while playing; never while paused.
//!
//! The window is moved/sized by the caller and then purely painted through
//! `UpdateLayeredWindow` — there is no WM_PAINT path at all. The window is
//! also draggable from anywhere (`HTCAPTION` in the wndproc) except the
//! bottom playback control bar (`HTCLIENT` there), without ever being
//! activated.

use std::ffi::c_void;
use std::ptr::null_mut;

use windows::core::{w, Error, HRESULT, Result};
use windows::Win32::Foundation::{GetLastError, COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, GetDC,
    GetTextExtentPoint32W, GetTextMetricsW, ReleaseDC, SelectObject, SetBkMode, SetTextColor,
    TextOutW, AC_SRC_ALPHA, AC_SRC_OVER, ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER,
    BLENDFUNCTION, BI_RGB, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DIB_RGB_COLORS, FF_MODERN,
    FIXED_PITCH, HBITMAP, HDC, HFONT, OUT_TT_PRECIS, TEXTMETRICW, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, UpdateLayeredWindow, ULW_ALPHA};

use crate::broker::Phase;
use crate::{log_debug, log_error, log_warn};

/// `Error::from_win32` was folded away in windows-rs 0.62 — rebuild it from
/// GetLastError so GDI failures carry the real reason.
fn last_error() -> Error {
    unsafe { Error::from_hresult(HRESULT::from_win32(GetLastError().0)) }
}

const PAD: i32 = 6;
/// The 200 px the spec asks for; panel height is derived from the font.
pub const WINDOW_W: i32 = 200;
/// Height of the playback control strip at the window's very bottom. The
/// wndproc hit-tests against this exact value (bottom strip = HTCLIENT →
/// button clicks; everywhere else = HTCAPTION → native drag), so draw and
/// hit geometry can never drift apart.
pub const CONTROL_BAR_H: i32 = 16;
/// Negative = per-em height: the glyph cell stays exact at any DPI.
const FONT_HEIGHT: i32 = -9;

/// Control-bar glyphs, in column order: prev track / -5s / +5s / next track.
/// The wndproc's `WM_LBUTTONDOWN` column mapping must match this order.
const W_PREV: &str = "<<";
const W_BACK: &str = "<";
const W_FWD: &str = ">";
const W_NEXT: &str = ">>";

fn accent(phase: Phase) -> [u8; 3] {
    match phase {
        Phase::Playing => [0xA8, 0xE0, 0xBE], // mint — active state, unchanged
        // Inactive states share the spec magenta (#E436C8).
        Phase::Paused => [0xE4, 0x36, 0xC8],
        Phase::Loading => [0x96, 0xC8, 0xEB], // sky
        Phase::Stopped => [0xE4, 0x36, 0xC8],
    }
}

pub struct Renderer {
    width: i32,
    height: i32,
    line_h: i32,
    screen_dc: HDC,
    mem_dc: HDC,
    bitmap: HBITMAP,
    bits: *mut u8,
    font: HFONT,
}

impl Renderer {
    /// `sample` is the current panel: its line count and the widest line
    /// determine the DIB (and therefore window) size.
    pub fn new(sample: &str) -> Result<Renderer> {
        unsafe {
            let screen_dc = GetDC(None);
            if screen_dc.is_invalid() {
                return Err(last_error());
            }
            let mem_dc = CreateCompatibleDC(Some(screen_dc));
            if mem_dc.is_invalid() {
                ReleaseDC(None, screen_dc);
                return Err(last_error());
            }

            // Monospace, 400 weight, grayscale antialiasing (ClearType's
            // subpixel fringes would smear in the alpha pass).
            let font = CreateFontW(
                FONT_HEIGHT,
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_TT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                ANTIALIASED_QUALITY,
                (FF_MODERN.0 | FIXED_PITCH.0) as u32,
                w!("Consolas"),
            );
            if font.is_invalid() {
                ReleaseDC(None, screen_dc);
                let _ = DeleteDC(mem_dc);
                return Err(last_error());
            }
            SelectObject(mem_dc, font.into());

            let mut tm = TEXTMETRICW::default();
            GetTextMetricsW(mem_dc, &mut tm).ok()?;
            let line_h = tm.tmHeight + tm.tmExternalLeading;

            let widest = sample
                .lines()
                .next()
                .unwrap_or_default()
                .encode_utf16()
                .collect::<Vec<u16>>();
            let mut extent = SIZE::default();
            GetTextExtentPoint32W(mem_dc, &widest, &mut extent).ok()?;
            if extent.cx > WINDOW_W - 2 * PAD {
                log_warn!(
                    "RENDER",
                    "panel line is {}px, window interior {}px — long rows will clip",
                    extent.cx,
                    WINDOW_W - 2 * PAD
                );
            }

            let rows = sample.lines().count() as i32;
            // Panel text plus the playback control strip at the bottom.
            let height = 2 * PAD + rows * line_h + CONTROL_BAR_H;

            let mut bi = BITMAPINFO::default();
            bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = WINDOW_W;
            bi.bmiHeader.biHeight = -height; // top-down
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            bi.bmiHeader.biCompression = BI_RGB.0; // 0x00RRGGBB-laid-out 32bpp
            let mut bits: *mut c_void = null_mut();
            let bitmap = CreateDIBSection(Some(mem_dc), &bi, DIB_RGB_COLORS, &mut bits, None, 0)?;
            SelectObject(mem_dc, bitmap.into());
            SetBkMode(mem_dc, TRANSPARENT);

            Ok(Renderer {
                width: WINDOW_W,
                height,
                line_h,
                screen_dc,
                mem_dc,
                bitmap,
                bits: bits as *mut u8,
                font,
            })
        }
    }

    /// Window size to create — the DIB and the window must match exactly.
    pub fn size(&self) -> (i32, i32) {
        (self.width, self.height)
    }

    /// Paint `panel` with `phase`'s accent color. Safe to call any number of
    /// times; each call is a full repaint of the layered surface.
    pub fn draw(&mut self, hwnd: HWND, panel: &str, phase: Phase) {
        unsafe {
            let len = (self.width * self.height) as usize * 4;
            std::ptr::write_bytes(self.bits, 0, len);

            SetTextColor(self.mem_dc, COLORREF(0x00FF_FFFF));
            for (i, line) in panel.lines().enumerate() {
                let wide: Vec<u16> = line.encode_utf16().collect();
                let _ = TextOutW(self.mem_dc, PAD, PAD + i as i32 * self.line_h, &wide);
            }

            // Playback control strip: one glyph centered in each of the four
            // equal columns (the wndproc maps clicks back through the same
            // geometry). Drawn white — the alpha pass below tints it with the
            // phase accent like every other lit pixel.
            let bar_top = self.height - CONTROL_BAR_H;
            let col_w = self.width / 4;
            let text_y = bar_top + ((CONTROL_BAR_H - self.line_h) / 2).max(0);
            for (i, label) in [W_PREV, W_BACK, W_FWD, W_NEXT].iter().enumerate() {
                let wide: Vec<u16> = label.encode_utf16().collect();
                let mut extent = SIZE::default();
                let _ = GetTextExtentPoint32W(self.mem_dc, &wide, &mut extent);
                let _ = TextOutW(
                    self.mem_dc,
                    i as i32 * col_w + (col_w - extent.cx) / 2,
                    text_y,
                    &wide,
                );
            }

            // luminance → alpha; premultiplied accent color. Pixels where GDI
            // never wrote keep a 1/255 ground plane: layered windows pass
            // mouse input (and OLE drops) straight through pixels with
            // alpha 0, so a truly empty surface would only accept input on
            // the glyph pixels themselves. Alpha 1 (~0.4% of premultiplied
            // black) is imperceptible but keeps the full rect hit-testable.
            let color = accent(phase);
            for px in std::slice::from_raw_parts_mut(self.bits, len).chunks_exact_mut(4) {
                let lum = px[0].max(px[1]).max(px[2]);
                if lum == 0 {
                    px[3] = 1;
                    continue;
                }
                px[0] = (u32::from(color[2]) * u32::from(lum) / 255) as u8; // B
                px[1] = (u32::from(color[1]) * u32::from(lum) / 255) as u8; // G
                px[2] = (u32::from(color[0]) * u32::from(lum) / 255) as u8; // R
                px[3] = lum; // A
            }

            // Bar separator: a dim accent line across the bar top so the
            // strip reads as controls, not stray glyphs. Premultiplied at
            // ~35% into the surface, after the luminance pass.
            let sep = (bar_top * self.width) as usize * 4;
            for px in std::slice::from_raw_parts_mut(self.bits, len)[sep..sep + self.width as usize * 4]
                .chunks_exact_mut(4)
            {
                px[0] = color[2] / 3; // B (premultiplied ~1/3 alpha)
                px[1] = color[1] / 3; // G
                px[2] = color[0] / 3; // R
                px[3] = 85;
            }

            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            if crate::logging::enabled(crate::logging::LVL_DEBUG) {
                let lit = std::slice::from_raw_parts(self.bits, len)
                    .chunks_exact(4)
                    .filter(|p| p[3] > 0)
                    .count();
                log_debug!("RENDER", "draw: {lit}/{} px lit", len / 4);
            }
            // Explicit position+size: some Windows builds silently ignore a
            // NULL/NULL (keep-current) ULW, leaving the surface transparent.
            let mut rc = RECT::default();
            let _ = GetWindowRect(hwnd, &mut rc);
            let pt_dst = POINT { x: rc.left, y: rc.top };
            let size = SIZE { cx: self.width, cy: self.height };
            if let Err(e) = UpdateLayeredWindow(
                hwnd,
                Some(self.screen_dc),
                Some(&pt_dst),
                Some(&size),
                Some(self.mem_dc),
                Some(&POINT { x: 0, y: 0 }),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            ) {
                log_error!("RENDER", "UpdateLayeredWindow failed: {e}");
            }
        }
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteDC(self.mem_dc); // also unselects bitmap+font
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteObject(self.font.into());
            ReleaseDC(None, self.screen_dc);
        }
    }
}
