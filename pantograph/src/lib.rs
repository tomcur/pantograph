#![expect(rustdoc::redundant_explicit_links, reason = "Necessary for cargo-rdme")]
//! `pantograph` provides channels for sending payloads across threads, where at least one side of
//! the channel is *wait-free*.
//!
//! Synchronization being "wait-free" is a stronger guarantee than "lock-free." Wait-free
//! operations complete in a bounded number of steps regardless of contention by other threads or
//! preemption of other threads by the operating system.
//!
//! In lock-based synchronization (like `Mutex` from Rust's standard library), if the operating
//! system preempts a thread that holds a lock, other threads waiting to acquire the lock are
//! blocked until it resumes.
//!
//! Lock-free synchronization does not use locking mechanisms, and the system is guaranteed to make
//! progress even when some threads are preempted by the operating system. However, individual
//! operations may fail under contention from other threads and need to be retried, which means no
//! bound can be given on the number of steps required to complete a lock-free operation.
//!
//! Wait-free and lock-free channels are useful for problems that must meet particular timing
//! requirements or that must be robust against preemption. They are not necessarily faster than
//! lock-based channels.
//!
//! # Channel flavors
//!
//! There are various channels to choose from. All channels currently implemented are
//! single-producer, single-consumer. In all cases, both sides are wait-free.
//!
//! - [`watch::lending::accumulate`](crate::watch::lending::accumulate)
//!   
//!   The sender accumulates into a mutable value until the receiver takes out the previous value.
//!   The channel lends out values mutably.
//!
//! - [`watch::lending::swap`](crate::watch::lending::swap)
//!   
//!   The receiver receives the latest value set the by the sender. The sender bounces between its
//!   staging buffer and a back buffer. The channel lends out values mutably.
//!
//! - [`bounded::lending::accumulate`](crate::bounded::lending::accumulate)
//!   
//!   The sender can publish values as long as there is room in the channel. The channel lends out
//!   values mutably. Effectively a bounded variant of the accumulating watch channel.
//!
//! - [`bounded::lending::overwrite`](crate::bounded::lending::overwrite)
//!   
//!   The sender can publish values as quickly as it wants, and overwrites unread values. The
//!   receiver will read values from oldest surviving to newest. The channel lends out values
//!   mutably.
//!
//! - [`bounded::moving::queue`](crate::bounded::moving::queue)
//!   
//!   The sender can only publish when there is space in the channel. The receiver reads values
//!   from oldest to newest. The channel transfers ownership of values.
//!   

#![no_std]

extern crate alloc;

pub mod bounded;
pub mod watch;
