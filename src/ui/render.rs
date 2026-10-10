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
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, FillRect,
    GdiFlush, GetDC, GetStockObject,
    GetTextExtentPoint32W, GetTextMetricsW, ReleaseDC, SelectObject, SetBkMode, SetTextColor,
    TextOutW, AC_SRC_ALPHA, AC_SRC_OVER, ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER,
    BLENDFUNCTION, BI_RGB, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DIB_RGB_COLORS, FF_MODERN,
    FIXED_PITCH, HBITMAP, HBRUSH, HDC, HFONT, OUT_TT_PRECIS, TEXTMETRICW, TRANSPARENT,
    WHITE_BRUSH,
};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, UpdateLayeredWindow, ULW_ALPHA};

use crate::broker::{EQ_BANDS, EQ_MAX_GAIN_DB, Phase};
use crate::{log_debug, log_error, log_warn};

/// `Error::from_win32` was folded away in windows-rs 0.62 — rebuild it from
/// GetLastError so GDI failures carry the real reason.
fn last_error() -> Error {
    unsafe { Error::from_hresult(HRESULT::from_win32(GetLastError().0)) }
}

const PAD: i32 = 3;
/// The 80 px compact floating window; panel height is derived from the font.
/// At the 9 px Consolas em this leaves room for ~14 text cells per line, so
/// the panel formats in `render_panel` are tuned to that budget.
pub const WINDOW_W: i32 = 80;
/// Height of the playback control strip at the window's very bottom. The
/// wndproc hit-tests against this exact value (bottom strip = HTCLIENT →
/// button clicks; everywhere else = HTCAPTION → native drag), so draw and
/// hit geometry can never drift apart.
pub const CONTROL_BAR_H: i32 = 16;
/// Braille equalizer rows: two fill rows above the middle baseline and two
/// below (6 levels per row → the ±12 dB gain range exactly).
const EQ_ROWS: usize = 4;
/// Negative = per-em height: the glyph cell stays exact at any DPI.
const FONT_HEIGHT: i32 = -9;

/// Which clickable status chunk sits under a point?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusToggle {
    /// The `eq:ON/OFF` chunk (toggles `Command::ToggleEq`).
    Eq,
    /// The `rep:ON/OFF` chunk (toggles `Command::ToggleLoop`).
    Repeat,
}

/// Clickable columns (in Consolas advance units from the left pad) of the
/// dual status line's two chunks: `eq:ON ` spans columns 0..6 and
/// `rep:OFF` columns 7..14. `render_panel` in broker.rs formats the line
/// with exactly this column table — drawing and hit-testing share it, so
/// they can never drift apart. Both chunks sit at fixed columns regardless
/// of ON/OFF width (the eq state is padded to 3 cells).
const STATUS_EQ_COLS: (i32, i32) = (0, 6);
const STATUS_REPEAT_COLS: (i32, i32) = (7, 7);

/// Which status chunk (if any) does a CLIENT-coordinate point hit? The
/// clickable band is the last panel line's full label+state chunks; the
/// gap between them (and the rest of the panel) stays caption (drag).
/// Pure so hit geometry stays unit-testable without GDI.
pub fn status_toggle_at(
    x: i32,
    y: i32,
    eq_top: i32,
    line_h: i32,
    char_w: i32,
) -> Option<StatusToggle> {
    let y0 = eq_top - line_h; // last panel line: the eq/repeat status row
    if y < y0 || y >= eq_top {
        return None;
    }
    let col_x = |col: i32| PAD + col * char_w;
    let (eq_col, eq_len) = STATUS_EQ_COLS;
    if x >= col_x(eq_col) && x < col_x(eq_col + eq_len) {
        return Some(StatusToggle::Eq);
    }
    let (rp_col, rp_len) = STATUS_REPEAT_COLS;
    if x >= col_x(rp_col) && x < col_x(rp_col + rp_len) {
        return Some(StatusToggle::Repeat);
    }
    None
}

/// Control-bar glyphs, in column order: prev track / -5s / +5s / next track.
/// The wndproc's `WM_LBUTTONDOWN` column mapping must match this order.
const W_PREV: &str = "<<";
const W_BACK: &str = "<";
const W_FWD: &str = ">";
const W_NEXT: &str = ">>";

/// Braille glyph holding `levels` fill steps in one cell: blank at 0, the
/// 6-step dot progression for 1..=6, saturated full cell beyond. The
/// progression fills the cell bottom-up: ⠠ ⠤ ⠴ ⠶ ⠾ ⠿.
fn braille_glyph(levels: i32) -> &'static str {
    match levels {
        1 => "⠠",
        2 => "⠤",
        3 => "⠴",
        4 => "⠶",
        5 => "⠾",
        6.. => "⠿",
        _ => " ",
    }
}

/// Braille glyph for row `row` (0 = adjacent to the middle baseline, growing
/// outward) of a band at `level` (-12..+12 dB) on the row's own side of the
/// baseline. Positive cells fill bottom-up; negative cells fill top-down,
/// so individual dots also grow away from the baseline on both sides.
fn braille_row_glyph(level: i32, row: i32) -> &'static str {
    let levels = level.abs() - 6 * row;
    if level < 0 {
        match levels {
            1 => "⠈",
            2 => "⠉",
            3 => "⠙",
            4 => "⠛",
            5 => "⠻",
            6.. => "⠿",
            _ => " ",
        }
    } else {
        braille_glyph(levels)
    }
}

/// Glyph for physical strip row `strip_row` (0 = top of the strip,
/// `EQ_ROWS - 1` = bottom) of a band at `level`. Encodes the side
/// exclusivity: positive levels light only the rows above the baseline,
/// negative levels only the rows below, and level 0 leaves every row blank.
/// These are physical GDI rows, ordered top-to-bottom; they are not the
/// side-relative row indices accepted by `braille_row_glyph`.
fn eq_strip_glyph(level: i32, strip_row: usize) -> &'static str {
    match strip_row {
        0 if level >= 7 => braille_row_glyph(level, 1), // outer positive: +7..+12
        1 if level >= 1 => braille_row_glyph(level, 0), // inner positive: +1..+6
        2 if level <= -1 => braille_row_glyph(level, 0), // inner negative: -1..-6
        3 if level <= -7 => braille_row_glyph(level, 1), // outer negative: -7..-12
        _ => " ",
    }
}

/// GDI cell origin and glyph for one physical row. Truncate toward zero so
/// fractional gains cannot light the inner/outer rows before ±1/±7 dB.
/// The DIB is top-down and the DC uses pixel coordinates, so increasing the
/// physical row index always increases Y by exactly one text line.
fn eq_strip_row(gain_db: f32, strip_row: usize, eq_top: i32, line_h: i32) -> (i32, &'static str) {
    let level = gain_db.clamp(-EQ_MAX_GAIN_DB, EQ_MAX_GAIN_DB) as i32;
    (eq_top + strip_row as i32 * line_h, eq_strip_glyph(level, strip_row))
}

/// Rasterize the six Unicode Braille bits as separate dots, rather than
/// relying on font fallback at a 9px em (where adjacent dots merge). Bits
/// 0..2 are the left column, top-to-bottom; bits 3..5 are the right column.
/// A one-pixel gap separates 2px dots, with the grid centered in its cell.
fn braille_dot_rect(glyph: &str, dot: usize, cell: RECT) -> Option<RECT> {
    if dot >= 6 {
        return None;
    }
    let mask = (glyph.chars().next()? as u32).checked_sub(0x2800)?;
    if mask & (1 << dot) == 0 {
        return None;
    }
    let width = cell.right - cell.left;
    let height = cell.bottom - cell.top;
    let dot_size = 2.min((width - 1) / 2).min((height - 2) / 3);
    if dot_size < 1 {
        return None;
    }
    let left = cell.left + (width - (2 * dot_size + 1)) / 2
        + (dot / 3) as i32 * (dot_size + 1);
    let top = cell.top + (height - (3 * dot_size + 2)) / 2
        + (dot % 3) as i32 * (dot_size + 1);
    Some(RECT { left, top, right: left + dot_size, bottom: top + dot_size })
}

/// Which of the 10 equalizer columns does a client x fall in? Pure mapping
/// shared by drawing and hit-testing.
fn eq_band_index(x: i32, width: i32) -> usize {
    (x.clamp(0, width - 1) * EQ_BANDS as i32 / width) as usize
}

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
    /// Monospace advance width of one Consolas cell — the unit behind the
    /// status-line column table (`STATUS_*_COLS`).
    char_w: i32,
    /// Client y where the Braille equalizer strip begins (panel text ends);
    /// recomputed on every draw from the panel's line count.
    eq_top: i32,
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
            let char_w = tm.tmAveCharWidth.max(1);

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
            // Panel text, the Braille equalizer strip, and the playback
            // control strip at the bottom.
            let height = 2 * PAD + rows * line_h + EQ_ROWS as i32 * line_h + CONTROL_BAR_H;

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
                char_w,
                eq_top: PAD + rows * line_h,
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

    /// Which equalizer band (if any) does a CLIENT-coordinate point hover?
    /// The strip spans the Braille rows between the panel text and the
    /// control bar; the column split is exact tenths of the window width.
    pub fn eq_band_at(&self, x: i32, y: i32) -> Option<usize> {
        if y < self.eq_top || y >= self.eq_top + EQ_ROWS as i32 * self.line_h {
            return None;
        }
        Some(eq_band_index(x, self.width))
    }

    /// Which clickable status chunk (eq / repeat) does a CLIENT-coordinate
    /// point hit, if any? Delegates to the pure column-table helper.
    pub fn status_toggle_at(&self, x: i32, y: i32) -> Option<StatusToggle> {
        status_toggle_at(x, y, self.eq_top, self.line_h, self.char_w)
    }

    /// Draw all four physical rows explicitly. Rasterize each glyph's exact
    /// Braille mask inside its cell so all six levels stay visibly distinct.
    fn draw_eq_strip(&self, eq: &[f32; EQ_BANDS]) {
        unsafe {
            let col_w = self.width / EQ_BANDS as i32;
            let brush = HBRUSH(GetStockObject(WHITE_BRUSH).0);
            // Top-to-bottom: outer positive, inner positive, inner negative,
            // outer negative. The invisible baseline is between rows 1 and 2.
            for strip_row in 0..EQ_ROWS {
                for (band, gain_db) in eq.iter().enumerate() {
                    let (y, glyph) = eq_strip_row(*gain_db, strip_row, self.eq_top, self.line_h);
                    if glyph == " " {
                        continue;
                    }
                    let cell = RECT {
                        left: band as i32 * col_w,
                        top: y,
                        right: (band as i32 + 1) * col_w,
                        bottom: y + self.line_h,
                    };
                    for dot in 0..6 {
                        if let Some(rect) = braille_dot_rect(glyph, dot, cell) {
                            let _ = FillRect(self.mem_dc, &rect, brush);
                        }
                    }
                }
            }
        }
    }

    /// Paint `panel` with `phase`'s accent color and the 10-band gains as
    /// the Braille equalizer. Safe to call any number of times; each call is
    /// a full repaint of the layered surface.
    pub fn draw(&mut self, hwnd: HWND, panel: &str, phase: Phase, eq: &[f32; EQ_BANDS]) {
        unsafe {
            let len = (self.width * self.height) as usize * 4;
            std::ptr::write_bytes(self.bits, 0, len);

            SetTextColor(self.mem_dc, COLORREF(0x00FF_FFFF));
            for (i, line) in panel.lines().enumerate() {
                let wide: Vec<u16> = line.encode_utf16().collect();
                let _ = TextOutW(self.mem_dc, PAD, PAD + i as i32 * self.line_h, &wide);
            }

            // Braille equalizer: 10 columns across the full width, filling
            // middle-out from the invisible baseline between the two inner
            // rows. Side exclusivity, the glyph, and its physical Y come
            // from the same pure mapping exercised by the row tests.
            self.eq_top = PAD + panel.lines().count() as i32 * self.line_h;
            self.draw_eq_strip(eq);

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

            // Complete batched GDI writes before touching the DIB through its
            // raw pointer; otherwise text can race the alpha pass or clearing
            // the next frame, leaving stale/duplicated pixels.
            if !GdiFlush().as_bool() {
                log_error!("RENDER", "GDI drawing batch failed");
                return;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn braille_progression_matches_spec() {
        // 0 = blank, then the 6-step dot progression, saturated beyond 6.
        let want = [" ", "⠠", "⠤", "⠴", "⠶", "⠾", "⠿"];
        for (levels, glyph) in want.iter().enumerate() {
            assert_eq!(braille_glyph(levels as i32), *glyph, "levels={levels}");
        }
        assert_eq!(braille_glyph(7), "⠿", "beyond 6 saturates the cell");
        assert_eq!(braille_glyph(-3), " ", "negative levels never reach a cell directly");
    }

    #[test]
    fn eq_physical_rows_have_their_own_glyph_and_gdi_y() {
        // Physical GDI order: outer +, inner +, inner -, outer -.
        // Fixed coordinates represent five 11px panel lines plus 3px padding.
        for (gain_db, glyphs) in [
            (-12.0, [" ", " ", "⠿", "⠿"]),
            (-11.0, [" ", " ", "⠿", "⠻"]),
            (-10.0, [" ", " ", "⠿", "⠛"]),
            (-9.0, [" ", " ", "⠿", "⠙"]),
            (-8.0, [" ", " ", "⠿", "⠉"]),
            (-7.0, [" ", " ", "⠿", "⠈"]),
            (-6.0, [" ", " ", "⠿", " "]),
            (-5.0, [" ", " ", "⠻", " "]),
            (-4.0, [" ", " ", "⠛", " "]),
            (-3.0, [" ", " ", "⠙", " "]),
            (-2.0, [" ", " ", "⠉", " "]),
            (-1.0, [" ", " ", "⠈", " "]),
            (0.0, [" ", " ", " ", " "]),
            (1.0, [" ", "⠠", " ", " "]),
            (5.0, [" ", "⠾", " ", " "]),
            (6.0, [" ", "⠿", " ", " "]),
            (7.0, ["⠠", "⠿", " ", " "]),
            (12.0, ["⠿", "⠿", " ", " "]),
        ] {
            assert_eq!(eq_strip_row(gain_db, 0, 58, 11), (58, glyphs[0]), "gain {gain_db}, physical row 0");
            assert_eq!(eq_strip_row(gain_db, 1, 58, 11), (69, glyphs[1]), "gain {gain_db}, physical row 1");
            assert_eq!(eq_strip_row(gain_db, 2, 58, 11), (80, glyphs[2]), "gain {gain_db}, physical row 2");
            assert_eq!(eq_strip_row(gain_db, 3, 58, 11), (91, glyphs[3]), "gain {gain_db}, physical row 3");
        }
    }

    #[test]
    fn eq_fractional_gains_do_not_cross_row_thresholds_early() {
        for (gain_db, expected) in [
            (-6.99, [" ", " ", "⠿", " "]),
            (-0.99, [" ", " ", " ", " "]),
            (-0.0, [" ", " ", " ", " "]),
            (0.99, [" ", " ", " ", " "]),
            (6.99, [" ", "⠿", " ", " "]),
        ] {
            let glyphs = [0, 1, 2, 3].map(|row| eq_strip_row(gain_db, row, 58, 11).1);
            assert_eq!(glyphs, expected, "gain {gain_db}");
        }
    }

    #[test]
    fn gdi_eq_pixels_stay_in_their_physical_cells_after_gain_changes() {
        // Render into a real top-down GDI DIB without creating a window or
        // injecting input. Exercise the same strip drawing as the live UI.
        let renderer = Renderer::new("KYOUKO STOPPED\n00:00/--:--\n(none)\nvol ||||-  80%\neq:ON  rep:OFF").unwrap();
        let len = (renderer.width * renderer.height) as usize * 4;
        let band = 3;
        let col_w = renderer.width / EQ_BANDS as i32;
        for (gain_db, expected_lit_rows) in [
            (5.0, [false, true, false, false]),
            (-5.0, [false, false, true, false]),
            (12.0, [true, true, false, false]),
            (-12.0, [false, false, true, true]),
            (0.0, [false, false, false, false]),
        ] {
            let mut gains = [0.0; EQ_BANDS];
            gains[band] = gain_db;
            unsafe {
                std::ptr::write_bytes(renderer.bits, 0, len);
                SetTextColor(renderer.mem_dc, COLORREF(0x00FF_FFFF));
                renderer.draw_eq_strip(&gains);
                assert!(GdiFlush().as_bool());
                let pixels = std::slice::from_raw_parts(renderer.bits, len);
                let mut lit_rows = [false; EQ_ROWS];
                for (pixel_index, pixel) in pixels.chunks_exact(4).enumerate() {
                    if pixel[..3].iter().all(|channel| *channel == 0) {
                        continue;
                    }
                    let x = pixel_index as i32 % renderer.width;
                    let y = pixel_index as i32 / renderer.width;
                    assert!(x >= band as i32 * col_w && x < (band as i32 + 1) * col_w,
                        "gain {gain_db} spilled into another band at ({x}, {y})");
                    assert!(y >= renderer.eq_top && y < renderer.eq_top + EQ_ROWS as i32 * renderer.line_h,
                        "gain {gain_db} spilled outside the strip at ({x}, {y})");
                    let physical_row = ((y - renderer.eq_top) / renderer.line_h) as usize;
                    lit_rows[physical_row] = true;
                }
                assert_eq!(lit_rows, expected_lit_rows, "GDI pixels at gain {gain_db}");
            }
        }
    }

    #[test]
    fn gdi_braille_levels_have_separate_visible_dots() {
        let renderer = Renderer::new("one\ntwo\nthree\nfour\nfive").unwrap();
        let len = (renderer.width * renderer.height) as usize * 4;
        let col_w = renderer.width / EQ_BANDS as i32;
        let mut masks = Vec::new();
        for level in 1..=6 {
            let mut gains = [0.0; EQ_BANDS];
            gains[3] = level as f32;
            unsafe {
                std::ptr::write_bytes(renderer.bits, 0, len);
                SetTextColor(renderer.mem_dc, COLORREF(0x00FF_FFFF));
                renderer.draw_eq_strip(&gains);
                assert!(GdiFlush().as_bool());
                let pixels = std::slice::from_raw_parts(renderer.bits, len);
                let mut mask = Vec::new();
                for y in renderer.eq_top + renderer.line_h..renderer.eq_top + 2 * renderer.line_h {
                    for x in 3 * col_w..4 * col_w {
                        let offset = ((y * renderer.width + x) * 4) as usize;
                        let lit = pixels[offset..offset + 3].iter().any(|v| *v != 0);
                        mask.push(lit);
                    }
                }
                // A level must contain that many separate dot components;
                // distinguishable masks alone still accept merged strokes.
                let mut unseen = mask.clone();
                let mut dots = 0;
                for start in 0..unseen.len() {
                    if !unseen[start] { continue; }
                    dots += 1;
                    let mut pending = vec![start];
                    unseen[start] = false;
                    while let Some(pixel) = pending.pop() {
                        let x = pixel as i32 % col_w;
                        let y = pixel as i32 / col_w;
                        for (nx, ny) in [(x - 1, y), (x + 1, y), (x, y - 1), (x, y + 1)] {
                            if nx < 0 || nx >= col_w || ny < 0 || ny >= renderer.line_h { continue; }
                            let next = (ny * col_w + nx) as usize;
                            if unseen[next] {
                                unseen[next] = false;
                                pending.push(next);
                            }
                        }
                    }
                }
                assert_eq!(dots, level, "GDI merged the Braille dots at level {level}");
                masks.push(mask);
            }
        }
        for level in 1..6 {
            assert_ne!(masks[level - 1], masks[level], "GDI renders levels {level} and {} identically", level + 1);
        }
    }

    #[test]
    fn gdi_negative_dots_grow_downward_and_keep_the_inner_row_full() {
        let renderer = Renderer::new("one\ntwo\nthree\nfour\nfive").unwrap();
        let len = (renderer.width * renderer.height) as usize * 4;
        let col_w = renderer.width / EQ_BANDS as i32;
        assert_eq!(col_w, 8);
        assert!(renderer.line_h >= 8);
        // Independent pixel positions for the requested top-down progression:
        // top right, top left, middle right, middle left, bottom right, bottom left.
        let dot_order = [(3, 0), (0, 0), (3, 3), (0, 3), (3, 6), (0, 6)];
        for magnitude in 1usize..=12 {
            let mut gains = [0.0; EQ_BANDS];
            gains[3] = -(magnitude as f32);
            unsafe {
                std::ptr::write_bytes(renderer.bits, 0, len);
                renderer.draw_eq_strip(&gains);
                assert!(GdiFlush().as_bool());
                let pixels = std::slice::from_raw_parts(renderer.bits, len);
                let actual: Vec<_> = pixels.chunks_exact(4).enumerate()
                    .filter(|(_, pixel)| pixel[..3].iter().any(|channel| *channel != 0))
                    .map(|(index, _)| (index as i32 % renderer.width, index as i32 / renderer.width))
                    .collect();
                let mut expected = Vec::new();
                for (physical_row, steps) in [(2, magnitude.min(6)), (3, magnitude.saturating_sub(6))] {
                    let origin_x = 3 * col_w + (col_w - 5) / 2;
                    let origin_y = renderer.eq_top + physical_row * renderer.line_h
                        + (renderer.line_h - 8) / 2;
                    for &(dot_x, dot_y) in dot_order.iter().take(steps) {
                        for dy in 0..2 {
                            for dx in 0..2 {
                                expected.push((origin_x + dot_x + dx, origin_y + dot_y + dy));
                            }
                        }
                    }
                }
                expected.sort_by_key(|&(x, y)| y * renderer.width + x);
                assert_eq!(actual, expected, "physical dot pixels at -{magnitude} dB");
            }
        }
    }

    #[test]
    fn eq_strip_sides_are_mutually_exclusive() {
        // Positive gains light only the rows above the baseline.
        for strip_row in 2..EQ_ROWS {
            assert_eq!(eq_strip_glyph(5, strip_row), " ", "+5 lower row {strip_row}");
        }
        // Negative gains light only the rows below the baseline.
        for strip_row in 0..2 {
            assert_eq!(eq_strip_glyph(-5, strip_row), " ", "-5 upper row {strip_row}");
        }
        // Zero leaves the whole strip blank (4/4 rows are the space cell).
        for strip_row in 0..EQ_ROWS {
            assert_eq!(eq_strip_glyph(0, strip_row), " ", "level 0 row {strip_row}");
        }
    }

    #[test]
    fn eq_strip_positive_fills_inner_row_first() {
        // Inner upper row (strip row 1) runs the 6-dot progression for +1..+6.
        for (lvl, glyph) in ["⠠", "⠤", "⠴", "⠶", "⠾", "⠿"].iter().enumerate() {
            assert_eq!(eq_strip_glyph(lvl as i32 + 1, 1), *glyph, "level +{}", lvl + 1);
            assert_eq!(eq_strip_glyph(lvl as i32 + 1, 0), " ", "outer row must stay blank at +{}", lvl + 1);
        }
        // +7..+12: inner row saturates, outer row runs the progression.
        for (lvl, glyph) in ["⠠", "⠤", "⠴", "⠶", "⠾", "⠿"].iter().enumerate() {
            assert_eq!(eq_strip_glyph(lvl as i32 + 7, 1), "⠿", "inner saturated at +{}", lvl + 7);
            assert_eq!(eq_strip_glyph(lvl as i32 + 7, 0), *glyph, "outer row at +{}", lvl + 7);
        }
    }

    #[test]
    fn eq_strip_negative_fills_inner_row_first() {
        // Inner lower row (strip row 2) runs the 6-dot progression for -1..-6.
        for (lvl, glyph) in ["⠈", "⠉", "⠙", "⠛", "⠻", "⠿"].iter().enumerate() {
            assert_eq!(eq_strip_glyph(-(lvl as i32) - 1, 2), *glyph, "level -{}", lvl + 1);
            assert_eq!(eq_strip_glyph(-(lvl as i32) - 1, 3), " ", "outer row must stay blank at -{}", lvl + 1);
        }
        // -7..-12: inner row saturates, outer (bottom-most) row fills next.
        for (lvl, glyph) in ["⠈", "⠉", "⠙", "⠛", "⠻", "⠿"].iter().enumerate() {
            assert_eq!(eq_strip_glyph(-(lvl as i32) - 7, 2), "⠿", "inner saturated at -{}", lvl + 7);
            assert_eq!(eq_strip_glyph(-(lvl as i32) - 7, 3), *glyph, "outer row at -{}", lvl + 7);
        }
    }

    #[test]
    fn band_rows_fill_middle_out() {
        // Level 1-6: only the row adjacent to the baseline fills.
        assert_eq!(braille_row_glyph(1, 0), "⠠");
        assert_eq!(braille_row_glyph(5, 0), "⠾");
        assert_eq!(braille_row_glyph(5, 1), " ", "outer row untouched below level 7");
        // Level 7-12: inner row saturated (⠿), outer row runs the progression.
        assert_eq!(braille_row_glyph(7, 0), "⠿");
        assert_eq!(braille_row_glyph(7, 1), "⠠");
        assert_eq!(braille_row_glyph(12, 1), "⠿");
        // Negative dots fill top-down in each row below the baseline.
        assert_eq!(braille_row_glyph(-3, 0), "⠙");
        assert_eq!(braille_row_glyph(-8, 1), "⠉");
        assert_eq!(braille_row_glyph(-9, 1), "⠙");
        // Neutral: everything blank.
        for row in 0..2 {
            assert_eq!(braille_row_glyph(0, row), " ");
        }
    }

    #[test]
    fn status_toggle_hit_regions() {
        // Renderer-shaped geometry: 5 panel lines of 11 px, char_w 5 →
        // eq_top = 58; the status row spans y ∈ [47, 58).
        let (eq_top, line_h, char_w) = (PAD + 5 * 11, 11, 5);
        let col_x = |col: i32| PAD + col * char_w;
        let mid = eq_top - line_h / 2;
        // eq chunk (columns 0..6) hits Eq anywhere in its band.
        for col in [0, 3, 5] {
            assert_eq!(
                status_toggle_at(col_x(col), mid, eq_top, line_h, char_w),
                Some(StatusToggle::Eq),
                "eq chunk col {col}"
            );
        }
        // The gap between the chunks (column 6) stays caption (drag).
        assert_eq!(status_toggle_at(col_x(6), mid, eq_top, line_h, char_w), None);
        // repeat chunk (columns 7..14) hits Repeat.
        for col in [7, 10, 13] {
            assert_eq!(
                status_toggle_at(col_x(col), mid, eq_top, line_h, char_w),
                Some(StatusToggle::Repeat),
                "repeat chunk col {col}"
            );
        }
        // Beyond the repeat chunk: caption again.
        assert_eq!(status_toggle_at(col_x(14), mid, eq_top, line_h, char_w), None);
        // The whole hit band fits inside the 80 px window.
        assert!(col_x(14) <= WINDOW_W, "repeat chunk right edge {} clips", col_x(14));
        // Vertically outside the status row (panel above, EQ strip below):
        // no status hit.
        assert_eq!(status_toggle_at(col_x(0), eq_top - line_h - 1, eq_top, line_h, char_w), None);
        assert_eq!(status_toggle_at(col_x(0), eq_top, eq_top, line_h, char_w), None);
    }

    #[test]
    fn eq_columns_map_by_exact_tenths() {
        // 80 px / 10 bands = 8 px per column.
        assert_eq!(eq_band_index(0, 80), 0);
        assert_eq!(eq_band_index(7, 80), 0);
        assert_eq!(eq_band_index(8, 80), 1);
        assert_eq!(eq_band_index(39, 80), 4);
        assert_eq!(eq_band_index(40, 80), 5);
        assert_eq!(eq_band_index(79, 80), 9);
        // Out-of-range client x clamps into the edge columns.
        assert_eq!(eq_band_index(-50, 80), 0);
        assert_eq!(eq_band_index(500, 80), 9);
    }
}
