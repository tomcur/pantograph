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
    sync::atomic::{AtomicU8, Ordering},
};

use alloc::sync::Arc;

use crossbeam_utils::CachePadded;

const BACKBUFFER_MASK: u8 = 0b0011;
const VALUE_SENT_MASK: u8 = 0b0100;

struct Shared<T> {
    buffer: [CachePadded<UnsafeCell<T>>; 3],
    stamp: CachePadded<AtomicU8>,
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
}

impl<T> Receiver<T> {
    /// Receive the next value. If the sender has not sent a new value, this returns `None`.
    pub fn recv(&mut self) -> Option<&mut T> {
        self.recv_mut()
    }

    /// Receive the next value. If the sender has not sent a new value, this returns `None`.
    pub fn recv_mut(&mut self) -> Option<&mut T> {
        if self.update() {
            Some(self.get_mut())
        } else {
            None
        }
    }

    /// Get the current value.
    pub fn get(&mut self) -> &T {
        &*self.get_mut()
    }

    /// Get the current value mutably. If you want to take the value out of the channel, consider
    /// using the channel with an `Option<T>` and calling `rx.get_mut().take()`.
    pub fn get_mut(&mut self) -> &mut T {
        unsafe { &mut *self.shared.buffer.get_unchecked(self.index as usize).get() }
    }

    /// Check for and swap in the next value the reader should observe.
    pub fn update(&mut self) -> bool {
        if self.shared.stamp.load(Ordering::Relaxed) & VALUE_SENT_MASK == VALUE_SENT_MASK {
            self.index = self.shared.stamp.swap(self.index, Ordering::AcqRel) & BACKBUFFER_MASK;
            true
        } else {
            false
        }
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
    };
    let shared = Arc::new(shared);

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
        time::{Duration, Instant},
    };

    use super::{channel, channel_with};

    #[test]
    fn basic() {
        let (mut tx, mut rx) = channel::<u128>(42);

        assert_eq!(*rx.get(), 42);
        assert_eq!(rx.recv().copied(), None);
        tx.send(128);
        assert_eq!(rx.recv().copied(), Some(128));
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
            assert_eq!(**rx.recv().unwrap(), *a);
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
        assert_eq!(**rx.recv().unwrap(), *a);
        assert!(rx.recv().is_none());
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

            let stop = Arc::new(AtomicBool::new(false));

            let t = {
                let stop = stop.clone();
                let val = val.clone();
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        for _ in 0..1000 {
                            tx.send(val.clone());
                        }
                    }
                })
            };

            let now = Instant::now();
            while Instant::now().duration_since(now).as_secs_f32() < 1.0 {
                rx.recv();
            }
            stop.store(true, Ordering::Relaxed);
            t.join().expect("join");
        }

        assert_eq!(Arc::strong_count(&val), 1);
    }

    #[test]
    fn multithread_read() {
        let (mut tx, mut rx) = channel(42);

        let stop = Arc::new(AtomicBool::new(false));
        let t = std::thread::spawn({
            let stop = stop.clone();
            move || {
                let now = Instant::now();
                for value in 0..1_000_000 {
                    if now.elapsed() > Duration::from_secs(1) {
                        break;
                    }
                    tx.send(value);
                }
                stop.store(true, Ordering::Relaxed);
            }
        });

        let mut prev = None;
        loop {
            if let Some(value) = rx.recv().copied() {
                if let Some(prev_value) = prev {
                    assert!(
                        prev_value < value,
                        "previous value {} not smaller than current value {}",
                        prev_value,
                        value
                    );
                }
                prev = Some(value);
            } else if stop.load(Ordering::Relaxed) {
                break;
            }
        }

        t.join().expect("join");
    }
}
