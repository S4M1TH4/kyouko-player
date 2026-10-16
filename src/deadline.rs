//! An owned, cancellable deadline. The worker parks until progress, completion,
//! or expiry; completed/local/cached playback has no watchdog wake-ups.
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

struct State {
    until: Instant,
    done: bool,
}

pub struct Deadline {
    state: Arc<(Mutex<State>, Condvar)>,
    worker: Option<JoinHandle<()>>,
}

impl Deadline {
    pub fn start(
        timeout: Duration,
        expire: impl FnOnce() + Send + 'static,
    ) -> std::io::Result<Self> {
        let state = Arc::new((
            Mutex::new(State {
                until: Instant::now() + timeout,
                done: false,
            }),
            Condvar::new(),
        ));
        let watched = state.clone();
        let worker = thread::Builder::new()
            .name("kyouko-source-deadline".into())
            .spawn(move || {
                let (lock, wake) = &*watched;
                let mut state = lock.lock().unwrap_or_else(|error| error.into_inner());
                while !state.done {
                    let remaining = state.until.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        state.done = true;
                        drop(state); // expiry never runs under the coordination lock
                        expire();
                        return;
                    }
                    state = wake
                        .wait_timeout(state, remaining)
                        .unwrap_or_else(|error| error.into_inner())
                        .0;
                }
            })?;
        Ok(Self {
            state,
            worker: Some(worker),
        })
    }

    pub fn progress(&self, timeout: Duration) {
        let mut state = self
            .state
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !state.done {
            state.until = Instant::now() + timeout;
            self.state.1.notify_one();
        }
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        self.state
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .done = true;
        self.state.1.notify_one();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::bounded;

    #[test]
    fn progress_reschedules_expiry_and_drop_cancels_without_waiting_for_timeout() {
        let (tx, rx) = bounded(1);
        let deadline = Deadline::start(Duration::from_secs(1), move || {
            tx.send(()).unwrap();
        })
        .unwrap();
        deadline.progress(Duration::from_millis(30));
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        drop(deadline);
        let (tx, rx) = bounded(1);
        let deadline = Deadline::start(Duration::from_secs(60), move || {
            let _ = tx.send(());
        })
        .unwrap();
        let started = Instant::now();
        drop(deadline);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(rx.try_recv().is_err());
    }
}
