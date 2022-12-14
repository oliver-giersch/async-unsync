//! **async-unsync** - Asynchronous channels for single-threaded use.
//!
//! This crate provides asynchronous but unsynchronized (`!Sync`) alternatives
//! to [`tokio::sync::mpsc`][1] channel types with almost identical APIs.
//!
//! Using synchronized data-structures in context that are statically known to
//! always execute on a single thread has non-trivial overhead.
//! The specialized (and much simpler) implementations in this library are
//! primarily intended for use in high-performance async code utilizing thread
//! local tasks and `!Send` futures.
//!
//! [1]: https://docs.rs/tokio/latest/tokio/sync/mpsc/index.html

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(feature = "std")]
mod alloc {
    pub use std::boxed;
    pub use std::collections;
    pub use std::rc;
}

#[cfg(not(feature = "std"))]
extern crate alloc;

pub mod bounded;
pub mod semaphore;
pub mod unbounded;

mod shared;

#[cfg(feature = "std")]
use std::error;

use core::fmt;

const UNCOUNTED: bool = false;
const COUNTED: bool = true;

/// An error that occurred trying to send a value through a closed channel.
#[derive(Clone, PartialEq, Eq)]
pub struct SendError<T>(pub T);

impl<T> fmt::Debug for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SendError").finish_non_exhaustive()
    }
}

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("failed to send value, channel closed")
    }
}

#[cfg(feature = "std")]
impl<T> error::Error for SendError<T> {}

/// An error that occurred trying to receive on a closed or empty channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TryRecvError {
    /// This **channel** is currently empty, but the **Sender**(s) have not yet disconnected,
    /// so data may yet become available.
    Empty,
    /// The **channel**’s sending half has become disconnected, and there will never be any more
    /// data received on it.
    Disconnected,
}

impl TryRecvError {
    /// Returns `true` if the error is [`TryRecvError::Empty`].
    pub fn is_empty(self) -> bool {
        matches!(self, Self::Empty)
    }

    /// Returns `true` if the error is [`TryRecvError::Disconnected`].
    pub fn is_disconnected(self) -> bool {
        matches!(self, Self::Disconnected)
    }
}

impl fmt::Display for TryRecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("channel is empty"),
            Self::Disconnected => f.write_str("channel is closed"),
        }
    }
}

#[cfg(feature = "std")]
impl error::Error for TryRecvError {}

/// An error that occurred trying to send a value through a closed or full
/// channel.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TrySendError<T> {
    Full(T),
    Closed(T),
}

impl<T> TrySendError<T> {
    /// Returns `true` if the error is [`TrySendError::Full`].
    pub fn is_full(&self) -> bool {
        matches!(self, Self::Full(_))
    }

    /// Returns `true` if the error is [`TrySendError::Closed`].
    pub fn is_closed(&self) -> bool {
        matches!(self, Self::Closed(_))
    }
}

impl<T> From<SendError<T>> for TrySendError<T> {
    fn from(err: SendError<T>) -> Self {
        Self::Closed(err.0)
    }
}

impl<T> fmt::Debug for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut dbg = f.debug_struct("TrySendError");
        match self {
            Self::Full(_) => dbg.field("full", &true).finish_non_exhaustive(),
            Self::Closed(_) => dbg.field("closed", &true).finish_non_exhaustive(),
        }
    }
}

impl<T> fmt::Display for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full(_) => f.write_str("failed to send value, channel full"),
            Self::Closed(_) => f.write_str("failed to send value, channel closed"),
        }
    }
}

#[cfg(feature = "std")]
impl<T> error::Error for TrySendError<T> {}

/// A bitmask storing whether a channel has been closed and the count of
/// currently senders.
struct Mask(usize);

impl Mask {
    fn new() -> Self {
        Self(0)
    }

    /// Resets the closed bit and sender count to one (if counted).
    fn reset<const COUNTED: bool>(&mut self) {
        if COUNTED {
            self.0 = 2;
        } else {
            self.0 = 0;
        }
    }

    /// Returns `true` if the closed bit is set.
    fn is_closed<const COUNTED: bool>(&self) -> bool {
        if COUNTED {
            self.0 & 0b1 == 1
        } else {
            self.0 == 1
        }
    }

    /// Sets the closed bit.
    fn close<const COUNTED: bool>(&mut self) {
        if COUNTED {
            self.0 |= 0b1;
        } else {
            self.0 = 1;
        }
    }

    // Increments the sender count by one.
    fn increase_sender_count(&mut self) {
        self.0 =
            self.0.checked_add(2).expect("cloning the sender would overflow the reference counter");
    }

    // Decrements the sender count by one and closes the queue if it reaches zero.
    fn decrease_sender_count(&mut self) {
        // can not underflow, starts at 2 and is only incremented ensuring no
        // overflow
        self.0 -= 2;
        if self.0 == 0 {
            self.0 = 1;
        }
    }
}
