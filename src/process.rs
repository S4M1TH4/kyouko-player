//! Source-process launch policy shared by metadata and audio downloads.

use std::process::{Command, Stdio};

/// Own the entire Windows source process tree, including ffmpeg/JS helpers.
/// The Child itself remains in the decoder's interruptible shared slot.
pub struct SourceChild {
    child: std::process::Child,
    _download_directory: Option<DownloadDirectory>,
    #[cfg(windows)]
    group: std::os::windows::io::OwnedHandle,
}

/// Native HLS downloads write relative fragment files even with stdout output.
/// Give each download a writable workspace, and remove leftovers after the
/// source process tree has been stopped and reaped.
struct DownloadDirectory(std::path::PathBuf);

impl DownloadDirectory {
    fn create() -> std::io::Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let path = std::env::temp_dir().join(format!(
                "kyouko-stream-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

impl Drop for DownloadDirectory {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            crate::log_warn!("AUDIO", "cannot remove download workspace: {error}");
        }
    }
}

impl SourceChild {
    pub fn spawn_download(command: &mut Command) -> std::io::Result<Self> {
        let directory = DownloadDirectory::create()?;
        let mut child = Self::track(command.current_dir(&directory.0).spawn()?)?;
        child._download_directory = Some(directory);
        Ok(child)
    }

    pub fn track(mut child: std::process::Child) -> std::io::Result<Self> {
        #[cfg(windows)]
        {
            use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
            use windows::Win32::Foundation::HANDLE;
            use windows::Win32::System::JobObjects::*;
            let group = unsafe {
                (|| -> windows::core::Result<OwnedHandle> {
                    let handle = CreateJobObjectW(None, windows::core::PCWSTR::null())?;
                    let group = OwnedHandle::from_raw_handle(handle.0);
                    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                    SetInformationJobObject(
                        handle,
                        JobObjectExtendedLimitInformation,
                        (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                        std::mem::size_of_val(&limits) as u32,
                    )?;
                    if let Err(error) =
                        AssignProcessToJobObject(handle, HANDLE(child.as_raw_handle()))
                    {
                        // A short-lived fixture/tool may exit before assignment.
                        if child.try_wait().ok().flatten().is_none() {
                            return Err(error);
                        }
                    }
                    Ok(group)
                })()
            };
            match group {
                Ok(group) => Ok(Self {
                    child,
                    group,
                    _download_directory: None,
                }),
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    Err(std::io::Error::other(error))
                }
            }
        }
        #[cfg(not(windows))]
        {
            let _ = &mut child;
            Ok(Self {
                child,
                _download_directory: None,
            })
        }
    }

    pub fn kill(&mut self) -> std::io::Result<()> {
        #[cfg(windows)]
        unsafe {
            use std::os::windows::io::AsRawHandle;
            let _ = windows::Win32::System::JobObjects::TerminateJobObject(
                windows::Win32::Foundation::HANDLE(self.group.as_raw_handle()),
                1,
            );
        }
        self.child.kill()
    }
}

impl std::ops::Deref for SourceChild {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.child
    }
}
impl std::ops::DerefMut for SourceChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.child
    }
}

impl Drop for SourceChild {
    fn drop(&mut self) {
        let _ = self.kill();
        let _ = self.child.wait();
    }
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

    const HELPER_READY: &str = "KYOUKO_HELPER_READY ";

    fn helper_pid(stdout: impl std::io::Read) -> std::io::Result<u32> {
        use std::io::{BufRead, BufReader};
        for line in BufReader::new(stdout).lines() {
            if let Some((_, pid)) = line?.split_once(HELPER_READY) {
                return pid
                    .trim()
                    .parse()
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error));
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "fixture exited before helper readiness",
        ))
    }

    fn fixture_command(role: &str) -> Command {
        use std::os::windows::process::CommandExt;
        let mut fixture = Command::new(std::env::current_exe().unwrap());
        fixture
            .args([
                "--exact",
                "process::tests::source_tree_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("KYOUKO_PROCESS_FIXTURE_ROLE", role)
            .creation_flags(0x0800_0000)
            .stdin(Stdio::null());
        fixture
    }

    #[test]
    #[ignore = "native process-only fixture launched by the source-tree test"]
    fn source_tree_fixture() {
        use std::io::{Read, Write};
        let Ok(role) = std::env::var("KYOUKO_PROCESS_FIXTURE_ROLE") else {
            return;
        };
        if role == "download" || role == "download-wait" {
            std::fs::write("--Frag1.part", b"HLS fragment").unwrap();
            println!("download-ready");
            std::io::stdout().flush().unwrap();
            if role == "download-wait" {
                loop {
                    std::thread::park();
                }
            }
            return;
        }
        let pid = match role.as_str() {
            "parent" => {
                // The test sends this permit only after assigning us to its
                // job. A fast helper therefore cannot escape job inheritance.
                let mut permit = [0];
                std::io::stdin().read_exact(&mut permit).unwrap();
                assert_eq!(permit, [b'G']);
                let mut helper = fixture_command("helper")
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap();
                // Use a separate pipe so killing this parent always releases
                // the test's stdout reader, even if helper cleanup regresses.
                let pid = helper_pid(helper.stdout.take().unwrap()).unwrap();
                assert_eq!(pid, helper.id());
                println!("{HELPER_READY}{pid}");
                std::io::stdout().flush().unwrap();
                let status = helper.wait().unwrap();
                panic!("helper exited before tree cleanup: {status}");
            }
            "helper" => std::process::id(),
            _ => panic!("unknown fixture role"),
        };
        println!("{HELPER_READY}{pid}");
        std::io::stdout().flush().unwrap();
        loop {
            std::thread::park();
        }
    }

    #[test]
    fn download_workspaces_are_private_and_cleaned_after_exit_or_interrupt() {
        use std::io::{BufRead, BufReader};
        let parent_directory = std::env::current_dir().unwrap();
        let mut directories = Vec::new();
        for role in ["download", "download-wait"] {
            let mut command = fixture_command(role);
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut child = SourceChild::spawn_download(&mut command).unwrap();
            let directory = command.get_current_dir().unwrap().to_path_buf();
            let mut output = BufReader::new(child.stdout.take().unwrap());
            let mut line = String::new();
            while !line.contains("download-ready") {
                line.clear();
                assert!(
                    output.read_line(&mut line).unwrap() > 0,
                    "fixture exited before readiness"
                );
            }
            assert_ne!(directory, parent_directory);
            assert!(!directories.contains(&directory));
            assert_eq!(
                std::fs::read(directory.join("--Frag1.part")).unwrap(),
                b"HLS fragment"
            );
            if role == "download" {
                assert!(child.wait().unwrap().success());
                assert!(directory.exists(), "workspace lives as long as its owner");
            }
            // The parked fixture must be terminated before its directory is removed.
            drop(child);
            assert!(!directory.exists());
            directories.push(directory);
        }
        assert_eq!(std::env::current_dir().unwrap(), parent_directory);
    }

    #[test]
    fn failed_download_spawn_cleans_its_workspace() {
        let mut command = Command::new("kyouko-nonexistent-download-fixture.exe");
        assert!(SourceChild::spawn_download(&mut command).is_err());
        assert!(!command.get_current_dir().unwrap().exists());
    }

    #[test]
    fn source_tree_cleanup_terminates_an_inherited_helper() {
        use std::io::{Read, Write};
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows::Win32::System::Threading::{
            OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess,
            WaitForSingleObject,
        };
        // Process-only fixture: no keyboard, mouse or desktop input is injected.
        let mut child = SourceChild::track(
            fixture_command("parent")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        )
        .unwrap();
        child.stdin.take().unwrap().write_all(b"G").unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let errors = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            text
        });
        let (tx, rx) = crossbeam_channel::bounded(1);
        let reader = std::thread::spawn(move || {
            let _ = tx.send(helper_pid(stdout));
        });
        let result = rx.recv_timeout(std::time::Duration::from_secs(30));
        // Retain a helper handle before killing the tree so PID reuse cannot
        // confuse verification. Always clean up before an assertion can panic.
        let helper = result
            .as_ref()
            .ok()
            .and_then(|result| result.as_ref().ok())
            .and_then(|pid| unsafe {
                OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, false, *pid)
                    .ok()
                    .map(|handle| OwnedHandle::from_raw_handle(handle.0))
            });
        let helper_before_cleanup = helper
            .as_ref()
            .map(|helper| unsafe { WaitForSingleObject(HANDLE(helper.as_raw_handle()), 0) });
        let _ = child.kill();
        let waited = child.wait();
        let read_joined = reader.join();
        let stderr = errors.join().unwrap();
        let helper_status = helper.as_ref().map(|helper| unsafe {
            let handle = HANDLE(helper.as_raw_handle());
            let status = WaitForSingleObject(handle, 5000);
            if status != WAIT_OBJECT_0 {
                // Report a real job-cleanup failure after explicitly reaping
                // the helper, rather than leaking a parked process on failure.
                let _ = TerminateProcess(handle, 1);
                let _ = WaitForSingleObject(handle, 5000);
            }
            status
        });
        waited.unwrap();
        read_joined.unwrap();
        assert!(
            matches!(result, Ok(Ok(_))),
            "helper fixture must signal readiness: {result:?}; stderr: {stderr}"
        );
        let helper = helper.expect("helper must be running before cleanup");
        assert_eq!(
            helper_before_cleanup,
            Some(WAIT_TIMEOUT),
            "helper must still be alive before job cleanup"
        );
        assert_eq!(
            helper_status,
            Some(WAIT_OBJECT_0),
            "the inherited helper must terminate with its job"
        );
        drop(helper);
    }

    #[test]
    fn source_process_has_eof_stdin_and_working_output_pipes() {
        let output = command("cmd.exe")
            .args(["/D", "/C", "set /p input= & echo pipe-ready"])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("pipe-ready"));
    }
}
