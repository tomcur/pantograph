//! `pantograph` provides channels for sending payloads across threads, where at least one side of
//! the channel is *wait-free*.
//!
//! Synchronization being "wait-free" is a stronger guarantee than "lock-free." Lock-free
//! guarantees the full system makes progress and cannot get in a deadlock; wait-free additionally
//! guarantees per-thread progress. Wait-free channels are useful for problems that must meet
//! particular timing requirements, but they are not necessarily faster than lock-free channels.
//!
//! The channels are organized into three groups.
//! - In `lf_wf`, producer(s) are lock-free, consumer(s) are wait-free.
//! - In `wf_lf`, producer(s) are wait-free, consumer(s) are lock-free.
//! - In [`wf_wf`], producer(s) and consumer(s) are wait-free.

#![no_std]

extern crate alloc;

pub mod wf_wf;
