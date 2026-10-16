//! Layered-window renderer: panel text → 32bpp ARGB DIB → `UpdateLayeredWindow`.
//!
//! GDI text output never writes the alpha byte, so the trick here is: render
//! white text onto a zeroed (fully transparent) DIB, then post-process once —
//! `alpha = luminance`, `rgb = accent × alpha` (premultiplied, as
//! `UpdateLayeredWindow` requires). That yields per-pixel-alpha antialiased
//! text with a single pass over the small DIB. Redraws happen on visual state
//! changes and at 1 Hz while playing; paused state changes can also redraw.
//!
//! The window is moved/sized by the caller and then purely painted through
//! `UpdateLayeredWindow`; the separate tracklist drawer uses WM_PAINT. The
//! overlay is draggable over panel text (`HTCAPTION`), except Track Info and X.
//! Those controls and the Braille EQ strip use `HTCLIENT`.

use std::ffi::c_void;
use std::ptr::null_mut;

use windows::Win32::Foundation::{COLORREF, GetLastError, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, ANTIALIASED_QUALITY, BACKGROUND_MODE, BI_RGB, BITMAPINFO,
    BITMAPINFOHEADER, BLENDFUNCTION, CLIP_DEFAULT_PRECIS, CreateCompatibleDC, CreateDIBSection,
    CreateFontW, CreateSolidBrush, DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, DT_END_ELLIPSIS,
    DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DeleteDC, DeleteObject, DrawTextW, FF_MODERN, FF_SWISS,
    FIXED_PITCH, FillRect, GdiFlush, GetDC, GetStockObject, GetTextExtentPoint32W, GetTextMetricsW,
    HBITMAP, HBRUSH, HDC, HFONT, OUT_TT_PRECIS, ReleaseDC, SelectObject, SetBkMode, SetTextColor,
    TEXTMETRICW, TRANSPARENT, TextOutW, WHITE_BRUSH,
};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, ULW_ALPHA, UpdateLayeredWindow};
use windows::core::{Error, HRESULT, Result, w};

use crate::broker::{EQ_BANDS, EQ_MAX_GAIN_DB, Phase, TrackList};
use crate::{log_debug, log_error, log_warn};

/// Capture the current Win32 error for a failed GDI call.
fn last_error() -> Error {
    unsafe { Error::from_hresult(HRESULT::from_win32(GetLastError().0)) }
}

const PAD: i32 = 3;
/// The 80 px compact floating window; panel height is derived from the font.
/// At the 9 px Consolas em this leaves room for ~14 text cells per line, so
/// the panel formats in `render_panel` are tuned to that budget.
pub const WINDOW_W: i32 = 80;
/// Braille equalizer rows: two fill rows above the middle baseline and two
/// below (6 levels per row → the ±12 dB gain range exactly).
const EQ_ROWS: usize = 4;
/// Negative requests character height rather than the full GDI cell height.
const FONT_HEIGHT: i32 = -9;
const QUIT_BUTTON_W: i32 = 8;

fn quit_button_rect(line_h: i32) -> RECT {
    RECT {
        left: WINDOW_W - PAD - QUIT_BUTTON_W,
        top: PAD,
        right: WINDOW_W - PAD,
        bottom: PAD + line_h,
    }
}

pub fn quit_button_at(x: i32, y: i32, line_h: i32) -> bool {
    let rect = quit_button_rect(line_h);
    (rect.left..rect.right).contains(&x) && (rect.top..rect.bottom).contains(&y)
}

pub fn track_info_at(x: i32, y: i32, line_h: i32) -> bool {
    line_h > 0 && (0..WINDOW_W).contains(&x) && (PAD + 2 * line_h..PAD + 3 * line_h).contains(&y)
}

pub const TRACKLIST_WIDTH: i32 = 420;
pub const TRACKLIST_ROW_H: i32 = 24;
const TRACKLIST_HEADER_H: i32 = 28;
const TRACKLIST_MAX_ROWS: usize = 12;
const TRACKLIST_BG: COLORREF = COLORREF(0x00241E1A);
const TRACKLIST_SELECTED: COLORREF = COLORREF(0x004A6040);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrackListLayout {
    pub width: i32,
    pub rows: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum TrackListClick {
    None,
    Dismiss,
    Jump(usize),
}

impl TrackListLayout {
    pub fn new(count: usize, work_width: i32, work_height: i32) -> Self {
        let available = ((work_height - TRACKLIST_HEADER_H) / TRACKLIST_ROW_H).max(1) as usize;
        Self {
            width: TRACKLIST_WIDTH.min(work_width.max(1)),
            rows: count.max(1).min(available).min(TRACKLIST_MAX_ROWS),
        }
    }

    pub fn height(self) -> i32 {
        TRACKLIST_HEADER_H + self.rows as i32 * TRACKLIST_ROW_H
    }

    pub fn first(self, count: usize, current: Option<usize>) -> usize {
        current
            .unwrap_or(0)
            .saturating_sub(self.rows / 2)
            .min(count.saturating_sub(self.rows))
    }

    pub fn scroll(self, first: usize, delta: i32, count: usize) -> usize {
        (first as i128 + delta as i128).clamp(0, count.saturating_sub(self.rows) as i128) as usize
    }

    pub fn click(self, x: i32, y: i32, first: usize, count: usize) -> TrackListClick {
        if x < 0 || x >= self.width || y < 0 || y >= self.height() {
            return TrackListClick::Dismiss;
        }
        if y < TRACKLIST_HEADER_H || y >= TRACKLIST_HEADER_H + self.rows as i32 * TRACKLIST_ROW_H {
            return TrackListClick::None;
        }
        let row = ((y - TRACKLIST_HEADER_H) / TRACKLIST_ROW_H) as usize;
        match first.checked_add(row).filter(|&index| index < count) {
            Some(index) => TrackListClick::Jump(index + 1),
            None => TrackListClick::None,
        }
    }
}

pub fn tracklist_position(anchor: RECT, work: RECT, layout: TrackListLayout) -> (i32, i32) {
    let preferred = if anchor.left - layout.width - 4 >= work.left {
        anchor.left - layout.width - 4
    } else {
        anchor.right + 4
    };
    (
        preferred.clamp(work.left, (work.right - layout.width).max(work.left)),
        anchor
            .top
            .clamp(work.top, (work.bottom - layout.height()).max(work.top)),
    )
}

/// Ordinary GDI surface for the temporary opaque, frameless track drawer.
pub struct TrackListRenderer {
    font: HFONT,
    background: HBRUSH,
    selected: HBRUSH,
}

impl TrackListRenderer {
    pub fn new() -> Result<Self> {
        unsafe {
            let renderer = Self {
                font: CreateFontW(
                    -14,
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
                    (DEFAULT_PITCH.0 | FF_SWISS.0) as u32,
                    w!("Segoe UI"),
                ),
                background: CreateSolidBrush(TRACKLIST_BG),
                selected: CreateSolidBrush(TRACKLIST_SELECTED),
            };
            if renderer.font.is_invalid()
                || renderer.background.is_invalid()
                || renderer.selected.is_invalid()
            {
                return Err(last_error());
            }
            Ok(renderer)
        }
    }

    pub fn draw(&self, dc: HDC, view: &TrackList, layout: TrackListLayout, first: usize) {
        unsafe {
            FillRect(
                dc,
                &RECT {
                    left: 0,
                    top: 0,
                    right: layout.width,
                    bottom: layout.height(),
                },
                self.background,
            );
            let old_font = SelectObject(dc, self.font.into());
            let old_mode = SetBkMode(dc, TRANSPARENT);
            let old_color = SetTextColor(dc, COLORREF(0x00F0F0F0));
            let text = |label: &str, top: i32, height: i32| {
                let mut wide = label
                    .chars()
                    .map(|c| if c.is_control() { ' ' } else { c })
                    .collect::<String>()
                    .encode_utf16()
                    .collect::<Vec<u16>>();
                let mut rect = RECT {
                    left: 10,
                    top,
                    right: layout.width - 10,
                    bottom: top + height,
                };
                DrawTextW(
                    dc,
                    &mut wide,
                    &mut rect,
                    DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
                );
            };
            text(
                &format!("Tracks ({})", view.titles.len()),
                0,
                TRACKLIST_HEADER_H,
            );
            if view.titles.is_empty() {
                text(
                    if view.loading {
                        "Loading playlist…"
                    } else {
                        "No tracks loaded"
                    },
                    TRACKLIST_HEADER_H,
                    TRACKLIST_ROW_H,
                );
            }
            for row in 0..layout.rows {
                let index = first + row;
                let Some(title) = view.titles.get(index) else {
                    break;
                };
                let top = TRACKLIST_HEADER_H + row as i32 * TRACKLIST_ROW_H;
                if view.current == Some(index) {
                    FillRect(
                        dc,
                        &RECT {
                            left: 0,
                            top,
                            right: layout.width,
                            bottom: top + TRACKLIST_ROW_H,
                        },
                        self.selected,
                    );
                }
                text(&format!("{}. {title}", index + 1), top, TRACKLIST_ROW_H);
            }
            SetTextColor(dc, old_color);
            SetBkMode(dc, BACKGROUND_MODE(old_mode as u32));
            SelectObject(dc, old_font);
        }
    }
}

impl Drop for TrackListRenderer {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.font.into());
            let _ = DeleteObject(self.background.into());
            let _ = DeleteObject(self.selected.into());
        }
    }
}

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
    (
        eq_top + strip_row as i32 * line_h,
        eq_strip_glyph(level, strip_row),
    )
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
    let left = cell.left + (width - (2 * dot_size + 1)) / 2 + (dot / 3) as i32 * (dot_size + 1);
    let top = cell.top + (height - (3 * dot_size + 2)) / 2 + (dot % 3) as i32 * (dot_size + 1);
    Some(RECT {
        left,
        top,
        right: left + dot_size,
        bottom: top + dot_size,
    })
}

/// Which of the 10 equalizer columns does a client x fall in? Pure mapping
/// shared by drawing and hit-testing.
fn eq_band_index(x: i32, width: i32) -> usize {
    (x.clamp(0, width - 1) * EQ_BANDS as i32 / width) as usize
}

/// ASCII header tokens avoid font fallback and leave ample room for quit X.
fn status_token(phase: Phase) -> &'static str {
    match phase {
        Phase::Playing => "PLY",
        Phase::Paused => "PSD",
        Phase::Stopped => "STP",
        Phase::Loading => "LDG",
    }
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

struct OverlayFrame {
    panel: String,
    phase: Phase,
    eq: [f32; EQ_BANDS],
}

pub struct Renderer {
    width: i32,
    height: i32,
    line_h: i32,
    /// Client y where the Braille equalizer strip begins (panel text ends);
    /// recomputed on every draw from the panel's line count.
    eq_top: i32,
    screen_dc: HDC,
    mem_dc: HDC,
    bitmap: HBITMAP,
    bits: *mut u8,
    font: HFONT,
    last_frame: Option<OverlayFrame>,
    #[cfg(test)]
    presentations: usize,
}

impl Renderer {
    /// Fixed-width DIB whose height follows the panel's line count and font.
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
            // Panel text and four Braille rows, with padding on both ends.
            let height = 2 * PAD + rows * line_h + EQ_ROWS as i32 * line_h;

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
                eq_top: PAD + rows * line_h,
                screen_dc,
                mem_dc,
                bitmap,
                bits: bits as *mut u8,
                font,
                last_frame: None,
                #[cfg(test)]
                presentations: 0,
            })
        }
    }

    /// Window size to create — the DIB and the window must match exactly.
    pub fn size(&self) -> (i32, i32) {
        (self.width, self.height)
    }

    /// Which equalizer band (if any) does a CLIENT-coordinate point hover?
    /// Only the four Braille rows accept EQ wheel input; the column split
    /// is exact tenths of the window width.
    pub fn eq_band_at(&self, x: i32, y: i32) -> Option<usize> {
        if !(0..self.width).contains(&x)
            || y < self.eq_top
            || y >= self.eq_top + EQ_ROWS as i32 * self.line_h
        {
            return None;
        }
        Some(eq_band_index(x, self.width))
    }

    /// Client input regions: quit X, Track Info and the Braille strip.
    /// Volume/EQ/repeat text remains part of the native drag surface.
    pub fn client_input_at(&self, x: i32, y: i32) -> bool {
        self.quit_button_at(x, y) || self.track_info_at(x, y) || self.eq_band_at(x, y).is_some()
    }

    pub fn quit_button_at(&self, x: i32, y: i32) -> bool {
        quit_button_at(x, y, self.line_h)
    }

    pub fn track_info_at(&self, x: i32, y: i32) -> bool {
        track_info_at(x, y, self.line_h)
    }

    pub fn track_info_y(&self) -> i32 {
        PAD + 2 * self.line_h
    }

    fn draw_panel_text(&self, panel: &str, phase: Phase) {
        unsafe {
            for (i, line) in panel.lines().enumerate() {
                if i == 0 {
                    // Branding and compact state share the existing header
                    // row, leaving the quit button pinned on the right.
                    let mut wide: Vec<u16> = format!("KYOUKO {}", status_token(phase))
                        .encode_utf16()
                        .collect();
                    let mut rect = RECT {
                        left: PAD,
                        top: PAD,
                        right: quit_button_rect(self.line_h).left,
                        bottom: PAD + self.line_h,
                    };
                    DrawTextW(
                        self.mem_dc,
                        &mut wide,
                        &mut rect,
                        DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
                    );
                } else {
                    let wide: Vec<u16> = line.encode_utf16().collect();
                    let _ = TextOutW(self.mem_dc, PAD, PAD + i as i32 * self.line_h, &wide);
                }
            }
        }
    }

    fn draw_quit_button(&self) {
        unsafe {
            let mut rect = quit_button_rect(self.line_h);
            DrawTextW(
                self.mem_dc,
                &mut ['X' as u16],
                &mut rect,
                windows::Win32::Graphics::Gdi::DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
        }
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
    /// a full repaint through the persistent memory DC when content changes.
    /// Layered presentation is atomic; unchanged snapshots do no GDI work.
    pub fn draw(&mut self, hwnd: HWND, panel: &str, phase: Phase, eq: &[f32; EQ_BANDS]) {
        if self
            .last_frame
            .as_ref()
            .is_some_and(|frame| frame.panel == panel && frame.phase == phase && frame.eq == *eq)
        {
            return;
        }
        unsafe {
            let len = (self.width * self.height) as usize * 4;
            std::ptr::write_bytes(self.bits, 0, len);

            SetTextColor(self.mem_dc, COLORREF(0x00FF_FFFF));
            self.draw_panel_text(panel, phase);
            self.draw_quit_button();

            // Braille equalizer: 10 columns across the full width, filling
            // middle-out from the invisible baseline between the two inner
            // rows. Side exclusivity, the glyph, and its physical Y come
            // from the same pure mapping exercised by the row tests.
            self.eq_top = PAD + panel.lines().count() as i32 * self.line_h;
            self.draw_eq_strip(eq);

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
            for px in std::slice::from_raw_parts_mut(self.bits, len)
                .as_chunks_mut::<4>()
                .0
            {
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

            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            if crate::logging::enabled(crate::logging::LVL_DEBUG) {
                let lit = std::slice::from_raw_parts(self.bits, len)
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .filter(|p| p[3] > 0)
                    .count();
                log_debug!("RENDER", "draw: {lit}/{} px lit", len / 4);
            }
            // Explicit position+size: some Windows builds silently ignore a
            // NULL/NULL (keep-current) ULW, leaving the surface transparent.
            let mut rc = RECT::default();
            let _ = GetWindowRect(hwnd, &mut rc);
            let pt_dst = POINT {
                x: rc.left,
                y: rc.top,
            };
            let size = SIZE {
                cx: self.width,
                cy: self.height,
            };
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
            } else {
                // Commit only after presentation succeeds, so failures retry.
                if let Some(frame) = &mut self.last_frame {
                    frame.panel.clear();
                    frame.panel.push_str(panel);
                    frame.phase = phase;
                    frame.eq = *eq;
                } else {
                    self.last_frame = Some(OverlayFrame {
                        panel: panel.into(),
                        phase,
                        eq: *eq,
                    });
                }
                #[cfg(test)]
                {
                    self.presentations += 1;
                }
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
    fn overlay_skips_identical_frames_but_presents_eq_phase_and_text_changes() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_POPUP,
        };
        let panel = "header\ntime\ntrack\nvolume\neq repeat";
        let mut renderer = Renderer::new(panel).unwrap();
        let (width, height) = renderer.size();
        unsafe {
            // A hidden native surface exercises real layered presentation;
            // there is no injected mouse input or desktop interaction.
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!(""),
                WS_POPUP,
                -32000,
                -32000,
                width,
                height,
                None,
                None,
                Some(GetModuleHandleW(None).unwrap().into()),
                None,
            )
            .unwrap();
            let mut gains = [0.0; EQ_BANDS];
            renderer.draw(hwnd, panel, Phase::Playing, &gains);
            assert_eq!(renderer.presentations, 1);
            for _ in 0..3 {
                renderer.draw(hwnd, panel, Phase::Playing, &gains);
            }
            assert_eq!(
                renderer.presentations, 1,
                "unchanged events do no presentation work"
            );
            gains[2] = 5.0;
            renderer.draw(hwnd, panel, Phase::Playing, &gains);
            assert_eq!(
                renderer.presentations, 2,
                "EQ is not part of the text panel"
            );
            renderer.draw(hwnd, panel, Phase::Paused, &gains);
            assert_eq!(renderer.presentations, 3, "phase changes header and accent");
            renderer.draw(
                hwnd,
                "header\nnew time\ntrack\nvolume\neq repeat",
                Phase::Paused,
                &gains,
            );
            assert_eq!(renderer.presentations, 4);
            assert_eq!(renderer.size(), (width, height));
            DestroyWindow(hwnd).unwrap();
        }
    }

    #[test]
    fn track_info_hit_test_only_carves_out_the_third_text_row() {
        assert!(track_info_at(0, PAD + 2 * 11, 11));
        assert!(track_info_at(WINDOW_W - 1, PAD + 3 * 11 - 1, 11));
        assert!(!track_info_at(20, PAD + 2 * 11 - 1, 11));
        assert!(!track_info_at(20, PAD + 3 * 11, 11));
        assert!(!track_info_at(-1, PAD + 2 * 11, 11));
        assert!(!track_info_at(WINDOW_W, PAD + 2 * 11, 11));
    }

    #[test]
    fn drawer_scroll_selection_and_outside_dismissal_share_geometry() {
        let layout = TrackListLayout::new(3100, 1920, 1080);
        assert_eq!(layout.rows, 12);
        let first = layout.first(3100, Some(3099));
        assert_eq!(first, 3088);
        assert_eq!(
            layout.click(
                10,
                TRACKLIST_HEADER_H + 11 * TRACKLIST_ROW_H + 1,
                first,
                3100
            ),
            TrackListClick::Jump(3100)
        );
        assert_eq!(layout.scroll(0, -3, 3100), 0);
        assert_eq!(layout.scroll(first, 3, 3100), first);
        for (x, y) in [
            (-1, 10),
            (layout.width, 10),
            (10, -1),
            (10, layout.height()),
        ] {
            assert_eq!(layout.click(x, y, first, 3100), TrackListClick::Dismiss);
        }
        assert_eq!(layout.click(10, 0, 0, 3100), TrackListClick::None);
        let empty = TrackListLayout::new(0, 1920, 1080);
        assert_eq!(
            empty.click(10, TRACKLIST_HEADER_H, 0, 0),
            TrackListClick::None
        );
        let work = RECT {
            left: -1920,
            top: -300,
            right: 0,
            bottom: 780,
        };
        let anchor = RECT {
            left: -84,
            top: 740,
            right: -4,
            bottom: 780,
        };
        let (x, y) = tracklist_position(anchor, work, layout);
        assert!(x >= work.left && x + layout.width <= work.right);
        assert!(y >= work.top && y + layout.height() <= work.bottom);
    }

    #[test]
    fn gdi_tracklist_highlights_only_the_selected_row() {
        let canvas = Renderer::new("header\ntime\ntrack\nvolume\neq repeat").unwrap();
        let renderer = TrackListRenderer::new().unwrap();
        let layout = TrackListLayout {
            width: WINDOW_W,
            rows: 2,
        };
        let view = TrackList {
            titles: vec!["音楽 A".into(), "Song B".into()],
            current: Some(1),
            loading: false,
        };
        renderer.draw(canvas.mem_dc, &view, layout, 0);
        unsafe {
            let _ = GdiFlush();
            use windows::Win32::Graphics::Gdi::GetPixel;
            assert_eq!(
                GetPixel(canvas.mem_dc, 2, TRACKLIST_HEADER_H + 2),
                TRACKLIST_BG
            );
            assert_eq!(
                GetPixel(canvas.mem_dc, 2, TRACKLIST_HEADER_H + TRACKLIST_ROW_H + 2),
                TRACKLIST_SELECTED
            );
        }
    }

    #[test]
    fn braille_progression_matches_spec() {
        // 0 = blank, then the 6-step dot progression, saturated beyond 6.
        let want = [" ", "⠠", "⠤", "⠴", "⠶", "⠾", "⠿"];
        for (levels, glyph) in want.iter().enumerate() {
            assert_eq!(braille_glyph(levels as i32), *glyph, "levels={levels}");
        }
        assert_eq!(braille_glyph(7), "⠿", "beyond 6 saturates the cell");
        assert_eq!(
            braille_glyph(-3),
            " ",
            "negative levels never reach a cell directly"
        );
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
            assert_eq!(
                eq_strip_row(gain_db, 0, 58, 11),
                (58, glyphs[0]),
                "gain {gain_db}, physical row 0"
            );
            assert_eq!(
                eq_strip_row(gain_db, 1, 58, 11),
                (69, glyphs[1]),
                "gain {gain_db}, physical row 1"
            );
            assert_eq!(
                eq_strip_row(gain_db, 2, 58, 11),
                (80, glyphs[2]),
                "gain {gain_db}, physical row 2"
            );
            assert_eq!(
                eq_strip_row(gain_db, 3, 58, 11),
                (91, glyphs[3]),
                "gain {gain_db}, physical row 3"
            );
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
        let renderer =
            Renderer::new("KYOUKO STOPPED\n00:00/--:--\n(none)\nvol ||||-  80%\neq:ON  rep:OFF")
                .unwrap();
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
                for (pixel_index, pixel) in pixels.as_chunks::<4>().0.iter().enumerate() {
                    if pixel[..3].iter().all(|channel| *channel == 0) {
                        continue;
                    }
                    let x = pixel_index as i32 % renderer.width;
                    let y = pixel_index as i32 / renderer.width;
                    assert!(
                        x >= band as i32 * col_w && x < (band as i32 + 1) * col_w,
                        "gain {gain_db} spilled into another band at ({x}, {y})"
                    );
                    assert!(
                        y >= renderer.eq_top
                            && y < renderer.eq_top + EQ_ROWS as i32 * renderer.line_h,
                        "gain {gain_db} spilled outside the strip at ({x}, {y})"
                    );
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
                    if !unseen[start] {
                        continue;
                    }
                    dots += 1;
                    let mut pending = vec![start];
                    unseen[start] = false;
                    while let Some(pixel) = pending.pop() {
                        let x = pixel as i32 % col_w;
                        let y = pixel as i32 / col_w;
                        for (nx, ny) in [(x - 1, y), (x + 1, y), (x, y - 1), (x, y + 1)] {
                            if nx < 0 || nx >= col_w || ny < 0 || ny >= renderer.line_h {
                                continue;
                            }
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
            assert_ne!(
                masks[level - 1],
                masks[level],
                "GDI renders levels {level} and {} identically",
                level + 1
            );
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
                let actual: Vec<_> = pixels
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .enumerate()
                    .filter(|(_, pixel)| pixel[..3].iter().any(|channel| *channel != 0))
                    .map(|(index, _)| {
                        (index as i32 % renderer.width, index as i32 / renderer.width)
                    })
                    .collect();
                let mut expected = Vec::new();
                for (physical_row, steps) in
                    [(2, magnitude.min(6)), (3, magnitude.saturating_sub(6))]
                {
                    let origin_x = 3 * col_w + (col_w - 5) / 2;
                    let origin_y = renderer.eq_top
                        + physical_row * renderer.line_h
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
            assert_eq!(
                eq_strip_glyph(5, strip_row),
                " ",
                "+5 lower row {strip_row}"
            );
        }
        // Negative gains light only the rows below the baseline.
        for strip_row in 0..2 {
            assert_eq!(
                eq_strip_glyph(-5, strip_row),
                " ",
                "-5 upper row {strip_row}"
            );
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
            assert_eq!(
                eq_strip_glyph(lvl as i32 + 1, 1),
                *glyph,
                "level +{}",
                lvl + 1
            );
            assert_eq!(
                eq_strip_glyph(lvl as i32 + 1, 0),
                " ",
                "outer row must stay blank at +{}",
                lvl + 1
            );
        }
        // +7..+12: inner row saturates, outer row runs the progression.
        for (lvl, glyph) in ["⠠", "⠤", "⠴", "⠶", "⠾", "⠿"].iter().enumerate() {
            assert_eq!(
                eq_strip_glyph(lvl as i32 + 7, 1),
                "⠿",
                "inner saturated at +{}",
                lvl + 7
            );
            assert_eq!(
                eq_strip_glyph(lvl as i32 + 7, 0),
                *glyph,
                "outer row at +{}",
                lvl + 7
            );
        }
    }

    #[test]
    fn eq_strip_negative_fills_inner_row_first() {
        // Inner lower row (strip row 2) runs the 6-dot progression for -1..-6.
        for (lvl, glyph) in ["⠈", "⠉", "⠙", "⠛", "⠻", "⠿"].iter().enumerate() {
            assert_eq!(
                eq_strip_glyph(-(lvl as i32) - 1, 2),
                *glyph,
                "level -{}",
                lvl + 1
            );
            assert_eq!(
                eq_strip_glyph(-(lvl as i32) - 1, 3),
                " ",
                "outer row must stay blank at -{}",
                lvl + 1
            );
        }
        // -7..-12: inner row saturates, outer (bottom-most) row fills next.
        for (lvl, glyph) in ["⠈", "⠉", "⠙", "⠛", "⠻", "⠿"].iter().enumerate() {
            assert_eq!(
                eq_strip_glyph(-(lvl as i32) - 7, 2),
                "⠿",
                "inner saturated at -{}",
                lvl + 7
            );
            assert_eq!(
                eq_strip_glyph(-(lvl as i32) - 7, 3),
                *glyph,
                "outer row at -{}",
                lvl + 7
            );
        }
    }

    #[test]
    fn band_rows_fill_middle_out() {
        // Level 1-6: only the row adjacent to the baseline fills.
        assert_eq!(braille_row_glyph(1, 0), "⠠");
        assert_eq!(braille_row_glyph(5, 0), "⠾");
        assert_eq!(
            braille_row_glyph(5, 1),
            " ",
            "outer row untouched below level 7"
        );
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
    fn compact_layout_keeps_size_and_routes_only_quit_track_info_and_eq_as_client_input() {
        let shared = crate::broker::SharedState::new();
        let panel = crate::broker::render_panel(&shared, None);
        let renderer = Renderer::new(&panel).unwrap();
        assert_eq!(panel.lines().count(), 5);
        assert_eq!(renderer.size(), (WINDOW_W, 2 * PAD + 9 * renderer.line_h));
        assert_eq!(
            renderer.height,
            renderer.eq_top + EQ_ROWS as i32 * renderer.line_h + PAD
        );
        for y in 0..renderer.height {
            for x in 0..renderer.width {
                let track = (PAD + 2 * renderer.line_h..PAD + 3 * renderer.line_h).contains(&y);
                let eq = (renderer.eq_top..renderer.height - PAD).contains(&y);
                let quit = (WINDOW_W - PAD - 8..WINDOW_W - PAD).contains(&x)
                    && (PAD..PAD + renderer.line_h).contains(&y);
                assert_eq!(
                    renderer.client_input_at(x, y),
                    quit || track || eq,
                    "client region ({x}, {y})"
                );
                assert_eq!(
                    renderer.eq_band_at(x, y),
                    eq.then_some((x / 8) as usize),
                    "wheel region ({x}, {y})"
                );
            }
        }
        for x in [-1, renderer.width] {
            assert!(!renderer.client_input_at(x, renderer.track_info_y()));
            assert_eq!(renderer.eq_band_at(x, renderer.eq_top), None);
        }
        for y in [-1, renderer.height] {
            assert!(!renderer.client_input_at(0, y));
            assert_eq!(renderer.eq_band_at(0, y), None);
        }
    }

    #[test]
    fn quit_button_edges_and_header_pixels_share_one_box_without_growing_the_window() {
        let renderer = Renderer::new("KYOUKO PLAYING\ntime\ntrack\nvolume\neq/repeat").unwrap();
        let rect = quit_button_rect(renderer.line_h);
        assert_eq!(renderer.width, 80);
        assert_eq!(renderer.height, 2 * PAD + 9 * renderer.line_h);
        for (x, y) in [
            (rect.left, rect.top),
            (rect.right - 1, rect.top),
            (rect.left, rect.bottom - 1),
            (rect.right - 1, rect.bottom - 1),
        ] {
            assert!(renderer.quit_button_at(x, y));
            assert!(renderer.client_input_at(x, y));
        }
        for (x, y) in [
            (rect.left - 1, rect.top),
            (rect.right, rect.top),
            (rect.left, rect.top - 1),
            (rect.left, rect.bottom),
        ] {
            assert!(!renderer.quit_button_at(x, y));
        }
        assert!(!quit_button_at(72, PAD, 0));
        unsafe {
            let len = (renderer.width * renderer.height) as usize * 4;
            std::ptr::write_bytes(renderer.bits, 0, len);
            SetTextColor(renderer.mem_dc, COLORREF(0x00FF_FFFF));
            renderer.draw_panel_text(
                "KYOUKO PLAYING WITH A VERY LONG HEADER\ntime\ntrack\nvolume\neq/repeat",
                Phase::Playing,
            );
            assert!(GdiFlush().as_bool());
            let lit_in_button = || {
                let pixels = std::slice::from_raw_parts(renderer.bits, len);
                (rect.top..rect.bottom)
                    .flat_map(|y| {
                        (rect.left..rect.right).map(move |x| (y * renderer.width + x) as usize)
                    })
                    .filter(|&index| {
                        pixels[index * 4..index * 4 + 3]
                            .iter()
                            .any(|&value| value != 0)
                    })
                    .count()
            };
            assert_eq!(lit_in_button(), 0, "header must not overlap the quit glyph");
            renderer.draw_quit_button();
            assert!(GdiFlush().as_bool());
            assert!(
                lit_in_button() > 0,
                "X must be visibly painted inside its hit box"
            );
        }
    }

    #[test]
    fn branding_and_compact_tokens_render_fully_before_pinned_quit() {
        let shared = crate::broker::SharedState::new();
        for (phase, token, header) in [
            (Phase::Playing, "PLY", "KYOUKO PLY"),
            (Phase::Paused, "PSD", "KYOUKO PSD"),
            (Phase::Stopped, "STP", "KYOUKO STP"),
            (Phase::Loading, "LDG", "KYOUKO LDG"),
        ] {
            assert_eq!(status_token(phase), token);
            assert!(token.is_ascii());
            shared.set_phase(phase);
            let panel = crate::broker::render_panel(&shared, None);
            let renderer = Renderer::new(&panel).unwrap();
            let reference = Renderer::new(&panel).unwrap();
            assert_eq!(renderer.size(), (80, 2 * PAD + 9 * renderer.line_h));
            let quit = quit_button_rect(renderer.line_h);
            unsafe {
                let len = (renderer.width * renderer.height) as usize * 4;
                std::ptr::write_bytes(renderer.bits, 0, len);
                std::ptr::write_bytes(reference.bits, 0, len);
                SetTextColor(renderer.mem_dc, COLORREF(0x00FF_FFFF));
                SetTextColor(reference.mem_dc, COLORREF(0x00FF_FFFF));
                let wide: Vec<u16> = header.encode_utf16().collect();
                let mut extent = SIZE::default();
                GetTextExtentPoint32W(renderer.mem_dc, &wide, &mut extent).unwrap();
                assert!(
                    PAD + extent.cx < quit.left,
                    "{header} needs no ellipsis and cannot overlap X"
                );
                renderer.draw_panel_text(&panel, phase);
                // Reference paints the full brand and token without clipping
                // or ellipsis; pixel equality proves no letters are lost.
                TextOutW(reference.mem_dc, PAD, PAD, &wide).unwrap();
                assert!(GdiFlush().as_bool());
                let first_row = (PAD * renderer.width) as usize * 4;
                let after_row = ((PAD + renderer.line_h) * renderer.width) as usize * 4;
                assert_eq!(
                    &std::slice::from_raw_parts(renderer.bits, len)[first_row..after_row],
                    &std::slice::from_raw_parts(reference.bits, len)[first_row..after_row],
                    "full {header} pixels"
                );
                renderer.draw_quit_button();
                assert!(GdiFlush().as_bool());
                let pixels = std::slice::from_raw_parts(renderer.bits, len);
                assert!(
                    (quit.top..quit.bottom).any(|y| (quit.left..quit.right).any(|x| {
                        let index = (y * renderer.width + x) as usize * 4;
                        pixels[index..index + 3].iter().any(|&value| value != 0)
                    })),
                    "X stays painted at the right edge"
                );
            }
        }
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
