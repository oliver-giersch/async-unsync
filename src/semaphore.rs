//! A simple asynchronous semaphore for limiting and sequencing access
//! to arbitrary shared resources.

use core::{
    cell::{Cell, UnsafeCell},
    fmt,
    future::Future,
    marker::PhantomPinned,
    mem::{self, MaybeUninit},
    pin::Pin,
    ptr::{self, NonNull},
    task::{Context, Poll, Waker},
};

/// An unsynchronized (`!Sync`), simple semaphore for asynchronous permit
/// acquisition.
pub struct Semaphore {
    shared: UnsafeCell<Shared>,
}

impl Semaphore {
    /// Creates a new semaphore with the initial number of permits.
    pub const fn new(permits: usize) -> Self {
        Self {
            shared: UnsafeCell::new(Shared { waiters: WaiterQueue::new(), permits, closed: false }),
        }
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
        unsafe { (*self.shared.get()).add_permits(n) };
    }

    /// Permanently reduces the number of available permits by `n`.
    pub fn remove_permits(&self, n: usize) {
        // SAFETY: no mutable or aliased access to shared possible
        let shared = unsafe { &mut (*self.shared.get()) };
        shared.permits = shared.permits.saturating_sub(n);
    }

    /// Acquires a single [`Permit`] or returns an [error](TryAcquireError), if
    /// there are no available permits.
    ///
    /// # Errors
    ///
    /// Fails, if the semaphore has been closed or has no available permits.
    pub fn try_acquire(&self) -> Result<Permit<'_>, TryAcquireError> {
        self.try_acquire_many(1)
    }

    /// Acquires `n` [`Permit`]s or returns an [error](TryAcquireError), if
    /// there are not enough available permits.
    ///
    /// # Errors
    ///
    /// Fails, if the semaphore has been closed or has not enough available
    /// permits.
    pub fn try_acquire_many(&self, n: usize) -> Result<Permit<'_>, TryAcquireError> {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.get()).try_acquire::<true>(n) }.map(|_| self.make_permit(n))
    }

    /// Acquires a single [`Permit`], potentially blocking until one becomes
    /// available.
    ///
    /// # Errors
    ///
    /// Fails, if the semaphore has been closed.
    pub async fn acquire(&self) -> Result<Permit<'_>, AcquireError> {
        self.acquire_many(1).await
    }

    /// Acquires `n` [`Permit`]s, potentially blocking until they become
    /// available.
    ///
    /// # Errors
    ///
    /// Fails, if the semaphore has been closed.
    pub async fn acquire_many(&self, n: usize) -> Result<Permit<'_>, AcquireError> {
        let _ = self.build_acquire(n).await?;
        Ok(self.make_permit(n))
    }

    /// Returns a new `Permit` without actually acquiring it.
    ///
    /// NOTE: Only use this to "revive" a Permit that has been explicitly
    /// [forgotten](Permit::forget)!
    pub(crate) fn make_permit(&self, count: usize) -> Permit<'_> {
        Permit { shared: &self.shared, count }
    }

    fn build_acquire(&self, wants: usize) -> Acquire<'_> {
        Acquire {
            shared: &self.shared,
            waiter: Waiter {
                wants,
                waker: LateInitWaker::new(),
                waiting: Cell::new(false),
                permits: Cell::new(0),
                next: Cell::new(ptr::null()),
                _pin: PhantomPinned,
            },
        }
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
    count: usize,
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
        unsafe { (*self.shared.get()).add_permits(self.count) };
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

#[cfg(feature = "alloc")]
impl From<TryAcquireError> for crate::error::TrySendError<()> {
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

/// The [`Future`] returned by [`acquire`](Semaphore::acquire), which
/// resolves when the required number of permits becomes available.
struct Acquire<'a> {
    /// The shared [`Semaphore`] state.
    shared: &'a UnsafeCell<Shared>,
    /// The state for waiting and resolving the future.
    waiter: Waiter,
}

impl Future for Acquire<'_> {
    type Output = Result<usize, AcquireError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: The `Acquire` future can not be moved before being dropped
        let waiter = unsafe { Pin::map_unchecked(self.as_ref(), |acquire| &acquire.waiter) };

        // SAFETY: no mutable or aliased access to shared possible
        match unsafe { (*self.shared.get()).poll_acquire(waiter, cx) } {
            Poll::Ready(res) => {
                // unconditionally setting waiting to false here avoids having
                // to traverse the waiter queue again when the future is
                // dropped.
                waiter.waiting.set(false);
                match res {
                    Ok(_) => {
                        let count = waiter.permits.take();
                        Poll::Ready(Ok(count))
                    }
                    Err(e) => Poll::Ready(Err(e)),
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for Acquire<'_> {
    fn drop(&mut self) {
        // SAFETY: no mutable or aliased access to shared possible
        let shared = unsafe { &mut (*self.shared.get()) };

        // remove the queued waker, if it was already enqueued
        if self.waiter.waiting.get() {
            // check, if there exists some entry in queue of waiters with the
            // same ID as this future
            unsafe { shared.waiters.try_remove(&self.waiter) };
        }

        // return all "unused" (i.e., not passed on into a [`Permit`]) permits
        // back to the semaphore
        let permits = self.waiter.permits.get();
        shared.add_permits(permits);
    }
}

/// The shared [`Semaphore`] state.
struct Shared {
    /// The queue of registered `Waker`s.
    waiters: WaiterQueue,
    /// The number of currently available permits.
    permits: usize,
    /// The flag indicating if the semaphore has been closed.
    closed: bool,
}

impl Shared {
    /// Closes the semaphore and notifies all remaining waiters.
    fn close(&mut self) -> usize {
        self.closed = true;
        // SAFETY: All waiters remain valid while they are enqueued
        let waiting = unsafe { self.waiters.len() };
        self.waiters = WaiterQueue::new();

        waiting
    }

    /// Returns `true` if the semaphore has been closed.
    fn is_closed(&self) -> bool {
        self.closed
    }

    /// Adds `n` permits and wakes all waiters whose requests can now be
    /// completed.
    fn add_permits(&mut self, mut n: usize) {
        while n > 0 {
            // keep checking the waiter queue until are permits are distributed
            if let Some(waiter) = self.waiters.front() {
                // SAFETY: All waiters remain valid while they are enqueued.
                let waiter = unsafe { waiter.as_ref() };
                // check, how many permits have already been assigned and
                // how many were requested
                let diff = waiter.wants - waiter.permits.get();
                if diff > n {
                    // waiter wants more permits than are still available
                    // the waiter gets all available permits & the loop
                    // terminated (n = 0)
                    waiter.permits.set(diff - n);
                    return;
                } else {
                    // the waiters request can be completed, assign all
                    // missing permits, wake the waiter, continue the loop
                    waiter.permits.set(waiter.wants);
                    n -= diff;

                    // SAFETY: All wakers are initialized when the `Waiter`s
                    // are enqueued and all waiters remain valid while they are
                    // enqueued.
                    unsafe {
                        waiter.waker.get().wake_by_ref();
                        // ...finally, dequeue the notified waker
                        self.waiters.pop_front(waiter);
                    };
                }
            } else {
                self.permits = self.permits.saturating_add(n);
                return;
            }
        }
    }

    /// Attempts to reduce available permits by up to `n` or returns an error,
    /// if the semaphore has been closed or has no available permits.
    fn try_acquire<const STRICT: bool>(&mut self, n: usize) -> Result<usize, TryAcquireError> {
        if self.is_closed() {
            return Err(TryAcquireError::Closed);
        }

        if n > self.permits {
            if STRICT || self.permits == 0 {
                return Err(TryAcquireError::NoPermits);
            }

            // hand out all available permits
            let count = self.permits;
            self.permits = 0;
            Ok(count)
        } else {
            // can not underflow because n <= permits
            self.permits -= n;
            Ok(n)
        }
    }

    fn poll_acquire(
        &mut self,
        waiter: Pin<&Waiter>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), AcquireError>> {
        // on first poll, check if there are enough permits to resolve
        // immediately or enqueue a waiter ticket to be notified (i.e. polled
        // again) later
        if !waiter.waiting.get() {
            return self.poll_acquire_initial(waiter, cx);
        }

        /*
         * on any subsequent poll (which may be spurious!), the following
         * conditions must be checked:
         *
         *   1. has the semaphore been closed in the meantime?
         *   2. is a waiter for this request still enqueued? (this will occur
         *      frequently with spurious polls)
         */

        if self.is_closed() {
            // a waiter *may* or *may not* be in the queue, but `Acquire::drop`
            // will take care of this eventually
            return Poll::Ready(Err(AcquireError(())));
        }

        /*
         * check, if the waiter is still enqueued in the `waiters` queue, in
         * which case the poll must be spurious, since any poll caused by a
         * deliberate wake-call would have been preceded by removing the ticket
         * from the queue
         */

        // SAFETY: The `AcquireState`s are part of each `Acquire` future and
        // thus share the same lifetime. When such a future is dropped, it will
        // make sure to remove itself from this list. Should a future be
        // "forgotten" it either exists on the heap and its memory location will
        // remain valid or, if it exists on the stack, it can't actually be
        // leaked, since it has to be pinned before it can insert itself into
        // the list
        if unsafe { self.waiters.contains(waiter.get_ref()) } {
            return Poll::Pending;
        }

        Poll::Ready(Ok(()))
    }

    fn poll_acquire_initial(
        &mut self,
        waiter: Pin<&Waiter>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), AcquireError>> {
        match self.try_acquire::<false>(waiter.wants) {
            Ok(n) => {
                // check if we got the desired amount or less
                waiter.permits.set(n);
                if n == waiter.wants {
                    return Poll::Ready(Ok(()));
                }
            }
            Err(TryAcquireError::Closed) => return Poll::Ready(Err(AcquireError(()))),
            _ => {}
        };

        // if no or not enough permits are currently available, enqueue a
        // waiter request ticket, to be notified when capacity becomes
        // available
        waiter.waiting.set(true);
        waiter.waker.set(cx.waker().clone());
        // SAFETY: All waiters remain valid while they are enqueued.
        //
        // Each `Acquire` future contains/owns a `Waiter` and may either live on
        // the stack or the heap.
        // Each future must be pinned before it can be polled and therefore both
        // the future and the waiter will remain in-place for their entire
        // lifetime.
        // When the future/waiter are cancelled or dropped, they will dequeue
        // themselves to ensure no iteration over freed data is possible.
        // Since they must be pinned, leaking or "forgetting" the futures does
        // not break this invariant:
        // In case of a heap-pinned future, the destructor will not be run, but
        // the data will still remain valid for the program duration.
        // In case of a future safely pinned to the stack, there is no way to
        // actually prevent the destructor from running, since only the pinned
        // reference can be leaked.
        unsafe { self.waiters.push_back(waiter.get_ref()) }

        return Poll::Pending;
    }
}

struct WaiterQueue {
    head: *const Waiter,
    tail: *const Waiter,
}

impl WaiterQueue {
    /// Returns a new empty queue.
    const fn new() -> Self {
        Self { head: ptr::null(), tail: ptr::null() }
    }

    /// Returns the first `Waiter` of `null`, if the queue is empty.
    fn front(&self) -> Option<NonNull<Waiter>> {
        NonNull::new(self.head as *mut Waiter)
    }

    /// Returns the number of currently enqueued `Waiter`s.
    ///
    /// # Safety
    ///
    /// All pointers must reference valid, live and non-aliased `Waiter`s.
    unsafe fn len(&self) -> usize {
        let mut curr = self.head;
        let mut waiting = 0;
        while !curr.is_null() {
            curr = unsafe { (*curr).next.get() };
            waiting += 1;
        }

        waiting
    }

    /// Returns `true` if the queue contains `waiter`.
    ///
    /// # Safety
    ///
    /// All pointers must reference valid, live and non-aliased `Waiter`s.
    unsafe fn contains(&self, waiter: *const Waiter) -> bool {
        let mut curr = self.head;
        while !curr.is_null() {
            if curr == waiter {
                return true;
            }

            let next = unsafe { (*curr).next.get() };
            curr = next;
        }

        false
    }

    /// Enqueues `waiter` at the back of the queue.
    ///
    /// # Safety
    ///
    /// All pointers must reference valid, live and non-aliased `Waiter`s.
    unsafe fn push_back(&mut self, waiter: *const Waiter) {
        if self.tail.is_null() {
            // queue is empty, insert waiter at head
            self.head = waiter;
            self.tail = waiter;
        } else {
            // queue is not empty, insert at tail
            unsafe { (*self.tail).next.set(waiter) };
            self.tail = waiter;
        }
    }

    /// Searches for `waiter` in the queue and removes it if found.
    ///
    /// # Safety
    ///
    /// All pointers must reference valid, live and non-aliased `Waiter`s.
    unsafe fn try_remove(&mut self, waiter: *const Waiter) {
        let mut pred = Cell::from_mut(&mut self.head);
        while !pred.get().is_null() {
            let curr = pred.get();
            let next = unsafe { &(*curr).next };
            if curr == waiter {
                pred.set(next.get());

                // if the last waiter is removed, tail must also become `null`
                if self.head.is_null() {
                    self.tail = ptr::null();
                }

                return;
            }

            pred = next;
        }
    }

    /// Removes `head` from the front of the queue.
    ///
    /// # Safety
    ///
    /// All pointers must reference valid, live and non-aliased `Waiter`s and
    /// `head` must be the current queue head.
    unsafe fn pop_front(&mut self, head: &Waiter) {
        self.head = head.next.get();
        if self.head.is_null() {
            self.tail = ptr::null();
        }
    }
}

/// A queue-able waiter that will be notified, when its requested number of
/// semaphore permits has been granted.
struct Waiter {
    /// The number of requested permits.
    wants: usize,
    /// The waker to be woken if the future is enqueued as waiting.
    ///
    /// This field is **never** used, if the waiter does not get enqueued,
    /// because its request can be fulfilled immediately.
    waker: LateInitWaker,
    /// The flag determining, whether this future has already been polled.
    waiting: Cell<bool>,
    /// The counter of already collected permits.
    permits: Cell<usize>,
    /// The pointer to the next enqueued waiter
    next: Cell<*const Self>,
    // see: https://gist.github.com/Darksonn/1567538f56af1a8038ecc3c664a42462
    // this marker lets miri pass the self-referential nature of this struct
    _pin: PhantomPinned,
}

/// The `Waker` in an `Acquire` future is only used in case it gets enqueued
/// in the `waiters` list or not at all.
///
/// `get` is only called during traversal of that list, so it is guaranteed to
/// have been initialized
struct LateInitWaker(UnsafeCell<MaybeUninit<Waker>>);

impl LateInitWaker {
    const fn new() -> Self {
        Self(UnsafeCell::new(MaybeUninit::uninit()))
    }

    fn set(&self, waker: Waker) {
        unsafe { (*self.0.get()).as_mut_ptr().write(waker) };
    }

    unsafe fn get(&self) -> &Waker {
        unsafe { (*self.0.get()).assume_init_ref() }
    }
}

#[cfg(test)]
mod tests {
    use futures_lite::future;

    use core::{
        future::Future as _,
        ptr,
        task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
    };

    #[test]
    fn try_acquire_one() {
        let sem = super::Semaphore::new(0);
        assert!(sem.try_acquire().is_err());
        sem.add_permits(2);
        let p1 = sem.try_acquire().unwrap();
        let p2 = sem.try_acquire().unwrap();
        assert_eq!(sem.available_permits(), 0);

        drop((p1, p2));
        assert_eq!(sem.available_permits(), 2);
    }

    #[test]
    fn try_acquire_many() {
        let sem = super::Semaphore::new(0);
        assert!(sem.try_acquire_many(3).is_err());
        sem.add_permits(2);
        assert!(sem.try_acquire_many(3).is_err());
        sem.add_permits(1);
        let permit = sem.try_acquire_many(3).unwrap();
        assert_eq!(permit.count, 3);
        drop(permit);
        assert_eq!(sem.available_permits(), 3);
    }

    #[test]
    fn acquire_one() {
        future::block_on(async {
            let sem = super::Semaphore::new(0);
            let mut fut = core::pin::pin!(sem.acquire());

            let permit = core::future::poll_fn(|cx| {
                assert!(fut.as_mut().poll(cx).is_pending());
                assert_eq!(sem.waiters(), 1);
                sem.add_permits(2);
                fut.as_mut().poll(cx)
            })
            .await
            .unwrap();

            assert_eq!(sem.available_permits(), 1);
            drop(permit);
            assert_eq!(sem.available_permits(), 2);
        });
    }

    #[test]
    fn poll_future() {
        static RAW_VTABLE: RawWakerVTable = RawWakerVTable::new(
            |_| RawWaker::new(ptr::null(), &RAW_VTABLE),
            |_| {},
            |_| {},
            |_| {},
        );

        let waker = unsafe { Waker::from_raw(RawWaker::new(ptr::null(), &RAW_VTABLE)) };
        let mut cx = Context::from_waker(&waker);

        let sem = super::Semaphore::new(0);
        let mut fut = Box::pin(sem.build_acquire(1));

        assert!(fut.as_mut().poll(&mut cx).is_pending());
        assert_eq!(sem.waiters(), 1);
        sem.add_permits(1);

        assert!(fut.as_mut().poll(&mut cx).is_ready());
        drop(fut);
        assert_eq!(sem.waiters(), 0);
    }

    #[test]
    fn acquire_two() {
        future::block_on(async {
            let sem = super::Semaphore::new(0);
            let mut fut1 = Box::pin(sem.acquire());
            let mut fut2 = Box::pin(sem.acquire());

            let permit = core::future::poll_fn(|cx| {
                // poll both futures once to establish order
                assert!(fut1.as_mut().poll(cx).is_pending());
                assert!(fut2.as_mut().poll(cx).is_pending());

                assert_eq!(sem.waiters(), 2);
                sem.add_permits(1);

                // due to established order, fut2 must not resolve before fut1
                assert!(fut2.as_mut().poll(cx).is_pending());
                // fut1 should resolve and the permit dropped right away,
                // allowing fut2 to resolve as well
                let res = fut1.as_mut().poll(cx);
                assert!(res.is_ready());
                assert_eq!(sem.waiters(), 1);
                res
            })
            .await
            .unwrap();

            drop(permit);
            assert_eq!(sem.waiters(), 0);

            let permit = fut2.await.unwrap();
            assert_eq!(sem.available_permits(), 0);
            drop(permit);

            assert_eq!(sem.available_permits(), 1);
        });
    }

    #[test]
    fn cleanup() {
        future::block_on(async {
            let sem = super::Semaphore::new(0);
            let mut fut = Box::pin(sem.acquire());

            // poll once to enqueue the future as waiting
            core::future::poll_fn(|cx| {
                assert!(fut.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;

            // dropping the future should clear up its queue entry
            drop(fut);
            assert_eq!(sem.waiters(), 0);
        });
    }

    #[test]
    fn cleanup_after_wake() {
        future::block_on(async {
            let sem = super::Semaphore::new(0);
            let mut fut = Box::pin(sem.acquire());

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
        future::block_on(async {
            let sem = super::Semaphore::new(1);
            let permit = sem.acquire().await.unwrap();

            let mut fut = Box::pin(sem.acquire());
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
            assert!(sem.try_acquire().is_err());
            assert!(sem.acquire().await.is_err());
        });
    }

    #[test]
    fn return_outstanding_permit_on_close() {
        future::block_on(async {
            let sem = super::Semaphore::new(1);
            let permit = sem.acquire().await.unwrap();

            let mut fut = Box::pin(sem.acquire());
            assert!(future::poll_once(&mut fut).await.is_none());
            assert_eq!(sem.waiters(), 1);

            // dropping a permit will transfer it to the next waiter, waking it
            drop(permit);
            assert_eq!(sem.waiters(), 0);
            assert_eq!(sem.available_permits(), 0);

            // closing by itself will not return the outstanding permit
            sem.close();
            assert_eq!(sem.available_permits(), 0);

            // ... but awaiting the Acquire future should!
            assert!(fut.await.is_err());
            assert_eq!(sem.available_permits(), 1);
        });
    }

    #[test]
    fn return_outstanding_permit_on_cancel() {
        future::block_on(async {
            let sem = super::Semaphore::new(0);

            let mut fut = Box::pin(sem.acquire());
            assert!(future::poll_once(&mut fut).await.is_none());
            assert_eq!(sem.waiters(), 1);

            sem.add_permits(1);
            assert_eq!(sem.waiters(), 0);

            // dropping the unresolved future must return the already
            // transferred permit ownership back to the semaphore
            drop(fut);

            assert_eq!(sem.waiters(), 0);
            assert_eq!(sem.available_permits(), 1);
        });
    }

    #[test]
    fn forget_acquire_future() {
        future::block_on(async {
            async fn acquire_and_forget(sem: &super::Semaphore) {
                let waiters = sem.waiters();
                let mut fut = std::pin::pin!(sem.acquire());
                assert!(future::poll_once(&mut fut).await.is_none());
                assert_eq!(sem.waiters(), waiters + 1);

                // this will not leak the future itself, but only the pinned
                // reference to it, so the actual future will still be dropped
                // correctly
                std::mem::forget(fut);
            }

            let sem = super::Semaphore::new(0);
            acquire_and_forget(&sem).await;
            assert_eq!(sem.waiters(), 0);

            // trash previously used stack space
            let mut arr = [0u8; 1000];
            for v in &mut arr {
                *v = 255;
            }

            let mut f1 = std::pin::pin!(sem.acquire());
            assert!(future::poll_once(&mut f1).await.is_none());
            let mut f2 = std::pin::pin!(sem.acquire());
            assert!(future::poll_once(&mut f2).await.is_none());
            let mut f3 = std::pin::pin!(sem.acquire());
            assert!(future::poll_once(&mut f3).await.is_none());

            assert_eq!(sem.waiters(), 3);
            assert_eq!(sem.available_permits(), 0);
            sem.add_permits(3);

            assert!(matches!(future::poll_once(&mut f1).await, Some(Ok(_))));
            assert!(matches!(future::poll_once(&mut f2).await, Some(Ok(_))));
            assert!(matches!(future::poll_once(&mut f3).await, Some(Ok(_))));

            assert_eq!(sem.waiters(), 0);
            assert_eq!(sem.available_permits(), 3);
        });
    }
}
