//! Owned Windows playlist metadata process. Cancellation and deadlines use a
//! duplicated process handle; the worker owns/reaps Child without holding locks.
use crossbeam_channel::{Receiver, Sender, bounded};
use std::io::Read;
use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows::Win32::System::Threading::TerminateProcess;
use windows::core::PCWSTR;

const MAX_METADATA_BYTES: u64 = 8 * 1024 * 1024;
// A full 5,000-entry playlist takes many paginated requests. Allow five
// minutes for flat metadata, plus ten seconds of startup/network margin.
// This is independent of the single-track title and audio download deadlines.
const METADATA_DEADLINE: Duration = Duration::from_secs(5 * 60 + 10);

pub struct PlaylistResult {
    pub request_id: u64,
    pub url: String,
    pub titles: Result<Vec<String>, String>,
}

pub struct PlaylistJob {
    process: Arc<OwnedHandle>,
    group: Arc<OwnedHandle>,
    cancelled: Arc<AtomicBool>,
    done: Arc<(Mutex<bool>, Condvar)>,
    result: Receiver<PlaylistResult>,
    worker: Option<JoinHandle<()>>,
}

fn terminate(process: &OwnedHandle, group: &OwnedHandle) {
    unsafe {
        let _ = TerminateJobObject(HANDLE(group.as_raw_handle()), 1);
        let _ = TerminateProcess(HANDLE(process.as_raw_handle()), 1);
    }
}

impl PlaylistJob {
    pub fn start(url: String, request_id: u64, notify: Sender<()>) -> Result<Self, String> {
        let mut command = crate::process::command("yt-dlp");
        command.args([
            "--flat-playlist",
            "--socket-timeout",
            "15",
            "--retries",
            "1",
            "--encoding",
            "utf-8",
            "--print",
            "%(playlist_index)s\t%(title|Unavailable track)j",
            &url,
        ]);
        Self::spawn(command, url, request_id, notify, METADATA_DEADLINE)
    }

    fn spawn(
        mut command: Command,
        url: String,
        request_id: u64,
        notify: Sender<()>,
        deadline: Duration,
    ) -> Result<Self, String> {
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("playlist process: {error}"))?;
        let handle = match child.as_handle().try_clone_to_owned() {
            Ok(handle) => handle,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.to_string());
            }
        };
        let group = unsafe {
            (|| -> windows::core::Result<OwnedHandle> {
                let handle = CreateJobObjectW(None, PCWSTR::null())?;
                let group = OwnedHandle::from_raw_handle(handle.0);
                let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of_val(&limits) as u32,
                )?;
                AssignProcessToJobObject(handle, HANDLE(child.as_raw_handle()))?;
                Ok(group)
            })()
        };
        let group = match group {
            Ok(group) => Arc::new(group),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.to_string());
            }
        };
        let process = Arc::new(handle);
        let cancelled = Arc::new(AtomicBool::new(false));
        let done = Arc::new((Mutex::new(false), Condvar::new()));
        let (result_tx, result) = bounded(1);
        let mut job = Self {
            process,
            group,
            cancelled,
            done,
            result,
            worker: None,
        };
        let process = job.process.clone();
        let group = job.group.clone();
        let cancelled = job.cancelled.clone();
        let done = job.done.clone();
        let timed_out = Arc::new(AtomicBool::new(false));
        let timeout_process = process.clone();
        let timeout_group = group.clone();
        let timeout_done = done.clone();
        let timeout_flag = timed_out.clone();
        let watchdog = match thread::Builder::new()
            .name("kyouko-playlist-deadline".into())
            .spawn(move || {
                let (lock, wake) = &*timeout_done;
                let finished = lock.lock().unwrap_or_else(|error| error.into_inner());
                let (finished, _) = wake
                    .wait_timeout_while(finished, deadline, |done| !*done)
                    .unwrap_or_else(|error| error.into_inner());
                if !*finished {
                    timeout_flag.store(true, Ordering::Release);
                    terminate(&timeout_process, &timeout_group);
                }
            }) {
            Ok(watchdog) => watchdog,
            Err(error) => {
                terminate(&job.process, &job.group);
                let _ = child.wait();
                return Err(error.to_string());
            }
        };
        // On spawn failure the closure (including Child) is dropped. The job
        // still owns a handle and Drop terminates the process; the watchdog is
        // explicitly joined below using its separately owned slot.
        let watchdog = Arc::new(Mutex::new(Some(watchdog)));
        let worker_watchdog = watchdog.clone();
        let worker = thread::Builder::new()
            .name("kyouko-ytlist".into())
            .spawn(move || {
                let mut bytes = Vec::new();
                let read = child
                    .stdout
                    .take()
                    .expect("piped metadata stdout")
                    .take(MAX_METADATA_BYTES + 1)
                    .read_to_end(&mut bytes);
                if read.is_err() || bytes.len() as u64 > MAX_METADATA_BYTES {
                    terminate(&process, &group);
                }
                let status = child.wait();
                *done.0.lock().unwrap_or_else(|error| error.into_inner()) = true;
                done.1.notify_all();
                let watchdog = worker_watchdog
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                if let Some(watchdog) = watchdog {
                    let _ = watchdog.join();
                }
                if cancelled.load(Ordering::Acquire) {
                    return;
                }
                let titles = if timed_out.load(Ordering::Acquire) {
                    Err(format!("playlist metadata deadline exceeded ({} seconds)", deadline.as_secs_f64()))
                } else if bytes.len() as u64 > MAX_METADATA_BYTES {
                    Err("playlist metadata exceeds the 8 MiB limit".into())
                } else {
                    read.map_err(|error| error.to_string())
                        .and_then(|_| status.map_err(|error| error.to_string()))
                        .and_then(|status| {
                            if status.success() {
                                crate::broker::parse_playlist_titles(&bytes)
                            } else {
                                Err(format!("playlist metadata exited with {status}"))
                            }
                        })
                };
                let _ = result_tx.try_send(PlaylistResult {
                    request_id,
                    url,
                    titles,
                });
                let _ = notify.try_send(());
                crate::ui::wake_broker();
            });
        match worker {
            Ok(worker) => {
                job.worker = Some(worker);
                Ok(job)
            }
            Err(error) => {
                terminate(&job.process, &job.group);
                *job.done.0.lock().unwrap_or_else(|error| error.into_inner()) = true;
                job.done.1.notify_all();
                let watchdog = watchdog
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                if let Some(watchdog) = watchdog {
                    let _ = watchdog.join();
                }
                Err(error.to_string())
            }
        }
    }

    pub fn take_result(&self) -> Option<PlaylistResult> {
        self.result.try_recv().ok()
    }
}

impl Drop for PlaylistJob {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        terminate(&self.process, &self.group);
        *self
            .done
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = true;
        self.done.1.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::WAIT_OBJECT_0;
    use windows::Win32::System::Threading::WaitForSingleObject;

    fn fixture(script: &str) -> Command {
        let mut command = crate::process::command("powershell.exe");
        command.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            script,
        ]);
        command
    }

    #[test]
    fn metadata_results_do_not_block_on_a_full_notification_pipe() {
        let (notify, events) = bounded(1);
        notify.send(()).unwrap();
        let mut job = PlaylistJob::spawn(
            fixture(r#"[Console]::WriteLine('1'+[char]9+'"Song"')"#),
            "test-playlist".into(),
            7,
            notify,
            Duration::from_secs(10),
        )
        .unwrap();
        job.worker.take().unwrap().join().unwrap();
        let result = job.take_result().unwrap();
        assert_eq!(result.request_id, 7);
        assert_eq!(result.url, "test-playlist");
        assert_eq!(result.titles.unwrap(), ["Song"]);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn full_size_playlist_metadata_keeps_every_index() {
        let (notify, events) = bounded(1);
        let job = PlaylistJob::spawn(
            fixture(r#"1..5000 | ForEach-Object { $title = if ($_ -ge 4780 -and $_ -le 4960) { 'Unavailable track' } else { '"Track '+$_+'"' }; [Console]::WriteLine($_.ToString()+[char]9+$title) }"#),
            "large-playlist".into(),
            10,
            notify,
            Duration::from_secs(10),
        ).unwrap();
        events.recv_timeout(Duration::from_secs(15)).unwrap();
        let titles = job.take_result().unwrap().titles.unwrap();
        assert_eq!(titles.len(), 5000);
        for (index, title) in titles.iter().enumerate() {
            let expected = if (4780..=4960).contains(&(index + 1)) {
                "Unavailable track".to_string()
            } else { format!("Track {}", index + 1) };
            assert_eq!(title, &expected);
        }
    }

    #[test]
    #[ignore = "live yt-dlp playlist metadata; requires KYOUKO_PLAYLIST_TEST_URL and network"]
    fn supplied_playlist_metadata_uses_production_deadline() {
        let raw = std::env::var("KYOUKO_PLAYLIST_TEST_URL").expect("KYOUKO_PLAYLIST_TEST_URL");
        let source = crate::broker::Source::from_raw(&raw);
        assert!(matches!(source, crate::broker::Source::Youtube { playlist_index: Some(1), .. }));
        let (notify, events) = bounded(1);
        let started = std::time::Instant::now();
        let job = PlaylistJob::start(source.raw().to_string(), 1, notify).unwrap();
        events.recv_timeout(METADATA_DEADLINE + Duration::from_secs(10)).unwrap();
        let result = job.take_result().unwrap();
        let titles = result.titles.unwrap();
        assert_eq!(result.url, source.raw());
        assert!(!titles.is_empty());
        println!("playlist metadata: {} entries in {:.3}s, deadline {}s, url {}",
            titles.len(), started.elapsed().as_secs_f64(), METADATA_DEADLINE.as_secs(), source.raw());
    }

    #[test]
    fn cancelling_metadata_reaps_process_and_joins_both_workers_each_time() {
        for id in 0..3 {
            let (notify, _events) = bounded(1);
            let job = PlaylistJob::spawn(
                fixture("[Threading.Thread]::Sleep(30000)"),
                "cancel-test".into(),
                id,
                notify,
                Duration::from_secs(20),
            )
            .unwrap();
            let process = job.process.as_handle().try_clone_to_owned().unwrap();
            let weak = Arc::downgrade(&job.process);
            drop(job);
            unsafe {
                assert_eq!(
                    WaitForSingleObject(HANDLE(process.as_raw_handle()), 1000),
                    WAIT_OBJECT_0
                );
            }
            assert!(
                weak.upgrade().is_none(),
                "no worker retains the job after cancellation"
            );
        }
    }

    #[test]
    fn metadata_deadline_terminates_stalled_process_and_reports_a_failure() {
        let (notify, events) = bounded(1);
        let job = PlaylistJob::spawn(
            fixture("[Threading.Thread]::Sleep(30000)"),
            "timeout-test".into(),
            9,
            notify,
            Duration::from_millis(150),
        )
        .unwrap();
        let process = job.process.as_handle().try_clone_to_owned().unwrap();
        events.recv_timeout(Duration::from_secs(5)).unwrap();
        let result = job.take_result().unwrap();
        assert_eq!(result.request_id, 9);
        assert!(result.titles.unwrap_err().contains("deadline"));
        drop(job);
        unsafe {
            assert_eq!(
                WaitForSingleObject(HANDLE(process.as_raw_handle()), 1000),
                WAIT_OBJECT_0
            );
        }
    }
}
