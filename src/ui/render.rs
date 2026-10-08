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

use crate::broker::{EQ_BANDS, EQ_MAX_GAIN_DB, Phase};
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
/// Braille equalizer rows: two fill rows above the middle baseline and two
/// below (6 levels per row → the ±12 dB gain range exactly).
const EQ_ROWS: usize = 4;
/// Negative = per-em height: the glyph cell stays exact at any DPI.
const FONT_HEIGHT: i32 = -9;

/// Which clickable status chunk sits under a point?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusToggle {
    /// The `eq   : ON/OFF` chunk (toggles `Command::ToggleEq`).
    Eq,
    /// The `repeat: ON/OFF` chunk (toggles `Command::ToggleLoop`).
    Repeat,
}

/// Clickable columns (in Consolas advance units from the left pad) of the
/// dual status line's two chunks: `eq   : ON ` spans columns 0..9 and
/// `repeat: OFF` columns 15..25. `render_panel` in broker.rs formats the
/// line with exactly this column table — drawing and hit-testing share it,
/// so they can never drift apart.
const STATUS_EQ_COLS: (i32, i32) = (0, 10);
const STATUS_REPEAT_COLS: (i32, i32) = (15, 11);

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
/// baseline. Direction comes from which side of the baseline the row sits
/// on, so negative levels use the same glyphs on the rows below the baseline.
fn braille_row_glyph(level: i32, row: i32) -> &'static str {
    braille_glyph(level.abs() - 6 * row)
}

/// Glyph for physical strip row `strip_row` (0 = top of the strip,
/// `EQ_ROWS - 1` = bottom) of a band at `level`. Encodes the side
/// exclusivity: positive levels light only the rows above the baseline,
/// negative levels only the rows below, and level 0 leaves every row blank.
/// Rows fill middle-out: `braille_row_glyph` maps the mirrored pair to the
/// inner-row-first progression in both directions.
fn eq_strip_glyph(level: i32, strip_row: usize) -> &'static str {
    let half = EQ_ROWS / 2;
    if level > 0 && strip_row < half {
        braille_row_glyph(level, (half - 1 - strip_row) as i32)
    } else if level < 0 && strip_row >= half {
        braille_row_glyph(level, (strip_row - half) as i32)
    } else {
        " "
    }
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
            // rows. One glyph per cell, centered in its tenth-column. Side
            // exclusivity and fill direction live in `eq_strip_glyph`.
            self.eq_top = PAD + panel.lines().count() as i32 * self.line_h;
            let col_w = self.width / EQ_BANDS as i32;
            for (band, gain) in eq.iter().enumerate() {
                let level = gain.round().clamp(-EQ_MAX_GAIN_DB, EQ_MAX_GAIN_DB) as i32;
                for strip_row in 0..EQ_ROWS {
                    let glyph = eq_strip_glyph(level, strip_row);
                    if glyph == " " {
                        continue;
                    }
                    let wide: Vec<u16> = glyph.encode_utf16().collect();
                    let mut extent = SIZE::default();
                    let _ = GetTextExtentPoint32W(self.mem_dc, &wide, &mut extent);
                    let _ = TextOutW(
                        self.mem_dc,
                        band as i32 * col_w + (col_w - extent.cx) / 2,
                        self.eq_top + strip_row as i32 * self.line_h,
                        &wide,
                    );
                }
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
        for (lvl, glyph) in ["⠠", "⠤", "⠴", "⠶", "⠾", "⠿"].iter().enumerate() {
            assert_eq!(eq_strip_glyph(-(lvl as i32) - 1, 2), *glyph, "level -{}", lvl + 1);
            assert_eq!(eq_strip_glyph(-(lvl as i32) - 1, 3), " ", "outer row must stay blank at -{}", lvl + 1);
        }
        // -7..-12: inner row saturates, outer (bottom-most) row fills next.
        for (lvl, glyph) in ["⠠", "⠤", "⠴", "⠶", "⠾", "⠿"].iter().enumerate() {
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
        // Negatives mirror on the rows below the baseline.
        assert_eq!(braille_row_glyph(-3, 0), "⠴");
        assert_eq!(braille_row_glyph(-8, 1), "⠤");
        assert_eq!(braille_row_glyph(-9, 1), "⠴");
        // Neutral: everything blank.
        for row in 0..2 {
            assert_eq!(braille_row_glyph(0, row), " ");
        }
    }

    #[test]
    fn status_toggle_hit_regions() {
        // Renderer-shaped geometry: 5 panel lines of 11 px, char_w 5 →
        // eq_top = 61; the status row spans y ∈ [50, 61).
        let (eq_top, line_h, char_w) = (PAD + 5 * 11, 11, 5);
        let col_x = |col: i32| PAD + col * char_w;
        let mid = eq_top - line_h / 2;
        // eq chunk (columns 0..10) hits Eq anywhere in its band.
        for col in [0, 5, 9] {
            assert_eq!(
                status_toggle_at(col_x(col), mid, eq_top, line_h, char_w),
                Some(StatusToggle::Eq),
                "eq chunk col {col}"
            );
        }
        // The gap between the chunks (columns 10..15) stays caption (drag).
        assert_eq!(status_toggle_at(col_x(12), mid, eq_top, line_h, char_w), None);
        // repeat chunk (columns 15..26) hits Repeat.
        for col in [15, 20, 25] {
            assert_eq!(
                status_toggle_at(col_x(col), mid, eq_top, line_h, char_w),
                Some(StatusToggle::Repeat),
                "repeat chunk col {col}"
            );
        }
        // Beyond the repeat chunk: caption again.
        assert_eq!(status_toggle_at(col_x(26), mid, eq_top, line_h, char_w), None);
        // Vertically outside the status row (panel above, EQ strip below):
        // no status hit.
        assert_eq!(status_toggle_at(col_x(0), eq_top - line_h - 1, eq_top, line_h, char_w), None);
        assert_eq!(status_toggle_at(col_x(0), eq_top, eq_top, line_h, char_w), None);
    }

    #[test]
    fn eq_columns_map_by_exact_tenths() {
        assert_eq!(eq_band_index(0, 200), 0);
        assert_eq!(eq_band_index(19, 200), 0);
        assert_eq!(eq_band_index(20, 200), 1);
        assert_eq!(eq_band_index(99, 200), 4);
        assert_eq!(eq_band_index(100, 200), 5);
        assert_eq!(eq_band_index(199, 200), 9);
        // Out-of-range client x clamps into the edge columns.
        assert_eq!(eq_band_index(-50, 200), 0);
        assert_eq!(eq_band_index(500, 200), 9);
    }
}
