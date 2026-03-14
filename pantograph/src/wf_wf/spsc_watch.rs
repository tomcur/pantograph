//! A single-producer, single-consumer channel. The consumer always reads the latest-produced
//! value, i.e., it "watches" the set value. The sender and receiver are wait-free.
//!
//! This uses a triple buffer internally.

use core::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    sync::atomic::{AtomicU8, Ordering},
};

use alloc::sync::Arc;

use crossbeam_utils::CachePadded;

const BACKBUFFER_MASK: u8 = 0b0011;
const VALUE_SENT_MASK: u8 = 0b0100;

struct Shared<T> {
    buffer: [CachePadded<UnsafeCell<MaybeUninit<T>>>; 3],
    stamp: CachePadded<AtomicU8>,
}

impl<T> Drop for Shared<T> {
    fn drop(&mut self) {
        if core::mem::needs_drop::<T>()
            && *self.stamp.get_mut() & VALUE_SENT_MASK == VALUE_SENT_MASK
        {
            let index = *self.stamp.get_mut() & BACKBUFFER_MASK;
            let val = unsafe { self.buffer.get_unchecked_mut(index as usize) }.get_mut();
            unsafe {
                val.assume_init_drop();
            }
        }
    }
}

/// The sender part of the channel.
pub struct Sender<T> {
    shared: Arc<Shared<T>>,
    index: u8,
    first_update: bool,
}

/// The receiver part of the channel.
pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
    index: u8,
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        if core::mem::needs_drop::<T>() {
            unsafe {
                (*self.shared.buffer.get_unchecked(self.index as usize).get()).assume_init_drop();
            }
        }
    }
}

unsafe impl<T: Send> Send for Sender<T> {}
unsafe impl<T: Send> Send for Receiver<T> {}

impl<T> Sender<T> {
    /// Send a new value (i.e., set the next value the reader will read). This returns a previous
    /// value if this is not the first time sending.
    pub fn send(&mut self, val: T) -> Option<T> {
        *unsafe { &mut *self.shared.buffer.get_unchecked(self.index as usize).get() } =
            MaybeUninit::new(val);

        self.index = self
            .shared
            .stamp
            .swap(VALUE_SENT_MASK | self.index, Ordering::AcqRel)
            & BACKBUFFER_MASK;

        if !self.first_update {
            Some(unsafe {
                (*self.shared.buffer.get_unchecked(self.index as usize).get()).assume_init_read()
            })
        } else {
            self.first_update = false;
            None
        }
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
        unsafe { (*self.shared.buffer.get_unchecked(self.index as usize).get()).assume_init_mut() }
    }

    pub fn update(&mut self) -> bool {
        if self.shared.stamp.load(Ordering::Relaxed) & VALUE_SENT_MASK == VALUE_SENT_MASK {
            self.index = self.shared.stamp.swap(self.index, Ordering::AcqRel) & BACKBUFFER_MASK;
            true
        } else {
            false
        }
    }
}

/// Construct a new watch channel.
pub fn channel<T>(init: T) -> (Sender<T>, Receiver<T>) {
    let shared = Shared {
        buffer: [
            CachePadded::new(UnsafeCell::new(MaybeUninit::new(init))),
            CachePadded::new(UnsafeCell::new(MaybeUninit::uninit())),
            CachePadded::new(UnsafeCell::new(MaybeUninit::uninit())),
        ],
        stamp: CachePadded::new(AtomicU8::new(2)),
    };
    let shared = Arc::new(shared);

    let tx = Sender {
        shared: shared.clone(),
        index: 1,
        first_update: true,
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

    use super::channel;

    #[test]
    fn test() {
        let (mut tx, mut rx) = channel::<u128>(42);

        assert_eq!(*rx.get(), 42);
        assert_eq!(rx.recv().copied(), None);
        assert_eq!(tx.send(128), None);
        assert_eq!(rx.recv().copied(), Some(128));
    }

    #[test]
    fn drops() {
        let val = Arc::new(42);

        let (mut tx, mut rx) = channel(val.clone());

        assert_eq!(Arc::strong_count(&val), 2);
        assert_eq!(rx.recv(), None);
        assert_eq!(Arc::strong_count(&val), 2);

        assert_eq!(tx.send(val.clone()), None);
        assert_eq!(Arc::strong_count(&val), 3);
        assert!(tx.send(val.clone()).is_some());
        assert_eq!(Arc::strong_count(&val), 3);
        assert!(tx.send(val.clone()).is_some());
        assert_eq!(Arc::strong_count(&val), 3);

        drop(rx);
        assert_eq!(Arc::strong_count(&val), 2);
        drop(tx);
        assert_eq!(Arc::strong_count(&val), 1);
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
                        for _ in 0..1000 {
                            let _ = tx.send(val.clone());
                        }
                    }
                })
            };

            let now = Instant::now();
            while Instant::now().duration_since(now).as_secs_f32() < 1.0 {
                let _ = rx.recv();
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
                    let _ = tx.send(value);
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
