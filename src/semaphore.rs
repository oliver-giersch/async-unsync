//! A simple async, unsync (!Sync) Semaphore for limiting and sequencing access
//! to shared resources.

use core::{
    cell::UnsafeCell,
    future::Future,
    mem,
    pin::Pin,
    task::{Context, Poll, Waker},
};

use crate::alloc::collections::VecDeque;

type WaiterId = usize;

/// A simple semaphore for asynchronous permit acquisition.
pub struct Semaphore {
    shared: UnsafeCell<Shared>,
}

impl Semaphore {
    /// Creates a new semaphore with the initial number of permits.
    pub fn new(permits: usize) -> Self {
        Self { shared: UnsafeCell::new(Shared { waiters: VecDeque::new(), id_pool: 0, permits }) }
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
    pub fn try_acquire_one(&self) -> Option<Permit<'_>> {
        // SAFETY: no mutable or aliased access to shared possible
        if unsafe { (*self.shared.get()).try_acquire_one() } {
            Some(Permit { shared: &self.shared })
        } else {
            None
        }
    }

    /// Acquires a [`Permit`], potentially blocking the calling [`Future`] until
    /// one becomes available.
    pub async fn acquire_one(&self) -> Permit<'_> {
        // SAFETY: no mutable or aliased access to shared possible
        let id = unsafe { (*self.shared.get()).next_id() };
        Acquire { shared: &self.shared, id, waiting: false }.await
    }
}

/// A permit representing access to the [`Semaphore`]'s guarded resource.
pub struct Permit<'a> {
    shared: &'a UnsafeCell<Shared>,
}

impl Permit<'_> {
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
    type Output = Permit<'a>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Self { shared, id, waiting } = self.get_mut();

        // SAFETY: no mutable or aliased access to shared possible
        match unsafe { (*shared.get()).poll_acquire_one(*id, waiting, cx) } {
            Poll::Ready(_) => {
                *waiting = false;
                Poll::Ready(Permit { shared: *shared })
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
            if let WakeStatus::Waiting(pos) = shared.woken(self.id) {
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
    id_pool: WaiterId,
    /// The number of currently available permits.
    permits: usize,
}

impl Shared {
    /// Returns the next ID from the pool.
    ///
    /// Guaranteed to wrap around on overflow.
    fn next_id(&mut self) -> WaiterId {
        // this may overflow, but it *should* be impossible for this to cause
        // ABA problem issues - there would have to be `usize::MAX + 1` queued
        // wakers for there to be an overlap of IDs, which is not supported by
        // the underlying data structure
        self.id_pool = self.id_pool.wrapping_add(1);
        self.id_pool
    }

    /// Returns the current [`WakeStatus`] for the given `id`.
    fn woken(&self, id: WaiterId) -> WakeStatus {
        match self.waiters.iter().position(|(_, i)| id == *i) {
            Some(pos) => WakeStatus::Waiting(pos),
            None => WakeStatus::Woken,
        }
    }

    /// Wakes the next waiter in line or returns a single permit.
    fn add_permit(&mut self) {
        match self.waiters.pop_front() {
            Some((waker, _)) => waker.wake(),
            None => self.permits += 1,
        }
    }

    /// Attempts to reduce available permits by one or returns `false`.
    fn try_acquire_one(&mut self) -> bool {
        self.permits.checked_sub(1).map_or(false, |_| true)
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
    ) -> Poll<()> {
        if !*waiting {
            // on first poll, check if there are enough permits or enqueue waker
            if self.try_acquire_one() {
                Poll::Ready(())
            } else {
                // if no permits are currently available, associate the waker
                // with the ID and register both with the semaphore
                self.waiters.push_back((cx.waker().clone(), id));
                *waiting = true;

                Poll::Pending
            }
        } else {
            // check, if polled by spurious wake, i.e., if the waiter ID is
            // still registered with the semaphore and has not yet been removed
            if matches!(self.woken(id), WakeStatus::Waiting(_)) {
                return Poll::Pending;
            }

            // ...otherwise, the future can resolve, waiting must be set to
            // `false` here, this prevents us from having to check the waiter
            // queue again when the future is eventually dropped
            *waiting = false;
            Poll::Ready(())
        }
    }
}

enum WakeStatus {
    Woken,
    Waiting(usize),
}

#[cfg(test)]
mod tests {
    use core::{future::Future as _, task::Poll};

    use crate::alloc::boxed::Box;

    use super::Semaphore;

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

            let waiters = unsafe { (*sem.shared.get()).waiters.len() };
            assert_eq!(waiters, 0);
            assert_eq!(sem.available_permits(), 1);
        });
    }
}
