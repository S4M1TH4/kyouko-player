//! The Windows presentation pump: 80 px layered window, tray integration,
//! and a `GetMessageW` loop that parks in the kernel whenever nothing is
//! happening. Every channel push from any thread arrives here as exactly one
//! posted message — data in channels, wake-ups in the window queue.
//!
//! Wake-up routing (`WM_APP`):
//! * `WM_APP_BROKER` (0x8001) — a Command is waiting in `cmd_rx`
//! * `WM_APP_STATUS` (0x8002) — a Status is waiting in `status_rx`
//! * `WM_APP_TRAY`   (0x8003) — tray icon mouse events
//!
//! Timer discipline: the 1 Hz panel timer exists **only** while `Playing`.
//! Pause/Stop kill it, so a paused player has zero scheduled wake-ups — the
//! thread sleeps in `GetMessageW` until a real event arrives.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crossbeam_channel::{Receiver, TryRecvError};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, POINTL, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ValidateRect;
use windows::Win32::System::Com::{
    DVASPECT_CONTENT, FORMATETC, IDataObject, STGMEDIUM, TYMED_HGLOBAL,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{
    CF_HDROP, CF_TEXT, CF_UNICODETEXT, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, IDropTarget,
    IDropTarget_Impl, OleInitialize, OleUninitialize, RegisterDragDrop, ReleaseStgMedium,
    RevokeDragDrop,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP, NIN_SELECT};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW,
    GetWindowLongPtrW, GetWindowRect, KillTimer, LoadCursorW, PostQuitMessage, RegisterClassW,
    PostMessageW, RegisterWindowMessageW, SetForegroundWindow, SetTimer, SetWindowLongPtrW,
    ShowWindow, SystemParametersInfoW, TrackPopupMenu, TranslateMessage,
    CREATESTRUCTW, GWLP_USERDATA, HTCAPTION, HTCLIENT, IDC_ARROW, MF_STRING, MSG,
    SPI_GETWORKAREA, SW_HIDE, SW_SHOWNOACTIVATE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WNDCLASSW, WM_CLOSE, WM_CONTEXTMENU,
    WM_DESTROY, WM_ENDSESSION, WM_ERASEBKGND, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEWHEEL, WM_NCCREATE, WM_NCHITTEST, WM_NCRBUTTONDOWN, WM_PAINT, WM_RBUTTONDOWN,
    WM_NULL, WM_RBUTTONUP, WM_TIMER,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{implement, Ref};

use crate::audio::AudioOut;

use super::render::{CONTROL_BAR_H, Renderer, StatusToggle, WINDOW_W};
use super::tray::Tray;
use super::{set_pump_hwnd, wake_broker, CMD_TX, WM_APP_BROKER, WM_APP_STATUS, WM_APP_TRAY};
use crate::broker::{Broker, Command, Flow, Phase, SharedState, Source, Status};
use crate::{log_debug, log_error, log_info, log_warn};

const TIMER_ID: usize = 1;
const TIMER_MS: u32 = 1000;
/// Non-zero command IDs for the two retained tray menu items.
const MENU_TOGGLE_WINDOW: usize = 1;
const MENU_QUIT: usize = 2;

struct UiState {
    hwnd: HWND,
    broker: Broker,
    cmd_rx: Receiver<Command>,
    status_rx: Receiver<Status>,
    shared: Arc<SharedState>,
    renderer: Renderer,
    tray: Tray,
    /// Owns the cpal stream; mirrors the phase onto WASAPI play/pause.
    audio_out: AudioOut,
    /// 1 Hz timer only exists while Playing — this is the "render loop sleeps
    /// when paused" guarantee, enforced here.
    timer_on: bool,
    last_phase: Phase,
    hidden: bool,
    taskbar_created: u32,
}

/// Win32 `GET_X_LPARAM`/`GET_Y_LPARAM`: unpack the signed 16-bit halves of
/// an LPARAM (windows-rs 0.62 dropped these helpers, so they live here).
/// Coordinates pack as sign-extended shorts — negative values are real
/// (multi-monitor layouts).
fn x_lparam(lp: LPARAM) -> i32 {
    (lp.0 & 0xFFFF) as u16 as i16 as i32
}

fn y_lparam(lp: LPARAM) -> i32 {
    ((lp.0 >> 16) & 0xFFFF) as u16 as i16 as i32
}

/// Win32 `GET_WHEEL_DELTA_WPARAM`: the high word of WM_MOUSEWHEEL's wParam.
fn wheel_delta(wp: WPARAM) -> i16 {
    (wp.0 >> 16) as u16 as i16
}

pub fn run(
    broker: Broker,
    cmd_rx: Receiver<Command>,
    status_rx: Receiver<Status>,
    audio_out: AudioOut,
) {
    let shared = broker.shared();

    unsafe {
        // Crisp text: 80 px means 80 physical pixels, never scaled.
        if let Err(e) =
            SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
        {
            log_warn!("UI", "per-monitor DPI v2 unavailable: {e}");
        }
    }

    let renderer = match Renderer::new(broker.view()) {
        Ok(r) => r,
        Err(e) => {
            log_error!("UI", "renderer init failed: {e} — falling back to headless");
            super::headless::run(broker, cmd_rx, status_rx, AudioOut::new(None));
            return;
        }
    };
    let (width, height) = renderer.size();

    let hinstance = match unsafe { GetModuleHandleW(None) } {
        Ok(h) => windows::Win32::Foundation::HINSTANCE(h.0),
        Err(e) => {
            log_error!("UI", "GetModuleHandleW failed: {e} — falling back to headless");
            drop(renderer);
            super::headless::run(broker, cmd_rx, status_rx, AudioOut::new(None));
            return;
        }
    };

    let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };

    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: hinstance,
        lpszClassName: w!("kyouko_echo"),
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
        ..Default::default()
    };
    if unsafe { RegisterClassW(&wc) } == 0 {
        log_error!("UI", "RegisterClassW failed — falling back to headless");
        drop(renderer);
        super::headless::run(broker, cmd_rx, status_rx, AudioOut::new(None));
        return;
    }

    let (pos_x, pos_y) = workarea_top_right(width, height);

    // OLE must be initialized on this thread before RegisterDragDrop.
    // S_OK *or* S_FALSE counts as available (S_FALSE = already initialized;
    // both are paired by OleUninitialize after the pump below).
    let ole = unsafe { OleInitialize(None) };
    if let Err(e) = &ole {
        log_warn!("UI", "OleInitialize failed: {e} — drag-and-drop unavailable");
    }
    // Kept alive past the pump: the window's registration holds a reference
    // that WM_DESTROY revokes; this local releases the last one afterwards.
    let drop_target: IDropTarget = DropTarget::new().into();

    // The state box rides through WM_NCCREATE via lpCreateParams and is
    // reclaimed after the pump exits. Tray icon is registered once the hwnd
    // exists (add() below).
    let state = Box::into_raw(Box::new(UiState {
        hwnd: HWND::default(),
        broker,
        cmd_rx,
        status_rx,
        shared,
        renderer,
        tray: Tray::new(),
        audio_out,
        timer_on: false,
        last_phase: Phase::Stopped,
        hidden: false,
        taskbar_created,
    }));

    let hwnd = match unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            w!("kyouko_echo"),
            w!("kyouko"),
            WS_POPUP,
            pos_x,
            pos_y,
            width,
            height,
            None,
            None,
            Some(hinstance),
            Some(state as *const UiState as *const c_void),
        )
    } {
        Ok(h) => h,
        Err(e) => {
            log_error!("UI", "CreateWindowExW failed: {e} — falling back to headless");
            let s = unsafe { *Box::from_raw(state) };
            super::headless::run(s.broker, s.cmd_rx, s.status_rx, s.audio_out);
            return;
        }
    };

    unsafe {
        (*state).hwnd = hwnd;
        set_pump_hwnd(hwnd.0 as isize);
        // Accept OLE drops (browser links arrive as text/URL formats, files
        // and folders as CF_HDROP) — see `DropTarget` below.
        if ole.is_ok() {
            if let Err(e) = RegisterDragDrop(hwnd, &drop_target) {
                log_warn!("UI", "RegisterDragDrop failed: {e}");
            }
        }
        (*state).tray.add(hwnd);
        // Content exists before the window is shown — no first-paint flash.
        let eq = (*state).shared.eq_gains();
        (*state).renderer.draw(hwnd, (*state).broker.view(), (*state).shared.phase(), &eq);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        log_info!(
            "UI",
            "echo window up: {width}x{height} @ ({pos_x},{pos_y}) — layered, topmost, non-activating"
        );

        // Drain anything that arrived during window creation, then sync.
        if !drain_commands(&mut *state) {
            drain_statuses(&mut *state);
            sync(&mut *state);
        }

        // The pump. GetMessageW parks in the kernel; there is no idle path.
        let mut msg = MSG::default();
        loop {
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 <= 0 {
                if r.0 == -1 {
                    log_error!("UI", "GetMessageW failed — exiting pump");
                }
                break; // 0 = WM_QUIT
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Reclaim state: Tray::drop removes the icon, Renderer::drop frees GDI.
        drop(Box::from_raw(state));
    }
    // Pair with OleInitialize (S_OK and S_FALSE both count). The drop
    // target's last reference releases right after this: the local above
    // goes out of scope when `run` returns.
    if ole.is_ok() {
        unsafe { OleUninitialize() };
    }
    log_info!("UI", "pump over — presentation released");
}

fn workarea_top_right(width: i32, _height: i32) -> (i32, i32) {
    let mut rc = RECT::default();
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut rc as *mut RECT as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    if ok.is_err() {
        return (96, 96);
    }
    (rc.right - width - 12, rc.top + 12)
}

/// Everything the pure core can't know about: timer lifecycle + repaint.
/// Called after every burst of broker activity.
fn sync(s: &mut UiState) {
    let phase = s.shared.phase();
    if phase != s.last_phase {
        // WASAPI follows the phase: running while Playing, stopped otherwise.
        s.audio_out.set_playing(phase == Phase::Playing);
        if phase == Phase::Playing && !s.timer_on {
            unsafe { SetTimer(Some(s.hwnd), TIMER_ID, TIMER_MS, None) };
            s.timer_on = true;
            log_info!("UI", "1 Hz panel timer ON (playing)");
        } else if phase != Phase::Playing && s.timer_on {
            unsafe { let _ = KillTimer(Some(s.hwnd), TIMER_ID); }
            s.timer_on = false;
            log_info!("UI", "1 Hz panel timer OFF — zero wake-ups until resumed");
        }
        s.tray.set_tooltip(&format!("kyouko — {}", phase_name(phase)));
        s.last_phase = phase;
    }
    if let Some(panel) = s.broker.take_refresh() {
        let eq = s.shared.eq_gains();
        s.renderer.draw(s.hwnd, &panel, phase, &eq);
    }
}

fn phase_name(p: Phase) -> &'static str {
    match p {
        Phase::Stopped => "stopped",
        Phase::Loading => "loading",
        Phase::Playing => "playing",
        Phase::Paused => "paused",
    }
}

/// true = a Quit was processed → caller must destroy the window.
fn drain_commands(s: &mut UiState) -> bool {
    loop {
        match s.cmd_rx.try_recv() {
            Ok(Command::ToggleWindow) => toggle_visible(s),
            Ok(cmd) => {
                if s.broker.handle_command(cmd) == Flow::Exit {
                    return true;
                }
            }
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => {
                log_warn!("UI", "command channel disconnected");
                return false;
            }
        }
    }
}

fn drain_statuses(s: &mut UiState) {
    loop {
        match s.status_rx.try_recv() {
            Ok(st) => s.broker.handle_status(st),
            Err(_) => return,
        }
    }
}

fn on_tray(s: &mut UiState, lp: LPARAM) {
    match lp.0 as u32 {
        // Left-click = play/pause, dispatched through the command channel so
        // the broker stays the single decision point (NIN_SELECT covers
        // NOTIFYICON_VERSION_4 shells; version 0 delivers WM_LBUTTONUP).
        WM_LBUTTONUP | NIN_SELECT => post_command(Command::TogglePause),
        WM_RBUTTONUP | WM_CONTEXTMENU => popup_menu(s),
        _ => {}
    }
}

fn toggle_visible(s: &mut UiState) {
    unsafe {
        let _ = ShowWindow(s.hwnd, if s.hidden { SW_SHOWNOACTIVATE } else { SW_HIDE });
    }
    s.hidden = !s.hidden;
    log_info!("UI", "echo window: {}", if s.hidden { "hidden" } else { "shown" });
}

/// The tray popup retains only Show/Hide Echo and Quit. Returning the command
/// ID avoids menu-message handlers, sticky reopen state, and mouse hooks.
fn popup_menu(s: &UiState) {
    unsafe {
        let mut pt = POINT::default();
        if let Err(e) = GetCursorPos(&mut pt) {
            log_warn!("UI", "tray menu cursor position unavailable: {e}");
            return;
        }
        let menu = match CreatePopupMenu() {
            Ok(menu) => menu,
            Err(e) => {
                log_warn!("UI", "tray menu creation failed: {e}");
                return;
            }
        };
        let label = if s.hidden { w!("Show Echo") } else { w!("Hide Echo") };
        if let Err(e) = AppendMenuW(menu, MF_STRING, MENU_TOGGLE_WINDOW, label)
            .and_then(|()| AppendMenuW(menu, MF_STRING, MENU_QUIT, w!("Quit")))
        {
            let _ = DestroyMenu(menu);
            log_warn!("UI", "tray menu population failed: {e}");
            return;
        }
        let _ = SetForegroundWindow(s.hwnd);
        log_debug!("UI", "tray menu: {} / Quit", if s.hidden { "Show Echo" } else { "Hide Echo" });
        let selected = TrackPopupMenu(
            menu, TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
            pt.x, pt.y, None, s.hwnd, None,
        ).0 as usize;
        let _ = DestroyMenu(menu);
        // Complete tray-menu dismissal before dispatching the selected action.
        let _ = PostMessageW(Some(s.hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        match selected {
            MENU_TOGGLE_WINDOW => post_command(Command::ToggleWindow),
            MENU_QUIT => {
                log_info!("UI", "tray quit chosen");
                post_command(Command::Quit);
            }
            _ => {}
        }
    }
}

/// Window and tray interactions share the terminal's command channel. The
/// broker stays the single decision point, with one wake per interaction.
fn post_command(cmd: Command) {
    if let Some(tx) = CMD_TX.get() {
        let _ = tx.try_send(cmd);
    }
    wake_broker();
}

// ---------------------------------------------------------------------------
// OLE drop target. Browsers hand link drags over as OLE string formats
// (`CF_UNICODETEXT`, `CF_TEXT`, the URL locator formats) — never as a file
// list, which is why the old `WM_DROPFILES`/HDROP path never fired for them.
// Files, folders and `.url` shortcuts still arrive as `CF_HDROP` and remain
// the fallback. Every payload funnels through the same command channel as
// the terminal, so the broker stays the single decision point.
//
// COM discipline: each `GetData` hit is followed by exactly one
// `ReleaseStgMedium`; `GlobalLock`/`GlobalUnlock` bracket every raw read of
// the payload; `DragFinish` is never called on OLE-owned memory (the shell
// owns those HGLOBALs).

/// The echo window's `IDropTarget`. One bit of state: the DragEnter verdict
/// that DragOver reuses, so no per-mouse-move data-object queries happen.
#[implement(IDropTarget)]
struct DropTarget {
    can_drop: AtomicBool,
}

impl DropTarget {
    fn new() -> Self {
        Self { can_drop: AtomicBool::new(false) }
    }

    /// Does the data object carry anything we can consume? Queried once per
    /// drag gesture (DragEnter), not once per DragOver.
    fn accepts(obj: &IDataObject) -> bool {
        [CF_UNICODETEXT, CF_TEXT, CF_HDROP].iter().any(|cf| Self::offers(obj, cf.0))
            || Self::offers(obj, registered_format(w!("UniformResourceLocatorW")))
            || Self::offers(obj, registered_format(w!("UniformResourceLocator")))
    }

    fn offers(obj: &IDataObject, cf: u16) -> bool {
        if cf == 0 {
            return false; // registered format unknown to this shell
        }
        unsafe { obj.QueryGetData(&string_format(cf)) }.is_ok()
    }
}

/// RegisterClipboardFormatW id for a named shell format (0 = failure).
fn registered_format(name: PCWSTR) -> u16 {
    unsafe { RegisterClipboardFormatW(name) as u16 }
}

/// A `FORMATETC` asking for one clipboard format's HGLOBAL rendering.
fn string_format(cf: u16) -> FORMATETC {
    FORMATETC {
        cfFormat: cf,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

impl IDropTarget_Impl for DropTarget_Impl {
    fn DragEnter(
        &self,
        pdataobj: Ref<'_, IDataObject>,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let accepted = match pdataobj.as_ref() {
            Some(obj) => DropTarget::accepts(obj),
            None => false,
        };
        log_info!("UI", "ole drag enter: accepted={accepted}");
        self.can_drop.store(accepted, Ordering::Relaxed);
        unsafe {
            *pdweffect = if accepted { DROPEFFECT_COPY } else { DROPEFFECT_NONE };
        }
        Ok(())
    }

    fn DragOver(
        &self,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        unsafe {
            *pdweffect = if self.can_drop.load(Ordering::Relaxed) {
                DROPEFFECT_COPY
            } else {
                DROPEFFECT_NONE
            };
        }
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        self.can_drop.store(false, Ordering::Relaxed);
        Ok(())
    }

    fn Drop(
        &self,
        pdataobj: Ref<'_, IDataObject>,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        unsafe { *pdweffect = DROPEFFECT_COPY };
        let Some(obj) = pdataobj.as_ref() else {
            return Ok(());
        };
        log_debug!("UI", "ole drop enter");

        // Priority 1: string formats. Browsers put the link URL in
        // CF_UNICODETEXT; the URL locator formats cover older shell
        // sources. A non-URL text payload falls through to the file list.
        for (cf, wide) in [
            (CF_UNICODETEXT.0, true),
            (CF_TEXT.0, false),
            (registered_format(w!("UniformResourceLocatorW")), true),
            (registered_format(w!("UniformResourceLocator")), false),
        ] {
            if cf == 0 {
                continue;
            }
            if let Some(url) = hglobal_string(obj, cf, wide).and_then(|t| url_from_drop_text(&t))
            {
                log_info!("UI", "drop: link {url}");
                post_command(Command::Load { source: Source::from_raw(&url), paused: false });
                return Ok(());
            }
        }

        // Priority 2: the shell file list (local media, folders, .url
        // shortcuts) — classified by the broker exactly as before.
        if let Ok(mut medium) = unsafe { obj.GetData(&string_format(CF_HDROP.0)) } {
            let paths = unsafe { hdrop_paths(&medium) };
            unsafe { ReleaseStgMedium(&mut medium) };
            if paths.is_empty() {
                log_warn!("UI", "drop: no usable payload — ignoring");
            } else {
                if paths.len() > 1 {
                    log_info!("UI", "drop: {} paths", paths.len());
                }
                log_info!("UI", "drop: {}", paths[0]);
                post_command(Command::LoadDropped(paths));
            }
        } else {
            log_warn!("UI", "drop: nothing kyouko accepts — ignoring");
        }
        Ok(())
    }
}

/// Pull a NUL-terminated string out of the first `TYMED_HGLOBAL` rendering
/// of clipboard format `cf` (wide = UTF-16, otherwise ANSI). The medium is
/// always released; `None` when the format is absent or carries no text.
fn hglobal_string(obj: &IDataObject, cf: u16, wide: bool) -> Option<String> {
    let Ok(mut medium) = (unsafe { obj.GetData(&string_format(cf)) }) else {
        return None;
    };
    let text = unsafe { medium_string(&medium, wide) };
    unsafe { ReleaseStgMedium(&mut medium) };
    text
}

/// Read the HGLOBAL payload (if the medium carries one) as NUL-terminated
/// text. GlobalSize bounds the scan; a missing terminator just takes the
/// whole allocation.
///
/// # Safety
/// `medium` must be a valid STGMEDIUM obtained from `GetData` (the caller
/// releases it afterwards).
unsafe fn medium_string(medium: &STGMEDIUM, wide: bool) -> Option<String> {
    if medium.tymed != TYMED_HGLOBAL.0 as u32 {
        return None;
    }
    // SAFETY: tymed == TYMED_HGLOBAL above makes hGlobal the active member.
    let h = unsafe { medium.u.hGlobal };
    if h.0.is_null() {
        return None;
    }
    let ptr = unsafe { GlobalLock(h) };
    if ptr.is_null() {
        return None;
    }
    let text = unsafe {
        let bytes = GlobalSize(h);
        if wide {
            let units = std::slice::from_raw_parts(ptr as *const u16, bytes / 2);
            let len = units.iter().position(|&c| c == 0).unwrap_or(units.len());
            String::from_utf16_lossy(&units[..len])
        } else {
            let raw = std::slice::from_raw_parts(ptr as *const u8, bytes);
            let len = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            String::from_utf8_lossy(&raw[..len]).into_owned()
        }
    };
    unsafe { let _ = GlobalUnlock(h); }
    if text.is_empty() { None } else { Some(text) }
}

/// Extract every path from a CF_HDROP medium. The shell's DROPFILES handle
/// is fed to `DragQueryFileW` directly — the same extraction the old
/// WM_DROPFILES path used, fed from the OLE medium instead of the message.
/// The medium itself is released by the caller (`ReleaseStgMedium`, never
/// `DragFinish`: the shell owns this HGLOBAL).
///
/// # Safety
/// `medium` must be a valid STGMEDIUM obtained from `GetData` (the caller
/// releases it afterwards).
unsafe fn hdrop_paths(medium: &STGMEDIUM) -> Vec<String> {
    let mut paths = Vec::new();
    if medium.tymed != TYMED_HGLOBAL.0 as u32 {
        return paths;
    }
    // SAFETY: tymed == TYMED_HGLOBAL above makes hGlobal the active member.
    let h = unsafe { medium.u.hGlobal };
    if h.0.is_null() {
        return paths;
    }
    let hdrop = HDROP(h.0);
    let count = unsafe { DragQueryFileW(hdrop, u32::MAX, None) };
    for index in 0..count {
        let path_len = unsafe { DragQueryFileW(hdrop, index, None) };
        if path_len == 0 {
            continue;
        }
        // The API null-terminates within the given buffer; the length query
        // excludes that terminator, so hand it the full buffer and slice the
        // terminator off afterwards.
        let mut buf = vec![0u16; path_len as usize + 1];
        unsafe { DragQueryFileW(hdrop, index, Some(&mut buf)) };
        let path = String::from_utf16_lossy(&buf).trim_end_matches('\0').to_string();
        if !path.is_empty() {
            paths.push(path);
        }
    }
    paths
}

/// Pull a http(s) URL out of dropped text. Browsers hand over the bare URL,
/// but some wrap it as "title\nurl" or quote it — scan whitespace-separated
/// tokens and keep the LAST http(s) one (title first, link last; a bare URL
/// has a single token). Anything not http(s) is rejected: local paths as
/// text are not a drop kyouko takes.
fn url_from_drop_text(text: &str) -> Option<String> {
    let mut found: Option<&str> = None;
    for token in text.split_whitespace() {
        let token = token.trim_matches(|c| c == '"' || c == '\'');
        if token.starts_with("http://") || token.starts_with("https://") {
            found = Some(token);
        }
    }
    found.map(str::to_string)
}

/// DestroyWindow from event context; WM_DESTROY does the teardown.
fn destroy(s: &mut UiState) {
    unsafe {
        let _ = DestroyWindow(s.hwnd);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let cs = lp.0 as *const CREATESTRUCTW;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
        }
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut UiState;
        if state.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let s = &mut *state;
        match msg {
            WM_APP_BROKER => {
                if drain_commands(s) {
                    destroy(s); // Quit processed: WM_DESTROY → PostQuitMessage
                } else {
                    sync(s);
                }
                LRESULT(0)
            }
            WM_APP_STATUS => {
                drain_statuses(s);
                sync(s);
                LRESULT(0)
            }
            WM_APP_TRAY => {
                on_tray(s, lp);
                LRESULT(0)
            }
            m if m == s.taskbar_created => {
                // Explorer restarted: re-register the tray icon.
                s.tray.add(s.hwnd);
                LRESULT(0)
            }
            WM_TIMER if wp.0 as usize == TIMER_ID => {
                // Elapsed-time tick; only scheduled while Playing.
                s.broker.tick();
                sync(s);
                LRESULT(0)
            }
            WM_NCHITTEST => {
                // Interactive zones = client area (clicks reach
                // WM_LBUTTONDOWN): the bottom control strip, the Braille EQ
                // strip (wheel tunes it; left-click there is a no-op), and
                // the eq/repeat status chunks. Everywhere else = caption, so
                // Windows natively drag-moves the window with left-click and
                // no custom drag state exists anywhere.
                let mut rc = RECT::default();
                let _ = GetWindowRect(hwnd, &mut rc);
                // WM_NCHITTEST's lParam is SCREEN coordinates — convert to
                // client space before the strip/status geometry tests.
                let x = x_lparam(lp) - rc.left;
                let y = y_lparam(lp) - rc.top;
                if y >= rc.bottom - rc.top - CONTROL_BAR_H
                    || s.renderer.eq_band_at(x, y).is_some()
                    || s.renderer.status_toggle_at(x, y).is_some()
                {
                    LRESULT(HTCLIENT as isize)
                } else {
                    LRESULT(HTCAPTION as isize)
                }
            }
            WM_LBUTTONDOWN => {
                // Client left-clicks arrive only over the HTCLIENT zones:
                // the control strip (four equal-width columns map to
                // prev / -5s / +5s / next) and the eq/repeat status chunks.
                let x = x_lparam(lp);
                let y = y_lparam(lp);
                let height = s.renderer.size().1;
                if y >= height - CONTROL_BAR_H {
                    let col = (x.clamp(0, WINDOW_W - 1) * 4 / WINDOW_W) as usize;
                    log_debug!("UI", "control bar click col {col}");
                    match col {
                        0 => post_command(Command::PrevTrack),
                        1 => post_command(Command::SeekRelative(-5.0)),
                        2 => post_command(Command::SeekRelative(5.0)),
                        _ => post_command(Command::NextTrack),
                    }
                } else if let Some(toggle) = s.renderer.status_toggle_at(x, y) {
                    match toggle {
                        StatusToggle::Eq => {
                            log_debug!("UI", "status click: toggle eq");
                            post_command(Command::ToggleEq);
                        }
                        StatusToggle::Repeat => {
                            log_debug!("UI", "status click: toggle repeat");
                            post_command(Command::ToggleLoop);
                        }
                    }
                }
                // Left-clicks on the Braille EQ strip do nothing (tuning is
                // wheel-driven); the window never enters a drag from an
                // HTCLIENT zone.
                LRESULT(0)
            }
            WM_RBUTTONDOWN | WM_NCRBUTTONDOWN => {
                // Right-click anywhere pauses/resumes. Both message flavors
                // arrive: the bar is HTCLIENT (WM_RBUTTONDOWN), the rest of
                // the window is HTCAPTION (WM_NCRBUTTONDOWN).
                log_debug!("UI", "right-click: toggle pause");
                post_command(Command::TogglePause);
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                // WM_MOUSEWHEEL's lParam is SCREEN coordinates (unlike the
                // mouse-button messages) — convert to client space against
                // the window rect before geometry tests.
                let mut rc = RECT::default();
                let _ = GetWindowRect(hwnd, &mut rc);
                let x = x_lparam(lp) - rc.left;
                let y = y_lparam(lp) - rc.top;
                let delta = wheel_delta(wp);
                if let Some(band) = s.renderer.eq_band_at(x, y) {
                    // Over the Braille equalizer: ±1 dB on the hovered
                    // column, clamped by SharedState at ±12 dB. The broker
                    // applies, refreshes the panel and persists state.cfg.
                    let old = s.shared.eq_gain_db(band);
                    let new = old + if delta >= 0 { 1.0 } else { -1.0 };
                    log_info!(
                        "UI",
                        "wheel eq[{}Hz]: {old:+.0} -> {new:+.0} dB",
                        crate::broker::EQ_BAND_HZ[band]
                    );
                    post_command(Command::EqGain { band: Some(band), gain_db: new });
                } else {
                    // Anywhere else on the panel: ±10% master volume.
                    // Routing here relies on
                    // the system "scroll inactive windows" setting (on by
                    // default) since a WS_EX_NOACTIVATE window never holds
                    // focus.
                    let old = s.shared.volume();
                    let new = (if delta >= 0 { old + 0.1 } else { old - 0.1 }).clamp(0.0, 1.0);
                    log_info!(
                        "UI",
                        "wheel volume: {}% -> {}%",
                        (old * 100.0).round() as i32,
                        (new * 100.0).round() as i32
                    );
                    post_command(Command::SetVolume(new));
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                // Closing the echo hides it; playback continues. Quit is
                // available through the tray menu, terminal, and console Ctrl+C.
                let _ = ShowWindow(s.hwnd, SW_HIDE);
                s.hidden = true;
                LRESULT(0)
            }
            WM_ENDSESSION => {
                if wp.0 != 0 {
                    s.broker.handle_command(Command::Quit);
                }
                destroy(s);
                LRESULT(0)
            }
            WM_DESTROY => {
                // Release the window's reference to the drop target before
                // the OLE apartment goes away (OleUninitialize runs after
                // the pump).
                let _ = RevokeDragDrop(hwnd);
                if s.timer_on {
                    let _ = KillTimer(Some(s.hwnd), TIMER_ID);
                    s.timer_on = false;
                }
                s.tray.remove();
                PostQuitMessage(0);
                LRESULT(0)
            }
            WM_PAINT => {
                let _ = ValidateRect(Some(hwnd), None);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1), // layered surface paints itself
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}
