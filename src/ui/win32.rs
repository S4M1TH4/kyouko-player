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
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, GetWindowLongPtrW, KillTimer, LoadCursorW,
    PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow, SetTimer,
    SetWindowLongPtrW, ShowWindow, SystemParametersInfoW, TrackPopupMenu, TranslateMessage,
    CREATESTRUCTW, GWLP_USERDATA, HTCAPTION, IDC_ARROW, MF_SEPARATOR, MF_STRING, MSG,
    SPI_GETWORKAREA, SW_HIDE, SW_SHOWNOACTIVATE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WNDCLASSW, WM_CLOSE, WM_CONTEXTMENU,
    WM_DESTROY, WM_ENDSESSION, WM_ERASEBKGND, WM_LBUTTONUP, WM_NCCREATE, WM_NCHITTEST,
    WM_PAINT, WM_RBUTTONUP, WM_TIMER, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};

use crate::audio::AudioOut;

use super::render::Renderer;
use super::tray::Tray;
use super::{set_pump_hwnd, wake_broker, CMD_TX, WM_APP_BROKER, WM_APP_STATUS, WM_APP_TRAY};
use crate::broker::{Broker, Command, Flow, Phase, SharedState, Status};
use crate::{log_error, log_info, log_warn};

const TIMER_ID: usize = 1;
const TIMER_MS: u32 = 1000;
const MENU_TOGGLE: usize = 1;
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
        WM_LBUTTONUP => toggle_visible(s),
        WM_RBUTTONUP | WM_CONTEXTMENU => popup_menu(s),
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

fn popup_menu(s: &mut UiState) {
    let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
        return;
    };
    let toggle_label = if s.hidden { w!("show echo") } else { w!("hide echo") };
    unsafe {
        let _ = AppendMenuW(menu, MF_STRING, MENU_TOGGLE, toggle_label);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, MENU_QUIT, w!("quit"));
        // KB135788: the menu's owner must be foreground or it won't dismiss.
        let _ = SetForegroundWindow(s.hwnd);
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let choice = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
            pt.x,
            pt.y,
            None,
            s.hwnd,
            None,
        );
        let _ = DestroyMenu(menu);
        match choice.0 as usize {
            MENU_TOGGLE => toggle_visible(s),
            MENU_QUIT => {
                log_info!("UI", "tray quit chosen");
                if let Some(tx) = CMD_TX.get() {
                    let _ = tx.try_send(Command::Quit);
                }
                wake_broker();
            }
            _ => {}
        }
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
