//! Bounded, preallocated PCM ring. Unlike general MPMC queues, try_pop uses
//! ONE strong CAS and returns Contended instead of retrying. No backoff, waiter
//! registry, allocation, overwrite or concurrent clear/reset operation exists.
//!
//! The ready ring has one writer (decoder) and two readers (output and stale
//! reclamation). The recycle ring has one writer (output) and one reader
//! (decoder); initialization occurs before either runs. Keeping interrupted
//! and reclaimed blocks on the decoder avoids a second recycling writer.
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PopError {
    Empty,
    Contended,
}

struct Slot<T> {
    sequence: AtomicUsize,
    value: UnsafeCell<MaybeUninit<T>>,
}

pub(crate) struct PcmQueue<T> {
    slots: Box<[Slot<T>]>,
    lap: usize,
    head: AtomicUsize,
    tail: AtomicUsize,
    #[cfg(test)]
    reader_wait: std::sync::atomic::AtomicU32,
}

// A producer exclusively claims the tail before writing; a consumer
// exclusively claims the head before reading. Slot sequence Release/Acquire
// pairs protect the value and prevent reuse until its former reader finishes.
unsafe impl<T: Send> Sync for PcmQueue<T> {}

impl<T> PcmQueue<T> {
    pub(crate) fn new(capacity: usize) -> Self {
        assert!(capacity > 1 && capacity < isize::MAX as usize / 2);
        Self {
            lap: capacity.next_power_of_two(),
            slots: (0..capacity)
                .map(|index| Slot {
                    sequence: AtomicUsize::new(index),
                    value: UnsafeCell::new(MaybeUninit::uninit()),
                })
                .collect(),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            #[cfg(test)]
            reader_wait: std::sync::atomic::AtomicU32::new(0),
        }
    }

    fn next(&self, position: usize) -> usize {
        let index = position & (self.lap - 1);
        if index + 1 == self.slots.len() {
            (position & !(self.lap - 1)).wrapping_add(self.lap)
        } else {
            position.wrapping_add(1)
        }
    }

    pub(crate) fn try_send(&self, value: T) -> Result<(), T> {
        let tail = self.tail.load(Ordering::Relaxed);
        let slot = &self.slots[tail & (self.lap - 1)];
        if slot.sequence.load(Ordering::Acquire) != tail {
            return Err(value);
        }
        // Production has a single writer on each ring. The CAS also rejects
        // accidental simultaneous writers without introducing a data race.
        if self
            .tail
            .compare_exchange(tail, self.next(tail), Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return Err(value);
        }
        unsafe {
            (*slot.value.get()).write(value);
        }
        slot.sequence.store(tail.wrapping_add(1), Ordering::Release);
        #[cfg(test)]
        if self.reader_wait.swap(0, Ordering::SeqCst) != 0 {
            atomic_wait::wake_one(&self.reader_wait);
        }
        Ok(())
    }

    pub(crate) fn try_recv(&self) -> Result<T, PopError> {
        let head = self.head.load(Ordering::Relaxed);
        self.claim_head(head)
    }

    fn claim_head(&self, head: usize) -> Result<T, PopError> {
        let slot = &self.slots[head & (self.lap - 1)];
        let expected = head.wrapping_add(1);
        let sequence = slot.sequence.load(Ordering::Acquire);
        if sequence != expected {
            // A released/reused slot means another consumer advanced this
            // snapshot. It is contention, NOT an empty queue. Stale reclamation
            // must keep making bounded progress instead of leaving paused PCM.
            return Err(if sequence.wrapping_sub(expected) as isize > 0 {
                PopError::Contended
            } else {
                PopError::Empty
            });
        }
        if self
            .head
            .compare_exchange(head, self.next(head), Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return Err(PopError::Contended);
        }
        let value = unsafe { (*slot.value.get()).assume_init_read() };
        slot.sequence
            .store(head.wrapping_add(self.lap), Ordering::Release);
        Ok(value)
    }

    pub(crate) fn has_capacity(&self) -> bool {
        let tail = self.tail.load(Ordering::Relaxed);
        self.slots[tail & (self.lap - 1)]
            .sequence
            .load(Ordering::Acquire)
            == tail
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub(crate) fn len(&self) -> usize {
        let tail = self.tail.load(Ordering::Acquire);
        let head = self.head.load(Ordering::Acquire);
        let laps = (tail & !(self.lap - 1)).wrapping_sub(head & !(self.lap - 1)) / self.lap;
        laps.wrapping_mul(self.slots.len())
            .wrapping_add(tail & (self.lap - 1))
            .wrapping_sub(head & (self.lap - 1))
            .min(self.slots.len())
    }
    pub(crate) fn capacity(&self) -> usize {
        self.slots.len()
    }
    #[cfg(test)]
    pub(crate) fn try_iter(&self) -> impl Iterator<Item = T> + '_ {
        std::iter::from_fn(|| self.try_recv().ok())
    }

    /// Test virtual sinks can block without putting a channel/waiter lock in
    /// the PCM path. Production has only the decoder's shared address waiter.
    #[cfg(all(test, windows))]
    pub(crate) fn recv_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Result<T, crossbeam_channel::RecvTimeoutError> {
        let until = std::time::Instant::now() + timeout;
        loop {
            self.reader_wait.swap(1, Ordering::SeqCst);
            if let Ok(value) = self.try_recv() {
                self.reader_wait.store(0, Ordering::SeqCst);
                return Ok(value);
            }
            let remaining = until.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                self.reader_wait.store(0, Ordering::SeqCst);
                return Err(crossbeam_channel::RecvTimeoutError::Timeout);
            }
            let expected = 1u32;
            unsafe {
                let _ = windows::Win32::System::Threading::WaitOnAddress(
                    (&self.reader_wait as *const std::sync::atomic::AtomicU32).cast(),
                    (&expected as *const u32).cast(),
                    size_of::<u32>(),
                    Some(remaining.as_millis().clamp(1, u32::MAX as u128 - 1) as u32),
                );
            }
        }
    }
}

impl<T> Drop for PcmQueue<T> {
    fn drop(&mut self) {
        // Last Arc is released only after users have retired. Every outstanding
        // value is initialized and uniquely owned by this now-exclusive ring.
        while let Ok(value) = self.try_recv() {
            drop(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capacity_ordering_and_many_wraparounds() {
        for capacity in [2, 3, 19, 71] {
            let queue = PcmQueue::new(capacity);
            for cycle in 0..1000 {
                for index in 0..capacity {
                    queue.try_send(cycle * capacity + index).unwrap();
                }
                assert!(!queue.has_capacity());
                assert_eq!(queue.try_send(usize::MAX), Err(usize::MAX));
                for index in 0..capacity {
                    assert_eq!(queue.try_recv(), Ok(cycle * capacity + index));
                }
                assert_eq!(queue.try_recv(), Err(PopError::Empty));
            }
        }
    }

    #[test]
    fn sequence_counters_wrap_even_at_non_power_of_two_capacity() {
        for capacity in [2, 3, 19] {
            let queue = PcmQueue::new(capacity);
            let base = usize::MAX & !(queue.lap - 1);
            queue.head.store(base, Ordering::Relaxed);
            queue.tail.store(base, Ordering::Relaxed);
            for (index, slot) in queue.slots.iter().enumerate() {
                slot.sequence.store(base + index, Ordering::Relaxed);
            }
            for cycle in 0..10 {
                for index in 0..capacity {
                    queue.try_send(cycle * capacity + index).unwrap();
                }
                assert_eq!(queue.len(), capacity);
                for index in 0..capacity {
                    assert_eq!(queue.try_recv(), Ok(cycle * capacity + index));
                }
                assert!(queue.is_empty());
            }
        }
    }

    #[test]
    fn released_head_snapshot_is_contention_not_empty() {
        let queue = PcmQueue::new(3);
        queue.try_send(1).unwrap();
        queue.try_send(2).unwrap();
        let stale = queue.head.load(Ordering::Relaxed);
        assert_eq!(queue.try_recv(), Ok(1));
        assert_eq!(queue.claim_head(stale), Err(PopError::Contended));
        assert_eq!(queue.try_recv(), Ok(2));
        assert_eq!(queue.try_recv(), Err(PopError::Empty));
    }

    #[test]
    fn competing_readers_own_each_block_exactly_once() {
        let queue = PcmQueue::new(1002);
        for index in 0..1000 {
            queue.try_send(index).unwrap();
        }
        std::thread::scope(|scope| {
            let receive = || {
                let mut values = Vec::new();
                // Finite workload; contention is returned to the caller.
                for _ in 0..2000 {
                    if let Ok(value) = queue.try_recv() {
                        values.push(value);
                    }
                }
                values
            };
            let first = scope.spawn(receive);
            let second = scope.spawn(receive);
            let mut values = first.join().unwrap();
            values.extend(second.join().unwrap());
            values.sort();
            assert_eq!(values, (0..1000).collect::<Vec<_>>());
        });
    }

    #[test]
    fn queued_and_claimed_values_have_exactly_one_drop_owner() {
        struct Counted(usize, std::sync::Arc<[AtomicUsize; 32]>);
        impl Drop for Counted {
            fn drop(&mut self) {
                self.1[self.0].fetch_add(1, Ordering::Relaxed);
            }
        }
        let counts = std::sync::Arc::new(std::array::from_fn(|_| AtomicUsize::new(0)));
        let queue = PcmQueue::new(32);
        for index in 0..32 {
            queue
                .try_send(Counted(index, counts.clone()))
                .unwrap_or_else(|_| panic!("initial capacity"));
        }
        let claimed: Vec<_> = (0..12).map(|_| queue.try_recv().unwrap()).collect();
        drop(queue);
        for index in 0..32 {
            assert_eq!(
                counts[index].load(Ordering::Relaxed),
                usize::from(index >= 12)
            );
        }
        drop(claimed);
        assert!(
            counts
                .iter()
                .all(|count| count.load(Ordering::Relaxed) == 1)
        );
    }
}
