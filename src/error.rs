//! Error types for fallible channel operations.

use core::fmt;

use crate::semaphore::TryAcquireError;

/// An error which can occur when sending a value through a closed channel.
#[derive(Clone, Copy, PartialEq, Eq)]
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
impl<T> std::error::Error for SendError<T> {}

/// An error which can occur when receiving on a closed or empty channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TryRecvError {
    /// This **channel** is currently empty, but the **Sender**(s) have not yet
    /// disconnected, so data may yet become available.
    Empty,
    /// The **channel**’s sending half has become disconnected, and there will
    /// never be any more data received on it.
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
impl std::error::Error for TryRecvError {}

/// An error which can occur when sending a value through a closed or full
/// channel.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TrySendError<T> {
    /// This **channel** is currently full.
    Full(T),
    /// This **channel**'s receiving half has been explicitly disconnected or
    /// dropped and the channel was closed.
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

    pub(crate) fn set<U>(self, value: U) -> TrySendError<U> {
        match self {
            Self::Full(_) => TrySendError::Full(value),
            Self::Closed(_) => TrySendError::Closed(value),
        }
    }
}

impl<T> From<SendError<T>> for TrySendError<T> {
    fn from(err: SendError<T>) -> Self {
        Self::Closed(err.0)
    }
}

impl<T> From<(TryAcquireError, T)> for TrySendError<T> {
    fn from((err, elem): (TryAcquireError, T)) -> Self {
        match err {
            TryAcquireError::Closed => Self::Closed(elem),
            TryAcquireError::NoPermits => Self::Full(elem),
        }
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
impl<T> std::error::Error for TrySendError<T> {}
