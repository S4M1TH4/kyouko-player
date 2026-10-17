//! Forward shell-open requests to the instance that owns this state directory.
//! WM_COPYDATA copies the payload across processes; the receiver only queues
//! a command, so extraction and decoding never block the sending Explorer task.

use std::path::Path;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, SMTO_ABORTIFHUNG, SMTO_BLOCK, SendMessageTimeoutW, WM_COPYDATA,
};
use windows::core::PCWSTR;

use crate::broker::Source;

const OPEN_SOURCE: usize = 0x4b59_4f01;
const MAX_PAYLOAD: usize = 128 * 1024;

/// Scope discovery to the same canonical directory as the instance lock.
/// A stable hash also lets different builds share the endpoint. Hidden overlay
/// windows remain discoverable, and isolated diagnostic profiles stay separate.
pub fn window_class(state_path: &Path) -> std::io::Result<Vec<u16>> {
    let directory = std::fs::canonicalize(
        state_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?;
    let mut hash = 0xcbf29ce484222325u64;
    for byte in directory.to_string_lossy().to_lowercase().bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
    }
    Ok(format!("kyouko_echo_{hash:016x}")
        .encode_utf16()
        .chain([0])
        .collect())
}

fn payload(source: &Source) -> std::io::Result<Vec<u8>> {
    // A shell launch may use a different working directory from the owner.
    let raw = match source {
        Source::File(path) => std::path::absolute(path)?.to_string_lossy().into_owned(),
        Source::Youtube { url, .. } => url.clone(),
    };
    if raw.is_empty() || raw.contains('\0') || raw.len() > MAX_PAYLOAD {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid open request",
        ));
    }
    Ok(raw.into_bytes())
}

fn decode(tag: usize, bytes: &[u8]) -> Option<Source> {
    if tag != OPEN_SOURCE || bytes.is_empty() || bytes.len() > MAX_PAYLOAD || bytes.contains(&0) {
        return None;
    }
    Some(Source::from_raw(std::str::from_utf8(bytes).ok()?))
}

/// The instance lock can become visible before its window exists. Retry only
/// during this duplicate launch, with a finite deadline and a bounded send.
pub fn forward(state_path: &Path, source: &Source) -> std::io::Result<()> {
    let class = window_class(state_path)?;
    let bytes = payload(source)?;
    let packet = COPYDATASTRUCT {
        dwData: OPEN_SOURCE,
        cbData: bytes.len() as u32,
        lpData: bytes.as_ptr().cast_mut().cast(),
    };
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(hwnd) = unsafe { FindWindowW(PCWSTR(class.as_ptr()), PCWSTR::null()) } {
            let mut accepted = 0;
            let remaining = until.saturating_duration_since(Instant::now());
            let sent = unsafe {
                SendMessageTimeoutW(
                    hwnd,
                    WM_COPYDATA,
                    WPARAM(0),
                    LPARAM((&packet as *const COPYDATASTRUCT) as isize),
                    SMTO_ABORTIFHUNG | SMTO_BLOCK,
                    remaining.as_millis().clamp(1, 3000) as u32,
                    Some(&mut accepted),
                )
            };
            if sent.0 == 0 {
                // A timeout has an ambiguous delivery result; do not send twice.
                return Err(std::io::Error::other(
                    "running player did not acknowledge the open request",
                ));
            }
            if accepted == 1 {
                return Ok(());
            }
            // Explicit rejection means nothing was queued. Let the owner drain
            // its bounded startup/worker queue before retrying within the deadline.
        }
        if Instant::now() >= until {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "running player has no ready window",
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Copy the OS-owned message bytes before returning from the window procedure.
///
/// # Safety
/// A non-null lparam must point to the COPYDATASTRUCT supplied by WM_COPYDATA,
/// whose lpData is valid for cbData bytes for the duration of this call.
pub unsafe fn receive(lparam: LPARAM) -> Option<Source> {
    let packet = unsafe { (lparam.0 as *const COPYDATASTRUCT).as_ref()? };
    if packet.lpData.is_null() || packet.cbData == 0 || packet.cbData as usize > MAX_PAYLOAD {
        return None;
    }
    let bytes =
        unsafe { std::slice::from_raw_parts(packet.lpData.cast::<u8>(), packet.cbData as usize) };
    decode(packet.dwData, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use windows::Win32::Foundation::{HWND, LRESULT};
    use windows::Win32::UI::WindowsAndMessaging::*;

    #[test]
    fn payload_preserves_unicode_spaces_and_resolves_relative_files() {
        for source in [
            Source::File("C:\\Music\\01. 野宮あゆみ — Dark Flight Dreamer.mp3".into()),
            Source::from_raw("https://www.nicovideo.jp/watch/sm18355305"),
        ] {
            assert_eq!(
                decode(OPEN_SOURCE, &payload(&source).unwrap()),
                Some(source)
            );
        }
        let relative = Source::File("music/日本語 song.mp3".into());
        let resolved = decode(OPEN_SOURCE, &payload(&relative).unwrap()).unwrap();
        assert_eq!(
            resolved.raw(),
            std::path::absolute(relative.raw())
                .unwrap()
                .to_str()
                .unwrap()
        );
        assert!(Path::new(resolved.raw()).is_absolute());
    }

    #[test]
    fn copydata_rejects_invalid_messages_and_copies_valid_payloads() {
        for (tag, bytes) in [
            (0, b"song.mp3".as_slice()),
            (OPEN_SOURCE, b"".as_slice()),
            (OPEN_SOURCE, b"song\0.mp3".as_slice()),
            (OPEN_SOURCE, &[0xff]),
        ] {
            assert!(decode(tag, bytes).is_none());
        }
        assert!(decode(OPEN_SOURCE, &vec![b'a'; MAX_PAYLOAD + 1]).is_none());
        let mut bytes = b"C:\\Music\\song.mp3".to_vec();
        let mut packet = COPYDATASTRUCT {
            dwData: OPEN_SOURCE,
            cbData: bytes.len() as u32,
            lpData: bytes.as_mut_ptr().cast(),
        };
        let received =
            unsafe { receive(LPARAM((&packet as *const COPYDATASTRUCT) as isize)) }.unwrap();
        bytes.fill(b'x');
        assert_eq!(received.raw(), "C:\\Music\\song.mp3");
        packet.cbData = u32::MAX;
        assert!(unsafe { receive(LPARAM((&packet as *const COPYDATASTRUCT) as isize)) }.is_none());
        assert!(unsafe { receive(LPARAM(0)) }.is_none());
    }

    #[test]
    fn window_discovery_is_scoped_to_the_canonical_state_directory() {
        let root = std::env::temp_dir().join(format!("kyouko-ipc-{}-scope", std::process::id()));
        std::fs::create_dir_all(root.join("child")).unwrap();
        let class = window_class(&root.join("state.cfg")).unwrap();
        assert_eq!(
            class,
            window_class(&root.join("child/../state.cfg")).unwrap()
        );
        assert_ne!(class, window_class(&root.join("child/state.cfg")).unwrap());
        assert!(window_class(Path::new("state.cfg")).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "hidden native receiver fixture, launched by the IPC test"]
    fn receiver_fixture() {
        thread_local! { static RECEIVED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        static REJECT_ONCE: std::cell::Cell<bool> = const { std::cell::Cell::new(true) }; }
        unsafe extern "system" fn receiver(
            hwnd: HWND,
            msg: u32,
            wp: WPARAM,
            lp: LPARAM,
        ) -> LRESULT {
            if msg == WM_COPYDATA {
                if REJECT_ONCE.with(|reject| reject.replace(false)) {
                    return LRESULT(0);
                }
                if let Some(source) = unsafe { receive(lp) } {
                    println!("IPC_OPEN {}", serde_json::to_string(source.raw()).unwrap());
                    std::io::stdout().flush().unwrap();
                    RECEIVED.with(|count| {
                        count.set(count.get() + 1);
                        if count.get() == 2 {
                            unsafe {
                                PostQuitMessage(0);
                            }
                        }
                    });
                    return LRESULT(1);
                }
                return LRESULT(0);
            }
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
        let state = std::path::PathBuf::from(std::env::var_os("KYOUKO_IPC_TEST_STATE").unwrap());
        let _guard = crate::config::InstanceGuard::acquire(&state)
            .unwrap()
            .unwrap();
        println!("IPC_LOCK_HELD");
        std::io::stdout().flush().unwrap();
        // Exercise the interval in which a second launch sees the lock before
        // the original instance has finished preparing its window.
        std::thread::sleep(Duration::from_millis(150));
        let class = window_class(&state).unwrap();
        unsafe {
            let module = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap();
            let wc = WNDCLASSW {
                lpfnWndProc: Some(receiver),
                hInstance: module.into(),
                lpszClassName: PCWSTR(class.as_ptr()),
                ..Default::default()
            };
            assert_ne!(RegisterClassW(&wc), 0);
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                PCWSTR(class.as_ptr()),
                PCWSTR::null(),
                WS_POPUP,
                -32000,
                -32000,
                1,
                1,
                None,
                None,
                Some(module.into()),
                None,
            )
            .unwrap();
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                DispatchMessageW(&msg);
            }
            DestroyWindow(hwnd).unwrap();
        }
    }

    #[test]
    fn repeated_open_requests_cross_processes_and_wait_for_owner_startup() {
        use std::os::windows::io::{AsHandle, AsRawHandle};
        use std::process::Stdio;
        let dir = std::env::temp_dir().join(format!("kyouko-ipc-{}-process", std::process::id()));
        let state = dir.join("state.cfg");
        let mut fixture = crate::process::SourceChild::track(
            crate::process::command(std::env::current_exe().unwrap().to_str().unwrap())
                .args([
                    "--exact",
                    "instance::tests::receiver_fixture",
                    "--ignored",
                    "--nocapture",
                ])
                .env("KYOUKO_IPC_TEST_STATE", &state)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        )
        .unwrap();
        let handle = fixture.as_handle().try_clone_to_owned().unwrap();
        let deadline = crate::deadline::Deadline::start(Duration::from_secs(10), move || unsafe {
            let _ = windows::Win32::System::Threading::TerminateProcess(
                windows::Win32::Foundation::HANDLE(handle.as_raw_handle()),
                1,
            );
        })
        .unwrap();
        let mut output = BufReader::new(fixture.stdout.take().unwrap()).lines();
        loop {
            let line = output
                .next()
                .expect("fixture exited before readiness")
                .unwrap();
            if line == "IPC_LOCK_HELD" {
                break;
            }
        }
        assert!(
            crate::config::InstanceGuard::acquire(&state)
                .unwrap()
                .is_none()
        );
        let first = Source::File("C:\\Music\\野宮あゆみ — Dark Flight Dreamer.mp3".into());
        let second = Source::File("relative track.mp3".into());
        forward(&state, &first).unwrap();
        forward(&state, &second).unwrap();
        let opened: Vec<String> = output
            .map(Result::unwrap)
            .filter_map(|line| {
                line.strip_prefix("IPC_OPEN ")
                    .map(|json| serde_json::from_str(json).unwrap())
            })
            .collect();
        assert!(fixture.wait().unwrap().success());
        drop(fixture);
        drop(deadline);
        assert_eq!(
            opened,
            [
                first.raw().to_string(),
                std::path::absolute(second.raw())
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string()
            ]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
