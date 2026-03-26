//! A single-producer, single-consumer channel. The consumer reads the latest-produced value.
//!
//! The sender and receiver are wait-free.
//!
//! The [`Sender`] regains ownership over old values, both read and unread, allowing it to reuse
//! old values. The [`Receiver`] gets mutable access to the value it's reading, allowing sending
//! signals back to the sender (or, e.g., [`core::mem::take`]).
//!
//! This uses a triple buffer internally.

#![expect(missing_debug_implementations, reason = "Deferred")]

use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, AtomicU8, Ordering},
};

use alloc::sync::Arc;

use crossbeam_utils::CachePadded;

const BACKBUFFER_MASK: u8 = 0b0011;
const VALUE_SENT_MASK: u8 = 0b0100;
const SENDER_DISCONNECTED_MASK: u8 = 0b1000;

/// Errors that can occur when trying to receive a value using [`Receiver::try_recv`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TryRecvError {
    /// The channel is empty.
    Empty,

    /// The channel is empty and the [`Sender`] side of the channel has disconnected.
    Disconnected,
}

struct Shared<T> {
    buffer: [CachePadded<UnsafeCell<T>>; 3],
    stamp: CachePadded<AtomicU8>,

    receiver_disconnected: AtomicBool,
}

/// The sender part of the channel.
pub struct Sender<T> {
    shared: Arc<Shared<T>>,
    index: u8,
}

/// The receiver part of the channel.
pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
    index: u8,

    /// Once the receiver has seen [`SENDER_DISCONNECTED_MASK`], this becomes true.
    sender_disconnected: bool,
}

unsafe impl<T: Send> Send for Sender<T> {}
unsafe impl<T: Send> Send for Receiver<T> {}

impl<T> Sender<T> {
    /// Send a new value (i.e., set the next value the reader will read).
    ///
    /// This returns the previously staged value to allow reuse.
    ///
    /// This is equivalent to replacing the value from [`Self::get_mut`] followed by
    /// [`Self::publish`].
    pub fn send(&mut self, val: T) -> T {
        let prev = core::mem::replace(self.get_mut(), val);
        self.publish();
        prev
    }

    /// Get the sender's currently staged value mutably, for in-place modification.
    ///
    /// Use [`publish`](Self::publish) to publish the staged value.
    pub fn get_mut(&mut self) -> &mut T {
        unsafe { &mut *self.shared.buffer.get_unchecked(self.index as usize).get() }
    }

    /// Publish the sender's staged value, making it visible to the receiver.
    ///
    /// The channel's back-buffer becomes the sender's new staged value in exchange.
    pub fn publish(&mut self) {
        self.index = self
            .shared
            .stamp
            .swap(VALUE_SENT_MASK | self.index, Ordering::AcqRel)
            & BACKBUFFER_MASK;
    }

    /// Returns `true` if the [`Receiver`] is still alive.
    pub fn is_connected(&self) -> bool {
        !self.shared.receiver_disconnected.load(Ordering::Relaxed)
    }
}

impl<T> Receiver<T> {
    /// Try to receive a value from the channel.
    ///
    /// Returns the value if one was available. Returns [`TryRecvError::Empty`] if the channel is
    /// empty, or [`TryRecvError::Disconnected`] if the channel is empty and the sender has been
    /// dropped.
    ///
    /// See also [`Self::update`].
    pub fn try_recv(&mut self) -> Result<&mut T, TryRecvError> {
        if self.update() {
            Ok(self.get_mut())
        } else if self.sender_disconnected {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    /// Get the current value.
    ///
    /// Use [`Self::try_recv`] or [`Self::update`] to receive a new value from the channel.
    pub fn get(&mut self) -> &T {
        &*self.get_mut()
    }

    /// Get the current value mutably.
    ///
    /// If you want to take the value out of the channel, consider using the channel with an
    /// `Option<T>` and calling `rx.get_mut().take()`.
    ///
    /// Use [`Self::try_recv`] or [`Self::update`] to receive a new value from the channel.
    pub fn get_mut(&mut self) -> &mut T {
        unsafe { &mut *self.shared.buffer.get_unchecked(self.index as usize).get() }
    }

    /// Check for and swap in the next value the reader should observe.
    ///
    /// See also [`Self::try_recv`].
    pub fn update(&mut self) -> bool {
        let stamp = self.shared.stamp.load(Ordering::Relaxed);
        self.sender_disconnected |= stamp & SENDER_DISCONNECTED_MASK != 0;

        if stamp & VALUE_SENT_MASK != 0 {
            let stamp = self.shared.stamp.swap(self.index, Ordering::AcqRel);
            self.sender_disconnected |= stamp & SENDER_DISCONNECTED_MASK != 0;
            self.index = stamp & BACKBUFFER_MASK;
            true
        } else {
            false
        }
    }

    /// Returns `true` if the [`Sender`] is still alive.
    ///
    /// This only updates after calling [`Self::try_recv`] or [`Self::update`].
    pub fn is_connected(&self) -> bool {
        !self.sender_disconnected
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.shared
            .stamp
            .fetch_or(SENDER_DISCONNECTED_MASK, Ordering::Relaxed);
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.shared
            .receiver_disconnected
            .store(true, Ordering::Relaxed);
    }
}

/// Construct a new channel.
///
/// The [`Receiver`] will see `init` immediately. The [`Sender`]'s staged value and the back-buffer
/// are initialized with `T::default()`.
pub fn channel<T: Default>(init: T) -> (Sender<T>, Receiver<T>) {
    channel_with(init, T::default(), T::default())
}

/// Construct a new channel, explicitly setting the initial, staged, and back-buffer values.
///
/// The [`Receiver`] will see `init` immediately. The [`Sender`]'s initial staged value is `staged`
/// and `back` is the back-buffer.
pub fn channel_with<T>(init: T, staged: T, back: T) -> (Sender<T>, Receiver<T>) {
    let shared = Shared {
        buffer: [
            CachePadded::new(UnsafeCell::new(init)),
            CachePadded::new(UnsafeCell::new(staged)),
            CachePadded::new(UnsafeCell::new(back)),
        ],
        stamp: CachePadded::new(AtomicU8::new(2)),
        receiver_disconnected: AtomicBool::new(false),
    };
    let shared = Arc::new(shared);

    let tx = Sender {
        shared: shared.clone(),
        index: 1,
    };

    let rx = Receiver {
        shared,
        index: 0,
        sender_disconnected: false,
    };

    (tx, rx)
}

#[cfg(test)]
mod test {
    extern crate std;

    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    use super::{TryRecvError, channel, channel_with};

    #[test]
    fn basic() {
        let (mut tx, mut rx) = channel::<u128>(42);

        assert_eq!(*rx.get(), 42);
        assert_eq!(rx.try_recv().err(), Some(TryRecvError::Empty));
        tx.send(128);
        assert_eq!(rx.try_recv().copied(), Ok(128));
    }

    #[test]
    fn drops() {
        let init = Arc::new(0);
        let staged = Arc::new(1);
        let back = Arc::new(2);
        let a = Arc::new(3);
        let b = Arc::new(4);

        {
            let (mut tx, mut rx) = channel_with(init.clone(), staged.clone(), back.clone());
            assert_eq!(Arc::strong_count(&init), 2);
            assert_eq!(Arc::strong_count(&staged), 2);
            assert_eq!(Arc::strong_count(&back), 2);

            // First send returns the old staged value. The back-buffer becomes the staged value.
            assert_eq!(*tx.send(a.clone()), *staged);
            assert_eq!(Arc::strong_count(&init), 2);
            assert_eq!(Arc::strong_count(&staged), 1);
            assert_eq!(Arc::strong_count(&a), 2);

            // Second send returns that now-staged old back-buffer value. The first value we sent
            // becomes staged.
            assert_eq!(*tx.send(b.clone()), *back);
            assert_eq!(Arc::strong_count(&init), 2);
            assert_eq!(Arc::strong_count(&back), 1);
            assert_eq!(Arc::strong_count(&b), 2);

            // Third send returns the first-sent value.
            assert_eq!(*tx.send(a.clone()), *a);
            assert_eq!(Arc::strong_count(&init), 2);
            assert_eq!(Arc::strong_count(&a), 2);
            assert_eq!(Arc::strong_count(&b), 2);

            // The receiver receives the latest value sent by the sender. The `init` value the
            // receiver was sitting on becomes the back-buffer.
            assert_eq!(**rx.try_recv().unwrap(), *a);
            assert_eq!(Arc::strong_count(&init), 2);
            assert_eq!(Arc::strong_count(&a), 2);
            assert_eq!(Arc::strong_count(&b), 2);

            // Send returns `b`.
            assert_eq!(*tx.send(b.clone()), *b);
            assert_eq!(Arc::strong_count(&init), 2);
            assert_eq!(Arc::strong_count(&a), 2);
            assert_eq!(Arc::strong_count(&b), 2);

            // Send returns `init`.
            assert_eq!(*tx.send(a.clone()), *init);
            assert_eq!(Arc::strong_count(&init), 1);
            assert_eq!(Arc::strong_count(&a), 3);
            assert_eq!(Arc::strong_count(&b), 2);
        }

        // Dropping both sides of the channel cleans everything up.
        assert_eq!(Arc::strong_count(&init), 1);
        assert_eq!(Arc::strong_count(&staged), 1);
        assert_eq!(Arc::strong_count(&back), 1);
        assert_eq!(Arc::strong_count(&a), 1);
        assert_eq!(Arc::strong_count(&b), 1);
    }

    #[test]
    fn drops_disconnect() {
        let init = Arc::new(0);
        let staged = Arc::new(1);
        let back = Arc::new(2);
        let a = Arc::new(3);

        let mut rx = {
            let (mut tx, rx) = channel_with(init.clone(), staged.clone(), back.clone());

            assert_eq!(Arc::strong_count(&init), 2);
            let prev = tx.send(a.clone());
            assert_eq!(*prev, *staged);
            assert_eq!(Arc::strong_count(&a), 2);
            drop(prev);
            drop(tx);
            rx
        };

        // The sent value survives `tx` drop.
        assert_eq!(**rx.try_recv().unwrap(), *a);
        assert_eq!(rx.try_recv().err(), Some(TryRecvError::Disconnected));
        drop(rx);

        // Dropping both sides of the channel cleans everything up.
        assert_eq!(Arc::strong_count(&init), 1);
        assert_eq!(Arc::strong_count(&staged), 1);
        assert_eq!(Arc::strong_count(&back), 1);
        assert_eq!(Arc::strong_count(&a), 1);
    }

    #[test]
    fn multithread_drop() {
        let val = Arc::new(42);

        {
            let (mut tx, mut rx) = channel_with(val.clone(), val.clone(), val.clone());

            let t = {
                let val = val.clone();
                std::thread::spawn(move || {
                    let now = Instant::now();
                    while now.elapsed().as_secs_f32() < 1.0 {
                        for _ in 0..1000 {
                            tx.send(val.clone());
                        }
                    }
                })
            };

            while rx.try_recv() != Err(TryRecvError::Disconnected) {}
            t.join().expect("join");
        }

        assert_eq!(Arc::strong_count(&val), 1);
    }

    #[test]
    fn multithread_read() {
        let (mut tx, mut rx) = channel(42);

        let t = std::thread::spawn(move || {
            let now = Instant::now();
            for value in 0..1_000_000 {
                if now.elapsed() > Duration::from_secs(1) {
                    break;
                }
                tx.send(value);
            }
        });

        let mut prev = None;
        loop {
            match rx.try_recv().copied() {
                Ok(value) => {
                    if let Some(prev_value) = prev {
                        assert!(
                            prev_value < value,
                            "previous value {} not smaller than current value {}",
                            prev_value,
                            value
                        );
                    }
                    prev = Some(value);
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => break,
            }
        }

        t.join().expect("join");
    }
}
