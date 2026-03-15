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

#![no_std]

extern crate alloc;

pub mod wf_wf;
