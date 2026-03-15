//! A single-producer, single-consumer channel. The sender can keep updating the set value until
//! the receiver has read the last value.
//!
//! The sender and receiver are wait-free.
//!
//! This uses a double buffer internally.

#![expect(missing_debug_implementations, reason = "Deferred")]

use alloc::sync::Arc;
use core::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    sync::atomic::{AtomicBool, AtomicU8, Ordering},
};

use crossbeam_utils::CachePadded;

const INDEX: u8 = 0b01;
const PUBLISHED: u8 = 0b10;

/// Errors that can occur when trying to receive a value using [`Receiver::try_recv`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TryRecvError {
    /// The channel is empty.
    Empty,

    /// The channel is empty and the [`Sender`] side of the channel has disconnected.
    Disconnected,
}

/// The [`Receiver`] side of the channel has disconnected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrySendError;

struct Shared<T> {
    values: [CachePadded<UnsafeCell<MaybeUninit<T>>>; 2],
    stamp: AtomicU8,
    disconnected: AtomicBool,
}

/// The sender side of the accumulating channel.
///
/// Construct the [`Sender`] and [`Receiver`] by calling [`channel`].
pub struct Sender<T> {
    shared: Arc<Shared<T>>,
    write_idx: usize,
}

/// The receiver side of the accumulating channel.
///
/// Construct the [`Sender`] and [`Receiver`] by calling [`channel`].
pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
    read_idx: usize,
}

unsafe impl<T: Send> Send for Sender<T> {}
unsafe impl<T: Send> Send for Receiver<T> {}

impl<T> Sender<T> {
    /// If the [`Receiver`] has read the previous value, send the current value and stage a fresh
    /// one by calling `init`.
    ///
    /// Returns `Ok(true)` if the previous value was sent. The newly staged value can be updated
    /// using [`Self::get_mut`].
    ///
    /// The first call to this method unconditionally publishes the initial value.
    pub fn try_send(&mut self, init: impl FnOnce() -> T) -> Result<bool, TrySendError> {
        // First do a `Relaxed` load to check, as this doesn't require synchronization on most
        // platforms.

        if self.shared.disconnected.load(Ordering::Relaxed) {
            return Err(TrySendError);
        } else if self.shared.stamp.load(Ordering::Relaxed) & PUBLISHED != 0 {
            return Ok(false);
        }

        self.shared.stamp.load(Ordering::Acquire);
        let stamp = self.write_idx as u8 | PUBLISHED;
        self.write_idx = (self.write_idx + 1) % 2;
        let val = init();
        unsafe { *self.shared.values.get_unchecked(self.write_idx).get() = MaybeUninit::new(val) };
        self.shared.stamp.store(stamp, Ordering::Release);
        Ok(true)
    }

    /// Get the currently staged value mutably.
    pub fn get_mut(&mut self) -> &mut T {
        unsafe { (*self.shared.values.get_unchecked(self.write_idx).get()).assume_init_mut() }
    }
}

impl<T> Receiver<T> {
    /// Try to receive a value from the channel.
    ///
    /// If the channel contains a value, this returns `Ok` with that value and signals the
    /// [`Sender`] that a new value can be published.
    ///
    /// Returns an error if the channel is empty. If the [`Sender`] is still alive, the error is
    /// [`TryRecvError::Empty`]. If the [`Sender`] has been dropped, returns returns
    /// [`TryRecvError::Disconnected`].
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        // First do a `Relaxed` load to check, as this doesn't require synchronization on most
        // platforms.
        let mut stamp = self.shared.stamp.load(Ordering::Relaxed);
        let mut disconnected = false;

        if stamp & PUBLISHED == 0 {
            if self.shared.disconnected.load(Ordering::Relaxed) {
                disconnected = true;
                // The channel is empty and the `Sender` has disconnected, but it may be that we
                // saw the disconnect signal before we saw the updated stamp, as both were loaded
                // with `Relaxed` semantics. Load the disconnect signal with `Acquire` semantics
                // and then load `stamp` again.
                self.shared.disconnected.load(Ordering::Acquire);
                stamp = self.shared.stamp.load(Ordering::Relaxed);
            }
        }
        if stamp & PUBLISHED == 0 {
            if disconnected {
                return Err(TryRecvError::Disconnected);
            } else {
                return Err(TryRecvError::Empty);
            }
        }

        self.shared.stamp.load(Ordering::Acquire);
        self.read_idx = (self.read_idx + 1) % 2;
        let val =
            unsafe { (*self.shared.values.get_unchecked(self.read_idx).get()).assume_init_read() };
        self.shared
            .stamp
            .store(stamp & !PUBLISHED, Ordering::Release);
        Ok(val)
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if core::mem::needs_drop::<T>() {
            // Drop the unpublished value.
            unsafe { (*self.shared.values.get_unchecked(self.write_idx).get()).assume_init_read() };
        }
        // The `Sender` disconnect has `Release` semantics, such that on `Sender` disconnect the
        // `Receiver` can acquire all the `Sender`'s previous writes.
        self.shared.disconnected.store(true, Ordering::Release);
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.shared.disconnected.store(true, Ordering::Relaxed);
    }
}

impl<T> Drop for Shared<T> {
    fn drop(&mut self) {
        if core::mem::needs_drop::<T>() {
            let stamp = *self.stamp.get_mut();
            // `Receiver` has not claimed the last published value.
            if stamp & PUBLISHED != 0 {
                let read_idx = (stamp & INDEX) as usize;
                let _ = unsafe { (*self.values.get_unchecked(read_idx).get()).assume_init_read() };
            }
        }
    }
}

/// Create an accumulating channel pair.
pub fn channel<T>(initial: T) -> (Sender<T>, Receiver<T>) {
    let shared = Shared {
        values: [
            CachePadded::new(UnsafeCell::new(MaybeUninit::new(initial))),
            CachePadded::new(UnsafeCell::new(MaybeUninit::uninit())),
        ],
        stamp: AtomicU8::new(0),
        disconnected: AtomicBool::new(false),
    };
    let shared = Arc::new(shared);

    (
        Sender {
            shared: shared.clone(),
            write_idx: 0,
        },
        Receiver {
            shared,
            read_idx: 1,
        },
    )
}

#[cfg(test)]
mod test {
    extern crate std;

    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Instant,
    };

    use super::{TryRecvError, channel};

    #[test]
    fn basic() {
        let (mut tx, mut rx) = channel(0u64);

        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));

        *tx.get_mut() += 10;
        *tx.get_mut() += 5;
        assert!(tx.try_send(|| 0).unwrap());

        assert_eq!(rx.try_recv(), Ok(15));
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn update_blocked_while_unconsumed() {
        let (mut tx, mut rx) = channel(0u64);

        *tx.get_mut() = 42;
        assert!(tx.try_send(|| 0).unwrap());

        // The value isn't published until the receiver consumes the last value.
        *tx.get_mut() += 1;
        assert!(!tx.try_send(|| 0).unwrap());
        *tx.get_mut() += 1;
        assert!(!tx.try_send(|| 0).unwrap());

        assert_eq!(rx.try_recv(), Ok(42));

        assert!(tx.try_send(|| 0).unwrap());
        assert_eq!(rx.try_recv(), Ok(2));
    }

    #[test]
    fn drops() {
        let val = Arc::new(42);

        let (mut tx, mut rx) = channel(val.clone());
        assert!(tx.try_send(|| val.clone()).unwrap());
        assert_eq!(Arc::strong_count(&val), 3);

        // Dropping the sender drops the newly initialized (and unpublished) value.
        drop(tx);
        assert_eq!(Arc::strong_count(&val), 2); // `val` + `published`

        // Receiver can still consume the previously published value.
        let received = rx.try_recv().unwrap();
        assert_eq!(*received, 42);
        assert_eq!(Arc::strong_count(&val), 2); // `val` + `received`

        // The receiver now holds nothing, so dropping it (which also drops `Shared`) doesn't drop
        // any values. We still have `val` + `received`.
        drop(rx);
        assert_eq!(Arc::strong_count(&val), 2);
    }

    #[test]
    fn multithread_accumulate() {
        let (mut tx, mut rx) = channel(0u64);

        let producer = std::thread::spawn({
            move || {
                let mut total = 0u64;
                let now = Instant::now();
                while now.elapsed().as_secs() < 1 {
                    *tx.get_mut() += 1;
                    total += 1;
                    let _ = tx.try_send(|| 0);
                }
                // Ensure final buffer is published
                loop {
                    if tx.try_send(|| 0).unwrap() {
                        break;
                    }
                    std::thread::yield_now();
                }
                drop(tx);
                total
            }
        });

        let mut received_total = 0u64;
        loop {
            match rx.try_recv() {
                Ok(val) => received_total += val,
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => break,
            }
        }

        let expected = producer.join().unwrap();
        assert_eq!(
            received_total, expected,
            "sum of values at the sender and receiver sides must be equal"
        );
    }

    #[test]
    fn multithread_drop() {
        let val = Arc::new(42);

        {
            let (mut tx, mut rx) = channel(val.clone());

            let stop = Arc::new(AtomicBool::new(false));

            let t = {
                let stop = stop.clone();
                let val = val.clone();
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        *tx.get_mut() = val.clone();
                        let _ = tx.try_send(|| val.clone());
                    }
                })
            };

            let now = Instant::now();
            while now.elapsed().as_secs() < 1 {
                let _ = rx.try_recv();
            }
            stop.store(true, Ordering::Relaxed);
            t.join().unwrap();
        }

        assert_eq!(Arc::strong_count(&val), 1);
    }
}
