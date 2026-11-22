//! Single-slot "newest wins" channel (§6 rule 9).
//!
//! Each pipeline stage (capture → encode → send) hands work to the next
//! through one of these. A slow consumer never builds a queue: an unread
//! value is replaced by the newer one and counted as dropped.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[derive(Debug)]
struct Shared<T> {
    slot: Mutex<Slot<T>>,
    cv: Condvar,
    dropped: AtomicU64,
}

#[derive(Debug)]
struct Slot<T> {
    value: Option<T>,
    closed: bool,
}

#[derive(Debug)]
pub struct Sender<T>(Arc<Shared<T>>);

#[derive(Debug)]
pub struct Receiver<T>(Arc<Shared<T>>);

pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let s = Arc::new(Shared {
        slot: Mutex::new(Slot {
            value: None,
            closed: false,
        }),
        cv: Condvar::new(),
        dropped: AtomicU64::new(0),
    });
    (Sender(s.clone()), Receiver(s))
}

impl<T> Sender<T> {
    /// Publish `v`, replacing any unread value.
    pub fn send(&self, v: T) {
        let mut g = self.0.slot.lock().expect("poisoned");
        if g.value.replace(v).is_some() {
            self.0.dropped.fetch_add(1, Ordering::Relaxed);
        }
        drop(g);
        self.0.cv.notify_one();
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if let Ok(mut g) = self.0.slot.lock() {
            g.closed = true;
        }
        self.0.cv.notify_all();
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum RecvError {
    Timeout,
    Closed,
}

impl<T> Receiver<T> {
    /// Wait for the newest value.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<T, RecvError> {
        let g = self.0.slot.lock().expect("poisoned");
        let (mut g, _) = self
            .0
            .cv
            .wait_timeout_while(g, timeout, |s| s.value.is_none() && !s.closed)
            .expect("poisoned");
        match g.value.take() {
            Some(v) => Ok(v),
            None if g.closed => Err(RecvError::Closed),
            None => Err(RecvError::Timeout),
        }
    }

    pub fn try_recv(&self) -> Option<T> {
        self.0.slot.lock().expect("poisoned").value.take()
    }

    /// Values overwritten before being read.
    pub fn dropped(&self) -> u64 {
        self.0.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_wins() {
        let (tx, rx) = channel();
        tx.send(1);
        tx.send(2);
        tx.send(3);
        assert_eq!(rx.try_recv(), Some(3));
        assert_eq!(rx.try_recv(), None);
        assert_eq!(rx.dropped(), 2);
    }

    #[test]
    fn timeout_and_close() {
        let (tx, rx) = channel::<u8>();
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(5)),
            Err(RecvError::Timeout)
        );
        drop(tx);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)),
            Err(RecvError::Closed)
        );
    }

    #[test]
    fn cross_thread() {
        let (tx, rx) = channel();
        let h = std::thread::spawn(move || rx.recv_timeout(Duration::from_secs(5)));
        tx.send(42);
        assert_eq!(h.join().unwrap(), Ok(42));
    }
}
