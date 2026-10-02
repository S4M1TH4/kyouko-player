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
use std::sync::Arc;

use crossbeam_channel::{Receiver, TryRecvError};

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ValidateRect;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Shell::NIN_SELECT;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CallNextHookEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
    DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, GetWindowLongPtrW, KillTimer,
    PostMessageW,
    LoadCursorW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow,
    SetMenuInfo, SetTimer, SetWindowLongPtrW, SetWindowsHookExW, ShowWindow,
    SystemParametersInfoW, TrackPopupMenu, TranslateMessage, UnhookWindowsHookEx,
    CREATESTRUCTW, GWLP_USERDATA, HTCAPTION, IDC_ARROW, MF_MENUBREAK, MF_SEPARATOR, MF_STRING,
    MENUINFO,
    MENUINFO_MASK, MENUINFO_STYLE, MSG, MIM_STYLE, MNS_NOTIFYBYPOS, SPI_GETWORKAREA,
    SW_HIDE, SW_SHOWNOACTIVATE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, TPM_NONOTIFY,
    TPM_RIGHTBUTTON, WH_GETMESSAGE, WNDCLASSW, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY,
    WM_ENDSESSION, WM_ERASEBKGND, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MENUCOMMAND,
    WM_NCCREATE, WM_NCHITTEST, WM_PAINT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_TIMER,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::Win32::System::Threading::GetCurrentThreadId;

use crate::audio::AudioOut;

use super::render::Renderer;
use super::tray::Tray;
use super::{set_pump_hwnd, wake_broker, CMD_TX, WM_APP_BROKER, WM_APP_STATUS, WM_APP_TRAY};
use crate::broker::{Broker, Command, Flow, Phase, SharedState, Status};
use crate::{log_debug, log_error, log_info, log_warn};

const TIMER_ID: usize = 1;
const TIMER_MS: u32 = 1000;
/// Tray menu item POSITIONS. The menu is created with MNS_NOTIFYBYPOS, so
/// selections arrive as WM_MENUCOMMAND with the position in wParam — that is
/// what lets the vol row tell a left-click (+10%) from a right-click (-10%).
/// Positions include separator rows: vol, sep, loop, eq, equalizer, sep,
/// window, sep, quit.
const MENU_POS_VOL: usize = 0;
const MENU_POS_LOOP: usize = 2;
const MENU_POS_EQ: usize = 3;
const MENU_POS_EQ_OPEN: usize = 4;
const MENU_POS_WINDOW: usize = 6;
const MENU_POS_QUIT: usize = 8;

/// Which popup a WM_MENUCOMMAND belongs to (positions are per-menu).
const KIND_MAIN: usize = 0;
const KIND_EQ_BANDS: usize = 1;
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
        let _ =
            AppendMenuW(menu, MF_STRING, MENU_POS_VOL, windows::core::PCWSTR(vol_label.as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, MENU_POS_LOOP, loop_label);
        let _ = AppendMenuW(menu, MF_STRING, MENU_POS_EQ, eq_label);
        let _ = AppendMenuW(menu, MF_STRING, MENU_POS_EQ_OPEN, w!("Equalizer"));
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

/// MNS_NOTIFYBYPOS dispatch: wParam = item position. NOTE: on current shells
/// this message arrives AFTER `TrackPopupMenu` has already returned and
/// closed the menu — which is why sticky items re-open via
/// `WM_APP_MENU_REOPEN`: posted after the command, so the pump processes the
/// volume change first and the reopened menu renders the fresh label.
fn on_menu_command(s: &mut UiState, wp: WPARAM) {
    use std::sync::atomic::Ordering;
    let right_clicked = MENU_RIGHT_CLICK.load(Ordering::Relaxed);
    match s.menu_kind {
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
                // wParam = KIND_MAIN | KIND_EQ_BANDS (posted by sticky rows).
                match wp.0 as usize {
                    KIND_EQ_BANDS => eq_popup(s, s.menu_pt),
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
            WM_NCHITTEST => LRESULT(HTCAPTION as isize), // drag anywhere, activate never
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
