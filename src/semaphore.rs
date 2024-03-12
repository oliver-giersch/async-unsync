//! A simple asynchronous semaphore for limiting and sequencing access
//! to arbitrary shared resources.

use core::{
    cell::{Cell, UnsafeCell},
    fmt,
    future::Future,
    marker::PhantomPinned,
    mem,
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
            shared: UnsafeCell::new(Shared {
                waiters: WaiterQueue::new(),
                permits,
                outstanding_permits: 0,
                closed: false,
            }),
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

    /// Returns the current number of handed out permits.
    pub fn outstanding_permits(&self) -> usize {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.get()).outstanding_permits }
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

    /// Returns `n` previously "forgotten" permits back to the semaphore.
    ///
    /// This method complements [`Permit::forget`] and has no other uses.
    pub fn return_permits(&self, n: usize) {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.get()).return_permits(n) }
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
        unsafe { (*self.shared.get()).try_acquire::<true>(n) }.map(|_| Permit::new(&self.shared, n))
    }

    /// Acquires a single [`Permit`], potentially blocking until one becomes
    /// available.
    ///
    /// # Errors
    ///
    /// Awaiting the [`Future`] fails, if the semaphore has been closed.
    pub fn acquire(&self) -> Acquire<'_> {
        self.build_acquire(1)
    }

    /// Acquires `n` [`Permit`]s, potentially blocking until they become
    /// available.
    ///
    /// # Errors
    ///
    /// Awaiting the [`Future`] fails, if the semaphore has been closed.
    pub fn acquire_many(&self, n: usize) -> Acquire<'_> {
        self.build_acquire(n)
    }

    /// Returns an correctly initialized [`Acquire`] future instance for
    /// acquiring `wants` permits.
    fn build_acquire(&self, wants: usize) -> Acquire<'_> {
        Acquire {
            shared: &self.shared,
            waiter: Waiter {
                wants,
                waker: LateInitWaker::new(),
                state: Cell::new(WaiterState::Inert),
                permits: Cell::new(0),
                next: Cell::new(ptr::null()),
                prev: Cell::new(ptr::null()),
                _marker: PhantomPinned,
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
    /// Returns a new [`Permit`] without actually acquiring it.
    ///
    /// NOTE: Only use this to "revive" a Permit that has been explicitly
    /// [forgotten](Permit::forget)!
    fn new(shared: &'a UnsafeCell<Shared>, count: usize) -> Self {
        Self { shared, count }
    }

    /// Drops the permit without returning it to the [`Semaphore`].
    ///
    /// This permanently reduces the number of available permits.
    pub fn forget(self) {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.get()).outstanding_permits -= self.count };
        mem::forget(self);
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        // SAFETY: no mutable or aliased access to shared possible
        let shared = unsafe { &mut (*self.shared.get()) };
        shared.return_permits(self.count);
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
pub struct Acquire<'a> {
    /// The shared [`Semaphore`] state.
    shared: &'a UnsafeCell<Shared>,
    /// The state for waiting and resolving the future.
    waiter: Waiter,
}

impl<'a> Future for Acquire<'a> {
    type Output = Result<Permit<'a>, AcquireError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: The `Acquire` future can not be moved before being dropped
        let waiter = unsafe { Pin::map_unchecked(self.as_ref(), |acquire| &acquire.waiter) };

        // SAFETY: no mutable or aliased access to shared possible
        match unsafe { (*self.shared.get()).poll_acquire(waiter, cx) } {
            Poll::Ready(res) => {
                // unconditionally setting waiting to false here avoids having
                // to traverse the waiter queue again when the future is
                // dropped.
                waiter.state.set(WaiterState::Woken);
                match res {
                    Ok(_) => {
                        let shared = self.as_ref().shared;
                        let count = waiter.permits.take();
                        Poll::Ready(Ok(Permit::new(shared, count)))
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
        if let WaiterState::Waiting = self.waiter.state.get() {
            // check, if there exists some entry in queue of waiters with the
            // same ID as this future
            // SAFETY: non-live waiters did not exist in queue, no aliased
            // access possible
            unsafe { shared.waiters.try_remove(&self.waiter) };
        }

        // return all "unused" (i.e., not passed on into a [`Permit`]) permits
        // back to the semaphore
        let permits = self.waiter.permits.get();
        // the order is important here, because `add_permits` may mark permits
        // as handed out again, if they are transfered to other waiters
        shared.outstanding_permits -= permits;
        shared.add_permits(permits);
    }
}

/// The shared [`Semaphore`] accounting state.
struct Shared {
    /// The queue of registered `Waker`s.
    waiters: WaiterQueue,
    /// The number of currently available permits.
    permits: usize,
    /// The number of currently outstanding permits.
    outstanding_permits: usize,
    /// The flag indicating if the semaphore has been closed.
    closed: bool,
}

impl Shared {
    /// Closes the semaphore and notifies all remaining waiters.
    fn close(&mut self) -> usize {
        // SAFETY: non-live waiters di not exist in queue, no aliased access
        // possible
        let woken = unsafe { self.waiters.wake_all() };
        self.closed = true;
        self.waiters = WaiterQueue::new();

        woken
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
                    self.outstanding_permits = self.outstanding_permits.saturating_add(diff - n);
                    return;
                } else {
                    // the waiters request can be completed, assign all
                    // missing permits, wake the waiter, continue the loop
                    waiter.permits.set(waiter.wants);
                    self.outstanding_permits = self.outstanding_permits.saturating_add(diff);
                    n -= diff;

                    // SAFETY: All wakers are initialized when the `Waiter`s
                    // are enqueued and all waiters remain valid while they are
                    // enqueued.
                    unsafe {
                        waiter.state.set(WaiterState::Woken);
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

    fn return_permits(&mut self, n: usize) {
        // underflow *would* be possible, since `Permit`s can be forgotten,
        // which reduces outstanding permits and then be created again
        // (internally) "out of thin air"; the order is important here, because
        // `add_permits` may mark permits as handed out again, if they are
        // transfered to other waiters
        self.outstanding_permits = self.outstanding_permits.saturating_sub(n);
        self.add_permits(n);
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
            self.outstanding_permits = self.outstanding_permits.saturating_add(count);
            Ok(count)
        } else {
            // can not underflow because n <= permits
            self.permits -= n;
            self.outstanding_permits = self.outstanding_permits.saturating_add(n);
            Ok(n)
        }
    }

    fn poll_acquire(
        &mut self,
        waiter: Pin<&Waiter>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), AcquireError>> {
        if self.closed {
            // a waiter *may* or *may not* be in the queue, but `Acquire::drop`
            // will take care of this eventually
            return Poll::Ready(Err(AcquireError(())));
        }

        match waiter.state.get() {
            WaiterState::Woken => Poll::Ready(Ok(())),
            WaiterState::Inert => self.poll_acquire_initial(waiter, cx),
            WaiterState::Waiting => Poll::Pending,
        }
    }

    fn poll_acquire_initial(
        &mut self,
        waiter: Pin<&Waiter>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), AcquireError>> {
        // on first poll, check if there are enough permits to resolve
        // immediately or enqueue a waiter ticket to be notified (i.e. polled
        // again) later
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
        waiter.state.set(WaiterState::Waiting);
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
        Poll::Pending
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
            // SAFETY: curr is non-null, validity is required by function safety
            curr = unsafe { (*curr).next.get() };
            waiting += 1;
        }

        waiting
    }

    /// Enqueues `waiter` at the back of the queue.
    ///
    /// # Safety
    ///
    /// All pointers must reference valid, live and non-aliased `Waiter`s.
    unsafe fn push_back(&mut self, waiter: &Waiter) {
        if self.tail.is_null() {
            // queue is empty, insert waiter at head
            self.head = waiter;
            self.tail = waiter;
        } else {
            // queue is not empty, insert at tail
            // SAFETY: non-live waiters did not exist in queue, no aliased
            // access possible
            unsafe { (*self.tail).next.set(waiter) };
            waiter.prev.set(self.tail);
            self.tail = waiter;
        }
    }

    /// Searches for `waiter` in the queue and removes it if found.
    ///
    /// # Safety
    ///
    /// All pointers must reference valid, live and non-aliased `Waiter`s.
    unsafe fn try_remove(&mut self, waiter: &Waiter) {
        let prev = waiter.prev.get();
        if prev.is_null() {
            self.head = waiter.next.get();
        } else {
            // SAFETY: prev is non-null, liveness required by function invariant
            unsafe { (*prev).next.set(waiter.next.get()) };
        }

        let next = waiter.next.get();
        if next.is_null() {
            self.tail = waiter.prev.get();
        } else {
            // SAFETY: next non-null, liveness required by function invariant
            unsafe { (*next).prev.set(waiter.prev.get()) };
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
        } else {
            unsafe { (*self.head).prev.set(ptr::null()) };
        }
    }

    unsafe fn wake_all(&mut self) -> usize {
        let mut curr = self.head;
        let mut woken = 0;

        while !curr.is_null() {
            // SAFETY: liveness/non-aliasedness required for all waiters by
            // function invariant, curr is non-null and valid
            unsafe {
                let waiter = &*curr;
                waiter.state.set(WaiterState::Woken);
                waiter.waker.get().wake_by_ref();
                curr = waiter.next.get();
            }

            woken += 1;
        }

        woken
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
    /// The flag indicating the waiter's state.
    state: Cell<WaiterState>,
    /// The counter of already collected permits.
    permits: Cell<usize>,
    /// The pointer to the next enqueued waiter
    next: Cell<*const Self>,
    /// The pointer to the previous enqueued waiter
    prev: Cell<*const Self>,
    // see: https://gist.github.com/Darksonn/1567538f56af1a8038ecc3c664a42462
    // this marker lets miri pass the self-referential nature of this struct
    _marker: PhantomPinned,
}

/// The current state of a [`Waiter`].
#[derive(Clone, Copy)]
enum WaiterState {
    /// The waiter is inert and its future has not yet been polled.
    Inert,
    /// The waiter's future has been polled and the waiter was enqueued.
    Waiting,
    /// The waiter's future has been polled to completion.
    ///
    /// If the waiter had been queued it is now no longer queued.
    Woken,
}

/// The `Waker` in an `Acquire` future is only used in case it gets enqueued
/// in the `waiters` list or not at all.
///
/// `get` is only called during traversal of that list, so it is guaranteed to
/// have been initialized
struct LateInitWaker(UnsafeCell<Option<Waker>>);

impl LateInitWaker {
    const fn new() -> Self {
        Self(UnsafeCell::new(None))
    }

    fn set(&self, waker: Waker) {
        // SAFETY: no mutable or aliased access to waker possible, writing the
        // waker is unproblematic due to the required liveness of the pointer.
        // this is never called when there already is a waker
        unsafe { self.0.get().write(Some(waker)) };
    }

    unsafe fn get(&self) -> &Waker {
        // SAFETY: initness required as function invariant
        match &*self.0.get() {
            Some(waker) => waker,
            None => core::hint::unreachable_unchecked(),
        }
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
        assert_eq!(sem.outstanding_permits(), 2);

        drop((p1, p2));
        assert_eq!(sem.available_permits(), 2);
        assert_eq!(sem.outstanding_permits(), 0);
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
        assert_eq!(sem.outstanding_permits(), 3);
        drop(permit);
        assert_eq!(sem.available_permits(), 3);
        assert_eq!(sem.outstanding_permits(), 0);
    }

    #[test]
    fn acquire_never() {
        future::block_on(async {
            let sem = super::Semaphore::new(0);
            let mut fut = core::pin::pin!(sem.acquire());

            core::future::poll_fn(|cx| {
                assert!(fut.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;

            assert_eq!(sem.available_permits(), 0);
            assert_eq!(sem.outstanding_permits(), 0);
        });
    }

    #[test]
    fn acquire() {
        future::block_on(async {
            let sem = super::Semaphore::new(0);
            let mut fut = core::pin::pin!(sem.acquire());
            core::future::poll_fn(|cx| {
                assert!(fut.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;

            sem.add_permits(1);
            let permit = fut.await.unwrap();
            drop(permit);
            assert_eq!(sem.available_permits(), 1);
        });
    }

    #[test]
    fn acquire_one() {
        future::block_on(async {
            let sem = super::Semaphore::new(0);
            let mut fut = core::pin::pin!(sem.acquire());

            // poll future once to enqueue waiter
            core::future::poll_fn(|cx| {
                assert!(fut.as_mut().poll(cx).is_pending());
                assert_eq!(sem.waiters(), 1);
                // add 2 permits, one goes directly to the enqueued waiter and
                // wakes it, one goes into the semaphore
                sem.add_permits(2);
                assert_eq!(sem.outstanding_permits(), 1);
                Poll::Ready(())
            })
            .await;

            // future must resolve now, since it has been woken
            let permit = fut.await.unwrap();
            assert_eq!(sem.available_permits(), 1);
            assert_eq!(sem.outstanding_permits(), 1);
            drop(permit);
            assert_eq!(sem.available_permits(), 2);
            assert_eq!(sem.outstanding_permits(), 0);
        });
    }

    #[test]
    fn poll_acquire_after_completion() {
        future::block_on(async {
            let sem = super::Semaphore::new(0);
            let mut fut = core::pin::pin!(sem.acquire());
            core::future::poll_fn(|cx| {
                assert!(fut.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;

            sem.add_permits(1);

            core::future::poll_fn(|cx| {
                assert!(fut.as_mut().poll(cx).is_ready());
                // polling again after completion, works in this case, but might
                // cause a Waker leak under other circumstances.
                assert!(fut.as_mut().poll(cx).is_ready());
                Poll::Ready(())
            })
            .await;

            assert_eq!(sem.available_permits(), 1);
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
        assert_eq!(sem.outstanding_permits(), 1);

        assert!(fut.as_mut().poll(&mut cx).is_ready());
        drop(fut);
        assert_eq!(sem.waiters(), 0);
    }

    #[test]
    fn acquire_many() {
        future::block_on(async {
            let sem = super::Semaphore::new(0);
            let mut f1 = Box::pin(sem.acquire_many(2));
            let mut f2 = Box::pin(sem.acquire_many(1));

            core::future::poll_fn(|cx| {
                // poll both futures once to establish order
                assert!(f1.as_mut().poll(cx).is_pending());
                assert!(f2.as_mut().poll(cx).is_pending());

                assert_eq!(sem.waiters(), 2);
                sem.add_permits(1);
                assert_eq!(sem.outstanding_permits(), 1);

                // due to established order, f2 must not resolve before f1
                assert!(f2.as_mut().poll(cx).is_pending());

                // adding another permit must wake f1
                sem.add_permits(1);
                assert_eq!(sem.outstanding_permits(), 2);
                assert_eq!(sem.waiters(), 1);
                Poll::Ready(())
            })
            .await;

            // f1 should resolve now
            let permit = f1.await.unwrap();
            assert_eq!(sem.waiters(), 1);
            assert_eq!(sem.outstanding_permits(), 2);

            // dropping the permit must pass one permit to the next waiter,
            // wake it and return the other permit back to the semaphore
            drop(permit);
            assert_eq!(sem.waiters(), 0);
            assert_eq!(sem.available_permits(), 1);
            assert_eq!(sem.outstanding_permits(), 1);

            let permit = f2.await.unwrap();
            assert_eq!(sem.available_permits(), 1);
            assert_eq!(sem.outstanding_permits(), 1);
            drop(permit);

            assert_eq!(sem.available_permits(), 2);
            assert_eq!(sem.outstanding_permits(), 0);
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

            // dropping the future should clear up its queue entry immediately
            drop(fut);
            assert_eq!(sem.waiters(), 0);
            assert_eq!(sem.outstanding_permits(), 0);

            let mut fut = Box::pin(sem.acquire());
            // poll once to enqueue the future as waiting
            core::future::poll_fn(|cx| {
                assert!(fut.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;

            // add 1 permit to wake future
            sem.add_permits(1);
            // ..and close semaphore
            assert_eq!(sem.close(), 0);

            assert!(fut.await.is_err());
            assert_eq!(sem.waiters(), 0);
            assert_eq!(sem.available_permits(), 1);
            assert_eq!(sem.outstanding_permits(), 0);
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

            // adding a permit will wake the Acquire future instead of
            // increasing the amount of available permits
            sem.add_permits(1);
            assert_eq!(sem.outstanding_permits(), 1);
            // dropping the future should return the added permit instead of
            // removing the waker from the queue
            drop(fut);

            assert_eq!(sem.waiters(), 0);
            assert_eq!(sem.available_permits(), 1);
            assert_eq!(sem.outstanding_permits(), 0);
        });
    }

    #[test]
    fn close() {
        future::block_on(async {
            let sem = super::Semaphore::new(1);
            let permit = sem.acquire().await.unwrap();
            assert_eq!(sem.outstanding_permits(), 1);

            let mut f1 = Box::pin(sem.acquire());
            let mut f2 = Box::pin(sem.acquire());
            core::future::poll_fn(|cx| {
                // poll once to enqueue the futures as waiting
                assert!(f1.as_mut().poll(cx).is_pending());
                assert!(f2.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;

            assert_eq!(sem.waiters(), 2);
            assert_eq!(sem.close(), 2);
            assert_eq!(sem.waiters(), 0);

            core::future::poll_fn(|cx| {
                // closing the semaphore should have woken the future
                match f1.as_mut().poll(cx) {
                    Poll::Ready(Err(_)) => Poll::Ready(()),
                    _ => panic!("acquire future should have resolved"),
                }
            })
            .await;

            // dropping the resolved future should have no effect
            drop(f1);
            assert_eq!(sem.available_permits(), 0);
            assert_eq!(sem.outstanding_permits(), 1);
            // awaiting f2 must not deadlock, even if not polled manually
            assert!(f2.await.is_err());

            // dropping the permit must return even though the semaphore has
            // been closed
            drop(permit);
            assert_eq!(sem.available_permits(), 1);
            assert_eq!(sem.outstanding_permits(), 0);

            // no further permits must be acquirable
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
            assert_eq!(sem.outstanding_permits(), 1);

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
