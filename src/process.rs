//! Source-process launch policy shared by metadata and audio downloads.

use std::process::{Command, Stdio};

/// Own the entire Windows source process tree, including ffmpeg/JS helpers.
/// The Child itself remains in the decoder's interruptible shared slot.
pub struct SourceChild {
    child: std::process::Child,
    #[cfg(windows)]
    group: std::os::windows::io::OwnedHandle,
}

impl SourceChild {
    pub fn track(mut child: std::process::Child) -> std::io::Result<Self> {
        #[cfg(windows)]
        {
            use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
            use windows::Win32::Foundation::HANDLE;
            use windows::Win32::System::JobObjects::*;
            let group = unsafe { (|| -> windows::core::Result<OwnedHandle> {
                let handle = CreateJobObjectW(None, windows::core::PCWSTR::null())?;
                let group = OwnedHandle::from_raw_handle(handle.0);
                let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(handle, JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(), std::mem::size_of_val(&limits) as u32)?;
                if let Err(error) = AssignProcessToJobObject(handle, HANDLE(child.as_raw_handle())) {
                    // A short-lived fixture/tool may exit before assignment.
                    if child.try_wait().ok().flatten().is_none() { return Err(error); }
                }
                Ok(group)
            })() };
            match group {
                Ok(group) => Ok(Self { child, group }),
                Err(error) => { let _ = child.kill(); let _ = child.wait(); Err(std::io::Error::other(error)) }
            }
        }
        #[cfg(not(windows))]
        { let _ = &mut child; Ok(Self { child }) }
    }

    pub fn kill(&mut self) -> std::io::Result<()> {
        #[cfg(windows)]
        unsafe {
            use std::os::windows::io::AsRawHandle;
            let _ = windows::Win32::System::JobObjects::TerminateJobObject(
                windows::Win32::Foundation::HANDLE(self.group.as_raw_handle()), 1);
        }
        self.child.kill()
    }
}

impl std::ops::Deref for SourceChild {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target { &self.child }
}
impl std::ops::DerefMut for SourceChild {
    fn deref_mut(&mut self) -> &mut Self::Target { &mut self.child }
}

impl Drop for SourceChild {
    fn drop(&mut self) { let _ = self.kill(); let _ = self.child.wait(); }
}

pub fn command(program: &str) -> Command {
    let mut command = Command::new(program);
    command.stdin(Stdio::null());
    #[cfg(all(windows, not(debug_assertions)))]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: console tools must not open a window when the
        // release player has no console to inherit. Pipe I/O still works.
        command.creation_flags(0x0800_0000);
    }
    command
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn source_tree_cleanup_terminates_an_inherited_helper() {
        use std::io::{BufRead, BufReader};
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
        // Process-only fixture: no keyboard, mouse or desktop input is injected.
        let script = r#"$helper=Start-Process cmd.exe -ArgumentList '/D','/C','ping -n 30 127.0.0.1 > nul' -WindowStyle Hidden -PassThru; [Console]::Out.WriteLine($helper.Id); [Console]::Out.Flush(); [Threading.Thread]::Sleep(30000)"#;
        let mut child = SourceChild::track(command("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap()).unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let reader = std::thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
            let _ = tx.send(result);
        });
        let result = rx.recv_timeout(std::time::Duration::from_secs(5));
        // Retain a helper handle before killing the tree so PID reuse cannot
        // confuse verification. Always clean up before an assertion can panic.
        let helper = result.as_ref().ok().and_then(|result| result.as_ref().ok())
            .and_then(|line| line.trim().parse().ok()).and_then(|pid| unsafe {
                OpenProcess(PROCESS_SYNCHRONIZE, false, pid).ok()
                    .map(|handle| OwnedHandle::from_raw_handle(handle.0))
            });
        let _ = child.kill();
        child.wait().unwrap();
        reader.join().unwrap();
        assert!(result.is_ok(), "helper fixture must signal readiness");
        let helper = helper.expect("helper must be running before cleanup");
        assert_eq!(unsafe { WaitForSingleObject(HANDLE(helper.as_raw_handle()), 1000) }, WAIT_OBJECT_0);
    }

    #[test]
    fn source_process_has_eof_stdin_and_working_output_pipes() {
        let output = command("cmd.exe")
            .args(["/D", "/C", "set /p input= & echo pipe-ready"])
            .output().unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("pipe-ready"));
    }
}
