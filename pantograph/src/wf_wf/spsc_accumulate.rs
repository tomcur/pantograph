//! A single-producer, single-consumer channel. The sender can keep updating the current value
//! until the receiver has read the previous value.
//!
//! The [`Sender`] and [`Receiver`] are wait-free.
//!
//! The channel retains ownership of values; the sender and receiver get `&mut T` access to their
//! respective slots.
//!
//! This uses a triple buffer internally.
//!
//! # Example
//!
//! This channel can be used to fold changes into a single message, such as counting events or
//! collecting statistics.
//!
//! In the following example, the sender accumulates a count and publishes it when the receiver is
//! ready for the next batch. The receiver reads at its own pace, and no values are missed.
//!
//! ```
//! use pantograph::wf_wf::spsc_accumulate;
//!
//! let (mut tx, mut rx) = spsc_accumulate::channel::<u64>();
//!
//! let producer = std::thread::spawn(move || {
//!     for _ in 0..1000 {
//!         *tx.get_mut() += 1;
//!         if tx.publish().is_ok() {
//!             *tx.get_mut() = 0;
//!         }
//!     }
//!     // Flush remaining data.
//!     while tx.publish().is_err() {}
//! });
//!
//! let mut total = 0u64;
//! loop {
//!     match rx.try_recv() {
//!         Ok(n) => total += *n,
//!         Err(spsc_accumulate::TryRecvError::Empty) => std::thread::yield_now(),
//!         Err(spsc_accumulate::TryRecvError::Disconnected) => break,
//!     }
//! }
//! producer.join().unwrap();
//!
//! assert_eq!(total, 1000);
//! ```

#![expect(missing_debug_implementations, reason = "Deferred")]

use alloc::sync::Arc;
use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, Ordering},
};

use crossbeam_utils::CachePadded;

/// Errors returned by [`Sender::publish`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TryPublishError {
    /// The [`Receiver`] has not yet consumed the previous value.
    Full,

    /// The [`Receiver`] has disconnected.
    Disconnected,
}

/// Errors returned by [`Sender::try_send`].
///
/// These are the same as [`TryPublishError`] but return the input value.
#[derive(Debug, PartialEq, Eq)]
pub enum TrySendError<T> {
    /// The [`Receiver`] has not yet consumed the previous value. The value is returned.
    Full(T),

    /// The [`Receiver`] has disconnected. The value is returned.
    Disconnected(T),
}

/// Errors that can occur when trying to receive a value using [`Receiver::try_recv`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TryRecvError {
    /// The channel is empty.
    Empty,

    /// The channel is empty and the [`Sender`] side of the channel has disconnected.
    Disconnected,
}

struct Shared<T> {
    slots: [CachePadded<UnsafeCell<T>>; 3],

    /// Set when the sender has published a value that the receiver has not yet consumed.
    published: CachePadded<AtomicBool>,

    sender_disconnected: AtomicBool,
    receiver_disconnected: AtomicBool,
}

/// The sender side of the accumulating channel.
///
/// Construct the [`Sender`] and [`Receiver`] by calling [`channel`].
pub struct Sender<T> {
    shared: Arc<Shared<T>>,
    index: u8,
}

/// The receiver side of the accumulating channel.
///
/// Construct the [`Sender`] and [`Receiver`] by calling [`channel`].
pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
    index: u8,
}

unsafe impl<T: Send> Send for Sender<T> {}
unsafe impl<T: Send> Send for Receiver<T> {}

impl<T> Sender<T> {
    /// Get the sender's staged value.
    pub fn get(&self) -> &T {
        // Safety: the sender exclusively owns the slot at `self.index`.
        unsafe {
            &*self
                .shared
                .slots
                .get_unchecked(usize::from(self.index))
                .get()
        }
    }

    /// Get the currently staged value mutably.
    pub fn get_mut(&mut self) -> &mut T {
        // Safety: the sender exclusively owns the slot at `self.index`.
        unsafe {
            &mut *self
                .shared
                .slots
                .get_unchecked(usize::from(self.index))
                .get()
        }
    }

    /// Returns `true` if the channel is full (the receiver has not consumed the last published
    /// value).
    ///
    /// This does not check whether the [`Receiver`] has disconnected. Use
    /// [`is_connected`](Self::is_connected) for that.
    pub fn is_full(&self) -> bool {
        self.shared.published.load(Ordering::Relaxed)
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
        if self.shared.published.load(Ordering::Relaxed) {
            return Err(TryPublishError::Full);
        }

        f(self.get_mut());

        // Acquire `published`. This synchronizes with the receiver's `Release` store when it
        // cleared `published`, ensuring we see the slot the receiver released.
        self.shared.published.load(Ordering::Acquire);

        // Publish our slot. Release ensures the receiver will see the data we wrote.
        self.shared.published.store(true, Ordering::Release);
        self.index = (self.index + 1) % 3;

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
        // Safety: the receiver exclusively owns the slot at `self.index`.
        unsafe { &*self.shared.slots.get_unchecked(self.index as usize).get() }
    }

    /// Get the receiver's current value mutably.
    pub fn get_mut(&mut self) -> &mut T {
        // Safety: the receiver exclusively owns the slot at `self.index`.
        unsafe { &mut *self.shared.slots.get_unchecked(self.index as usize).get() }
    }

    /// Returns `true` if the channel is full, in which case the next call to [`Self::try_recv`] is
    /// guaranteed to return `Ok`.
    ///
    /// This does not check whether the [`Sender`] has disconnected. Use
    /// [`is_connected`](Self::is_connected) for that.
    pub fn is_full(&self) -> bool {
        self.shared.published.load(Ordering::Relaxed)
    }

    /// Try to receive the last value published by the [`Sender`].
    ///
    /// If the sender has published a value since our last read, this returns `Ok` with that value
    /// and signals the sender that a new value can be published.
    ///
    /// Returns an error if the channel is empty. If the [`Sender`] is still alive, the error is
    /// [`TryRecvError::Empty`]. If the [`Sender`] has been dropped, returns
    /// [`TryRecvError::Disconnected`].
    pub fn try_recv(&mut self) -> Result<&mut T, TryRecvError> {
        // Acquire `published` to synchronize with the sender's `Release` store, ensuring we see
        // the data the sender wrote to the published slot.
        let mut published = self.shared.published.load(Ordering::Acquire);
        let mut disconnected = false;

        if !published && self.shared.sender_disconnected.load(Ordering::Relaxed) {
            disconnected = true;
            // The channel is empty and the `Sender` has disconnected, but it may be that we saw
            // the disconnect signal before we saw `published`, as the disconnect was loaded with
            // `Relaxed` semantics. Load the disconnect signal with `Acquire` semantics and then
            // load `published` again.
            self.shared.sender_disconnected.load(Ordering::Acquire);
            published = self.shared.published.load(Ordering::Acquire);
        }
        if !published {
            if disconnected {
                return Err(TryRecvError::Disconnected);
            } else {
                return Err(TryRecvError::Empty);
            }
        }

        // Advance to the published slot and clear `published`, signalling the sender that it can
        // publish again.
        self.index = (self.index + 1) % 3;
        self.shared.published.store(false, Ordering::Release);

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

/// Create an accumulating channel pair.
///
/// All slots are initialized with [`Default::default`].
pub fn channel<T: Default>() -> (Sender<T>, Receiver<T>) {
    channel_with(T::default)
}

/// Create an accumulating channel pair, initializing all slots by repeatedly calling `init`.
///
/// `init` is called 3 times.
pub fn channel_with<T>(mut init: impl FnMut() -> T) -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared {
        slots: [
            CachePadded::new(UnsafeCell::new(init())),
            CachePadded::new(UnsafeCell::new(init())),
            CachePadded::new(UnsafeCell::new(init())),
        ],
        published: CachePadded::new(AtomicBool::new(false)),
        sender_disconnected: AtomicBool::new(false),
        receiver_disconnected: AtomicBool::new(false),
    });

    let tx = Sender {
        shared: shared.clone(),
        index: 1,
    };

    let rx = Receiver { shared, index: 0 };

    (tx, rx)
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

    use super::{TryPublishError, TryRecvError, TrySendError, channel, channel_with};

    #[test]
    fn basic() {
        let (mut tx, mut rx) = channel::<u64>();

        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));

        *tx.get_mut() += 10;
        *tx.get_mut() += 5;
        tx.publish().unwrap();

        *tx.get_mut() = 42;
        assert_eq!(tx.publish(), Err(TryPublishError::Full));

        assert_eq!(*rx.try_recv().unwrap(), 15);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));

        tx.publish().unwrap();
        assert_eq!(*rx.try_recv().unwrap(), 42);
    }

    #[test]
    fn drops() {
        let a = Arc::new(1);
        let b = Arc::new(2);

        {
            let (mut tx, mut rx) = channel_with(|| Arc::new(0));

            tx.try_send(a.clone()).unwrap();
            assert_eq!(Arc::strong_count(&a), 2);

            let received = rx.try_recv().unwrap();
            assert_eq!(**received, 1);
            assert_eq!(Arc::strong_count(&a), 2);

            tx.try_send(b.clone()).unwrap();
            assert_eq!(Arc::strong_count(&b), 2);
        }

        assert_eq!(Arc::strong_count(&a), 1);
        assert_eq!(Arc::strong_count(&b), 1);
    }

    #[test]
    fn sender_disconnect() {
        let (mut tx, mut rx) = channel::<u64>();

        *tx.get_mut() = 42;
        tx.publish().unwrap();
        drop(tx);

        assert!(!rx.is_connected());
        assert_eq!(*rx.try_recv().unwrap(), 42);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
    }

    #[test]
    fn receiver_disconnect() {
        let (mut tx, rx) = channel::<u64>();

        drop(rx);

        assert!(!tx.is_connected());
        assert_eq!(tx.publish(), Err(TryPublishError::Disconnected));
        assert_eq!(tx.try_send(1), Err(TrySendError::Disconnected(1)));
    }

    #[test]
    fn multithread_accumulate() {
        let (mut tx, mut rx) = channel::<u64>();

        let producer = std::thread::spawn(move || {
            let mut total = 0_u64;
            let now = Instant::now();
            while now.elapsed().as_secs() < 1 {
                *tx.get_mut() += 1;
                total += 1;
                if tx.publish().is_ok() {
                    *tx.get_mut() = 0;
                }
            }
            // Ensure final buffer is published.
            loop {
                if tx.publish().is_ok() {
                    break;
                }
                std::thread::yield_now();
            }
            drop(tx);
            total
        });

        let mut received_total = 0_u64;
        loop {
            match rx.try_recv() {
                Ok(val) => received_total += *val,
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
            let (mut tx, mut rx) = channel_with(|| val.clone());

            let stop = Arc::new(AtomicBool::new(false));

            let t = {
                let stop = stop.clone();
                let val = val.clone();
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        *tx.get_mut() = val.clone();
                        let _ = tx.publish();
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
