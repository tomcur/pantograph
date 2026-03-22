//! A bounded single-producer, single-consumer channel.
//!
//! The [`Sender`] and [`Receiver`] are wait-free.
//!
//! When the channel is full, because the receiver has not consumed old values, the sender's
//! [`try_send`](Sender::try_send) returns the value back.
//!
//! # Internal details
//!
//! There are `capacity` physical slots. The sender and receiver maintain atomic sequence counters
//! that track the read and write positions to synchronize slot access.

#![expect(missing_debug_implementations, reason = "Deferred")]

use core::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use alloc::boxed::Box;
use alloc::sync::Arc;

use crossbeam_utils::CachePadded;

/// Errors returned by [`Sender::try_send`].
#[derive(Debug, PartialEq, Eq)]
pub enum TrySendError<T> {
    /// The channel is full. The value is returned.
    Full(T),

    /// The [`Receiver`] has disconnected. The value is returned.
    Disconnected(T),
}

/// Errors returned by [`Receiver::try_recv`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TryRecvError {
    /// The channel is empty.
    Empty,

    /// The channel is empty and the [`Sender`] has disconnected.
    Disconnected,
}

struct Shared<T> {
    slots: Box<[CachePadded<UnsafeCell<MaybeUninit<T>>>]>,

    /// Number of values published by the sender. Written by the sender, read by the receiver.
    sender_seq: CachePadded<AtomicU32>,
    /// Number of values consumed by the receiver. Written by the receiver, read by the sender.
    receiver_seq: CachePadded<AtomicU32>,

    capacity: u32,

    sender_disconnected: AtomicBool,
    receiver_disconnected: AtomicBool,
}

/// The sender side of the channel.
pub struct Sender<T> {
    shared: Arc<Shared<T>>,
    /// Local sequence number, always equal to the shared `sender_seq`.
    seq: u32,
    /// Snapshot of `receiver_seq`.
    ///
    /// This may be stale, lagging behind the actual value; i.e., except for wrapping around the
    /// `u32`, this is always `<=` the actual value.
    cached_receiver_seq: u32,
}

/// The receiver side of the channel.
pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
    /// Local sequence number, always equal to the shared `receiver_seq`.
    seq: u32,
    /// Snapshot of `sender_seq`.
    ///
    /// This may be stale, lagging behind the actual value; i.e., except for wrapping around the
    /// `u32`, this is always `<=` the actual value.
    cached_sender_seq: u32,
}

unsafe impl<T: Send> Send for Sender<T> {}
unsafe impl<T: Send> Send for Receiver<T> {}

impl<T> Sender<T> {
    /// Returns `true` if the channel is full.
    ///
    /// When this returns `false`, a subsequent [`try_send`](Self::try_send) will not fail with
    /// [`TrySendError::Full`] (it may still return [`TrySendError::Disconnected`]).
    ///
    /// This does not check whether the [`Receiver`] has disconnected. Use
    /// [`is_connected`](Self::is_connected) for that.
    pub fn is_full(&mut self) -> bool {
        if self.seq.wrapping_sub(self.cached_receiver_seq) < self.shared.capacity {
            return false;
        }

        // This synchronizes with the receiver's `Release` store of `receiver_seq`, ensuring it
        // will have consumed the slot before we start writing to it.
        self.cached_receiver_seq = self.shared.receiver_seq.load(Ordering::Acquire);
        self.seq.wrapping_sub(self.cached_receiver_seq) == self.shared.capacity
    }

    /// Try to send a value into the channel.
    ///
    /// Returns `Ok(())` if the value was successfully published. Returns the value back if the
    /// receiver has disconnected ([`TrySendError::Disconnected`]) or the channel is full
    /// ([`TrySendError::Full`]).
    ///
    /// Use [`Self::is_full`] to check for capacity.
    pub fn try_send(&mut self, val: T) -> Result<(), TrySendError<T>> {
        if self.shared.receiver_disconnected.load(Ordering::Relaxed) {
            return Err(TrySendError::Disconnected(val));
        }

        if self.is_full() {
            return Err(TrySendError::Full(val));
        }

        // Safety: we have confirmed there's room in the buffer, so this slot is not being accessed
        // by the receiver. The slot will be uninitialized, as this is either its first use or was
        // previously consumed by the receiver.
        unsafe {
            self.shared
                .slots
                .get_unchecked((self.seq % self.shared.capacity) as usize)
                .get()
                .write(MaybeUninit::new(val));
        }

        self.seq = self.seq.wrapping_add(1);
        self.shared.sender_seq.store(self.seq, Ordering::Release);

        Ok(())
    }

    /// Returns `true` if the [`Receiver`] is still alive.
    pub fn is_connected(&self) -> bool {
        !self.shared.receiver_disconnected.load(Ordering::Relaxed)
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.shared
            .sender_disconnected
            .store(true, Ordering::Release);
    }
}

impl<T> Receiver<T> {
    /// Try to receive a value from the channel.
    ///
    /// Returns the value if one was available. Returns [`TryRecvError::Empty`] if the channel is
    /// empty, or [`TryRecvError::Disconnected`] if the channel is empty and the sender has been
    /// dropped.
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        if self.cached_sender_seq == self.seq {
            // Cache says the channel is empty. Reload the value, as the sender may have published
            // values since our last read.
            //
            // This synchronizes with the sender's `Release` store of `sender_seq`, ensuring we see
            // the slot data the sender wrote.
            self.cached_sender_seq = self.shared.sender_seq.load(Ordering::Acquire);

            if self.cached_sender_seq == self.seq {
                // Still empty. Check if the sender has disconnected.
                //
                // Acquire the disconnect flag. If set, the `Release` of the sender's setting of
                // `sender_disconnected` ensures we see all prior stores, including any bumps of
                // `sender_seq` that had not been synchronized yet.
                if self.shared.sender_disconnected.load(Ordering::Acquire) {
                    self.cached_sender_seq = self.shared.sender_seq.load(Ordering::Relaxed);
                    if self.cached_sender_seq == self.seq {
                        return Err(TryRecvError::Disconnected);
                    }
                } else {
                    return Err(TryRecvError::Empty);
                }
            }
        }

        // Safety: the channel is not empty, so the sender has published at least one more value
        // than we have consumed. This slot contains an initialized value that is not concurrently
        // accessed by the sender.
        let val = unsafe {
            (*self
                .shared
                .slots
                .get_unchecked((self.seq % self.shared.capacity) as usize)
                .get())
            .assume_init_read()
        };

        self.seq = self.seq.wrapping_add(1);
        self.shared.receiver_seq.store(self.seq, Ordering::Release);

        Ok(val)
    }

    /// Returns `true` if the [`Sender`] is still alive.
    pub fn is_connected(&self) -> bool {
        !self.shared.sender_disconnected.load(Ordering::Relaxed)
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.shared
            .receiver_disconnected
            .store(true, Ordering::Relaxed);
    }
}

impl<T> Drop for Shared<T> {
    fn drop(&mut self) {
        if core::mem::needs_drop::<T>() {
            let sender_seq = *self.sender_seq.get_mut();
            let mut receiver_seq = *self.receiver_seq.get_mut();
            while receiver_seq != sender_seq {
                unsafe {
                    (*self
                        .slots
                        .get_unchecked((receiver_seq % self.capacity) as usize)
                        .get())
                    .assume_init_drop();
                }
                receiver_seq = receiver_seq.wrapping_add(1);
            }
        }
    }
}

/// Construct a bounded queue with the given capacity.
///
/// The capacity must be at least 1 and at most `u32::MAX`.
pub fn channel<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    assert!(capacity >= 1, "capacity must be at least 1");
    assert!(
        capacity <= u32::MAX as usize,
        "capacity must be at most `u32::MAX`"
    );
    #[expect(
        clippy::cast_possible_truncation,
        reason = "By the assert above, this does not truncate"
    )]
    let capacity = capacity as u32;

    let slots: Box<[_]> = (0..capacity)
        .map(|_| CachePadded::new(UnsafeCell::new(MaybeUninit::uninit())))
        .collect();

    let shared = Arc::new(Shared {
        slots,
        sender_seq: CachePadded::new(AtomicU32::new(0)),
        receiver_seq: CachePadded::new(AtomicU32::new(0)),
        capacity,
        sender_disconnected: AtomicBool::new(false),
        receiver_disconnected: AtomicBool::new(false),
    });

    let tx = Sender {
        shared: shared.clone(),
        seq: 0,
        cached_receiver_seq: 0,
    };

    let rx = Receiver {
        shared,
        seq: 0,
        cached_sender_seq: 0,
    };

    (tx, rx)
}

#[cfg(test)]
mod test {
    extern crate std;

    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::{TryRecvError, TrySendError, channel};

    #[test]
    fn basic() {
        let (mut tx, mut rx) = channel::<u64>(4);

        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        tx.try_send(3).unwrap();
        tx.try_send(4).unwrap();
        assert!(matches!(tx.try_send(5), Err(TrySendError::Full(_))));

        assert_eq!(rx.try_recv(), Ok(1));
        assert_eq!(rx.try_recv(), Ok(2));
        assert_eq!(rx.try_recv(), Ok(3));
        assert_eq!(rx.try_recv(), Ok(4));
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn capacity_one() {
        let (mut tx, mut rx) = channel::<u64>(1);

        tx.try_send(1).unwrap();
        assert_eq!(tx.try_send(2), Err(TrySendError::Full(2)));
        assert_eq!(rx.try_recv(), Ok(1));

        tx.try_send(2).unwrap();
        assert_eq!(rx.try_recv(), Ok(2));
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn drops() {
        let a = Arc::new(1);
        let b = Arc::new(2);

        {
            let (mut tx, mut rx) = channel(2);

            tx.try_send(a.clone()).unwrap();
            tx.try_send(b.clone()).unwrap();
            assert_eq!(Arc::strong_count(&a), 2);
            assert_eq!(Arc::strong_count(&b), 2);

            // Channel full, value is returned.
            assert!(matches!(tx.try_send(a.clone()), Err(TrySendError::Full(_))));
            assert_eq!(Arc::strong_count(&a), 2);

            // Consume a. Moves it out of the channel.
            assert_eq!(*rx.try_recv().unwrap(), 1);
            assert_eq!(Arc::strong_count(&a), 1);
            assert_eq!(Arc::strong_count(&b), 2);
        }

        assert_eq!(Arc::strong_count(&a), 1);
        assert_eq!(Arc::strong_count(&b), 1);
    }

    #[test]
    fn sender_disconnect() {
        let (mut tx, mut rx) = channel::<u64>(4);

        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        drop(tx);

        assert_eq!(rx.try_recv(), Ok(1));
        assert_eq!(rx.try_recv(), Ok(2));
        assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
    }

    #[test]
    fn receiver_disconnect() {
        let (mut tx, rx) = channel::<u64>(4);

        tx.try_send(1).unwrap();
        drop(rx);

        assert!(!tx.is_connected());
        assert_eq!(tx.try_send(2), Err(TrySendError::Disconnected(2)));
    }

    #[test]
    fn multithread_ordering() {
        let (mut tx, mut rx) = channel::<u64>(16);

        let producer = std::thread::spawn(move || {
            let mut total = 0_u64;
            let now = Instant::now();
            for value in 1..1_000_000_u64 {
                if now.elapsed() > Duration::from_secs(1) {
                    break;
                }
                loop {
                    match tx.try_send(value) {
                        Ok(()) => {
                            total += value;
                            break;
                        }
                        Err(TrySendError::Full(_)) => std::thread::yield_now(),
                        Err(TrySendError::Disconnected(_)) => return total,
                    }
                }
            }
            total
        });

        let mut prev = None;
        let mut received_total = 0_u64;
        loop {
            match rx.try_recv() {
                Ok(value) => {
                    if let Some(prev_value) = prev {
                        assert!(
                            prev_value < value,
                            "previous value {} not smaller than current value {}",
                            prev_value,
                            value,
                        );
                    }
                    prev = Some(value);
                    received_total += value;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => break,
            }
        }

        let expected = producer.join().unwrap();
        assert_eq!(
            received_total, expected,
            "sum of sent and received values must be equal"
        );
    }

    #[test]
    fn multithread_drop() {
        let val = Arc::new(42);

        {
            let (mut tx, mut rx) = channel(4);

            let t = {
                let val = val.clone();
                std::thread::spawn(move || {
                    loop {
                        for _ in 0..100 {
                            match tx.try_send(val.clone()) {
                                Ok(()) | Err(TrySendError::Full(_)) => {}
                                Err(TrySendError::Disconnected(_)) => return,
                            }
                        }
                    }
                })
            };

            let now = Instant::now();
            while now.elapsed() < Duration::from_secs(1) {
                let _ = rx.try_recv();
            }
            drop(rx);
            t.join().expect("join");
        }

        assert_eq!(Arc::strong_count(&val), 1);
    }
}
