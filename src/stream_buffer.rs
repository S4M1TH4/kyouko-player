//! One-track compressed audio cache. The producer blocks on its input pipe;
//! readers block on a condition variable until bytes arrive. No idle polling.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use symphonia::core::io::MediaSource;

pub const MAX_STREAM_BYTES: usize = 20 * 1024 * 1024;

#[derive(Default)]
struct Bytes {
    data: Vec<u8>,
    done: bool,
    cancelled: bool,
    error: Option<String>,
}

pub struct StreamBuffer {
    bytes: Mutex<Bytes>,
    ready: Condvar,
    downloaded_only: AtomicBool,
    limit: usize,
}

impl StreamBuffer {
    pub fn new() -> Arc<Self> {
        Self::with_limit(MAX_STREAM_BYTES)
    }

    fn with_limit(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            bytes: Mutex::new(Bytes::default()),
            ready: Condvar::new(),
            downloaded_only: AtomicBool::new(false),
            limit,
        })
    }

    pub fn append(&self, chunk: &[u8]) -> io::Result<()> {
        let mut bytes = self.bytes.lock().unwrap_or_else(|e| e.into_inner());
        if bytes.cancelled || bytes.done {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "stream download stopped",
            ));
        }
        let required = bytes
            .data
            .len()
            .checked_add(chunk.len())
            .filter(|&n| n <= self.limit)
            .ok_or_else(|| io::Error::other("stream exceeds the 20 MiB RAM limit"))?;
        if required > bytes.data.capacity() {
            // One allocation avoids a transient old+new Vec allocation
            // exceeding the cap when a large track grows near 20 MiB.
            let additional = self.limit - bytes.data.len();
            bytes
                .data
                .try_reserve_exact(additional)
                .map_err(io::Error::other)?;
        }
        bytes.data.extend_from_slice(chunk);
        self.ready.notify_all();
        Ok(())
    }

    pub fn finish(&self, result: io::Result<()>) {
        let mut bytes = self.bytes.lock().unwrap_or_else(|e| e.into_inner());
        bytes.done = true;
        bytes.error = result.err().map(|error| error.to_string());
        self.ready.notify_all();
    }

    pub fn cancel(&self) {
        let mut bytes = self.bytes.lock().unwrap_or_else(|e| e.into_inner());
        if !bytes.done {
            bytes.cancelled = true;
        }
        self.ready.notify_all();
    }

    pub fn complete(&self) -> bool {
        let bytes = self.bytes.lock().unwrap_or_else(|e| e.into_inner());
        bytes.done && !bytes.cancelled && bytes.error.is_none()
    }

    pub fn downloaded_seek(&self) -> DownloadedSeek<'_> {
        self.downloaded_only.store(true, Ordering::Release);
        DownloadedSeek(self)
    }

    pub fn reader(self: &Arc<Self>) -> MemoryReader {
        MemoryReader {
            buffer: self.clone(),
            position: 0,
        }
    }
}

/// During a user seek, refuse uncached reads/offsets instead of waiting for
/// network data. Normal decoding/probing may wait for the producer.
pub struct DownloadedSeek<'a>(&'a StreamBuffer);

impl Drop for DownloadedSeek<'_> {
    fn drop(&mut self) {
        self.0.downloaded_only.store(false, Ordering::Release);
    }
}

pub struct MemoryReader {
    buffer: Arc<StreamBuffer>,
    position: u64,
}

impl Read for MemoryReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let mut bytes = self.buffer.bytes.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if bytes.cancelled {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "stream cancelled",
                ));
            }
            if let Some(error) = &bytes.error {
                return Err(io::Error::other(error.clone()));
            }
            if self.position < bytes.data.len() as u64 {
                let start = self.position as usize;
                let count = out.len().min(bytes.data.len() - start);
                out[..count].copy_from_slice(&bytes.data[start..start + count]);
                self.position += count as u64;
                return Ok(count);
            }
            if bytes.done {
                return Ok(0);
            }
            if self.buffer.downloaded_only.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "seek target is not downloaded yet",
                ));
            }
            bytes = self
                .buffer
                .ready
                .wait(bytes)
                .unwrap_or_else(|e| e.into_inner());
        }
    }
}

impl Seek for MemoryReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let mut bytes = self.buffer.bytes.lock().unwrap_or_else(|e| e.into_inner());
        let base = match from {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::Current(n) => self.position as i128 + n as i128,
            SeekFrom::End(n) => {
                while !bytes.done && !bytes.cancelled {
                    if self.buffer.downloaded_only.load(Ordering::Acquire) {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "stream length is not known yet",
                        ));
                    }
                    bytes = self
                        .buffer
                        .ready
                        .wait(bytes)
                        .unwrap_or_else(|e| e.into_inner());
                }
                bytes.data.len() as i128 + n as i128
            }
        };
        if bytes.cancelled {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "stream cancelled",
            ));
        }
        if let Some(error) = &bytes.error {
            return Err(io::Error::other(error.clone()));
        }
        if base < 0 || base > self.buffer.limit as i128 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid stream offset",
            ));
        }
        if base > bytes.data.len() as i128 && self.buffer.downloaded_only.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "seek target is not downloaded yet",
            ));
        }
        self.position = base as u64;
        Ok(self.position)
    }
}

impl MediaSource for MemoryReader {
    // MP4/MKV probe requires a known total length if it sees a seekable
    // source. Use progressive parsing until complete, then expose random
    // access; a user seek can always access already downloaded bytes.
    fn is_seekable(&self) -> bool {
        self.buffer.complete() || self.buffer.downloaded_only.load(Ordering::Acquire)
    }
    fn byte_len(&self) -> Option<u64> {
        let bytes = self.buffer.bytes.lock().unwrap_or_else(|e| e.into_inner());
        (bytes.done || self.buffer.downloaded_only.load(Ordering::Acquire))
            .then_some(bytes.data.len() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seek_math_and_replay_read_the_same_allocation() {
        let buffer = StreamBuffer::new();
        buffer.append(b"0123456789").unwrap();
        buffer.finish(Ok(()));
        let mut reader = buffer.reader();
        assert_eq!(reader.seek(SeekFrom::Start(5)).unwrap(), 5);
        assert_eq!(reader.seek(SeekFrom::Current(-2)).unwrap(), 3);
        assert_eq!(reader.seek(SeekFrom::End(-2)).unwrap(), 8);
        let mut tail = [0; 2];
        reader.read_exact(&mut tail).unwrap();
        assert_eq!(&tail, b"89");
        assert!(reader.seek(SeekFrom::Current(-11)).is_err());
        assert_eq!(reader.stream_position().unwrap(), 10);
        let mut replay = buffer.reader();
        assert!(Arc::ptr_eq(&reader.buffer, &replay.buffer));
        replay.read_exact(&mut tail).unwrap();
        assert_eq!(&tail, b"01");
    }

    #[test]
    fn downloaded_seek_is_immediate_and_does_not_wait_for_future_bytes() {
        let buffer = StreamBuffer::new();
        buffer.append(b"abcd").unwrap();
        let mut reader = buffer.reader();
        let guard = buffer.downloaded_seek();
        assert_eq!(reader.seek(SeekFrom::Start(2)).unwrap(), 2);
        assert_eq!(
            reader.seek(SeekFrom::Start(5)).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(reader.stream_position().unwrap(), 2);
        assert_eq!(
            reader.seek(SeekFrom::End(0)).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        reader.seek(SeekFrom::Start(4)).unwrap();
        assert_eq!(
            reader.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(guard);
        buffer.append(b"e").unwrap();
        reader.read_exact(&mut [0]).unwrap();
    }

    #[test]
    fn incremental_reader_wakes_on_bytes_and_cancellation() {
        let buffer = StreamBuffer::new();
        let mut reader = buffer.reader();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut bytes = [0; 3];
            reader.read_exact(&mut bytes).unwrap();
            tx.send(bytes).unwrap();
            reader.read(&mut [0]).unwrap_err().kind()
        });
        buffer.append(b"abc").unwrap();
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap(),
            *b"abc"
        );
        buffer.cancel();
        assert_eq!(worker.join().unwrap(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn allocation_is_bounded_and_is_freed_with_the_last_owner() {
        let buffer = StreamBuffer::with_limit(16);
        buffer.append(&[0; 16]).unwrap();
        let overflow = buffer.append(&[0]).unwrap_err();
        buffer.finish(Err(overflow));
        assert!(
            !buffer.complete(),
            "a truncated or over-limit buffer cannot be replayed"
        );
        assert!(buffer.bytes.lock().unwrap().data.capacity() <= 16);
        let weak = Arc::downgrade(&buffer);
        let reader = buffer.reader();
        drop(buffer);
        assert!(weak.upgrade().is_some());
        drop(reader);
        assert!(weak.upgrade().is_none());
    }
}
