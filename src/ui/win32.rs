//! The Windows presentation pump: 200 px layered window, tray integration,
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
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP, NIN_SELECT};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CallNextHookEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
    DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, GetWindowLongPtrW, GetWindowRect,
    KillTimer, PostMessageW,
    LoadCursorW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow,
    SetMenuInfo, SetTimer, SetWindowLongPtrW, SetWindowsHookExW, ShowWindow,
    SystemParametersInfoW, TrackPopupMenu, TranslateMessage, UnhookWindowsHookEx,
    CREATESTRUCTW, GWLP_USERDATA, HTCAPTION,
    HTCLIENT, IDC_ARROW, MF_MENUBREAK, MF_SEPARATOR, MF_STRING,
    MENUINFO,
    MENUINFO_MASK, MENUINFO_STYLE, MSG, MIM_STYLE, MNS_NOTIFYBYPOS, SPI_GETWORKAREA,
    SW_HIDE, SW_SHOWNOACTIVATE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, TPM_NONOTIFY,
    TPM_RIGHTBUTTON, WH_GETMESSAGE, WNDCLASSW, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY,
    WM_ENDSESSION, WM_ERASEBKGND, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MENUCOMMAND,
    WM_MOUSEWHEEL, WM_NCCREATE, WM_NCHITTEST, WM_NCRBUTTONDOWN, WM_PAINT, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_TIMER,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{implement, Ref};

use crate::audio::AudioOut;

use super::render::{CONTROL_BAR_H, Renderer, WINDOW_W};
use super::tray::Tray;
use super::{set_pump_hwnd, wake_broker, CMD_TX, WM_APP_BROKER, WM_APP_STATUS, WM_APP_TRAY};
use crate::broker::{Broker, Command, Flow, Phase, SharedState, Source, Status};
use crate::{log_debug, log_error, log_info, log_warn};

const TIMER_ID: usize = 1;
const TIMER_MS: u32 = 1000;
/// Tray menu item POSITIONS. The menu is created with MNS_NOTIFYBYPOS, so
/// selections arrive as WM_MENUCOMMAND with the position in wParam — that is
/// what lets the vol row tell a left-click (+10%) from a right-click (-10%).
/// Positions include separator rows: vol, sep, loop, eq, equalizer, seek,
/// sep, window, sep, quit.
const MENU_POS_VOL: usize = 0;
const MENU_POS_LOOP: usize = 2;
const MENU_POS_EQ: usize = 3;
const MENU_POS_EQ_OPEN: usize = 4;
const MENU_POS_SEEK: usize = 5;
const MENU_POS_WINDOW: usize = 7;
const MENU_POS_QUIT: usize = 9;

/// Which popup a WM_MENUCOMMAND belongs to (positions are per-menu).
const KIND_MAIN: usize = 0;
const KIND_EQ_BANDS: usize = 1;
const KIND_SEEK: usize = 2;
const EQ_BAND_STEP_DB: f32 = 1.0;

/// Strip position -> band index for the 5x2 grid (column-major: 31/1k,
/// 62/2k, 125/4k, 250/8k, 500/16k).
const EQ_STRIP_ORDER: [usize; crate::broker::EQ_BANDS] = [0, 5, 1, 6, 2, 7, 3, 8, 4, 9];

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
    /// Where the current tray-menu session was opened; sticky re-opens
    /// rebuild the menu at exactly this point.
    menu_pt: POINT,
    /// Which popup the WM_MENUCOMMAND dispatch applies to (positions are
    /// per-menu, so the handler must know which menu was tracked).
    menu_kind: usize,
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
        // Crisp text: 200 px means 200 physical pixels, never scaled.
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
        menu_pt: POINT::default(),
        menu_kind: KIND_MAIN,
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
        (*state).renderer.draw(hwnd, (*state).broker.view(), (*state).shared.phase());
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
        s.renderer.draw(s.hwnd, &panel, phase);
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
            // Presentation-side command: never reaches the broker.
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
        WM_RBUTTONUP | WM_CONTEXTMENU => {
            let mut pt = POINT::default();
            unsafe {
                let _ = GetCursorPos(&mut pt);
            }
            s.menu_pt = pt;
            popup_menu(s, pt);
        }
        _ => {}
    }
}

fn toggle_visible(s: &mut UiState) {
    unsafe {
        if s.hidden {
            let _ = ShowWindow(s.hwnd, SW_SHOWNOACTIVATE);
            s.hidden = false;
        } else {
            let _ = ShowWindow(s.hwnd, SW_HIDE);
            s.hidden = true;
        }
    }
}

/// Set by `menu_message_hook` while the tray menu is open: true when the
/// mouse traffic flowing through the menu loop was right-button traffic.
/// A thread-scoped WH_GETMESSAGE hook is the only deterministic way to know
/// which button selected an item — menus commit right-click selection on
/// button-UP, so by the time WM_MENUCOMMAND arrives the button is released.
static MENU_RIGHT_CLICK: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static HOOK_FIRED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);


/// Thread hook, installed ONLY for the lifetime of the popup menu (a thread
/// already blocked in the modal menu loop — zero cost to the audio/CPU
/// guarantees). It records the button of the last mouse message the menu
/// loop pulls from the queue, before that message is dispatched.
unsafe extern "system" fn menu_message_hook(
    code: i32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    if code >= 0 {
        if !HOOK_FIRED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            log_debug!("UI", "menu hook first fire (message pump alive)");
        }
        let msg = lp.0 as *const MSG;
        if !msg.is_null() {
            // SAFETY: WH_GETMESSAGE hands us a valid MSG pointer for every
            // HC_ACTION callback; null-guarded regardless.
            let message = unsafe { (*msg).message };
            match message {
                WM_RBUTTONDOWN | WM_RBUTTONUP => {
                    MENU_RIGHT_CLICK.store(true, std::sync::atomic::Ordering::Relaxed)
                }
                WM_LBUTTONDOWN | WM_LBUTTONUP => {
                    MENU_RIGHT_CLICK.store(false, std::sync::atomic::Ordering::Relaxed)
                }
                _ => {}
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wp, lp) }
}

/// Tray menu selections travel through the same command channel as the
/// terminal — the menu is a producer, the broker stays the single decision
/// point (one posted wake per selection; no polling anywhere).
fn post_command(cmd: Command) {
    if let Some(tx) = CMD_TX.get() {
        let _ = tx.try_send(cmd);
    }
    wake_broker();
}

/// Ask the pump to re-open a menu at the recorded position — `KIND_MAIN` or
/// `KIND_EQ_BANDS`. Posted AFTER the selection's command, so queue order
/// guarantees the reopened menu renders fresh labels.
fn reopen_menu(s: &mut UiState, kind: usize) {
    unsafe {
        let _ = PostMessageW(Some(s.hwnd), WM_APP_MENU_REOPEN, WPARAM(kind as _), LPARAM(0));
    }
}

/// Re-open the popup at the recorded position. Posted — not called — so the
/// pump processes the selection's command FIRST and the reopened menu always
/// renders the fresh state (e.g. the new `vol N`).
const WM_APP_MENU_REOPEN: u32 = 0x8004; // WM_APP + 4

/// Record of where the current menu session was opened; sticky re-opens
/// rebuild the menu at exactly this point so the vol row stays under the
/// cursor between clicks.
///
/// One modal pass: build the menu from live state, run `TrackPopupMenu` at
/// `pt`, tear it down. Sticky selections (vol/loop/eq) re-open via
/// `WM_APP_MENU_REOPEN`; dismissal (ESC / outside click) and closing
/// selections (window/quit) simply are not re-opened.
fn popup_menu(s: &mut UiState, pt: POINT) {
    let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
        return;
    };
    // Labels built fresh from live state each pass. (Play/Pause is NOT a
    // menu item — left-click on the icon toggles it.)
    let vol_label: Vec<u16> = format!("vol {}", (s.shared.volume() * 100.0).round() as i32)
        .encode_utf16()
        .chain([0])
        .collect();
    let loop_label = if s.shared.loop_enabled() { w!("Loop: ON") } else { w!("Loop: OFF") };
    let eq_label = if s.shared.eq_enabled() { w!("EQ: ON") } else { w!("EQ: OFF") };
    let window_label = if s.hidden { w!("Show Echo") } else { w!("Hide Echo") };
    unsafe {
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_POS_VOL,
            windows::core::PCWSTR(vol_label.as_ptr()),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, MENU_POS_LOOP, loop_label);
        let _ = AppendMenuW(menu, MF_STRING, MENU_POS_EQ, eq_label);
        let _ = AppendMenuW(menu, MF_STRING, MENU_POS_EQ_OPEN, w!("Equalizer"));
        let _ = AppendMenuW(menu, MF_STRING, MENU_POS_SEEK, w!("Seek"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, MENU_POS_WINDOW, window_label);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, MENU_POS_QUIT, w!("quit"));
        // Selections arrive as WM_MENUCOMMAND (position in wParam) — that is
        // what lets the vol row tell a left-click (+10%) from a right (-10%).
        let info = MENUINFO {
            cbSize: std::mem::size_of::<MENUINFO>() as u32,
            fMask: MENUINFO_MASK(MIM_STYLE.0),
            dwStyle: MENUINFO_STYLE(MNS_NOTIFYBYPOS.0),
            ..Default::default()
        };
        let _ = SetMenuInfo(menu, &info);
        // KB135788: the menu's owner must be foreground or it won't dismiss.
        let _ = SetForegroundWindow(s.hwnd);
        // TPM_RIGHTBUTTON is required so the vol row can be right-clicked.
        MENU_RIGHT_CLICK.store(false, std::sync::atomic::Ordering::Relaxed);
        s.menu_kind = KIND_MAIN;
        let hook = SetWindowsHookExW(
            WH_GETMESSAGE,
            Some(menu_message_hook),
            None,
            GetCurrentThreadId(),
        );
        if let Err(e) = &hook {
            log_error!("UI", "menu hook install failed: {e}");
        }
        let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_NONOTIFY, pt.x, pt.y, None, s.hwnd, None);
        if let Ok(h) = hook {
            let _ = UnhookWindowsHookEx(h);
        }
        let _ = DestroyMenu(menu);
    }
}

/// The 10-band strip as a 5 x 2 grid: menus flow top-to-bottom, so
/// `EQ_STRIP_ORDER` emits column-major pairs (31/1k, 62/2k, 125/4k, 250/8k,
/// 500/16k) and MF_MENUBREAK opens a new column before each odd top row.
/// Labels are the bare abbreviated frequency — deliberately narrow so the
/// grid stays ~250 logical px; live gains live in the echo panel and the
/// debug log. Tracked directly at `pt` — visually it replaces the main
/// menu, keeping the sticky re-open mechanics identical to the volume row.
fn eq_popup(s: &mut UiState, pt: POINT) {
    let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
        return;
    };
    unsafe {
        for (pos, &band) in EQ_STRIP_ORDER.iter().enumerate() {
            let hz = crate::broker::EQ_BAND_HZ[band];
            let text: Vec<u16> = if hz >= 1000 {
                format!("{}k", hz / 1000)
            } else {
                format!("{hz}")
            }
            .encode_utf16()
            .chain([0])
            .collect();
            // A new column starts before 62, 125, 250, 500 — each column
            // then stacks exactly two rows.
            let flags =
                if pos > 0 && pos % 2 == 0 { MF_STRING | MF_MENUBREAK } else { MF_STRING };
            let _ = AppendMenuW(menu, flags, pos, windows::core::PCWSTR(text.as_ptr()));
        }
        let info = MENUINFO {
            cbSize: std::mem::size_of::<MENUINFO>() as u32,
            fMask: MENUINFO_MASK(MIM_STYLE.0),
            dwStyle: MENUINFO_STYLE(MNS_NOTIFYBYPOS.0),
            ..Default::default()
        };
        let _ = SetMenuInfo(menu, &info);
        let _ = SetForegroundWindow(s.hwnd);
        MENU_RIGHT_CLICK.store(false, std::sync::atomic::Ordering::Relaxed);
        s.menu_kind = KIND_EQ_BANDS;
        let hook = SetWindowsHookExW(
            WH_GETMESSAGE,
            Some(menu_message_hook),
            None,
            GetCurrentThreadId(),
        );
        if let Err(e) = &hook {
            log_error!("UI", "menu hook install failed: {e}");
        }
        let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_NONOTIFY, pt.x, pt.y, None, s.hwnd, None);
        if let Ok(h) = hook {
            let _ = UnhookWindowsHookEx(h);
        }
        let _ = DestroyMenu(menu);
    }
}

/// The seek strip: four horizontal columns — `<<` previous track, `<` -5s,
/// `>` +5s, `>>` next track. Labels are static (nothing here changes state
/// visibly), but the strip is STILL sticky via WM_APP_MENU_REOPEN so several
/// clicks can be chained without re-opening the menu. All dispatches are
/// plain left-clicks routed through the command channel.
fn seek_popup(s: &mut UiState, pt: POINT) {
    let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
        return;
    };
    unsafe {
        for (pos, label) in [
            (0usize, w!("<<")),
            (1usize, w!("<")),
            (2usize, w!(">")),
            (3usize, w!(">>")),
        ] {
            let flags = if pos == 0 { MF_STRING } else { MF_STRING | MF_MENUBREAK };
            let _ = AppendMenuW(menu, flags, pos, label);
        }
        let info = MENUINFO {
            cbSize: std::mem::size_of::<MENUINFO>() as u32,
            fMask: MENUINFO_MASK(MIM_STYLE.0),
            dwStyle: MENUINFO_STYLE(MNS_NOTIFYBYPOS.0),
            ..Default::default()
        };
        let _ = SetMenuInfo(menu, &info);
        let _ = SetForegroundWindow(s.hwnd);
        MENU_RIGHT_CLICK.store(false, std::sync::atomic::Ordering::Relaxed);
        s.menu_kind = KIND_SEEK;
        let hook = SetWindowsHookExW(
            WH_GETMESSAGE,
            Some(menu_message_hook),
            None,
            GetCurrentThreadId(),
        );
        if let Err(e) = &hook {
            log_error!("UI", "menu hook install failed: {e}");
        }
        let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_NONOTIFY, pt.x, pt.y, None, s.hwnd, None);
        if let Ok(h) = hook {
            let _ = UnhookWindowsHookEx(h);
        }
        let _ = DestroyMenu(menu);
    }
}

/// MNS_NOTIFYBYPOS dispatch: wParam = item position. NOTE: on current shells
/// this message arrives AFTER `TrackPopupMenu` has already returned and
/// closed the menu — which is why sticky items re-open via
/// `WM_APP_MENU_REOPEN`: posted after the command, so the pump processes the
/// volume change first and the reopened menu renders the fresh label.
fn on_menu_command(s: &mut UiState, wp: WPARAM) {
    use std::sync::atomic::Ordering;
    log_debug!(
        "UI",
        "menu command: kind={} pos={} right={}",
        s.menu_kind,
        wp.0,
        MENU_RIGHT_CLICK.load(Ordering::Relaxed)
    );
    let right_clicked = MENU_RIGHT_CLICK.load(Ordering::Relaxed);
    match s.menu_kind {
        // Seek strip: `<<` prev track, `<` -5s, `>` +5s, `>>` next track.
        // Every dispatch is sticky — reopen_menu keeps the strip on screen
        // (it was missing here, which is why the strip closed after one
        // click).
        KIND_SEEK => match wp.0 as usize {
            0 => {
                post_command(Command::PrevTrack);
                reopen_menu(s, KIND_SEEK);
            }
            1 => {
                post_command(Command::SeekRelative(-5.0));
                reopen_menu(s, KIND_SEEK);
            }
            2 => {
                post_command(Command::SeekRelative(5.0));
                reopen_menu(s, KIND_SEEK);
            }
            3 => {
                post_command(Command::NextTrack);
                reopen_menu(s, KIND_SEEK);
            }
            _ => {}
        },
        KIND_EQ_BANDS => {
            // Position maps through EQ_STRIP_ORDER to the band index.
            // Left-click +1 dB, right-click -1 dB, clamped by SharedState.
            // Sticky: reopen the strip with fresh labels.
            let pos = wp.0 as usize;
            if pos >= crate::broker::EQ_BANDS {
                return;
            }
            let band = EQ_STRIP_ORDER[pos];
            let delta = if right_clicked { -EQ_BAND_STEP_DB } else { EQ_BAND_STEP_DB };
            let old = s.shared.eq_gain_db(band);
            let new = old + delta;
            log_info!(
                "UI",
                "tray eq[{}Hz]: {old:+.1} -> {new:+.1} dB ({})",
                crate::broker::EQ_BAND_HZ[band],
                if right_clicked { "right" } else { "left" }
            );
            post_command(Command::EqGain { band: Some(band), gain_db: new });
            reopen_menu(s, KIND_EQ_BANDS);
        }
        _ => match wp.0 as usize {
            MENU_POS_SEEK => {
                // Swap the main menu for the seek strip, in place.
                reopen_menu(s, KIND_SEEK);
            }
            MENU_POS_VOL => {
                let old = s.shared.volume();
                let new = (old + if right_clicked { -0.1 } else { 0.1 }).clamp(0.0, 1.0);
                log_info!(
                    "UI",
                    "tray volume: {}% -> {}% ({})",
                    (old * 100.0).round() as i32,
                    (new * 100.0).round() as i32,
                    if right_clicked { "right" } else { "left" }
                );
                post_command(Command::SetVolume(new));
                reopen_menu(s, KIND_MAIN);
            }
            MENU_POS_LOOP => {
                post_command(Command::ToggleLoop);
                reopen_menu(s, KIND_MAIN);
            }
            MENU_POS_EQ => {
                post_command(Command::ToggleEq);
                reopen_menu(s, KIND_MAIN);
            }
            MENU_POS_EQ_OPEN => {
                // Swap the main menu for the band strip, in place.
                reopen_menu(s, KIND_EQ_BANDS);
            }
            MENU_POS_WINDOW => post_command(Command::ToggleWindow),
            MENU_POS_QUIT => {
                log_info!("UI", "tray quit chosen");
                post_command(Command::Quit);
            }
            _ => {} // separator rows carry no command
        },
    }
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
            WM_MENUCOMMAND => {
                on_menu_command(s, wp);
                LRESULT(0)
            }
            WM_APP_MENU_REOPEN => {
                // wParam = KIND_MAIN | KIND_EQ_BANDS | KIND_SEEK (posted by
                // sticky rows).
                match wp.0 as usize {
                    KIND_EQ_BANDS => eq_popup(s, s.menu_pt),
                    KIND_SEEK => seek_popup(s, s.menu_pt),
                    _ => popup_menu(s, s.menu_pt),
                }
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
                // Bottom control strip = client area (button clicks reach
                // WM_LBUTTONDOWN); everywhere else = caption, so Windows
                // natively drag-moves the window with left-click and no
                // custom drag state exists anywhere.
                let y = y_lparam(lp);
                let mut rc = RECT::default();
                let _ = GetWindowRect(hwnd, &mut rc);
                if y >= rc.bottom - CONTROL_BAR_H {
                    LRESULT(HTCLIENT as isize)
                } else {
                    LRESULT(HTCAPTION as isize)
                }
            }
            WM_LBUTTONDOWN => {
                // Only the control strip is HTCLIENT, so a client left-click
                // is a bar click: four equal-width columns map to
                // prev / -5s / +5s / next, matching the drawn glyphs.
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
                }
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
                // Wheel over the panel: ±10% volume (up = louder), same step
                // as the tray's vol row. Routing here relies on the system
                // "scroll inactive windows" setting (on by default) since a
                // WS_EX_NOACTIVATE window never holds focus.
                let delta = wheel_delta(wp);
                let old = s.shared.volume();
                let new = (if delta >= 0 { old + 0.1 } else { old - 0.1 }).clamp(0.0, 1.0);
                log_info!(
                    "UI",
                    "wheel volume: {}% -> {}%",
                    (old * 100.0).round() as i32,
                    (new * 100.0).round() as i32
                );
                post_command(Command::SetVolume(new));
                LRESULT(0)
            }
            WM_CLOSE => {
                // Closing the echo hides it; playback continues. Quit lives in
                // the tray menu and the terminal.
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
