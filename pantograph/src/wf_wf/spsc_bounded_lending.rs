//! A bounded, lending single-producer, single-consumer channel.
//!
//! The [`Sender`] and [`Receiver`] are wait-free.
//!
//! When the channel is full, because the receiver has not consumed old values, the sender's
//! [`Sender::try_send`] operation fails.
//!
//! The channel retains ownership of values; the sender and receiver get `&mut T` access to their
//! respective slots.
//!
//! # Internal details
//!
//! There are `capacity + 2` physical slots. The sender and receiver maintain atomic sequence
//! counters that track the read and write positions to synchronize slot access. The sender and
//! receiver always occupy distinct slots, so the channel can lend out those slots' contents
//! exclusively.

#![expect(missing_debug_implementations, reason = "Deferred")]

use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use alloc::boxed::Box;
use alloc::sync::Arc;

use crossbeam_utils::CachePadded;

/// Errors returned by [`Sender::publish`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TryPublishError {
    /// The channel is full.
    Full,

    /// The [`Receiver`] has disconnected.
    Disconnected,
}

/// Errors returned by [`Sender::try_send`].
///
/// These are the same as [`TryPublishError`] but return the input value.
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
    /// The ring buffer.
    slots: Box<[CachePadded<UnsafeCell<T>>]>,

    /// Number of values published by the sender. Written by the sender, read by the receiver.
    sender_seq: CachePadded<AtomicU32>,
    /// Number of values consumed by the receiver. Written by the receiver, read by the sender.
    receiver_seq: CachePadded<AtomicU32>,

    /// The capacity of the channel. The ring buffer's size is `capacity + 2`.
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
    /// Get the sender's staged value.
    pub fn get(&self) -> &T {
        // Safety: the sender exclusively owns the slot at `self.seq`.
        unsafe {
            &*self
                .shared
                .slots
                .get_unchecked(self.seq as usize % self.shared.slots.len())
                .get()
        }
    }

    /// Get the sender's staged value mutably.
    ///
    /// Use [`publish`](Self::publish) to publish the staged value.
    pub fn get_mut(&mut self) -> &mut T {
        // Safety: the sender exclusively owns the slot at `self.seq`.
        unsafe {
            &mut *self
                .shared
                .slots
                .get_unchecked(self.seq as usize % self.shared.slots.len())
                .get()
        }
    }

    /// Returns `true` if the channel is full.
    ///
    /// When this returns `false`, a subsequent [`try_send`](Self::try_send) will not fail with
    /// [`TrySendError::Full`] (it may still return [`TrySendError::Disconnected`]).
    ///
    /// This does not check whether the [`Receiver`] has disconnected. Use
    /// [`is_connected`](Self::is_connected) for that.
    pub fn is_full(&mut self) -> bool {
        if self.seq.wrapping_sub(self.cached_receiver_seq) <= self.shared.capacity {
            return false;
        }

        // This synchronizes with the receiver's `Release` store of `receiver_seq`, ensuring it
        // will have consumed the slot before we start writing to it.
        self.cached_receiver_seq = self.shared.receiver_seq.load(Ordering::Acquire);
        self.seq.wrapping_sub(self.cached_receiver_seq) > self.shared.capacity
    }

    /// Publish the value, calling `f` with the staged value if the publish will succeed.
    ///
    /// This allows us to write the atomics only once, with [`Self::publish`] calling with `f` a
    /// no-op and [`Self::try_send`] swapping the staged value out.
    #[inline(always)]
    fn publish_with(&mut self, f: impl FnOnce(&mut T)) -> Result<(), TryPublishError> {
        if self.shared.receiver_disconnected.load(Ordering::Relaxed) {
            return Err(TryPublishError::Disconnected);
        }
        if self.is_full() {
            return Err(TryPublishError::Full);
        }
        f(self.get_mut());
        self.seq = self.seq.wrapping_add(1);
        self.shared.sender_seq.store(self.seq, Ordering::Release);
        Ok(())
    }

    /// Publish the staged value into the channel.
    ///
    /// The sender's new staged value is an old value from the channel.
    pub fn publish(&mut self) -> Result<(), TryPublishError> {
        self.publish_with(
            #[inline(always)]
            |_| (),
        )
    }

    /// Replace the staged value and publish it. Returns the old staged value on success, or the
    /// new value back on failure.
    ///
    /// This is equivalent to replacing the value from [`Self::get_mut`] followed by
    /// [`Self::publish`].
    pub fn try_send(&mut self, mut val: T) -> Result<T, TrySendError<T>> {
        match self.publish_with(
            #[inline(always)]
            |slot| core::mem::swap(slot, &mut val),
        ) {
            Ok(()) => Ok(val),
            Err(TryPublishError::Full) => Err(TrySendError::Full(val)),
            Err(TryPublishError::Disconnected) => Err(TrySendError::Disconnected(val)),
        }
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
    /// Get the receiver's current value.
    pub fn get(&self) -> &T {
        // Safety: the receiver exclusively owns the slot at `self.seq`.
        unsafe {
            &*self
                .shared
                .slots
                .get_unchecked(self.seq as usize % self.shared.slots.len())
                .get()
        }
    }

    /// Get the receiver's current value mutably.
    pub fn get_mut(&mut self) -> &mut T {
        // Safety: the receiver exclusively owns the slot at `self.seq`.
        unsafe {
            &mut *self
                .shared
                .slots
                .get_unchecked(self.seq as usize % self.shared.slots.len())
                .get()
        }
    }

    /// Try to receive a value from the channel.
    ///
    /// Returns the value if one was available. Returns [`TryRecvError::Empty`] if the channel is
    /// empty, or [`TryRecvError::Disconnected`] if the channel is empty and the sender has been
    /// dropped.
    pub fn try_recv(&mut self) -> Result<&mut T, TryRecvError> {
        if self.cached_sender_seq.wrapping_sub(self.seq) < 2 {
            // Cache says the channel is empty. Reload the value, as the sender may have published
            // values since our last read.
            //
            // This synchronizes with the sender's `Release` store of `sender_seq`, ensuring we see
            // the slot data the sender wrote.
            self.cached_sender_seq = self.shared.sender_seq.load(Ordering::Acquire);

            if self.cached_sender_seq.wrapping_sub(self.seq) < 2 {
                // Still empty. Check if the sender has disconnected.
                //
                // Acquire the disconnect flag. If set, the `Release` of the sender's setting of
                // `sender_disconnected` ensures we see all prior stores, including any bumps of
                // `sender_seq` that had not been synchronized yet.
                if self.shared.sender_disconnected.load(Ordering::Acquire) {
                    self.cached_sender_seq = self.shared.sender_seq.load(Ordering::Relaxed);
                    if self.cached_sender_seq.wrapping_sub(self.seq) < 2 {
                        return Err(TryRecvError::Disconnected);
                    }
                } else {
                    return Err(TryRecvError::Empty);
                }
            }
        }

        self.seq = self.seq.wrapping_add(1);
        self.shared.receiver_seq.store(self.seq, Ordering::Release);

        Ok(self.get_mut())
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

/// Construct a bounded, lending channel with the given capacity.
///
/// All slots are initialized with [`Default::default`]. The capacity must be at least 1, and at
/// most `u32::MAX - 2`.
pub fn channel<T: Default>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    channel_with(capacity, || T::default())
}

/// Construct a bounded, lending channel with the given capacity, initializing all slots by
/// repeatedly calling `init`.
///
/// The capacity must be at least 1, and at most `u32::MAX - 2`. `init` is called `capacity + 2`
/// times.
pub fn channel_with<T>(capacity: usize, mut init: impl FnMut() -> T) -> (Sender<T>, Receiver<T>) {
    assert!(capacity >= 1, "capacity must be at least 1");
    assert!(
        capacity <= u32::MAX as usize - 2,
        "capacity must be at most `u32::MAX - 2`"
    );
    #[expect(
        clippy::cast_possible_truncation,
        reason = "By the assert above, this does not truncate"
    )]
    let capacity = capacity as u32;
    let buffer_size = capacity + 2;

    let slots: Box<[_]> = (0..buffer_size)
        .map(|_| CachePadded::new(UnsafeCell::new(init())))
        .collect();

    let shared = Arc::new(Shared {
        slots,
        capacity,
        sender_seq: CachePadded::new(AtomicU32::new(1)),
        receiver_seq: CachePadded::new(AtomicU32::new(0)),
        sender_disconnected: AtomicBool::new(false),
        receiver_disconnected: AtomicBool::new(false),
    });

    let tx = Sender {
        shared: shared.clone(),
        seq: 1,
        cached_receiver_seq: 0,
    };

    let rx = Receiver {
        shared,
        seq: 0,
        cached_sender_seq: 1,
    };

    (tx, rx)
}

#[cfg(test)]
mod test {
    extern crate std;

    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::{TryPublishError, TryRecvError, TrySendError, channel, channel_with};

    #[test]
    fn basic() {
        let (mut tx, mut rx) = channel::<u64>(4);

        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        tx.try_send(3).unwrap();
        tx.try_send(4).unwrap();
        assert!(matches!(tx.try_send(5), Err(TrySendError::Full(_))));

        assert_eq!(*rx.try_recv().unwrap(), 1);
        assert_eq!(*rx.try_recv().unwrap(), 2);
        assert_eq!(*rx.try_recv().unwrap(), 3);
        assert_eq!(*rx.try_recv().unwrap(), 4);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn capacity_one() {
        let (mut tx, mut rx) = channel::<u64>(1);

        tx.try_send(1).unwrap();
        assert_eq!(tx.try_send(2), Err(TrySendError::Full(2)));
        assert_eq!(*rx.try_recv().unwrap(), 1);

        tx.try_send(2).unwrap();
        assert_eq!(*rx.try_recv().unwrap(), 2);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn drops() {
        let a = Arc::new(1);
        let b = Arc::new(2);

        {
            let (mut tx, mut rx) = channel_with(2, || Arc::new(0));

            tx.try_send(a.clone()).unwrap();
            tx.try_send(b.clone()).unwrap();
            assert_eq!(Arc::strong_count(&a), 2);
            assert_eq!(Arc::strong_count(&b), 2);

            // Channel full, value is returned.
            assert!(matches!(tx.try_send(a.clone()), Err(TrySendError::Full(_))));
            assert_eq!(Arc::strong_count(&a), 2);

            // Consume a. The lending channel still holds a in the slot.
            let received = rx.try_recv().unwrap();
            assert_eq!(**received, 1);
            assert_eq!(Arc::strong_count(&a), 2);
            assert_eq!(Arc::strong_count(&b), 2);
        }

        // Dropping the channel drops all slot values.
        assert_eq!(Arc::strong_count(&a), 1);
        assert_eq!(Arc::strong_count(&b), 1);
    }

    #[test]
    fn sender_disconnect() {
        let (mut tx, mut rx) = channel::<u64>(4);

        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        drop(tx);

        assert_eq!(*rx.try_recv().unwrap(), 1);
        assert_eq!(*rx.try_recv().unwrap(), 2);
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
                        Ok(_) => {
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
                    let value = *value;
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
            let (mut tx, mut rx) = channel_with(4, || val.clone());

            let t = {
                let val = val.clone();
                std::thread::spawn(move || {
                    loop {
                        for _ in 0..100 {
                            match tx.try_send(val.clone()) {
                                Ok(_) | Err(TrySendError::Full(_)) => {}
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

    #[test]
    fn publish() {
        let (mut tx, mut rx) = channel::<u64>(4);

        // Publish the default staged value.
        tx.publish().unwrap();
        assert_eq!(*rx.try_recv().unwrap(), 0);

        // Mutate staged value in place, then publish.
        *tx.get_mut() = 42;
        tx.publish().unwrap();
        assert_eq!(*rx.try_recv().unwrap(), 42);

        // Fill the channel to capacity, then publish should fail with Full.
        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        tx.try_send(3).unwrap();
        tx.try_send(4).unwrap();
        assert_eq!(tx.publish(), Err(TryPublishError::Full));

        // Drain one slot, publish succeeds again.
        assert_eq!(*rx.try_recv().unwrap(), 1);
        *tx.get_mut() = 99;
        tx.publish().unwrap();
        assert_eq!(*rx.try_recv().unwrap(), 2);
        assert_eq!(*rx.try_recv().unwrap(), 3);
        assert_eq!(*rx.try_recv().unwrap(), 4);
        assert_eq!(*rx.try_recv().unwrap(), 99);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));

        // Publish after receiver disconnect returns Disconnected.
        drop(rx);
        assert_eq!(tx.publish(), Err(TryPublishError::Disconnected));
    }
}
