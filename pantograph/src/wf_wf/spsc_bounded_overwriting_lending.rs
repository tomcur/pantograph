//! A bounded single-producer, single-consumer channel that overwrites old values.
//!
//! The [`Sender`] and [`Receiver`] are wait-free.
//!
//! When the sender publishes a value while the channel is full (because the receiver has not
//! consumed old values), the oldest value is overwritten. This is intended for receivers that
//! intermittently drain the channel. This channel provides no backpressure: only use it if the
//! receiver is likely to keep up when it is actively reading.
//!
//! A receiver that consistently lags behind the sender or is only interested in the latest value
//! may be better served by [`crate::wf_wf::spsc_watch`] or
//! [`crate::wf_wf::spsc_accumulate_lending`].
//!
//! The channel retains ownership of values; the sender and receiver get `&mut T` access to their
//! current slot.
//!
//! # Internal details
//!
//! There are `capacity + 2` physical slots. One slot is owned by the sender, and one by the
//! receiver. The remaining `capacity` slots sit in a ring buffer of entries. Publishing a value
//! atomically swaps the sender's currently owned slot with the ring buffer. Receiving similarly
//! swaps a slot, with additional bookkeeping to check for lapping.

#![expect(missing_debug_implementations, reason = "Deferred")]

use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};

use alloc::boxed::Box;
use alloc::sync::Arc;

use crossbeam_utils::CachePadded;

/// Errors that can occur when trying to receive a value using [`Receiver::try_recv`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TryRecvError {
    /// The channel is empty.
    Empty,

    /// The [`Sender`] has lapped the [`Receiver`], and sent two values while the receiver was
    /// catching up.
    ///
    /// The receiver has bumped itself forwards towards the sender and will not see the values in
    /// between.
    ///
    /// This is unlikely to occur, but can happen in cases where the [`Sender`] is very fast, and
    /// the [`Receiver`] gets stalled during its [`Receiver::try_recv`] call, perhaps due to
    /// preemption.
    Raced,

    /// The channel is empty and the [`Sender`] side of the channel has disconnected.
    Disconnected,
}

/// Helpers to pack and unpack the ring buffer's atomic `(writer_sequence, slot_index)` pairs.
struct Entry;

impl Entry {
    const fn pack(seq: u32, slot: u32) -> u64 {
        ((seq as u64) << 32) | (slot as u64)
    }

    #[expect(clippy::cast_possible_truncation, reason = "Deliberate truncation")]
    const fn unpack(packed: u64) -> (u32, u32) {
        ((packed >> 32) as u32, packed as u32)
    }
}

struct Shared<T> {
    slots: Box<[CachePadded<UnsafeCell<T>>]>,
    entries: Box<[CachePadded<AtomicU64>]>,
    sender_seq: CachePadded<AtomicU32>,

    capacity: u32,

    sender_disconnected: AtomicBool,
    receiver_disconnected: AtomicBool,
}

/// The sender side of the channel.
pub struct Sender<T> {
    shared: Arc<Shared<T>>,

    /// The slot currently owned by the sender.
    slot: u32,
    /// The current sender sequence.
    seq: u32,
}

/// The receiver side of the channel.
pub struct Receiver<T> {
    shared: Arc<Shared<T>>,

    /// The slot currently owned by the receiver.
    slot: u32,
    /// The current receiver sequence.
    seq: u32,
}

unsafe impl<T: Send> Send for Sender<T> {}
unsafe impl<T: Send> Send for Receiver<T> {}

impl<T> Sender<T> {
    /// Get the sender's staged value.
    pub fn get(&self) -> &T {
        // Safety: the sender exclusively owns `self.slot`. No other entity holds this index.
        unsafe { &*self.shared.slots.get_unchecked(self.slot as usize).get() }
    }

    /// Get the sender's staged value mutably.
    ///
    /// Use [`publish`](Self::publish) to publish the staged value.
    pub fn get_mut(&mut self) -> &mut T {
        // Safety: the sender exclusively owns `self.slot`. No other entity holds this index.
        unsafe { &mut *self.shared.slots.get_unchecked(self.slot as usize).get() }
    }

    /// Publish the staged value into the channel.
    ///
    /// The sender's new staged value is an old value from the channel.
    pub fn publish(&mut self) {
        let write_idx = self.seq % self.shared.capacity;

        // Swap our (seq, slot) pair with whatever is in the entry. We then own the slot that was
        // that entry.
        //
        // We store our sequence number in the entry, which the receiver can use to figure out if
        // it has been raced between checking how behind us it is and actually reading the slot.
        let (_, reclaimed) = Entry::unpack(
            unsafe { self.shared.entries.get_unchecked(write_idx as usize) }
                .swap(Entry::pack(self.seq, self.slot), Ordering::AcqRel),
        );
        self.slot = reclaimed;

        self.seq = self.seq.wrapping_add(1);

        // We store the sequence number with `Release` semantics, so that the `Receiver` reading
        // this with `Acquire` is guaranteed to see the fresh slot we wrote just now.
        self.shared.sender_seq.store(self.seq, Ordering::Release);
    }

    /// Replace the staged value and publish it. Returns the previously staged value.
    ///
    /// This is equivalent to replacing the value from [`Self::get_mut`] followed by
    /// [`Self::publish`].
    pub fn send(&mut self, val: T) -> T {
        let old = core::mem::replace(self.get_mut(), val);
        self.publish();
        old
    }

    /// Returns `true` if the receiver is still alive.
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
        // Safety: the receiver exclusively owns `self.slot`. No other entity holds this index.
        unsafe { &*self.shared.slots.get_unchecked(self.slot as usize).get() }
    }

    /// Get the receiver's current value mutably.
    ///
    /// If you want to take ownership of the value, consider using [`core::mem::take`] or
    /// using `Option<T>` with [`Option::take`].
    pub fn get_mut(&mut self) -> &mut T {
        // Safety: the receiver exclusively owns `self.slot`. No other entity holds this index.
        unsafe { &mut *self.shared.slots.get_unchecked(self.slot as usize).get() }
    }

    /// Try receiving a new value from the channel.
    ///
    /// Returns `Ok(val)` if a new value was received, [`TryRecvError::Empty`] if no new value is
    /// available, or [`TryRecvError::Disconnected`] if the sender has been dropped and all values
    /// have been drained.
    ///
    /// If the [`Sender`] has lapped the receiver (published more than `capacity` values since the
    /// last read), intermediate values are skipped and the oldest surviving value is received. In
    /// the unlikely event the sender races the receiver twice during this process, producing
    /// values faster than the tight compare-and-swap operations of the receiver, the receiver
    /// gives up and bumps itself forward to the sender's current position, skipping all
    /// intermediate values. A [`TryRecvError::Raced`] error is returned in this case.
    pub fn try_recv(&mut self) -> Result<&mut T, TryRecvError> {
        let mut sender_seq = self.shared.sender_seq.load(Ordering::Acquire);

        if sender_seq.wrapping_sub(self.seq) == 0 {
            // We may not see the sender's disconnect immediately and falsely return `false`, but
            // that's benign.
            if self.shared.sender_disconnected.load(Ordering::Acquire) {
                // Acquire the `sender_seq` again. Acquiring the `sender_disconnected` just now has
                // released `sender_seq`, ensuring we will see the latest value and we don't
                // falsely report the channel is disconnected (we first need to drain it).
                sender_seq = self.shared.sender_seq.load(Ordering::Acquire);
                if sender_seq.wrapping_sub(self.seq) == 0 {
                    return Err(TryRecvError::Disconnected);
                }
            } else {
                return Err(TryRecvError::Empty);
            }
        }

        // If the sender lapped us, skip to the oldest live entry.
        if sender_seq.wrapping_sub(self.seq) > self.shared.capacity {
            self.seq = sender_seq.wrapping_sub(self.shared.capacity);
        }

        // Load the the published `(sender_seq, slot)` pair at this entry. The `sender_seq` in
        // the entry stores the low 32 bits of the sender's full `sender_seq` at the time the
        // sender produced the entry.
        //
        // It is possible that between us reading `self.shared.sender_seq` and this slot, the
        // sender has overwritten the slot, in which case `entry_sender_seq` won't match
        // `sender_seq`. In that case, we immediately fetch the next slot. In the unlikely event
        // *that* entry's `entry_sender_seq` does not match `sender_seq + 1` (meaning the sender
        // outraced our tight loading!), we simply accept that we're going to miss some values and
        // bump ourselves to that slot.
        let (entry_sender_seq, new_slot) = Entry::unpack(
            self.shared.entries[(self.seq % self.shared.capacity) as usize].load(Ordering::Relaxed),
        );

        // Theoretically there's an ABA problem here if there are 2^32 sends between reading
        // `entry_sender_seq` and the CAS.
        if entry_sender_seq == self.seq
            && self.shared.entries[(self.seq % self.shared.capacity) as usize]
                .compare_exchange(
                    Entry::pack(entry_sender_seq, new_slot),
                    Entry::pack(entry_sender_seq, self.slot),
                    // Note we don't need `Acquire` semantics here, because we've already
                    // `Acquire`d `self.shared.sender_seq`, which was `Release`d after the sender
                    // wrote this.
                    Ordering::Release,
                    Ordering::Relaxed,
                )
                .is_ok()
        {
            self.slot = new_slot;
            self.seq = self.seq.wrapping_add(1);

            return Ok(self.get_mut());
        }

        // As noted above as a possibility, the sender has raced us. Immediately try the next slot.
        self.seq = self.seq.wrapping_add(1);
        let (entry_sender_seq, new_slot) = Entry::unpack(
            self.shared.entries[(self.seq % self.shared.capacity) as usize].load(Ordering::Relaxed),
        );

        // If the entry's `entry_sender_seq` does not match or the next compare-exchange fails, the
        // sender has raced us again and is likely in a very tight loop. Give up.
        if entry_sender_seq != self.seq {
            self.seq = entry_sender_seq.wrapping_add(1);
            return Err(TryRecvError::Raced);
        }

        if let Err(res) = self.shared.entries[(self.seq % self.shared.capacity) as usize]
            .compare_exchange(
                Entry::pack(entry_sender_seq, new_slot),
                Entry::pack(entry_sender_seq, self.slot),
                // As opposed to the `compare_exchange` above, we *do* require `Acquire` semantics
                // here, as this slot was `Release`d by the sender *after* the
                // `self.shared.sender_seq` we `Acquire`d above.
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
        {
            let (entry_sender_seq, _) = Entry::unpack(res);
            self.seq = entry_sender_seq.wrapping_add(1);
            return Err(TryRecvError::Raced);
        }

        self.slot = new_slot;
        self.seq = self.seq.wrapping_add(1);

        Ok(self.get_mut())
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.shared
            .receiver_disconnected
            .store(true, Ordering::Relaxed);
    }
}

/// Construct a new bounded overwriting channel with the given capacity.
///
/// All slots are initialized with [`Default::default`]. The capacity must be at least 1, and at
/// most `u32::MAX - 2`.
///
/// The [`Receiver`] will not see any value until the [`Sender`] publishes one, but the receiver's
/// slot is always accessible via [`Receiver::get`] / [`Receiver::get_mut`].
pub fn channel<T: Default>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    channel_with(capacity, || T::default())
}

/// Construct a new bounded, overwriting channel with the given capacity, initializing all slots by
/// repeatedly calling `init`.
///
/// The capacity must be at least 1, and at most `u32::MAX - 2`. `init` is called `capacity + 2` times.
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
    let total_slots = capacity + 2;

    let slots: Box<[_]> = (0..total_slots)
        .map(|_| CachePadded::new(UnsafeCell::new(init())))
        .collect();

    // Entries hold (seq=0, slot_index)..capacity.
    // Slot `capacity` is the receiver's initial slot.
    // Slot `capacity+1` is the sender's initial slot.
    let entries: Box<[_]> = (0..capacity)
        .map(|i| CachePadded::new(AtomicU64::new(Entry::pack(0, i))))
        .collect();

    let shared = Arc::new(Shared {
        slots,
        entries,
        sender_seq: CachePadded::new(AtomicU32::new(0)),

        capacity,
        sender_disconnected: AtomicBool::new(false),
        receiver_disconnected: AtomicBool::new(false),
    });

    let tx = Sender {
        shared: shared.clone(),
        slot: total_slots - 1,
        seq: 0,
    };

    let rx = Receiver {
        shared,
        slot: total_slots - 2,
        seq: 0,
    };

    (tx, rx)
}

#[cfg(test)]
mod test {
    extern crate std;

    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::{Duration, Instant};

    use super::{TryRecvError, channel, channel_with};

    #[test]
    fn basic() {
        let (mut tx, mut rx) = channel::<u64>(4);

        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        tx.send(10);
        assert_eq!(*rx.try_recv().unwrap(), 10);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn fill_to_capacity() {
        let (mut tx, mut rx) = channel::<u64>(4);

        tx.send(1);
        tx.send(2);
        tx.send(3);
        tx.send(4);

        assert_eq!(*rx.try_recv().unwrap(), 1);
        assert_eq!(*rx.try_recv().unwrap(), 2);
        assert_eq!(*rx.try_recv().unwrap(), 3);
        assert_eq!(*rx.try_recv().unwrap(), 4);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn overwrite() {
        let (mut tx, mut rx) = channel::<u64>(2);

        tx.send(1);
        tx.send(2);
        tx.send(3);
        tx.send(4);

        // Oldest surviving values are 3 and 4.
        assert_eq!(*rx.try_recv().unwrap(), 3);
        assert_eq!(*rx.try_recv().unwrap(), 4);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn interleaved() {
        let (mut tx, mut rx) = channel::<u64>(2);

        tx.send(1);
        assert_eq!(*rx.try_recv().unwrap(), 1);

        tx.send(2);
        tx.send(3);
        assert_eq!(*rx.try_recv().unwrap(), 2);
        assert_eq!(*rx.try_recv().unwrap(), 3);

        tx.send(4);
        tx.send(5);
        tx.send(6); // overwrites 4
        assert_eq!(*rx.try_recv().unwrap(), 5);
        assert_eq!(*rx.try_recv().unwrap(), 6);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn send_returns_old_staged() {
        let (mut tx, _rx) = channel::<u64>(4);

        // The sender's initial staged value is T::default() = 0.
        let old = tx.send(10);
        assert_eq!(old, 0);

        // After publish, the sender gets back a free-pool slot (also default).
        let old = tx.send(20);
        assert_eq!(old, 0);
    }

    #[test]
    fn drops() {
        let a = Arc::new(1);
        let b = Arc::new(2);
        let c = Arc::new(3);

        {
            let (mut tx, mut rx) = channel_with(2, || Arc::new(0));

            tx.send(a.clone());
            tx.send(b.clone());
            assert_eq!(Arc::strong_count(&a), 2);
            assert_eq!(Arc::strong_count(&b), 2);

            // Overwrites the oldest entry.
            tx.send(c.clone());
            assert_eq!(Arc::strong_count(&a), 2);
            assert_eq!(Arc::strong_count(&b), 2);
            assert_eq!(Arc::strong_count(&c), 2);

            let received = rx.try_recv().unwrap();
            assert_eq!(**received, *b);
        }

        assert_eq!(Arc::strong_count(&a), 1);
        assert_eq!(Arc::strong_count(&b), 1);
        assert_eq!(Arc::strong_count(&c), 1);
    }

    #[test]
    fn multithread_drop() {
        let val = Arc::new(42);

        {
            let (mut tx, mut rx) = channel_with(4, || val.clone());

            let stop = Arc::new(AtomicBool::new(false));

            let t = {
                let stop = stop.clone();
                let val = val.clone();
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        for _ in 0..100 {
                            tx.send(val.clone());
                        }
                    }
                })
            };

            let now = Instant::now();
            while now.elapsed() < Duration::from_secs(1) {
                let _ = rx.try_recv();
            }
            stop.store(true, Ordering::Relaxed);
            t.join().expect("join");
        }

        assert_eq!(Arc::strong_count(&val), 1);
    }

    #[test]
    fn multithread_ordering() {
        let (mut tx, mut rx) = channel::<u64>(16);

        let stop = Arc::new(AtomicBool::new(false));
        let t = std::thread::spawn({
            let stop = stop.clone();
            move || {
                let now = Instant::now();
                for value in 1..1_000_000_u64 {
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
                }
                Err(TryRecvError::Empty | TryRecvError::Raced) => {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                }
                Err(TryRecvError::Disconnected) => break,
            }
        }

        t.join().unwrap();
    }

    #[test]
    fn sender_disconnect_drain_then_error() {
        let (mut tx, mut rx) = channel::<u64>(4);

        tx.send(1);
        tx.send(2);
        drop(tx);

        // Drain remaining values.
        assert_eq!(*rx.try_recv().unwrap(), 1);
        assert_eq!(*rx.try_recv().unwrap(), 2);

        // The channel is now disconnected with nothing left.
        assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
    }
}
