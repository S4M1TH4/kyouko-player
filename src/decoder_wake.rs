//! One decoder waiter. Registration precedes predicate rechecking. Publishers
//! change the atomic before waking: a pre-registration notification is observed
//! by the acquiring swap/recheck, and a later notification makes WaitOnAddress
//! refuse to sleep on the registered value. Spurious returns recheck predicates.
use crate::broker::DecoderCmd;
#[cfg(test)]
use crossbeam_channel::SendError;
use crossbeam_channel::{Receiver, Sender, TrySendError};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

pub(crate) const COMMAND: u32 = 1;
pub(crate) const CAPACITY: u32 = 2;
pub(crate) const DRAIN: u32 = 4;

#[derive(Default)]
pub(crate) struct DecoderWake {
    interest: AtomicU32,
    producers: AtomicUsize,
    #[cfg(test)]
    wakes: AtomicU32,
    #[cfg(test)]
    waiting: std::sync::OnceLock<Sender<u32>>,
}

impl DecoderWake {
    pub(crate) fn register(&self, interest: u32) {
        self.interest.swap(interest, Ordering::SeqCst);
    }
    pub(crate) fn cancel(&self) {
        // Acquire notifications that raced the predicate check too. An RMW
        // preserves their publication chain across cancellation and the NEXT
        // registration; a plain store could sever that synchronization.
        self.interest.swap(0, Ordering::SeqCst);
    }
    pub(crate) fn wait(&self, interest: u32) {
        #[cfg(test)]
        if let Some(waiting) = self.waiting.get() {
            let _ = waiting.try_send(interest);
        }
        atomic_wait::wait(&self.interest, interest);
    }
    #[cfg(test)]
    pub(crate) fn watch_wait(&self) -> Receiver<u32> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        assert!(self.waiting.set(tx).is_ok());
        rx
    }
    #[cfg(test)]
    pub(crate) fn wake_count(&self) -> u32 {
        self.wakes.load(Ordering::Relaxed)
    }
    pub(crate) fn notify(&self, reason: u32) {
        if reason == 0 {
            return;
        }
        // Only the first useful notification changes a registered wait. While
        // running (zero interest), this does no OS wake. Multiple released
        // blocks in a callback are reported together by its caller.
        if self.interest.fetch_and(!reason, Ordering::SeqCst) & reason != 0 {
            #[cfg(test)]
            self.wakes.fetch_add(1, Ordering::Relaxed);
            atomic_wait::wake_one(&self.interest);
        }
    }
}

pub(crate) struct CommandSender {
    tx: Option<Sender<DecoderCmd>>,
    pub(crate) wake: Arc<DecoderWake>,
}

impl Clone for CommandSender {
    fn clone(&self) -> Self {
        self.wake.producers.fetch_add(1, Ordering::Relaxed);
        Self {
            tx: self.tx.clone(),
            wake: self.wake.clone(),
        }
    }
}

impl CommandSender {
    pub(crate) fn capacity(&self) -> Option<usize> {
        self.tx.as_ref().unwrap().capacity()
    }
    #[cfg(test)]
    pub(crate) fn send(&self, command: DecoderCmd) -> Result<(), SendError<DecoderCmd>> {
        let result = self.tx.as_ref().unwrap().send(command);
        if result.is_ok() {
            self.wake.notify(COMMAND);
        }
        result
    }
    pub(crate) fn try_send(&self, command: DecoderCmd) -> Result<(), TrySendError<DecoderCmd>> {
        let result = self.tx.as_ref().unwrap().try_send(command);
        if result.is_ok() {
            self.wake.notify(COMMAND);
        }
        result
    }
}

impl Drop for CommandSender {
    fn drop(&mut self) {
        // Publish channel disconnection before notifying a capacity/drain wait.
        drop(self.tx.take());
        if self.wake.producers.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.wake.notify(COMMAND);
        }
    }
}

#[derive(Clone)]
pub(crate) struct CommandReceiver {
    rx: Receiver<DecoderCmd>,
    pub(crate) wake: Arc<DecoderWake>,
}

impl std::ops::Deref for CommandReceiver {
    type Target = Receiver<DecoderCmd>;
    fn deref(&self) -> &Self::Target {
        &self.rx
    }
}

impl CommandReceiver {
    pub(crate) fn recv(&self) -> Result<DecoderCmd, crossbeam_channel::RecvError> {
        loop {
            self.wake.register(COMMAND);
            match self.rx.try_recv() {
                Ok(command) => {
                    self.wake.cancel();
                    return Ok(command);
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    self.wake.cancel();
                    return Err(crossbeam_channel::RecvError);
                }
                Err(crossbeam_channel::TryRecvError::Empty) => self.wake.wait(COMMAND),
            }
        }
    }
}

pub(crate) fn command_channel(capacity: usize) -> (CommandSender, CommandReceiver) {
    let (tx, rx) = crossbeam_channel::bounded(capacity);
    let wake = Arc::new(DecoderWake::default());
    wake.producers.store(1, Ordering::Relaxed);
    (
        CommandSender {
            tx: Some(tx),
            wake: wake.clone(),
        },
        CommandReceiver { rx, wake },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notifications_coalesce_and_only_wake_interested_waiters() {
        let wake = DecoderWake::default();
        for _ in 0..100 {
            wake.notify(CAPACITY);
        }
        assert_eq!(wake.wakes.load(Ordering::Relaxed), 0);
        wake.register(COMMAND | DRAIN);
        wake.notify(CAPACITY);
        assert_eq!(wake.wakes.load(Ordering::Relaxed), 0);
        for _ in 0..100 {
            wake.notify(DRAIN);
        }
        assert_eq!(wake.wakes.load(Ordering::Relaxed), 1);
        wake.cancel();
    }

    #[test]
    fn publication_before_registration_and_between_recheck_and_wait_cannot_be_lost() {
        use std::sync::atomic::AtomicBool;
        for before in [true, false] {
            let wake = DecoderWake::default();
            let ready = AtomicBool::new(false);
            if before {
                ready.store(true, Ordering::Release);
                wake.notify(CAPACITY);
            }
            wake.register(COMMAND | CAPACITY);
            if before {
                assert!(ready.load(Ordering::Acquire));
            } else {
                assert!(!ready.load(Ordering::Acquire));
                ready.store(true, Ordering::Release);
                wake.notify(CAPACITY);
                // Must return immediately: the compared registration changed.
                wake.wait(COMMAND | CAPACITY);
                assert!(ready.load(Ordering::Acquire));
            }
            wake.cancel();
        }
    }

    #[test]
    fn every_publication_registration_recheck_and_sleep_interleaving_has_a_wake_or_work() {
        // Enumerate every merge of PUBLISH -> NOTIFY and REGISTER -> CHECK ->
        // SLEEP. The real atomic operations implement each ordering. The OS
        // wait is entered only when its compared value is already different;
        // sleeping orderings assert that the later notification issues a wake.
        fn merges(publisher: usize, waiter: usize, order: &mut Vec<u8>, orders: &mut Vec<Vec<u8>>) {
            if publisher == 2 && waiter == 3 {
                orders.push(order.clone());
                return;
            }
            if publisher < 2 {
                order.push(publisher as u8);
                merges(publisher + 1, waiter, order, orders);
                order.pop();
            }
            if waiter < 3 {
                order.push(2 + waiter as u8);
                merges(publisher, waiter + 1, order, orders);
                order.pop();
            }
        }
        let mut orders = Vec::new();
        merges(0, 0, &mut Vec::new(), &mut orders);
        assert_eq!(orders.len(), 10);
        for order in orders {
            let wake = DecoderWake::default();
            let ready = std::sync::atomic::AtomicBool::new(false);
            let mut observed = false;
            let mut asleep = false;
            for operation in &order {
                match operation {
                    0 => ready.store(true, Ordering::Release),
                    1 => {
                        wake.notify(CAPACITY);
                        if asleep {
                            assert_eq!(wake.wake_count(), 1);
                            asleep = false;
                        }
                    }
                    2 => wake.register(COMMAND | CAPACITY),
                    3 => observed = ready.load(Ordering::Acquire),
                    4 if observed => wake.cancel(),
                    4 => {
                        if wake.interest.load(Ordering::SeqCst) == COMMAND | CAPACITY {
                            asleep = true;
                        } else {
                            wake.wait(COMMAND | CAPACITY);
                        }
                    }
                    _ => unreachable!(),
                }
            }
            assert!(!asleep, "lost wake in {order:?}");
            assert!(ready.load(Ordering::Acquire));
            wake.cancel();
        }
    }

    #[test]
    fn command_and_last_sender_drop_interrupt_a_registered_wait() {
        for disconnect in [false, true] {
            let (tx, rx) = command_channel(2);
            let (registered, registration) = crossbeam_channel::bounded(0);
            let (done, completed) = crossbeam_channel::bounded(0);
            let worker = std::thread::spawn(move || {
                rx.wake.register(COMMAND | CAPACITY);
                registered.send(()).unwrap();
                rx.wake.wait(COMMAND | CAPACITY);
                rx.wake.cancel();
                done.send(rx.try_recv().is_err()).unwrap();
            });
            registration.recv().unwrap();
            if disconnect {
                drop(tx);
            } else {
                tx.send(DecoderCmd::Stop).unwrap();
            }
            assert_eq!(
                completed
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap(),
                disconnect
            );
            worker.join().unwrap();
        }
    }
}
