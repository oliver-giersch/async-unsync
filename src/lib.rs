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

#![cfg_attr(all(not(test), not(feature = "std")), no_std)]
#![warn(clippy::inconsistent_struct_constructor)]
#![warn(clippy::match_same_arms)]
#![warn(clippy::missing_errors_doc)]
#![warn(clippy::missing_panics_doc)]
#![warn(clippy::missing_safety_doc)]
#![warn(clippy::undocumented_unsafe_blocks)]
#![warn(missing_docs)]

#[cfg(feature = "std")]
mod alloc {
    pub use std::boxed;
    pub use std::collections;
    pub use std::rc;
}

#[cfg(all(feature = "alloc", not(feature = "std")))]
extern crate alloc;

#[cfg(feature = "alloc")]
pub mod bounded;
#[cfg(feature = "alloc")]
pub mod oneshot;
#[cfg(feature = "alloc")]
pub mod unbounded;

pub mod semaphore;

#[cfg(feature = "alloc")]
pub use crate::error::*;

#[cfg(feature = "alloc")]
mod error;
#[cfg(feature = "alloc")]
mod mask;
#[cfg(feature = "alloc")]
mod shared;
