//! A simple asynchronous semaphore for limiting and sequencing access
//! to arbitrary shared resources.

use core::{
    cell::UnsafeCell,
    fmt,
    future::Future,
    mem,
    pin::Pin,
    task::{Context, Poll, Waker},
};

use crate::alloc::collections::VecDeque;

/// An unsynchronized (`!Sync`), simple semaphore for asynchronous permit
/// acquisition.
pub struct Semaphore {
    shared: UnsafeCell<Shared>,
}

impl Semaphore {
    /// Creates a new semaphore with the initial number of permits.
    pub const fn new(permits: usize) -> Self {
        Self { shared: UnsafeCell::new(Shared { waiters: VecDeque::new(), id_pool: 0, permits }) }
    }

    /// Closes the semaphore and returns the number of notified pending waiters.
    ///
    /// This prevents the semaphore from issuing new permits and notifies all
    /// pending waiters.
    pub fn close(&self) -> usize {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.get()).close() }
    }

    /// Returns `true` if the semaphore has been closed
    pub fn is_closed(&self) -> bool {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.get()).is_closed() }
    }

    /// Returns the number of currently registered [`Future`]s waiting for a
    /// [`Permit`].
    pub fn waiters(&self) -> usize {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.get()).waiters.len() }
    }

    /// Returns the current number of available permits.
    pub fn available_permits(&self) -> usize {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.get()).permits }
    }

    /// Adds `n` new permits to the semaphore.
    pub fn add_permits(&self, n: usize) {
        // SAFETY: no mutable or aliased access to shared possible
        let shared = unsafe { &mut (*self.shared.get()) };

        for _ in 0..n {
            shared.add_permit();
        }
    }

    /// Permanently reduces the number of available permits by `n`.
    pub fn remove_permits(&self, n: usize) {
        // SAFETY: no mutable or aliased access to shared possible
        let shared = unsafe { &mut (*self.shared.get()) };
        shared.permits = shared.permits.saturating_sub(n);
    }

    /// Acquires a [`Permit`] or returns [`None`] if there are no available
    /// permits.
    pub fn try_acquire_one(&self) -> Result<Permit<'_>, TryAcquireError> {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.get()).try_acquire_one() }.map(|_| self.make_permit())
    }

    /// Acquires a [`Permit`], potentially blocking the calling [`Future`] until
    /// one becomes available.
    pub async fn acquire_one(&self) -> Result<Permit<'_>, AcquireError> {
        // SAFETY: no mutable or aliased access to shared possible
        let id = unsafe { (*self.shared.get()).next_id() };
        Acquire { shared: &self.shared, id, waiting: false }.await
    }

    /// Returns a new `Permit` without actually acquiring it.
    ///
    /// NOTE: Only use this to "revive" a Permit that has been explicity
    /// [forgotten](Permit::forget)!
    pub(crate) fn make_permit(&self) -> Permit<'_> {
        Permit { shared: &self.shared }
    }
}

impl fmt::Debug for Semaphore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Semaphore")
            .field("is_closed", &self.is_closed())
            .field("available_permits", &self.available_permits())
            .field("waiters", &self.waiters())
            .finish_non_exhaustive()
    }
}

/// A permit representing access to the [`Semaphore`]'s guarded resource.
pub struct Permit<'a> {
    shared: &'a UnsafeCell<Shared>,
}

impl<'a> Permit<'a> {
    /// Drops the permit without returning it to the [`Semaphore`].
    ///
    /// This permanently reduces the number of available permits.
    pub fn forget(self) {
        mem::forget(self);
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.get()).add_permit() };
    }
}

impl fmt::Debug for Permit<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Permit").finish_non_exhaustive()
    }
}

/// An error which can occur when a [`Semaphore`] has been closed.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct AcquireError(());

impl fmt::Display for AcquireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("semaphore closed")
    }
}

#[cfg(feature = "std")]
impl std::error::Error for AcquireError {}

/// An error which can occur when a [`Semaphore`] has been closed or has no
/// available permits.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub enum TryAcquireError {
    /// The semaphore has been [closed](Semaphore::close) and can not issue new
    /// permits.
    Closed,
    /// The semaphore has no available permits.
    NoPermits,
}

impl From<TryAcquireError> for crate::TrySendError<()> {
    fn from(err: TryAcquireError) -> Self {
        match err {
            TryAcquireError::Closed => Self::Closed(()),
            TryAcquireError::NoPermits => Self::Full(()),
        }
    }
}

impl fmt::Display for TryAcquireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TryAcquireError::Closed => f.write_str("semaphore closed"),
            TryAcquireError::NoPermits => f.write_str("no permits available"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for TryAcquireError {}

type WaiterId = usize;

/// The [`Future`] returned by [`acquire_one`](Semaphore::acquire_one), which
/// resolves when a [`Permit`] becomes available.
struct Acquire<'a> {
    /// The shared [`Semaphore`] state.
    shared: &'a UnsafeCell<Shared>,
    /// The ID for this future.
    id: WaiterId,
    /// The flag determining, whether this future has already been polled.
    waiting: bool,
}

impl<'a> Future for Acquire<'a> {
    type Output = Result<Permit<'a>, AcquireError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Self { shared, id, waiting } = self.get_mut();

        // SAFETY: no mutable or aliased access to shared possible
        match unsafe { (*shared.get()).poll_acquire_one(*id, waiting, cx) } {
            Poll::Ready(res) => {
                *waiting = false;
                match res {
                    Ok(_) => Poll::Ready(Ok(Permit { shared })),
                    Err(e) => Poll::Ready(Err(e)),
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for Acquire<'_> {
    fn drop(&mut self) {
        // remove the queued waker, if it was already enqueued, otherwise, no
        // action is required for cleanup
        if self.waiting {
            // SAFETY: no mutable or aliased access to shared possible
            let shared = unsafe { &mut (*self.shared.get()) };
            // check, if there exists some entry in queue of waiters with the
            // same ID as this future
            if let Some(pos) = shared.woken(self.id) {
                // remove the enqueued waiting waker and forget about it
                let _ = shared.waiters.remove(pos).unwrap();
            } else {
                // the waker has already been dequed but the future was not
                // resolved (`waiting` was not reset to false!), so we either
                // wake the next waiter in line or add back a permit
                // NOTE: this can happen, if the waiting waker has already been
                // dequeued and the waker woken, but the future has not been
                // polled again before being dropped
                shared.add_permit();
            }
        }
    }
}

/// The shared [`Semaphore`] state.
struct Shared {
    /// The queue of registered `Waker`s.
    waiters: VecDeque<(Waker, WaiterId)>,
    /// The pool of uniquely assigned waiter IDs.
    ///
    /// NOTE: The LSB is used as a flag bit determining if the semaphore has
    /// been closed, so IDs always have to be incremented by 2.
    id_pool: WaiterId,
    /// The number of currently available permits.
    permits: usize,
}

impl Shared {
    fn close(&mut self) -> usize {
        self.id_pool |= 0b1;
        let waiters = self.waiters.len();
        for (waker, _) in self.waiters.drain(..) {
            waker.wake();
        }

        waiters
    }

    fn is_closed(&self) -> bool {
        self.id_pool & 0b1 != 0
    }

    /// Returns the next ID from the pool.
    ///
    /// Guaranteed to wrap around on overflow.
    fn next_id(&mut self) -> WaiterId {
        let id = self.id_pool & (usize::MAX - 1);

        // this may overflow, but it *should* be impossible for this to cause
        // ABA problem issues - there would have to be `usize::MAX + 1` queued
        // wakers for there to be an overlap of IDs, which is not supported by
        // the underlying data structure (see [`Vec::with_capacity`])
        self.id_pool = self.id_pool.wrapping_add(2);
        id
    }

    /// Returns the current position in the queue of waites for the given `id`
    /// or [`None`], if the waiter has already been woken.
    fn woken(&self, id: WaiterId) -> Option<usize> {
        self.waiters.iter().position(|(_, i)| id == *i)
    }

    /// Wakes the next waiter in line or returns a single permit.
    fn add_permit(&mut self) {
        match self.waiters.pop_front() {
            Some((waker, _)) => waker.wake(),
            None => self.permits += 1,
        }
    }

    /// Attempts to reduce available permits by one or returns `false`, if there
    /// are no available permits.
    fn try_acquire_one(&mut self) -> Result<(), TryAcquireError> {
        if self.is_closed() {
            return Err(TryAcquireError::Closed);
        }

        match self.permits.checked_sub(1) {
            Some(res) => {
                self.permits = res;
                Ok(())
            }
            None => Err(TryAcquireError::NoPermits),
        }
    }

    /// Polls the semaphore with a unique `id`.
    ///
    /// The given `waiting` state must be false on first poll. It will be set to
    /// `true` when the semaphore registers the given `cx`'s [`Waker`] and
    /// associates it with the given `id`.
    fn poll_acquire_one(
        &mut self,
        id: WaiterId,
        waiting: &mut bool,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), AcquireError>> {
        if !*waiting {
            // on first poll, check if there are enough permits or enqueue waker
            match self.try_acquire_one() {
                Ok(_) => Poll::Ready(Ok(())),
                Err(TryAcquireError::Closed) => Poll::Ready(Err(AcquireError(()))),
                Err(TryAcquireError::NoPermits) => {
                    // if no permits are currently available, associate the
                    // waker with the ID and register both with the semaphore
                    self.waiters.push_back((cx.waker().clone(), id));
                    *waiting = true;

                    Poll::Pending
                }
            }
        } else {
            // check, if the semaphore has been closed
            if self.is_closed() {
                *waiting = false;
                return Poll::Ready(Err(AcquireError(())));
            }

            // check, if polled by spurious wake, i.e., if the waiter ID is
            // still registered with the semaphore and has not yet been removed
            if self.woken(id).is_some() {
                return Poll::Pending;
            }

            // ...otherwise, the future can resolve, waiting must be set to
            // `false` here, this prevents us from having to check the waiter
            // queue again when the future is eventually dropped
            *waiting = false;
            Poll::Ready(Ok(()))
        }
    }
}

#[cfg(test)]
mod tests {
    use core::{future::Future as _, task::Poll};

    use crate::alloc::boxed::Box;

    use super::Semaphore;

    #[test]
    fn id_pool() {
        let mut shared = super::Shared {
            waiters: crate::alloc::collections::VecDeque::new(),
            id_pool: 0,
            permits: 1,
        };

        shared.close();
        assert_eq!(shared.next_id(), 0);
        assert_eq!(shared.next_id(), 2);
        assert_eq!(shared.next_id(), 4);
    }

    #[test]
    fn acquire_one() {
        futures_lite::future::block_on(async {
            let sem = Semaphore::new(0);
            let fut = sem.acquire_one();
            futures_lite::pin!(fut);

            let permit = core::future::poll_fn(|cx| {
                assert!(fut.as_mut().poll(cx).is_pending());
                sem.add_permits(2);
                fut.as_mut().poll(cx)
            })
            .await;

            assert_eq!(sem.available_permits(), 1);
            drop(permit);
            assert_eq!(sem.available_permits(), 2);
        });
    }

    #[test]
    fn acquire_two() {
        futures_lite::future::block_on(async {
            let sem = Semaphore::new(0);

            let fut1 = sem.acquire_one();
            let fut2 = sem.acquire_one();
            futures_lite::pin!(fut1, fut2);

            core::future::poll_fn(|cx| {
                // poll both futures once to establish order
                assert!(fut1.as_mut().poll(cx).is_pending());
                assert!(fut2.as_mut().poll(cx).is_pending());

                sem.add_permits(1);

                // due to established order, fut2 must not resolve before fut1
                assert!(fut2.as_mut().poll(cx).is_pending());
                // fut1 should resolve and the permit dropped right away,
                // allowing fut2 to resolve as well
                assert!(fut1.as_mut().poll(cx).is_ready());
                assert!(fut2.as_mut().poll(cx).is_ready());
                Poll::Ready(())
            })
            .await;

            assert_eq!(sem.available_permits(), 1);
        });
    }

    #[test]
    fn cleanup() {
        futures_lite::future::block_on(async {
            let sem = Semaphore::new(0);
            let mut fut = Box::pin(sem.acquire_one());

            core::future::poll_fn(|cx| {
                // poll once to enque the future as waiting
                assert!(fut.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;

            // dropping the future should clear up its queue entry
            drop(fut);

            let waiters = unsafe { (*sem.shared.get()).waiters.len() };
            assert_eq!(waiters, 0);
        });
    }

    #[test]
    fn cleanup_after_wake() {
        futures_lite::future::block_on(async {
            let sem = Semaphore::new(0);
            let mut fut = Box::pin(sem.acquire_one());

            core::future::poll_fn(|cx| {
                // poll once to enque the future as waiting
                assert!(fut.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;

            // adding a permit will wake the Acquire future instead of increasing the amount of
            // available permits
            sem.add_permits(1);
            // dropping the future should return the added permit instead of removing the waker from
            // the queue
            drop(fut);

            assert_eq!(sem.waiters(), 0);
            assert_eq!(sem.available_permits(), 1);
        });
    }

    #[test]
    fn close() {
        futures_lite::future::block_on(async {
            let sem = Semaphore::new(1);
            let permit = sem.acquire_one().await.unwrap();

            let mut fut = Box::pin(sem.acquire_one());
            core::future::poll_fn(|cx| {
                // poll once to enque the future as waiting
                assert!(fut.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;

            assert_eq!(sem.close(), 1);
            assert_eq!(sem.waiters(), 0);

            core::future::poll_fn(|cx| {
                // closing the semaphore should have woken the future
                match fut.as_mut().poll(cx) {
                    Poll::Ready(Err(_)) => Poll::Ready(()),
                    _ => panic!("acquire future should have resolved"),
                }
            })
            .await;

            // dropping the future should have no effect
            drop(fut);
            assert_eq!(sem.available_permits(), 0);
            drop(permit);
            assert_eq!(sem.available_permits(), 1);

            // no further permits can be acquired
            assert!(sem.try_acquire_one().is_err());
            assert!(sem.acquire_one().await.is_err());
        });
    }
}
